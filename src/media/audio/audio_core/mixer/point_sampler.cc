// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/media/audio/audio_core/mixer/point_sampler.h"

#include <fuchsia/media/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include <memory>
#include <utility>

#include "fidl/fuchsia.mediastreams/cpp/wire_types.h"
#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/lib/processing/gain.h"
#include "src/media/audio/lib/processing/point_sampler.h"
#include "src/media/audio/lib/processing/position_manager.h"
#include "src/media/audio/lib/processing/sampler.h"

namespace media::audio::mixer {

namespace {

using ::media_audio::Fixed;
using ::media_audio::PositionManager;
using ::media_audio::Sampler;

fuchsia_mediastreams::wire::AudioSampleFormat ToNewSampleFormat(
    fuchsia::media::AudioSampleFormat sample_format) {
  switch (sample_format) {
    case fuchsia::media::AudioSampleFormat::UNSIGNED_8:
      return fuchsia_mediastreams::wire::AudioSampleFormat::kUnsigned8;
    case fuchsia::media::AudioSampleFormat::SIGNED_16:
      return fuchsia_mediastreams::wire::AudioSampleFormat::kSigned16;
    case fuchsia::media::AudioSampleFormat::SIGNED_24_IN_32:
      return fuchsia_mediastreams::wire::AudioSampleFormat::kSigned24In32;
    case fuchsia::media::AudioSampleFormat::FLOAT:
    default:
      return fuchsia_mediastreams::wire::AudioSampleFormat::kFloat;
  }
}

media_audio::Format ToNewFormat(const fuchsia::media::AudioStreamType& format) {
  return media_audio::Format::CreateOrDie(
      {ToNewSampleFormat(format.sample_format), format.channels, format.frames_per_second});
}

}  // namespace

std::unique_ptr<Mixer> PointSampler::Select(const fuchsia::media::AudioStreamType& source_format,
                                            const fuchsia::media::AudioStreamType& dest_format,
                                            Gain::Limits gain_limits) {
  TRACE_DURATION("audio", "PointSampler::Select");

  auto point_sampler =
      media_audio::PointSampler::Create(ToNewFormat(source_format), ToNewFormat(dest_format));
  if (!point_sampler) {
    return nullptr;
  }

  struct MakePublicCtor : PointSampler {
    MakePublicCtor(Gain::Limits gain_limits, std::unique_ptr<Sampler> point_sampler)
        : PointSampler(gain_limits, std::move(point_sampler)) {}
  };
  return std::make_unique<MakePublicCtor>(gain_limits, std::move(point_sampler));
}

void PointSampler::Mix(float* dest_ptr, int64_t dest_frames, int64_t* dest_offset_ptr,
                       const void* source_void_ptr, int64_t source_frames, Fixed* source_offset_ptr,
                       bool accumulate) {
  TRACE_DURATION("audio", "PointSampler::Mix");

  auto info = &bookkeeping();
  PositionManager::CheckPositions(
      dest_frames, dest_offset_ptr, source_frames, source_offset_ptr->raw_value(),
      point_sampler_->pos_filter_length().raw_value(), info->step_size.raw_value(),
      info->rate_modulo(), info->denominator(), info->source_pos_modulo);

  Sampler::Source source{source_void_ptr, source_offset_ptr, source_frames};
  Sampler::Dest dest{dest_ptr, dest_offset_ptr, dest_frames};
  if (info->gain.IsSilent()) {
    // If the gain is silent, the mixer simply skips over the appropriate range in the destination
    // buffer, leaving whatever data is already there. We do not take further effort to clear the
    // buffer if `accumulate` is false. In fact, we IGNORE `accumulate` if silent. The caller is
    // responsible for clearing the destination buffer before Mix is initially called.
    point_sampler_->Process(source, dest, Sampler::Gain{.type = media_audio::GainType::kSilent},
                            true);
  } else if (info->gain.IsUnity()) {
    point_sampler_->Process(source, dest, Sampler::Gain{.type = media_audio::GainType::kUnity},
                            accumulate);
  } else if (info->gain.IsRamping()) {
    point_sampler_->Process(
        source, dest,
        Sampler::Gain{.type = media_audio::GainType::kRamping, .scale_ramp = info->scale_arr.get()},
        accumulate);
  } else {
    point_sampler_->Process(
        source, dest,
        Sampler::Gain{.type = media_audio::GainType::kNonUnity, .scale = info->gain.GetGainScale()},
        accumulate);
  }
}

}  // namespace media::audio::mixer
