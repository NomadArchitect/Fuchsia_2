// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_WIRE_MESSAGING_H_
#define LIB_FIDL_LLCPP_WIRE_MESSAGING_H_

#ifdef __Fuchsia__
#include <lib/fidl/llcpp/client_end.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/transaction.h>
#include <zircon/fidl.h>
#endif  // __Fuchsia__

namespace fidl {

template <typename FidlMethod>
struct WireRequest;

template <typename FidlMethod>
struct WireResponse;

#ifdef __Fuchsia__
// SyncClient owns a client endpoint and exposes synchronous FIDL calls.
template <typename FidlProtocol>
class WireSyncClient;

// This is the wire async client for the given protocol.
template <typename FidlProtocol>
class WireClient;

// This is the wire sync event handler for the given protocol.
template <typename FidlProtocol>
class WireSyncEventHandler;

// AsyncEventHandler is used by asynchronous clients and adds a callback
// for unbind completion on top of EventHandlerInterface.
template <typename FidlProtocol>
class WireAsyncEventHandler;

// Pure-virtual interface to be implemented by a server.
// This interface uses typed channels (i.e. |fidl::ClientEnd<SomeProtocol>|
// and |fidl::ServerEnd<SomeProtocol>|).
template <typename FidlProtocol>
class WireServer;

// EventSender owns a server endpoint and exposes methods for sending events.
template <typename FidlProtocol>
class WireEventSender;

template <typename FidlMethod>
class WireResponseContext;

template <typename FidlMethod>
class WireResult;

template <typename FidlMethod>
class WireUnownedResult;

namespace internal {

// WeakEventSender borrows the server endpoint from a binding object and
// exposes methods for sending events.
template <typename FidlProtocol>
class WireWeakEventSender;

// ClientImpl implements both synchronous and asynchronous FIDL calls,
// working together with the |::fidl::internal::ClientBase| class to safely
// borrow channel ownership from the binding object.
template <typename FidlProtocol>
class WireClientImpl;

template <typename FidlProtocol>
class WireEventHandlerInterface;

template <typename FidlProtocol>
class WireCaller;

template <typename FidlProtocol>
struct WireServerDispatcher;

}  // namespace internal

// |WireCall| is used to make method calls directly on a |fidl::ClientEnd|
// without having to set up a client. Call it like:
//   WireCall(client_end).Method(args...);
template <typename FidlProtocol>
fidl::internal::WireCaller<FidlProtocol> WireCall(const fidl::ClientEnd<FidlProtocol>& client_end) {
  return fidl::internal::WireCaller<FidlProtocol>(client_end.borrow());
}

// |WireCall| is used to make method calls directly on a |fidl::ClientEnd|
// without having to set up a client. Call it like:
//   WireCall(client_end).Method(args...);
template <typename FidlProtocol>
fidl::internal::WireCaller<FidlProtocol> WireCall(
    const fidl::UnownedClientEnd<FidlProtocol>& client_end) {
  return fidl::internal::WireCaller<FidlProtocol>(client_end);
}

enum class DispatchResult;

// Dispatches the incoming message to one of the handlers functions in the protocol.
//
// This function should only be used in very low-level code, such as when manually
// dispatching a message to a server implementation.
//
// If there is no matching handler, it closes all the handles in |msg| and closes the channel with
// a |ZX_ERR_NOT_SUPPORTED| epitaph, before returning |fidl::DispatchResult::kNotFound|.
//
// Ownership of handles in |msg| are always transferred to the callee.
//
// The caller does not have to ensure |msg| has a |ZX_OK| status. It is idiomatic to pass a |msg|
// with potential errors; any error would be funneled through |InternalError| on the |txn|.
template <typename FidlProtocol>
fidl::DispatchResult WireDispatch(fidl::WireServer<FidlProtocol>* impl, fidl::IncomingMessage&& msg,
                                  fidl::Transaction* txn) {
  return fidl::internal::WireServerDispatcher<FidlProtocol>::Dispatch(impl, std::move(msg), txn);
}

// Dispatches the incoming message to one of the handlers functions in the protocol.
//
// This function should only be used in very low-level code, such as when manually
// dispatching a message to a server implementation.
//
// If there is no matching handler, it closes all the handles in |msg| and closes the channel with
// a |ZX_ERR_NOT_SUPPORTED| epitaph, before returning |fidl::DispatchResult::kNotFound|.
//
// This function takes a |const fidl_incoming_msg_t*| to aid interop with driver C APIs.
// Prefer using the overload with |fidl::IncomingMessage&&| if possible.
//
// Ownership of handles in |msg| are always transferred to the callee.
template <typename FidlProtocol>
fidl::DispatchResult WireDispatch(fidl::WireServer<FidlProtocol>* impl,
                                  const fidl_incoming_msg_t* msg, fidl::Transaction* txn) {
  return fidl::internal::WireServerDispatcher<FidlProtocol>::Dispatch(
      impl, fidl::IncomingMessage::FromEncodedCMessage(msg), txn);
}

// Attempts to dispatch the incoming message to a handler function in the server implementation.
//
// This function should only be used in very low-level code, such as when manually
// dispatching a message to a server implementation.
//
// If there is no matching handler, it returns |fidl::DispatchResult::kNotFound|, leaving the
// message and transaction intact. In all other cases, it consumes the message and returns
// |fidl::DispatchResult::kFound|. It is possible to chain multiple TryDispatch functions in this
// manner.
//
// The caller does not have to ensure |msg| has a |ZX_OK| status. It is idiomatic to pass a |msg|
// with potential errors; any error would be funneled through |InternalError| on the |txn|.
template <typename FidlProtocol>
fidl::DispatchResult WireTryDispatch(fidl::WireServer<FidlProtocol>* impl,
                                     fidl::IncomingMessage& msg, fidl::Transaction* txn) {
  return fidl::internal::WireServerDispatcher<FidlProtocol>::TryDispatch(impl, msg, txn);
}
#endif  // __Fuchsia__

}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_WIRE_MESSAGING_H_
