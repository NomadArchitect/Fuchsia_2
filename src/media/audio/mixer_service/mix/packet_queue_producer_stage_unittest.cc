// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/mixer_service/mix/packet_queue_producer_stage.h"

#include <lib/zx/time.h>

#include <memory>
#include <unordered_map>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/media/audio/mixer_service/common/basic_types.h"
#include "src/media/audio/mixer_service/mix/packet_view.h"

namespace media_audio_mixer_service {
namespace {

using ::fuchsia_mediastreams::wire::AudioSampleFormat;
using ::testing::ElementsAre;

const Format kFormat = Format::CreateOrDie({AudioSampleFormat::kFloat, 2, 48000});

class PacketQueueProducerStageTest : public ::testing::Test {
 public:
  PacketQueueProducerStageTest() : packet_queue_producer_stage_(kFormat) {}

  const void* PushPacket(uint32_t packet_id, int64_t start = 0, int64_t length = 1) {
    void* payload =
        packet_payloads_.emplace(packet_id, std::vector<float>(length, 0.0f)).first->second.data();
    packet_queue_producer_stage_.push(
        PacketView({kFormat, Fixed(start), length, payload}),
        [this, packet_id]() { released_packets_.push_back(packet_id); });
    return payload;
  }

  PacketQueueProducerStage& packet_queue_producer_stage() { return packet_queue_producer_stage_; }
  const std::vector<uint32_t>& released_packets() const { return released_packets_; }

 private:
  PacketQueueProducerStage packet_queue_producer_stage_;
  std::unordered_map<int32_t, std::vector<float>> packet_payloads_;
  std::vector<uint32_t> released_packets_;
};

TEST_F(PacketQueueProducerStageTest, Push) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push packet.
  PushPacket(0);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Flush the queue.
  packet_queue.clear();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));
}

