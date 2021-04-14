// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fidl-handler.h"

#include <fuchsia/io/llcpp/fidl.h>
#include <lib/fidl/internal.h>
#include <lib/fidl/txn_header.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

namespace fs {
namespace {

namespace fio = fuchsia_io;

zx_status_t Reply(fidl_txn_t* txn, const fidl_outgoing_msg_t* msg) {
  auto connection = FidlConnection::FromTxn(txn);
  auto message = fidl::OutgoingMessage::FromEncodedCMessage(msg);
  message.set_txid(connection->Txid());
  message.Write(connection->Channel());
  return message.status();
}

// Don't actually send anything on a channel when completing this operation.
// This is useful for mocking out "close" requests.
zx_status_t NullReply(fidl_txn_t* reply, const fidl_outgoing_msg_t* msg) { return ZX_OK; }

}  // namespace

zx_status_t ReadMessage(zx_handle_t h, FidlDispatchFunction dispatch) {
  ZX_ASSERT(zx_object_get_info(h, ZX_INFO_HANDLE_VALID, NULL, 0, NULL, NULL) == ZX_OK);
  uint8_t bytes[ZXFIDL_MAX_MSG_BYTES];
  zx_handle_info_t handles[ZXFIDL_MAX_MSG_HANDLES];
  fidl_incoming_msg_t msg = {
      .bytes = bytes,
      .handles = handles,
      .num_bytes = 0,
      .num_handles = 0,
  };

  zx_status_t r = zx_channel_read_etc(h, 0, bytes, handles, countof(bytes), countof(handles),
                                      &msg.num_bytes, &msg.num_handles);
  if (r != ZX_OK) {
    return r;
  }

  if (msg.num_bytes < sizeof(fidl_message_header_t)) {
    FidlHandleInfoCloseMany(msg.handles, msg.num_handles);
    return ZX_ERR_IO;
  }

  auto header = reinterpret_cast<fidl_message_header_t*>(msg.bytes);
  fidl_txn_t txn = {
      .reply = Reply,
  };
  FidlConnection connection(std::move(txn), h, header->txid);

  // Callback is responsible for decoding the message, and closing
  // any associated handles.
  return dispatch(&msg, &connection);
}

zx_status_t CloseMessage(FidlDispatchFunction dispatch) {
  fidl::WireRequest<fio::Node::Close>::OwnedEncodedMessage request(zx_txid_t(0));
  auto msg_bytes = request.GetOutgoingMessage().CopyBytes();
  fidl_incoming_msg_t msg = {
      .bytes = msg_bytes.data(),
      .handles = nullptr,
      .num_bytes = static_cast<uint32_t>(msg_bytes.size()),
      .num_handles = 0,
  };

  fidl_txn_t txn = {
      .reply = NullReply,
  };
  FidlConnection connection(std::move(txn), ZX_HANDLE_INVALID, 0);

  // Remote side was closed.
  dispatch(&msg, &connection);
  return ERR_DISPATCHER_DONE;
}

}  // namespace fs
