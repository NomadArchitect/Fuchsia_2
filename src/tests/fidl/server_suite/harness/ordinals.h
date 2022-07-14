// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_
#define SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_

#include <cstdint>

namespace server_suite {

// To find all ordinals:
//
//     cat
//     out/default/fidling/gen/src/tests/fidl/server_suite/fidl/fidl.serversuite/llcpp/fidl/fidl.serversuite/cpp/wire_messaging.cc
//     | grep -e 'constexpr.*kTarget.*Ordinal' -A 1
//
// While using `jq` would be much nicer, large numbers are mishandled and the
// displayed ordinal ends up being incorrect.
//
// Ordinals are redefined here even though they may be accessible via C++
// binding definitions to ensure they are unchanged by changes in the bindings.
static const uint64_t kOrdinalOneWayNoPayload = 5311082811961759320lu;
static const uint64_t kOrdinalTwoWayNoPayload = 6235614372471446922lu;
static const uint64_t kOrdinalTwoWayResult = 4276344194462732275lu;
static const uint64_t kOrdinalGetHandleRights = 3148800032398744921lu;
static const uint64_t kOrdinalGetSignalableEventRights = 3631219818281166758lu;
static const uint64_t kOrdinalEchoAsTransferableSignalableEvent = 4125688742220565241lu;

}  // namespace server_suite

#endif  // SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_
