// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_TESTING_CONSUMER_STAGE_WRAPPER_H_
#define SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_TESTING_CONSUMER_STAGE_WRAPPER_H_

#include <vector>

#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/services/mixer/common/basic_types.h"
#include "src/media/audio/services/mixer/mix/consumer_stage.h"
#include "src/media/audio/services/mixer/mix/simple_packet_queue_producer_stage.h"
#include "src/media/audio/services/mixer/mix/testing/defaults.h"
#include "src/media/audio/services/mixer/mix/testing/fake_consumer_stage_writer.h"

namespace media_audio {

// Wraps a SimplePacketQueueProducerStage -> ConsumerStage pipeline, where the ConsumerStage uses a
// FakeConsumerStageWriter.
struct ConsumerStageWrapper {
  ConsumerStageWrapper(Format f, zx::duration presentation_delay,
                       PipelineDirection pipeline_direction = PipelineDirection::kOutput,
                       zx_koid_t reference_clock_koid = DefaultClockKoid())
      : format(f) {
    packet_queue = MakeDefaultPacketQueue(format),
    command_queue = std::make_shared<ConsumerStage::CommandQueue>();
    writer = std::make_shared<FakeConsumerStageWriter>();
    consumer = std::make_shared<ConsumerStage>(ConsumerStage::Args{
        .pipeline_direction = pipeline_direction,
        .presentation_delay = presentation_delay,
        .format = format,
        .reference_clock_koid = reference_clock_koid,
        .command_queue = command_queue,
        .writer = writer,
    });
    consumer->AddSource(packet_queue, {});
  }

  std::shared_ptr<std::vector<float>> PushPacket(Fixed start_frame, int64_t length) {
    auto payload = std::make_shared<std::vector<float>>(length * format.channels());
    packet_queue->push(PacketView({format, start_frame, length, payload->data()}));
    return payload;
  }

  const Format format;
  std::shared_ptr<ConsumerStage> consumer;
  std::shared_ptr<ConsumerStage::CommandQueue> command_queue;
  std::shared_ptr<FakeConsumerStageWriter> writer;
  std::shared_ptr<SimplePacketQueueProducerStage> packet_queue;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_MIXER_MIX_TESTING_CONSUMER_STAGE_WRAPPER_H_
