// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/mixer_service/mix/pipeline_stage.h"

#include <optional>

#include "src/media/audio/mixer_service/common/basic_types.h"

namespace media_audio_mixer_service {

void PipelineStage::Advance(Fixed frame) {
  // TODO(fxbug.dev/87651): Add more logging and tracing etc (similar to `ReadableStream`).
  FX_CHECK(!is_locked_);

  // Advance the next readable frame.
  if (next_readable_frame_ && frame <= *next_readable_frame_) {
    // Next read frame is already passed the advanced point.
    return;
  }
  next_readable_frame_ = frame;

  if (cached_packet_ && frame < cached_packet_->end()) {
    // Cached packet is still in use.
    return;
  }
  cached_packet_ = std::nullopt;
  AdvanceImpl(frame);
}

std::optional<PipelineStage::Packet> PipelineStage::Read(Fixed start_frame, int64_t frame_count) {
  // TODO(fxbug.dev/87651): Add more logging and tracing etc (similar to `ReadableStream`).
  FX_CHECK(!is_locked_);

  // Once a frame has been consumed, it cannot be locked again, we cannot travel backwards in time.
  FX_CHECK(!next_readable_frame_ || start_frame >= *next_readable_frame_);

  // Check if we can reuse the cached packet.
  if (auto out_packet = ReadFromCachedPacket(start_frame, frame_count)) {
    return out_packet;
  }
  cached_packet_ = std::nullopt;

  auto packet = ReadImpl(start_frame, frame_count);
  if (!packet) {
    Advance(start_frame + Fixed(frame_count));
    return std::nullopt;
  }
  FX_CHECK(packet->length() > 0);

  is_locked_ = true;
  if (!packet->is_cached_) {
    return packet;
  }

  cached_packet_ = std::move(packet);
  auto out_packet = ReadFromCachedPacket(start_frame, frame_count);
  FX_CHECK(out_packet);
  return out_packet;
}

PipelineStage::Packet PipelineStage::MakeCachedPacket(Fixed start_frame, int64_t frame_count,
                                                      void* payload) {
  // This packet will be stored in `cached_packet_`. It won't be returned to the `Read` caller,
  // instead we'll use `ReadFromCachedPacket` to return a proxy to this packet.
  return Packet({format_, start_frame, frame_count, payload}, /*is_cached=*/true,
                /*destructor=*/nullptr);
}

PipelineStage::Packet PipelineStage::MakeUncachedPacket(Fixed start_frame, int64_t frame_count,
                                                        void* payload) {
  return Packet({format_, start_frame, frame_count, payload}, /*is_cached=*/false,
                [this, start_frame](int64_t frames_consumed) {
                  // Unlock the stream.
                  is_locked_ = false;
                  Advance(start_frame + Fixed(frames_consumed));
                });
}

std::optional<PipelineStage::Packet> PipelineStage::ForwardPacket(
    std::optional<Packet>&& packet, std::optional<Fixed> start_frame) {
  if (!packet) {
    return std::nullopt;
  }
  const auto packet_start = start_frame ? *start_frame : packet->start();
  return Packet(
      // Wrap the packet with a proxy so we can be notified when the packet is unlocked.
      {packet->format(), packet_start, packet->length(), packet->payload()},
      /*is_cached=*/false,
      [this, packet_start, packet = std::move(packet)](int64_t frames_consumed) mutable {
        // Unlock the stream.
        is_locked_ = false;
        // What is consumed from the proxy is also consumed from the source packet.
        packet->set_frames_consumed(frames_consumed);
        // Destroy the source packet before calling `Advance` to ensure the source stream is
        // unlocked before it is advanced.
        packet = std::nullopt;
        Advance(packet_start + Fixed(frames_consumed));
      });
}

std::optional<PipelineStage::Packet> PipelineStage::ReadFromCachedPacket(Fixed start_frame,
                                                                         int64_t frame_count) {
  if (cached_packet_) {
    if (auto intersect = cached_packet_->IntersectionWith(start_frame, frame_count)) {
      return MakeUncachedPacket(intersect->start(), intersect->length(), intersect->payload());
    }
  }
  return std::nullopt;
}

}  // namespace media_audio_mixer_service
