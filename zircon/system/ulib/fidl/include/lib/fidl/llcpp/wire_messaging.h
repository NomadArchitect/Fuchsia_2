// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_WIRE_MESSAGING_H_
#define LIB_FIDL_LLCPP_WIRE_MESSAGING_H_

#include <lib/fit/function.h>

#ifdef __Fuchsia__
#include <lib/fidl/llcpp/client_end.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/soft_migration.h>
#include <lib/fidl/llcpp/transaction.h>
#include <zircon/fidl.h>

#endif  // __Fuchsia__

// # Wire messaging layer
//
// This header contains forward definitions that support sending and receiving
// wire domain objects over Zircon channels for IPC. The code generator should
// populate the implementation by generating template specializations for each
// class over FIDL method/protocol markers.
//
// Note: a recurring pattern below is a pair of struct/using declaration:
//
//     template <typename T> struct FooTraits;
//     template <typename T> using Foo = typename FooTraits<T>::Foo;
//
// The extra |FooTraits| type is a workaround for C++ not having type alias
// partial specialization. The code generator would specialize |FooTraits|,
// and the using-declarations are helpers to pull out items from the struct.
namespace fidl {

template <typename FidlMethod>
struct WireRequest;

template <typename FidlMethod>
struct WireResponse;

#ifdef __Fuchsia__
// WireSyncClient owns a client endpoint and exposes synchronous FIDL calls.
template <typename FidlProtocol>
class WireSyncClient;

template <typename FidlProtocol>
WireSyncClient(fidl::ClientEnd<FidlProtocol>) -> WireSyncClient<FidlProtocol>;

// WireClient implements a client and exposes both synchronous and asynchronous
// calls.
template <typename FidlProtocol>
class WireClient;

// WireSyncEventHandler is used by synchronous clients to handle events for the
// given protocol.
template <typename FidlProtocol>
class WireSyncEventHandler;

// WireAsyncEventHandler is used by asynchronous clients and adds a callback
// for unbind completion on top of WireEventHandlerInterface.
template <typename FidlProtocol>
class WireAsyncEventHandler;

// WireServer is a pure-virtual interface to be implemented by a server.
// This interface uses typed channels (i.e. |fidl::ClientEnd<SomeProtocol>|
// and |fidl::ServerEnd<SomeProtocol>|).
template <typename FidlProtocol>
class WireServer;

// WireEventSender owns a server endpoint and exposes methods for sending
// events.
template <typename FidlProtocol>
class WireEventSender;

template <typename FidlMethod>
class WireResponseContext;

template <typename FidlMethod>
class WireResult;

template <typename FidlMethod>
class WireUnownedResult;

template <typename FidlMethod>
using WireClientCallback = ::fit::callback<void(::fidl::WireUnownedResult<FidlMethod>&)>;

#endif  // __Fuchsia__

namespace internal {

template <typename FidlMethod>
struct WireOrdinal;

#ifdef __Fuchsia__

// WireWeakEventSender borrows the server endpoint from a binding object and
// exposes methods for sending events.
template <typename FidlProtocol>
class WireWeakEventSender;

// WireClientImpl implements both synchronous and asynchronous FIDL calls,
// working together with the |::fidl::internal::ClientBase| class to safely
// borrow channel ownership from the binding object.
template <typename FidlProtocol>
class WireClientImpl;

// |WireSyncClientImpl| implements synchronous FIDL calls with managed buffers.
template <typename FidlProtocol>
class WireSyncClientImpl;

// |WireSyncBufferClientImpl| implements synchronous FIDL calls with
// caller-provided buffers.
template <typename FidlProtocol>
class WireSyncBufferClientImpl;

template <typename FidlProtocol>
class WireEventHandlerInterface;

template <typename FidlProtocol>
class WireEventDispatcher;

template <typename FidlProtocol>
struct WireServerDispatcher;

template <typename FidlMethod>
class WireRequestView {
 public:
  WireRequestView(fidl::WireRequest<FidlMethod>* request) : request_(request) {}
  fidl::WireRequest<FidlMethod>* operator->() const { return request_; }

 private:
  fidl::WireRequest<FidlMethod>* request_;
};

template <typename FidlMethod>
class WireCompleterBase;

template <typename FidlMethod>
struct WireMethodTypes {
  using Completer = fidl::Completer<>;
};

template <typename FidlMethod>
using WireCompleter = typename fidl::internal::WireMethodTypes<FidlMethod>::Completer;

#endif  // __Fuchsia__

}  // namespace internal

#ifdef __Fuchsia__

enum class DispatchResult;

// Dispatches the incoming message to one of the handlers functions in the protocol.
//
// This function should only be used in very low-level code, such as when manually
// dispatching a message to a server implementation.
//
// If there is no matching handler, it closes all the handles in |msg| and notifies
// |txn| of the error.
//
// Ownership of handles in |msg| are always transferred to the callee.
//
// The caller does not have to ensure |msg| has a |ZX_OK| status. It is idiomatic to pass a |msg|
// with potential errors; any error would be funneled through |InternalError| on the |txn|.
template <typename FidlProtocol>
void WireDispatch(fidl::WireServer<FidlProtocol>* impl, fidl::IncomingMessage&& msg,
                  fidl::Transaction* txn) {
  fidl::internal::WireServerDispatcher<FidlProtocol>::Dispatch(impl, std::move(msg), txn);
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
  FIDL_EMIT_STATIC_ASSERT_ERROR_FOR_TRY_DISPATCH(FidlProtocol);
  return fidl::internal::WireServerDispatcher<FidlProtocol>::TryDispatch(impl, msg, txn);
}
#endif  // __Fuchsia__

}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_WIRE_MESSAGING_H_
