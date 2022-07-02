// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_SINC_SAMPLER_H_
#define SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_SINC_SAMPLER_H_

#include <fuchsia/media/cpp/fidl.h>

#include <memory>
#include <utility>

#include "src/media/audio/audio_core/mixer/mixer.h"
#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/lib/processing/sampler.h"

namespace media::audio::mixer {

class SincSampler : public Mixer {
 public:
  static std::unique_ptr<Mixer> Select(const fuchsia::media::AudioStreamType& source_format,
                                       const fuchsia::media::AudioStreamType& dest_format,
                                       Gain::Limits gain_limits = {});

  void EagerlyPrepare() override;
  void Mix(float* dest_ptr, int64_t dest_frames, int64_t* dest_offset_ptr,
           const void* source_void_ptr, int64_t source_frames, Fixed* source_offset_ptr,
           bool accumulate) override;

 protected:
  SincSampler(Gain::Limits gain_limits, std::shared_ptr<media_audio::Sampler> sinc_sampler)
      : Mixer(sinc_sampler->pos_filter_length() - Fixed::FromRaw(1),
              sinc_sampler->neg_filter_length() - Fixed::FromRaw(1), gain_limits),
        sinc_sampler_(std::move(sinc_sampler)) {}

 private:
  std::shared_ptr<media_audio::Sampler> sinc_sampler_;
};

}  // namespace media::audio::mixer

#endif  // SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_SINC_SAMPLER_H_
