// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_SILENCE_PADDING_STAGE_H_
#define SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_SILENCE_PADDING_STAGE_H_

#include <optional>
#include <vector>

#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/services/mixer/mix/mix_job_context.h"
#include "src/media/audio/services/mixer/mix/pipeline_stage.h"
#include "src/media/audio/services/mixer/mix/ptr_decls.h"

namespace media_audio {

// A stage wrapper that appends silence after each discontiguous chunk of audio to "ring out" or
// "fade out" audio processors. This wrapper can be used when the following conditions are met:
//
//   1. The audio processor assumes that the source stream is preceded by an infinite amount of
//      silence. That is, we don't need to inject silence into the beginning of the stream; initial
//      silence is assumed.
//
//   2. After the audio processor is fed `silence_frame_count` worth of silence, it emits no more
//      audible sound; all further output is below the noise floor until it is fed another
//      non-silent chunk of audio. Put differently, `silence_frame_count` is the minimum number of
//      frames necessary to "ring out" or "fade out" any effects or filters applied by the audio
//      processor.
//
// For example, when a resampling filter produces destination frame X, it actually samples from a
// wider range of the source stream surrounding the corresponding source frame X. This range is
// defined by a "negative filter width" and a "positive filter width":
//
//   +----------------X----------------+  source stream
//              |     ^     |
//              +-----+-----+
//                 ^     ^
//    negative width     positive width
//
// Such a filter will need to be fed `negative_width+positive_width` worth of silence after each
// non-silent segment. To illustrate:
//
//   A-----------------------B                      C-------------------...
//                           |     ^     |    |     ^     |
//                           +-----+-----+    +-----+-----+
//                              ^     ^
//                neg_filter_width   pos_filter_width
//
// In this example, the source stream includes a chunk of non-silent data in frames [A,B], followed
// later by another non-silent chunk starting at frame C. `SilencePaddingStage`'s job is to generate
// silence to "ring out" the stream between frames B and C.
//
// To produce the destination frame corresponding to source frame A, the filter assumes A is
// preceded by infinite silence (recall condition 1, above). This covers the range
// [A-neg_filter_width,A]. `SilencePaddingStage` does nothing in this range.
//
// To produce the destination frame corresponding to source frame B + neg_filter_width, the filter
// needs to be fed neg_filter_width + pos_filter_width worth of silence following frame B. This
// quiesces the filter into a silent state. Beyond this frame, the filter is in a silent state and
// does not need to be fed additional silent frames before frame C.
//
// If B and C are separated a non-integral number of frames, there are two cases:
//
//   * If `SilencePaddingStage` was created with `round_down_fractional_frames = true`, then at most
//     floor(C - B) frames are generated immediately after B. For example, if B = 10, C = 15.5, and
//     `silence_frame_count = 20`, we generate silence at frames [10,15), leaving a gap in the
//     fractional range [15, 15.5).
//
//   * If `SilencePaddingStage` was created with `round_down_fractional_frames = false`, then at
//     most ceil(C - B) frames are generated immediately after B. For example, if B = 10, C = 15.5,
//     and `silence_frame_count = 20`, we generate silence at frames [10,16), where the last frame
//     of silence overlaps with C.
//
// The second mode (`round_down_fractional_frames = false`) is useful for pipeline stages that
// sample a source stream using SampleAndHold. In the above example, SampleAndHold samples source
// frame C = 15.5 into dest frame 16. If we generate silence in the range [10, 15), this leaves a
// full-frame gap before C, even though we have generated only 5 frames of silence and
// `silence_frame_count = 20`. Hence, in this case, it's better to generate ceil(C - B) frames of
// silence.
class SilencePaddingStage : public PipelineStage {
 public:
  SilencePaddingStage(Format format, zx_koid_t reference_clock_koid, Fixed silence_frame_count,
                      bool round_down_fractional_frames)
      : PipelineStage("SilencePaddingStage", format, reference_clock_koid),
        // Round up to generate an integer number of frames.
        silence_frame_count_(silence_frame_count.Ceiling()),
        round_down_fractional_frames_(round_down_fractional_frames),
        silence_buffer_(silence_frame_count_ * format.bytes_per_frame(), 0) {}

  // Implements `PipelineStage`.
  void AddSource(PipelineStagePtr source) final {
    FX_CHECK(!source_) << "SilencePaddingStage does not support multiple sources";
    FX_CHECK(source) << "SilencePaddingStage cannot add null source";
    FX_CHECK(source->format() == format())
        << "SilencePaddingStage format does not match with source format";
    source_ = std::move(source);
  }
  void RemoveSource(PipelineStagePtr source) final {
    FX_CHECK(source_) << "SilencePaddingStage source was not found";
    FX_CHECK(source) << "SilencePaddingStage cannot remove null source";
    FX_CHECK(source_ == source) << "SilencePaddingStage source " << source_->name()
                                << " does not match with " << source->name();
    source_ = nullptr;
  }

 protected:
  void AdvanceSelfImpl(Fixed frame) final {}
  void AdvanceSourcesImpl(MixJobContext& ctx, Fixed frame) final;
  std::optional<Packet> ReadImpl(MixJobContext& ctx, Fixed start_frame, int64_t frame_count) final;

 private:
  const int64_t silence_frame_count_;
  const bool round_down_fractional_frames_;

  PipelineStagePtr source_ = nullptr;

  // Last non-silent data frame that was returned from `source_`.
  std::optional<Fixed> last_data_frame_ = std::nullopt;

  // Silence buffer filled with `silence_frame_count_` zero frames.
  std::vector<char> silence_buffer_;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_SILENCE_PADDING_STAGE_H_
