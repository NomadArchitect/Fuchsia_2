// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_
#define SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_

#include <cstdint>
#include <memory>

#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/lib/processing/position_manager.h"
#include "src/media/audio/lib/processing/sampler.h"

namespace media_audio {

class SincSampler : public Sampler {
 public:
  // Creates new `SincSampler` for a given `source_format` and `dest_format`.
  static std::unique_ptr<Sampler> Create(const Format& source_format, const Format& dest_format);

  Type type() const final { return Type::kSincSampler; }

 protected:
  SincSampler(Fixed pos_filter_length, Fixed neg_filter_length)
      : Sampler(pos_filter_length, neg_filter_length) {}
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_LIB_PROCESSING_SINC_SAMPLER_H_
