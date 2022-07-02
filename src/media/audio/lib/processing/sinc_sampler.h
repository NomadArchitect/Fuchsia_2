// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_
#define SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_

#include <cstdint>
#include <memory>

#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/lib/processing/sampler.h"

namespace media_audio {

class SincSampler : public Sampler {
 public:
  // Creates new `SincSampler` for a given `source_format` and `dest_format`.
  static std::shared_ptr<Sampler> Create(const Format& source_format, const Format& dest_format);

  // TODO(fxbug.dev/87651): This is temporary to preserve the existing `media::audio::Mixer` API, to
  // be refactored once we switch to the new mixer service mix stage.
  virtual void SetRateValues(int64_t step_size, uint64_t rate_modulo, uint64_t denominator,
                             uint64_t* source_pos_mod) = 0;

 protected:
  SincSampler(Fixed pos_filter_length, Fixed neg_filter_length)
      : Sampler(pos_filter_length, neg_filter_length) {}
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_
