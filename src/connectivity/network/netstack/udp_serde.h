// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_NETSTACK_UDP_SERDE_H_
#define SRC_CONNECTIVITY_NETWORK_NETSTACK_UDP_SERDE_H_

#include <ifaddrs.h>
#include <stdbool.h>

#include "src/lib/fxl/fxl_export.h"

// `udp_serde` exposes methods for serializing and deserializing FIDL messages
// used in the Fast UDP protocol. These methods serialize using a custom wire
// format, including specialized mechanisms for padding and versioning.

// This library is highly customized for the needs of its two users (Netstack
// and fdio) and should not be relied upon by anyone else.

// TODO(https://fxbug.dev/97607): Consider replacing this library with FIDL-at-rest.

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Buffer {
  unsigned char* buf;
  uint64_t buf_size;
} Buffer;

typedef enum IpAddrType { Ipv4, Ipv6 } IpAddrType;

typedef enum DeserializeSendMsgMetaError {
  DeserializeSendMsgMetaErrorNone,
  DeserializeSendMsgMetaErrorInputBufferNull,
  DeserializeSendMsgMetaErrorInputBufferTooSmall,
  DeserializeSendMsgMetaErrorNonZeroPrelude,
  DeserializeSendMsgMetaErrorFailedToDecode,
} DeserializeSendMsgMetaError;

#define kMaxIpAddrSize 16

typedef struct IpAddress {
  IpAddrType addr_type;
  uint8_t addr[kMaxIpAddrSize];
  uint8_t addr_size;
} IpAddress;

typedef struct DeserializeSendMsgMetaResult {
  DeserializeSendMsgMetaError err;
  bool has_addr;
  IpAddress to_addr;
  uint16_t port;
} DeserializeSendMsgMetaResult;

// Utility for deserializing a SendMsgMeta from a provided buffer of bytes
// using the LLCPP bindings.
//
// Returns a `DeserializeSendMsgMetaResult` exposing metadata from the SendMsgMeta.
// On success, the `err` field of the returned result will be set to
// `DeserializeSendMsgMetaErrorNone`. On failure, it will be set to an error
// describing the reason for the failure.
FXL_EXPORT DeserializeSendMsgMetaResult deserialize_send_msg_meta(Buffer buf);

typedef struct CmsgSet {
  bool has_ip_tos;
  uint8_t ip_tos;

  bool has_ip_ttl;
  uint8_t ip_ttl;

  bool has_ipv6_tclass;
  uint8_t ipv6_tclass;

  bool has_ipv6_hoplimit;
  uint8_t ipv6_hoplimit;

  bool has_timestamp_nanos;
  int64_t timestamp_nanos;
} CmsgSet;

typedef struct RecvMsgMeta {
  CmsgSet cmsg_set;
  IpAddrType from_addr_type;
  uint16_t payload_size;
  uint16_t port;
} RecvMsgMeta;

typedef enum SerializeRecvMsgMetaError {
  SerializeRecvMsgMetaErrorNone,
  SerializeRecvMsgMetaErrorOutputBufferNull,
  SerializeRecvMsgMetaErrorOutputBufferTooSmall,
  SerializeRecvMsgMetaErrorFromAddrBufferNull,
  SerializeRecvMsgMetaErrorFromAddrBufferTooSmall,
  SerializeRecvMsgMetaErrorFailedToEncode,
} SerializeRecvMsgMetaError;

// Utility for serializing a RecvMsgMeta into the provided `out_buf` based on the
// metadata provided in `meta` and `from_addr`.
//
// On success, returns SerializeRecvMsgMetaErrorNone. On failure, returns an error
// describing the reason for the failure.
FXL_EXPORT SerializeRecvMsgMetaError serialize_recv_msg_meta(RecvMsgMeta meta, Buffer from_addr,
                                                             Buffer out_buf);

// The length of the prelude bytes in a Tx message.
FXL_EXPORT extern const uint32_t kTxUdpPreludeSize;

// The length of the prelude bytes in an Rx message.
FXL_EXPORT extern const uint32_t kRxUdpPreludeSize;

#ifdef __cplusplus
}

#include <fidl/fuchsia.posix.socket/cpp/wire.h>
#include <lib/stdcompat/span.h>

// Utility for serializing a SendMsgMeta into the provided buffer using the LLCPP
// bindings.
//
// On success, returns true. On failure, returns false.
FXL_EXPORT bool serialize_send_msg_meta(fuchsia_posix_socket::wire::SendMsgMeta& meta,
                                        cpp20::span<uint8_t> out_buf);

// Utility for deserializing a RecvMsgMeta from the provided buffer.
//
// Returns a DecodedMessage<RecvMsgPayload>. On success, the DecodedMessage will
// be have `ok() == true`. On failure, the DecodedMessage will have `ok() == false`.
FXL_EXPORT fidl::unstable::DecodedMessage<fuchsia_posix_socket::wire::RecvMsgMeta>
deserialize_recv_msg_meta(cpp20::span<uint8_t> buf);

#endif  // __cplusplus

#endif  // SRC_CONNECTIVITY_NETWORK_NETSTACK_UDP_SERDE_H_