TEST_F(PacketQueueProducerStageTest, Read) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  const void* payload_0 = PushPacket(0, 0, 20);
  const void* payload_1 = PushPacket(1, 20, 20);
  const void* payload_2 = PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Now pop the packets one by one.
  {
    // Packet #0:
    const auto buffer = packet_queue.Read(Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start());
    EXPECT_EQ(20, buffer->length());
    EXPECT_EQ(20, buffer->end());
    EXPECT_EQ(payload_0, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  {
    // Packet #1:
    const auto buffer = packet_queue.Read(Fixed(20), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(20, buffer->start());
    EXPECT_EQ(20, buffer->length());
    EXPECT_EQ(40, buffer->end());
    EXPECT_EQ(payload_1, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1));

  {
    // Packet #2:
    const auto buffer = packet_queue.Read(Fixed(40), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(40, buffer->start());
    EXPECT_EQ(20, buffer->length());
    EXPECT_EQ(60, buffer->end());
    EXPECT_EQ(payload_2, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(PacketQueueProducerStageTest, ReadMultiplePerPacket) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  const auto bytes_per_frame = packet_queue.format().bytes_per_frame();

  // Push packet.
  const void* payload = PushPacket(0, 0, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Read the first 10 bytes of the packet.
    const auto buffer = packet_queue.Read(Fixed(0), 10);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start());
    EXPECT_EQ(10, buffer->length());
    EXPECT_EQ(10, buffer->end());
    EXPECT_EQ(payload, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Read the next 10 bytes of the packet.
    const auto buffer = packet_queue.Read(Fixed(10), 10);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(10, buffer->start());
    EXPECT_EQ(10, buffer->length());
    EXPECT_EQ(20, buffer->end());
    EXPECT_EQ(static_cast<const uint8_t*>(payload) + 10 * bytes_per_frame, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  // Now that the packet has been fully consumed, it should be released.
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));
}

TEST_F(PacketQueueProducerStageTest, ReadNotFullyConsumed) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop, consume 0/20 bytes.
    auto buffer = packet_queue.Read(Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start());
    EXPECT_EQ(20, buffer->length());
    buffer->set_frames_consumed(0);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop, consume 5/20 bytes.
    auto buffer = packet_queue.Read(Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start());
    EXPECT_EQ(20, buffer->length());
    buffer->set_frames_consumed(5);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop again, consume 10/15 bytes.
    auto buffer = packet_queue.Read(Fixed(5), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(5, buffer->start());
    EXPECT_EQ(15, buffer->length());
    buffer->set_frames_consumed(10);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop again, this time consume it fully.
    auto buffer = packet_queue.Read(Fixed(15), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(15, buffer->start());
    EXPECT_EQ(5, buffer->length());
    buffer->set_frames_consumed(5);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  // Flush the queue to release the remaining packets.
  packet_queue.clear();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(PacketQueueProducerStageTest, ReadSkipsOverPacket) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Ask for packet 0.
    const auto buffer = packet_queue.Read(Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start());
    EXPECT_EQ(20, buffer->length());
    EXPECT_EQ(20, buffer->end());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  {
    // Ask for packet 2, skipping over packet 1.
    const auto buffer = packet_queue.Read(Fixed(40), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(40, buffer->start());
    EXPECT_EQ(20, buffer->length());
    EXPECT_EQ(60, buffer->end());
  }
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(PacketQueueProducerStageTest, ReadNulloptThenClear) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Since the queue is empty, this should return nullopt.
  const auto buffer = packet_queue.Read(Fixed(0), 10);
  EXPECT_FALSE(buffer.has_value());

  // Push some packets, then flush them immediately.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  packet_queue.clear();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(PacketQueueProducerStageTest, Advance) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  PushPacket(3, 60, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // The last frame in the first packet is 19.
  // Verify that advancing to that frame does not release the first packet.
  packet_queue.Advance(Fixed(19));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Advance again with the same frame to verify it is idempotent.
  packet_queue.Advance(Fixed(19));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Now advance to the next packet.
  packet_queue.Advance(Fixed(20));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  // Now advance beyond packet 1 and 2 in one go (until just before packet 3 should be released).
  packet_queue.Advance(Fixed(79));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));

  // Finally advance past the end of all packets.
  packet_queue.Advance(Fixed(1000));
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2, 3));
}

TEST_F(PacketQueueProducerStageTest, ReportUnderflow) {
  PacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  std::vector<zx::duration> reported_underflows;
  packet_queue.SetUnderflowReporter(
      [&reported_underflows](zx::duration duration) { reported_underflows.push_back(duration); });

  // This test uses 48k fps, so 10ms = 480 frames.
  constexpr int64_t kPacketSize = 480;
  constexpr int64_t kFrameAt05ms = kPacketSize / 2;
  constexpr int64_t kFrameAt15ms = kPacketSize + kPacketSize / 2;
  constexpr int64_t kFrameAt20ms = 2 * kPacketSize;

  {
    // Advance to t=20ms.
    const auto buffer = packet_queue.Read(Fixed(0), 2 * kPacketSize);
    EXPECT_FALSE(buffer);
    EXPECT_TRUE(reported_underflows.empty());
  }

  // Push two packets, one that fully underflows and one that partially underflows.
  PushPacket(0, kFrameAt05ms, kPacketSize);
  PushPacket(1, kFrameAt15ms, kPacketSize);

  {
    // The next `Read` advances to t=25ms, returning part of the queued packet.
    reported_underflows.clear();
    const auto buffer = packet_queue.Read(Fixed(kFrameAt20ms), kPacketSize);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(kFrameAt20ms, buffer->start());
    EXPECT_EQ(kPacketSize / 2, buffer->length());
    EXPECT_THAT(reported_underflows, ElementsAre(zx::msec(15), zx::msec(5)));
  }
  // After packet is released, the queue should be empty.
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1));
}

}  // namespace
}  // namespace media_audio_mixer_service
