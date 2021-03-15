// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/internal.h>
#include <lib/fidl/llcpp/array.h>
#include <lib/fidl/llcpp/coding.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/sync_call.h>
#include <lib/zx/channel.h>
#include <zircon/fidl.h>

#include <memory>
#include <optional>
#include <utility>

#include <fidl/test/coding/fuchsia/llcpp/fidl.h>
#include <zxtest/zxtest.h>

using ::fidl_test_coding_fuchsia::TypesTest;
using NonNullableChannelRequest = TypesTest::NonNullableChannelRequest;

namespace {

// Because the EncodedMessage/DecodedMessage classes close handles using the corresponding
// Zircon system call instead of calling a destructor, we indirectly test for handle closure
// via the ZX_ERR_PEER_CLOSED error message.

void HelperExpectPeerValid(zx::channel& channel) {
  const char* foo = "A";
  EXPECT_EQ(channel.write(0, foo, 1, nullptr, 0), ZX_OK);
}

void HelperExpectPeerInvalid(zx::channel& channel) {
  const char* foo = "A";
  EXPECT_EQ(channel.write(0, foo, 1, nullptr, 0), ZX_ERR_PEER_CLOSED);
}

TEST(LlcppTypesTests, EncodedMessageTest) {
  NonNullableChannelRequest msg(0);

  // Capture the extra handle here; it will not be cleaned by encoded_message
  zx::channel channel_1;

  EXPECT_EQ(zx::channel::create(0, &msg.channel, &channel_1), ZX_OK);

  {
    fidl::OwnedEncodedMessage<NonNullableChannelRequest> encoded(&msg);

    HelperExpectPeerValid(channel_1);
  }

  HelperExpectPeerInvalid(channel_1);
}

// Start with a message, then encode, decode and encode again.
TEST(LlcppTypesTests, RoundTripTest) {
  TypesTest::NonNullableChannelRequest msg(10);

  // Capture the extra handle here; it will not be cleaned by encoded_message
  zx::channel channel_1;

  EXPECT_EQ(zx::channel::create(0, &msg.channel, &channel_1), ZX_OK);

  zx_handle_t unsafe_handle_backup(msg.channel.get());

  // We need to define our own storage because it is used after encoded is deleted.
  FIDL_ALIGNDECL uint8_t storage[sizeof(TypesTest::NonNullableChannelRequest)];

  auto encoded = new fidl::UnownedEncodedMessage<TypesTest::NonNullableChannelRequest>(
      storage, sizeof(storage), &msg);
  EXPECT_EQ(encoded->GetOutgoingMessage().byte_actual(),
            sizeof(TypesTest::NonNullableChannelRequest));

  uint8_t golden_encoded[] = {0x0a, 0x00, 0x00, 0x00,   // txid
                              0x00, 0x00, 0x00, 0x01,   // flags and version
                              0xa1, 0xd4, 0x9b, 0x76,   // low bytes of ordinal
                              0x82, 0x41, 0x13, 0x06,   // high bytes of ordinal
                              0xff, 0xff, 0xff, 0xff,   // handle present
                              0x00, 0x00, 0x00, 0x00};  // Padding

  // Byte-accurate comparison
  EXPECT_EQ(memcmp(golden_encoded, encoded->GetOutgoingMessage().bytes(),
                   encoded->GetOutgoingMessage().byte_actual()),
            0);

  HelperExpectPeerValid(channel_1);

  // Decode
  auto decoded =
      fidl::DecodedMessage<TypesTest::NonNullableChannelRequest>::FromOutgoingWithRawHandleCopy(
          encoded);
  EXPECT_TRUE(decoded.ok());
  EXPECT_NULL(decoded.error(), "%s", decoded.error());
  EXPECT_EQ(decoded.PrimaryObject()->_hdr.txid, 10);
  EXPECT_EQ(decoded.PrimaryObject()->_hdr.ordinal, 0x6134182769bd4a1lu);
  EXPECT_EQ(decoded.PrimaryObject()->channel.get(), unsafe_handle_backup);
  // encoded_message should be consumed
  EXPECT_EQ(encoded->GetOutgoingMessage().handle_actual(), 0);
  delete encoded;
  // At this point, encoded is destroyed but not decoded, it should not accidentally close the
  // channel.
  HelperExpectPeerValid(channel_1);

  // Encode
  {
    fidl::OwnedEncodedMessage<TypesTest::NonNullableChannelRequest> encoded2(
        decoded.PrimaryObject());
    EXPECT_TRUE(encoded2.ok());
    EXPECT_NULL(encoded2.error(), "%s", encoded2.error());

    // Byte-level comparison
    EXPECT_EQ(encoded2.GetOutgoingMessage().byte_actual(),
              sizeof(TypesTest::NonNullableChannelRequest));
    EXPECT_EQ(memcmp(golden_encoded, encoded2.GetOutgoingMessage().bytes(),
                     encoded2.GetOutgoingMessage().byte_actual()),
              0);
    EXPECT_EQ(encoded2.GetOutgoingMessage().handle_actual(), 1);
    EXPECT_EQ(encoded2.GetOutgoingMessage().handles()[0].handle, unsafe_handle_backup);

    HelperExpectPeerValid(channel_1);
  }
  // Encoded message was destroyed, bringing down the handle with it
  HelperExpectPeerInvalid(channel_1);
}

TEST(LlcppTypesTests, ArrayLayoutTest) {
  static_assert(sizeof(fidl::Array<uint8_t, 3>) == sizeof(uint8_t[3]));
  static_assert(sizeof(fidl::Array<fidl::Array<uint8_t, 7>, 3>) == sizeof(uint8_t[3][7]));

  constexpr fidl::Array<uint8_t, 3> a = {1, 2, 3};
  constexpr uint8_t b[3] = {1, 2, 3};
  EXPECT_EQ((&a[2] - &a[0]), (&b[2] - &b[0]));
}

TEST(LlcppTypesTests, StringView) {
  fidl::FidlAllocator allocator;

  fidl::StringView view;
  EXPECT_TRUE(view.empty());
  EXPECT_TRUE(view.is_null());

  view.Set(allocator, "123");

  EXPECT_FALSE(view.empty());
  EXPECT_EQ(view.size(), 3);
  EXPECT_BYTES_EQ(view.data(), "123", 3);

  EXPECT_EQ(view.at(1), '2');
}

TEST(LlcppTypesTests, VectorView) {
  fidl::FidlAllocator allocator;

  fidl::VectorView<int> view;
  EXPECT_TRUE(view.empty());
  EXPECT_TRUE(view.data() == nullptr);

  view.Allocate(allocator, 3);
  const int data[] = {1, 2, 3};
  view[0] = data[0];
  view[1] = data[1];
  view[2] = data[2];

  EXPECT_EQ(view.count(), 3);
  EXPECT_BYTES_EQ(view.data(), data, 3);

  EXPECT_EQ(view.at(1), 2);
}

TEST(LlcppTypesTests, InlineMessageBuffer) {
  fidl::internal::InlineMessageBuffer<32> buffer;
  ASSERT_EQ(32, buffer.size());
  ASSERT_EQ(reinterpret_cast<uint8_t*>(&buffer), buffer.data());
  ASSERT_EQ(buffer.data(), buffer.view().data);
  ASSERT_EQ(32, buffer.view().capacity);

  const fidl::internal::InlineMessageBuffer<32> const_buffer;
  ASSERT_EQ(reinterpret_cast<const uint8_t*>(&const_buffer), const_buffer.data());
}

TEST(LlcppTypesTests, BoxedMessageBuffer) {
  fidl::internal::BoxedMessageBuffer<32> buffer;
  ASSERT_EQ(32, buffer.size());
  ASSERT_NE(reinterpret_cast<uint8_t*>(&buffer), buffer.data());
  ASSERT_EQ(buffer.data(), buffer.view().data);
  ASSERT_EQ(32, buffer.view().capacity);

  const fidl::internal::BoxedMessageBuffer<32> const_buffer;
  ASSERT_NE(reinterpret_cast<const uint8_t*>(&const_buffer), const_buffer.data());
}

TEST(LlcppTypesTests, ResponseStorageAllocationStrategyTest) {
  // The stack allocation limit of 512 bytes is defined in
  // zircon/system/ulib/fidl/include/lib/fidl/llcpp/sync_call.h

  static_assert(sizeof(TypesTest::RequestOf512BytesRequest) == 512);
  // Buffers for messages no bigger than 512 bytes are embedded, for this request,
  // OwnedEncodedMessage size is bigger than 512 bytes.
  static_assert(sizeof(fidl::OwnedEncodedMessage<TypesTest::RequestOf512BytesRequest>) > 512);

  static_assert(sizeof(TypesTest::RequestOf513BytesRequest) == 520);
  // Buffers for messages bigger than 512 bytes are store on the heap, for this request,
  // OwnedEncodedMessage size is smaller than 512 bytes.
  static_assert(sizeof(fidl::OwnedEncodedMessage<TypesTest::RequestOf513BytesRequest>) < 512);
}

}  // namespace
