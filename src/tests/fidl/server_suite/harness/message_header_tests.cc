// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/tests/fidl/server_suite/harness/harness.h"
#include "src/tests/fidl/server_suite/harness/ordinals.h"

using namespace channel_util;

namespace server_suite {

// Check that the channel is closed when a new one-way request with a non-zero txid is received.
SERVER_TEST(OneWayWithNonZeroTxid) {
  ASSERT_OK(client_end().write(header(56 /* txid not 0 */, kOrdinalOneWayNoPayload,
                                      fidl::MessageDynamicFlags::kStrictMethod)));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

// Check that the channel is closed when a new two-way request with a zero txid is received.
SERVER_TEST(TwoWayNoPayloadWithZeroTxid) {
  ASSERT_OK(client_end().write(
      header(0, kOrdinalTwoWayNoPayload, fidl::MessageDynamicFlags::kStrictMethod)));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

// Check that the server closes the channel when unknown ordinals are received.
SERVER_TEST(UnknownOrdinalCausesClose) {
  ASSERT_OK(client_end().write(
      header(0, /* some wrong ordinal */ 8888888lu, fidl::MessageDynamicFlags::kStrictMethod)));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

// Check that the server closes the channel when an unknown magic number is received.
SERVER_TEST(BadMagicNumberCausesClose) {
  ASSERT_OK(client_end().write(as_bytes(fidl_message_header_t{
      .txid = 123,
      .at_rest_flags = {FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0},
      .dynamic_flags = FIDL_MESSAGE_HEADER_DYNAMIC_FLAGS_STRICT_METHOD,
      .magic_number = 0xff,  // Chosen to be invalid
      .ordinal = kOrdinalTwoWayNoPayload,
  })));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

// Check that the server closes the channel when unknown at rest flags are received.
SERVER_TEST(BadAtRestFlagsCausesClose) {
  ASSERT_OK(client_end().write(as_bytes(fidl_message_header_t{
      .txid = 123,
      .at_rest_flags = {100, 200},
      .dynamic_flags = FIDL_MESSAGE_HEADER_DYNAMIC_FLAGS_STRICT_METHOD,
      .magic_number = kFidlWireFormatMagicNumberInitial,
      .ordinal = kOrdinalTwoWayNoPayload,
  })));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

// Check that the server closes the channel when unknown dynamic flags are received.
SERVER_TEST(BadDynamicFlagsCausesClose) {
  ASSERT_OK(client_end().write(as_bytes(fidl_message_header_t{
      .txid = 123,
      .at_rest_flags = {FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0},
      .dynamic_flags = 100,
      .magic_number = kFidlWireFormatMagicNumberInitial,
      .ordinal = kOrdinalTwoWayNoPayload,
  })));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_PEER_CLOSED));
  ASSERT_FALSE(client_end().is_signal_present(ZX_CHANNEL_READABLE));
}

}  // namespace server_suite
