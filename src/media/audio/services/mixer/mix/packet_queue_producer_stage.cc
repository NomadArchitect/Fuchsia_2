// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/mix/packet_queue_producer_stage.h"

#include <lib/zx/time.h>

#include <memory>
#include <optional>
#include <utility>

#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/services/mixer/mix/mix_job_context.h"

namespace media_audio {

void PacketQueueProducerStage::AdvanceImpl(Fixed frame) {
  while (!pending_packet_queue_.empty()) {
    const auto& pending_packet = pending_packet_queue_.front();
    if (pending_packet.end() > frame) {
      return;
    }
    pending_packet_queue_.pop_front();
  }
}

std::optional<PipelineStage::Packet> PacketQueueProducerStage::ReadImpl(MixJobContext& ctx,
                                                                        Fixed start_frame,
                                                                        int64_t frame_count) {
  // Clean up pending packets before `start_frame`.
  while (!pending_packet_queue_.empty()) {
    auto& pending_packet = pending_packet_queue_.front();
    // If the packet starts before the requested frame and has not been seen before, it underflowed.
    if (const Fixed underflow_frame_count = start_frame - pending_packet.start();
        !pending_packet.seen_in_read_ && underflow_frame_count >= Fixed(1)) {
      ReportUnderflow(underflow_frame_count);
    }
    if (pending_packet.end() > start_frame) {
      pending_packet.seen_in_read_ = true;
      break;
    }
    pending_packet_queue_.pop_front();
  }

  if (pending_packet_queue_.empty()) {
    return std::nullopt;
  }

  // Read the next pending packet.
  const auto& pending_packet = pending_packet_queue_.front();
  if (const auto intersect = pending_packet.IntersectionWith(start_frame, frame_count)) {
    // We don't need to cache the returned packet, since we don't generate any data dynamically.
    return MakeUncachedPacket(intersect->start(), intersect->length(), intersect->payload());
  }
  return std::nullopt;
}

void PacketQueueProducerStage::ReportUnderflow(Fixed underlow_frame_count) {
  ++underflow_count_;
  if (underflow_reporter_) {
    // We estimate the underflow duration using the stream's frame rate. However, this can be an
    // underestimate in three ways:
    //
    // * If the stream has been paused, this does not include the time spent paused.
    //
    // * Frames are typically read in batches. This does not account for the batch size. In practice
    //   we expect the batch size should be 10ms or less, which puts a bound on this underestimate.
    //
    // * `underflow_frame_count` is ultimately derived from the reference clock of the stage. For
    //   example, if the reference clock is running slower than the system monotonic clock, then the
    //   underflow will appear shorter than it actually was. This error is bounded by the maximum
    //   rate difference of the reference clock, which is +/-0.1% (see `zx_clock_update`).
    const auto duration =
        zx::duration(format().frames_per_ns().Inverse().Scale(underlow_frame_count.Ceiling()));
    underflow_reporter_(duration);
  }
}

}  // namespace media_audio
