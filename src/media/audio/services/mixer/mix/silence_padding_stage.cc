// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/mix/silence_padding_stage.h"

#include <optional>
#include <utility>

#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/services/mixer/mix/mix_job_context.h"
#include "src/media/audio/services/mixer/mix/packet_view.h"
#include "src/media/audio/services/mixer/mix/pipeline_stage.h"

namespace media_audio {

void SilencePaddingStage::AdvanceImpl(Fixed frame) {
  if (source_) {
    source_->Advance(frame);
  }
}

std::optional<PipelineStage::Packet> SilencePaddingStage::ReadImpl(MixJobContext& ctx,
                                                                   Fixed start_frame,
                                                                   int64_t frame_count) {
  // Read the next packet from `source_`.
  std::optional<PipelineStage::Packet> next_packet = std::nullopt;
  if (source_) {
    Fixed source_start_frame = start_frame;
    const Fixed source_end_frame = source_start_frame + Fixed(frame_count);
    // Advance to our source's next readable frame. This is needed when the source stream contains
    // gaps. For example, given a sequence of calls:
    //
    //   Read(100, 10);
    //   Read(105, 10);
    //
    // If `silence_frame_count_ = 5` and our source does not have any data for the range [100, 110),
    // then at the first call, our source will return `std::nullopt` and we will return 5 frames of
    // silence. At the next call, the caller asks for frames 105, but the source has already
    // advanced to frame 110. We know that frames [105, 110) are empty, so we must advance our
    // request to frames [110, 115).
    if (const auto next_readable_frame = source_->next_readable_frame()) {
      source_start_frame = std::max(source_start_frame, *next_readable_frame);
    }
    if (const int64_t source_frame_count = Fixed(source_end_frame - source_start_frame).Floor();
        source_frame_count > 0) {
      next_packet = source_->Read(ctx, source_start_frame, source_frame_count);
    }
  }

  // We emit silent frames following each packet:
  //
  //                                                   +-------------+
  //   +------------------| (silence_frame_count_) ... | next_packet |
  //                      ^                            +-------------+
  //               last_data_frame_
  //
  // If there are more than `silence_frame_count_` separating `last_data_frame_` and `next_packet`,
  // we leave those extra frames empty. We do not emit a silent packet unless `last_data_frame_` and
  // `next_packet` are separated by at least one full frame.
  if (last_data_frame_) {
    const Fixed silence_start_frame = *last_data_frame_;
    // Always generate an integral number of frames.
    int64_t silence_frame_count = silence_frame_count_;
    if (next_packet && next_packet->start() < silence_start_frame + silence_frame_count) {
      silence_frame_count = round_down_fractional_frames_
                                ? Fixed(next_packet->start() - silence_start_frame).Floor()
                                : Fixed(next_packet->start() - silence_start_frame).Ceiling();
    }
    // If the silent region intersects with our request, return a silent packet.
    if (silence_frame_count > 0) {
      if (const auto intersect = PacketView({
                                                .format = format(),
                                                .start = silence_start_frame,
                                                .length = silence_frame_count,
                                                .payload = silence_buffer_.data(),
                                            })
                                     .IntersectionWith(start_frame, frame_count)) {
        // We are emitting silence before `next_packet`, so we have not consumed any frames yet.
        if (next_packet) {
          next_packet->set_frames_consumed(0);
        }
        FX_CHECK(intersect->length() <= silence_frame_count_);
        return MakeCachedPacket(intersect->start(), intersect->length(), silence_buffer_.data());
      }
    }
  }

  // Passthrough `next_packet`.
  if (next_packet) {
    last_data_frame_ = next_packet->end();
    return ForwardPacket(std::move(next_packet));
  }
  return std::nullopt;
}

}  // namespace media_audio
