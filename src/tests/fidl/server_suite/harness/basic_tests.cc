// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/tests/fidl/server_suite/harness/harness.h"
#include "src/tests/fidl/server_suite/harness/ordinals.h"

namespace server_suite {

// Check that the test runner is set up correctly without doing anything else.
SERVER_TEST(Setup) {}

// Check that a one-way call is received at Target.
SERVER_TEST(OneWayNoPayload) {
  ASSERT_OK(client_end().write(
      header(0, kOrdinalOneWayNoPayload, fidl::MessageDynamicFlags::kStrictMethod)));

  WAIT_UNTIL([this]() { return reporter().received_one_way_no_payload(); });
}

// Check that Target replies to a two-way call.
SERVER_TEST(TwoWayNoPayload) {
  constexpr zx_txid_t kTxid = 123u;

  ASSERT_OK(client_end().write(
      header(kTxid, kOrdinalTwoWayNoPayload, fidl::MessageDynamicFlags::kStrictMethod)));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_READABLE));

  ASSERT_OK(client_end().read_and_check(
      header(kTxid, kOrdinalTwoWayNoPayload, fidl::MessageDynamicFlags::kStrictMethod)));
}

// Check that Target replies to a two-way call with a result (for a method using error syntax).
SERVER_TEST(TwoWayResultWithPayload) {
  constexpr zx_txid_t kTxid = 123u;

  Bytes bytes_in = {
      // clang-format off
      header(kTxid, kOrdinalTwoWayResult, fidl::MessageDynamicFlags::kStrictMethod),
      union_ordinal(1), out_of_line_envelope(24, 0),
      string_length(3), pointer_present(),
      'a','b','c', padding(5),
      // clang-format on
  };
  ASSERT_OK(client_end().write(bytes_in));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_READABLE));

  Bytes bytes_out = {
      // clang-format off
      header(kTxid, kOrdinalTwoWayResult, fidl::MessageDynamicFlags::kStrictMethod),
      union_ordinal(1), out_of_line_envelope(24, 0),
      string_length(3), pointer_present(),
      'a','b','c', padding(5),
      // clang-format on
  };
  ASSERT_OK(client_end().read_and_check(bytes_out));
}

// Check that Target replies to a two-way call with an error (for a method using error syntax).
SERVER_TEST(TwoWayResultWithError) {
  constexpr zx_txid_t kTxid = 123u;

  Bytes bytes_in = {
      // clang-format off
      header(kTxid, kOrdinalTwoWayResult, fidl::MessageDynamicFlags::kStrictMethod),
      union_ordinal(2), inline_envelope(u32(123), false),
      // clang-format on
  };
  ASSERT_OK(client_end().write(bytes_in));

  ASSERT_OK(client_end().wait_for_signal(ZX_CHANNEL_READABLE));

  Bytes bytes_out = {
      // clang-format off
      header(kTxid, kOrdinalTwoWayResult, fidl::MessageDynamicFlags::kStrictMethod),
      union_ordinal(2), inline_envelope(u32(123), false),
      // clang-format on
  };
  ASSERT_OK(client_end().read_and_check(bytes_out));
}

}  // namespace server_suite
