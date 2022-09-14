// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/mix/ring_buffer_consumer_writer.h"

namespace media_audio {

RingBufferConsumerWriter::RingBufferConsumerWriter(std::shared_ptr<RingBuffer> buffer)
    :  // TODO(fxbug.dev/87651): When ConsumerStage::Writers can write a different sample type than
       // the parent ConsumerStage, we'll have different source and dest formats here.
      stream_converter_(StreamConverter::Create(buffer->format(), buffer->format())),
      buffer_(std::move(buffer)) {}

void RingBufferConsumerWriter::WriteData(int64_t start_frame, int64_t frame_count,
                                         const void* data) {
  WriteInternal(start_frame, frame_count, data);
}

void RingBufferConsumerWriter::WriteSilence(int64_t start_frame, int64_t frame_count) {
  WriteInternal(start_frame, frame_count, nullptr);
}

void RingBufferConsumerWriter::End() {
  // no-op
}

void RingBufferConsumerWriter::WriteInternal(int64_t start_frame, int64_t frame_count,
                                             const void* data) {
  const int64_t end_frame = start_frame + frame_count;
  const int64_t bytes_per_frame = buffer_->format().bytes_per_frame();

  // We must write the entire range.
  while (start_frame < end_frame) {
    auto packet = buffer_->PrepareToWrite(start_frame, frame_count);
    if (data) {
      stream_converter_->CopyAndClip(data, packet.payload(), packet.length());
      data = static_cast<const char*>(data) + packet.length() * bytes_per_frame;
    } else {
      stream_converter_->WriteSilence(packet.payload(), packet.length());
    }

    start_frame += packet.length();
    frame_count -= packet.length();
  }
}

}  // namespace media_audio
