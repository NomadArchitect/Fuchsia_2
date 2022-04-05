// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_RING_BUFFER_PRODUCER_STAGE_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_RING_BUFFER_PRODUCER_STAGE_H_

#include <lib/fzl/vmo-mapper.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <functional>
#include <memory>
#include <optional>
#include <utility>

#include "src/media/audio/mixer_service/common/basic_types.h"
#include "src/media/audio/mixer_service/mix/producer_stage.h"

namespace media_audio_mixer_service {

class RingBufferProducerStage : public ProducerStage {
 public:
  // A function that returns the safe read frame for the current time.
  // TODO(fxbug.dev/87651): Move this out to a common `ring_buffer` file as `SafeReadWriteFn`?
  using SafeReadFrameFn = std::function<int64_t()>;

  RingBufferProducerStage(Format format, fzl::VmoMapper vmo_mapper, int64_t frame_count,
                          SafeReadFrameFn safe_read_frame_fn,
                          std::unique_ptr<AudioClock> audio_clock,
                          TimelineFunction ref_time_to_frac_presentation_frame = {})
      : ProducerStage("RingBufferProducerStage", format, std::move(audio_clock),
                      ref_time_to_frac_presentation_frame),
        vmo_mapper_(std::move(vmo_mapper)),
        frame_count_(frame_count),
        safe_read_frame_fn_(std::move(safe_read_frame_fn)) {
    FX_CHECK(vmo_mapper_.start());
    FX_CHECK(vmo_mapper_.size() >= static_cast<uint64_t>(format.bytes_per_frame() * frame_count_));
    FX_CHECK(safe_read_frame_fn_);
  }

  // Returns the ring buffer's size in frames.
  int64_t frame_count() const { return frame_count_; }

 protected:
  // Since there are no resources to release, advancing is a no-op.
  void AdvanceImpl(Fixed frame) final {}

  // Implements `PipelineStage`.
  std::optional<Packet> ReadImpl(Fixed start_frame, int64_t frame_count) final;

 private:
  fzl::VmoMapper vmo_mapper_;
  int64_t frame_count_ = 0;
  SafeReadFrameFn safe_read_frame_fn_;
};

}  // namespace media_audio_mixer_service

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_RING_BUFFER_PRODUCER_STAGE_H_
