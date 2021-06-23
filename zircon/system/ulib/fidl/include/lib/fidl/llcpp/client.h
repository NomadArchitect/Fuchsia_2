// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_CLIENT_H_
#define LIB_FIDL_LLCPP_CLIENT_H_

#include <lib/fidl/llcpp/client_base.h>
#include <lib/fidl/llcpp/client_end.h>
#include <lib/fidl/llcpp/wire_messaging.h>

namespace fidl {

// |fidl::ObserveTeardown| is used with |fidl::WireSharedClient| and allows
// custom logic to run on teardown completion, represented by a callable
// |callback| that takes no parameters and returns |void|. It should be supplied
// as the last argument when constructing or binding the client. See lifecycle
// notes on |fidl::WireSharedClient|.
template <typename Callable>
fidl::internal::AnyTeardownObserver ObserveTeardown(Callable&& callback) {
  static_assert(std::is_convertible<Callable, fit::closure>::value,
                "|callback| must have the signature `void fn()`.");
  return fidl::internal::AnyTeardownObserver::ByCallback(std::forward<Callable>(callback));
}

// |fidl::ShareUntilTeardown| configures a |fidl::WireSharedClient| to co-own
// the supplied |object| until teardown completion. It may be used to extend the
// lifetime of user objects responsible for handling messages. It should be
// supplied as the last argument when constructing or binding the client. See
// lifecycle notes on |fidl::WireSharedClient|.
template <typename T>
fidl::internal::AnyTeardownObserver ShareUntilTeardown(std::shared_ptr<T> object) {
  return fidl::internal::AnyTeardownObserver::ByOwning(object);
}

// A client for sending and receiving wire messages.
//
// Generated FIDL APIs are accessed by 'dereferencing' the |Client|:
//
//     // Creates a client that speaks over |client_end|, on the |my_dispatcher| dispatcher.
//     fidl::Client client(std::move(client_end), my_dispatcher);
//
//     // Call the |Foo| method asynchronously, passing in a callback that will be
//     // invoked on a dispatcher thread when the server response arrives.
//     auto status = client->Foo(args, [] (Result result) {});
//
// ## Lifecycle
//
// A client must be **bound** to an endpoint before it could be used.
// This association between the endpoint and the client is called a "binding".
// Binding a client to an endpoint starts the monitoring of incoming messages.
// Those messages are appropriately dispatched: to response callbacks,
// to event handlers, etc. FIDL methods (asyncrhonous or synchronous) may only
// be invoked on a bound client.
//
// Internally, a client is a lightweight reference to the binding, performing
// its duties indirectly through that object, as illustrated by the simplified
// diagram below:
//
//                 references               makes
//       client  ------------->  binding  -------->  FIDL call
//
// This means that the client _object_ and the binding have overlapping but
// slightly different lifetimes. For example, the binding may terminate in
// response to fatal communication errors, leaving the client object alive
// but unable to make any calls.
//
// To stop the monitoring of incoming messages, one may **teardown** the
// binding. When teardown is initiated, the client will not monitor new messages
// on the endpoint. Ongoing callbacks will be allowed to run to completion. When
// teardown is complete, further calls on the same client will fail.
// Unfulfilled response callbacks will be dropped.
//
// Destruction of a client object will initiate teardown.
//
// |Unbind| may be called on a |Client| to explicitly initiate teardown.
//
// |WaitForChannel| unbinds the endpoint from the client, allowing the endpoint
// to be recovered as the return value. As part of this process, it will
// initiate teardown. Care must be taken when using this function, as it will be
// waiting for any synchronous calls to finish, and will forget about any
// in-progress asynchronous calls.
//
// TODO(fxbug.dev/68742): We may want to also wait for asynchronous calls, or
// panic when there are in-flight asynchronous calls.
//
// ## Thread safety
//
// FIDL method calls on this class are thread-safe. |Unbind|, |Clone|, and
// |WaitForChannel| are also thread-safe, and may be invoked in parallel with
// FIDL method calls. However, those operations must be synchronized with
// operations that consume or mutate the |Client| itself:
//
// - Assigning a new value to the |Client| variable.
// - Moving the |Client| to a different location.
// - Destroying the |Client| variable.
//
template <typename Protocol>
class Client final {
  using ClientImpl = fidl::internal::WireClientImpl<Protocol>;

 public:
  // Create an initialized Client which manages the binding of the client end of
  // a channel to a dispatcher, as if that client had been default-constructed
  // then later bound to that endpoint via |Bind|.
  //
  // It is a logic error to use a dispatcher that is shutting down or already
  // shut down. Doing so will result in a panic.
  //
  // If any other error occurs during initialization, the
  // |event_handler->on_fidl_error| handler will be invoked asynchronously with
  // the reason, if specified.
  //
  // TODO(fxbug.dev/75485): Take a raw pointer to the event handler.
  template <typename AsyncEventHandler = fidl::WireAsyncEventHandler<Protocol>>
  Client(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
         std::shared_ptr<AsyncEventHandler> event_handler = nullptr) {
    Bind(std::move(client_end), dispatcher, std::move(event_handler));
  }

  // Create an uninitialized Client. The client may then be bound to an endpoint
  // later via |Bind|.
  //
  // Prefer using the constructor overload that binds the client to a channel
  // atomically during construction. Use this default constructor only when the
  // client must be constructed first before a channel could be obtained (for
  // example, if the client is an instance variable).
  Client() = default;

  // Returns if the |Client| is initialized.
  bool is_valid() const { return controller_.is_valid(); }
  explicit operator bool() const { return is_valid(); }

  // If the current |Client| is the last instance controlling the current
  // connection, the destructor of this |Client| will trigger unbinding, which
  // will cause any strong references to the |ClientBase| to be released.
  //
  // When the last |Client| destructs:
  // - The channel will be closed.
  // - Pointers obtained via |get| will be invalidated.
  // - Unbinding will happen, implying:
  //   * In-progress calls will be forgotten. Async callbacks will be dropped.
  //   * The |Unbound| callback in the |event_handler| will be invoked, if one
  //     was specified when creating or binding the client, on a dispatcher
  //     thread.
  //
  // See also: |Unbind|.
  ~Client() = default;

  // |fidl::Client|s can be safely moved without affecting any in-flight FIDL
  // method calls. Note that calling methods on a client should be serialized
  // with respect to operations that consume the client, such as moving it or
  // destroying it.
  Client(Client&& other) noexcept = default;
  Client& operator=(Client&& other) noexcept = default;

  // Initializes the client by binding the |client_end| endpoint to the
  // dispatcher.
  //
  // It is a logic error to invoke |Bind| on a dispatcher that is shutting down
  // or already shut down. Doing so will result in a panic.
  //
  // When other errors occur during binding, the |event_handler->on_fidl_error|
  // handler will be asynchronously invoked with the reason, if specified.
  //
  // It is not allowed to call |Bind| on an initialized client. To rebind a
  // |Client| to a different endpoint, simply replace the |Client| variable with
  // a new instance.
  //
  // TODO(fxbug.dev/75485): Take a raw pointer to the event handler.
  void Bind(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
            std::shared_ptr<fidl::WireAsyncEventHandler<Protocol>> event_handler = nullptr) {
    controller_.Bind(std::make_shared<ClientImpl>(), client_end.TakeChannel(), dispatcher,
                     event_handler.get(), fidl::ShareUntilTeardown(event_handler));
  }

  // Begins to unbind the channel from the dispatcher. May be called from any
  // thread. If provided, the |fidl::WireAsyncEventHandler<Protocol>::Unbound|
  // is invoked asynchronously on a dispatcher thread.
  //
  // NOTE: |Bind| must have been called before this.
  //
  // WARNING: While it is safe to invoke |Unbind| from any thread, it is unsafe
  // to wait on the |fidl::WireAsyncEventHandler<Protocol>::Unbound| from a
  // dispatcher thread, as that will likely deadlock.
  //
  // Unbinding can happen automatically via RAII. |Client|s will release
  // resources automatically when they are destructed. See also: |~Client|.
  void Unbind() { controller_.Unbind(); }

  // Returns another |Client| instance sharing the same channel.
  //
  // Prefer to |Clone| only when necessary e.g. extending the lifetime of a
  // |Client| to a different scope. Any living clone will prevent the cleanup of
  // the channel, unless one explicitly call |WaitForChannel|.
  Client Clone() { return Client(*this); }

  // Returns the underlying channel. Unbinds from the dispatcher if required.
  //
  // NOTE: |Bind| must have been called before this.
  //
  // WARNING: This is a blocking call. It waits for completion of dispatcher
  // unbind and of any channel operations, including synchronous calls which may
  // block indefinitely. It should not be invoked on the dispatcher thread if
  // the dispatcher is single threaded.
  fidl::ClientEnd<Protocol> WaitForChannel() {
    return fidl::ClientEnd<Protocol>(controller_.WaitForChannel());
  }

  // Returns the interface for making outgoing FIDL calls. If the client has
  // been unbound, calls on the interface return error with status
  // |ZX_ERR_CANCELED| and reason |fidl::Reason::kUnbind|.
  //
  // Persisting this pointer to a local variable is discouraged, since that
  // results in unsafe borrows. Always prefer making calls directly via the
  // |fidl::Client| reference-counting type. A client may be cloned and handed
  // off through the |Clone| method.
  ClientImpl* operator->() const { return get(); }
  ClientImpl& operator*() const { return *get(); }

 private:
  ClientImpl* get() const { return static_cast<ClientImpl*>(controller_.get()); }

  Client(const Client& other) noexcept = default;
  Client& operator=(const Client& other) noexcept = default;

  internal::ClientController controller_;
};

template <typename Protocol, typename AsyncEventHandlerReference>
Client(fidl::ClientEnd<Protocol>, async_dispatcher_t*, AsyncEventHandlerReference&&)
    -> Client<Protocol>;

template <typename Protocol>
Client(fidl::ClientEnd<Protocol>, async_dispatcher_t*) -> Client<Protocol>;

// |WireSharedClient| is a client for sending and receiving wire messages. It is
// suitable for systems with less defined threading guarantees, by providing the
// building blocks to implement a two-phase asynchronous shutdown pattern.
//
// During teardown, |WireSharedClient| exposes a synchronization point beyond
// which it will not make any more upcalls to user code. The user may then
// arrange any objects that are the recipient of client callbacks to be
// destroyed after the synchronization point. As a result, when destroying an
// entire subsystem, the teardown of the client may be requested from an
// arbitrary thread, in parallel with any callbacks to user code, while
// avoiding use-after-free of user objects.
//
// In addition, |WireSharedClient| supports cloning multiple instances sharing
// the same underlying endpoint.
//
// ## Lifecycle
//
// See lifecycle notes on |fidl::Client| for general lifecycle information.
// Here we note the additional subtleties and two-phase shutdown features
// exclusive to |WireSharedClient|.
//
// Teardown of the binding is an asynchronous process, to account for the
// possibility of in-progress calls to user code. For example, the bindings
// runtime could be invoking a response callback from a dispatcher thread, while
// the user initiates teardown from an unrelated thread.
//
// There are a number of ways to monitor the completion of teardown:
//
// ### Owned event handler
//
// Transfer the ownership of an event handler to the bindings as an
// implementation of |std::unique_ptr<fidl::WireAsyncEventHandler<Protocol>>|
// when binding the client. After teardown is complete, the event handler will
// be destroyed. It is safe to destroy the user objects referenced by any client
// callbacks from within the event handler destructor.
//
// Here is an example for this pattern:
//
//     // A |DatabaseClient| makes requests using a FIDL client. It may be
//     // called from multiple threads.
//     class DatabaseClient {
//      public:
//       // Creates a database client that is owned by the FIDL bindings.
//       // Call |Teardown| to schedule the destruction of the client.
//       static DatabaseClient* Create() { return new DatabaseClient(); }
//
//       void Teardown() { client_.AsyncTeardown(); }
//
//      private:
//       DatabaseClient() {
//         // Pass in a |TeardownObserver| (see below) to listen for the
//         // completion of FIDL client teardown.
//         client_.Bind(client_end, dispatcher, std::make_unique<TeardownObserver>(this));
//       }
//       fidl::WireSharedClient<DbProtocol> client_;
//     };
//
//     class TeardownObserver : public fidl::WireAsyncEventHandler<DbProtocol> {
//      public:
//       ~TeardownObserver() override {
//         // Delete the client that was allocated in |DatabaseClient::Create|.
//         delete database_client_;
//       }
//      private:
//       DatabaseClient* database_client_;
//     };
//
// ### Custom teardown observer
//
// Provide an instance of |fidl::internal::AnyTeardownObserver| to the bindings.
// The observer will be notified when teardown is complete. There are several
// ways to create a teardown observer:
//
// |fidl::ObserveTeardown| takes an arbitrary callable and wraps it in a
// teardown observer:
//
//     fidl::WireSharedClient<DbProtocol> client;
//
//     // Let's say |db_client| is constructed on the heap;
//     DatabaseClient* db_client = CreateDatabaseClient();
//     // ... and needs to be freed by |DestroyDatabaseClient|.
//     auto observer = fidl::ObserveTeardown([] { DestroyDatabaseClient(); });
//
//     // |db_client| may implement |fidl::WireAsyncEventHandler<DbProtocol>|.
//     // |observer| will be notified and destroy |db_client| after teardown.
//     client.Bind(client_end, dispatcher, db_client, std::move(observer));
//
// |fidl::ShareUntilTeardown| takes a |std::shared_ptr<T>|, and arranges the
// binding to destroy its shared reference after teardown:
//
//     // Let's say |db_client| is always managed by a shared pointer.
//     std::shared_ptr<DatabaseClient> db_client = CreateDatabaseClient();
//
//     // |db_client| will be kept alive as long as the binding continues
//     // to exist. After teardown completes, |db_client| will be destroyed
//     // only if there are no other shared references (such as from other
//     // related user objects).
//     auto observer = fidl::ShareUntilTeardown(db_client);
//     client.Bind(client_end, dispatcher, *db_client, std::move(observer));
//
// A |WireSharedClient| may be |Clone|d, with the clone referencing the same
// endpoint. Automatic teardown occurs when the last clone bound to the
// endpoint is destructed.
//
// |AsyncTeardown| may be called on a |WireSharedClient| to explicitly initiate
// teardown.
//
// ## Thread safety
//
// FIDL method calls on this class are thread-safe. |AsyncTeardown|, |Clone|, and
// |WaitForChannel| are also thread-safe, and may be invoked in parallel with
// FIDL method calls. However, those operations must be synchronized with
// operations that consume or mutate the |WireSharedClient| itself:
//
// - Assigning a new value to the |WireSharedClient| variable.
// - Moving the |WireSharedClient| to a different location.
// - Destroying the |WireSharedClient| variable.
//
// When teardown completes, the binding will notify the user from a |dispatcher|
// thread, unless the user shuts down the |dispatcher| while there are active
// clients associated with it. In that case, those clients will be synchronously
// torn down, and the notification (e.g. destroying the event handler) will
// happen on the thread invoking dispatcher shutdown.
template <typename Protocol>
class WireSharedClient final {
  using ClientImpl = fidl::internal::WireClientImpl<Protocol>;

 public:
  // Creates an initialized |WireSharedClient| which manages the binding of the
  // client end of a channel to a dispatcher.
  //
  // It is a logic error to use a dispatcher that is shutting down or already
  // shut down. Doing so will result in a panic.
  //
  // If any other error occurs during initialization, the
  // |event_handler->on_fidl_error| handler will be invoked asynchronously with
  // the reason, if specified.
  //
  // |event_handler| will be destroyed when teardown completes.
  template <typename AsyncEventHandler = fidl::WireAsyncEventHandler<Protocol>>
  WireSharedClient(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
                   std::unique_ptr<AsyncEventHandler> event_handler) {
    Bind(std::move(client_end), dispatcher, std::move(event_handler));
  }

  // Creates a |WireSharedClient| that supports custom behavior on teardown
  // completion via |teardown_observer|. Through helpers that return an
  // |AnyTeardownObserver|, users may link the completion of teardown to the
  // invocation of a callback or the lifecycle of related business objects. See
  // for example |fidl::ObserveTeardown| and |fidl::ShareUntilTeardown|.
  //
  // This overload does not demand taking ownership of |event_handler| by
  // |std::unique_ptr|, hence is suitable when the |event_handler| needs to be
  // managed independently of the client lifetime.
  //
  // See |WireSharedClient| above for other behavior aspects of the constructor.
  template <typename AsyncEventHandler = fidl::WireAsyncEventHandler<Protocol>>
  WireSharedClient(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
                   AsyncEventHandler* event_handler,
                   fidl::internal::AnyTeardownObserver teardown_observer =
                       fidl::internal::AnyTeardownObserver::Noop()) {
    Bind(std::move(client_end), dispatcher, event_handler, std::move(teardown_observer));
  }

  // Overload of |WireSharedClient| that omits the |event_handler|, to
  // workaround C++ limitations on default arguments.
  //
  // See |WireSharedClient| above for other behavior aspects of the constructor.
  template <typename AsyncEventHandler = fidl::WireAsyncEventHandler<Protocol>>
  WireSharedClient(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
                   fidl::internal::AnyTeardownObserver teardown_observer =
                       fidl::internal::AnyTeardownObserver::Noop()) {
    Bind(std::move(client_end), dispatcher, nullptr, std::move(teardown_observer));
  }

  // Creates an uninitialized |WireSharedClient|.
  //
  // Prefer using the constructor overload that binds the client to a channel
  // atomically during construction. Use this default constructor only when the
  // client must be constructed first before a channel could be obtained (for
  // example, if the client is an instance variable).
  WireSharedClient() = default;

  // Returns if the |WireSharedClient| is initialized.
  bool is_valid() const { return controller_.is_valid(); }
  explicit operator bool() const { return is_valid(); }

  // If the current |WireSharedClient| is the last instance controlling the
  // current connection, the destructor of this |WireSharedClient| will trigger
  // teardown.
  //
  // When the last |WireSharedClient| destructs:
  // - The channel will be closed.
  // - Pointers obtained via |get| will be invalidated.
  // - Teardown will be initiated. See the **Lifecycle** section from the
  //   class documentation of |fidl::Client|.
  //
  // See also: |AsyncTeardown|.
  ~WireSharedClient() = default;

  // |fidl::WireSharedClient|s can be safely moved without affecting any in-progress
  // operations. Note that calling methods on a client should be serialized with
  // respect to operations that consume the client, such as moving it or
  // destroying it.
  WireSharedClient(WireSharedClient&& other) noexcept = default;
  WireSharedClient& operator=(WireSharedClient&& other) noexcept = default;

  // Initializes the client by binding the |client_end| endpoint to the dispatcher.
  //
  // It is a logic error to invoke |Bind| on a dispatcher that is shutting down
  // or already shut down. Doing so will result in a panic.
  //
  // It is not allowed to call |Bind| on an initialized client. To rebind a
  // |WireSharedClient| to a different endpoint, simply replace the
  // |WireSharedClient| variable with a new instance.
  //
  // When other error occurs during binding, the |event_handler->on_fidl_error|
  // handler will be asynchronously invoked with the reason, if specified.
  //
  // |event_handler| will be destroyed when teardown completes.
  void Bind(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
            std::unique_ptr<fidl::WireAsyncEventHandler<Protocol>> event_handler) {
    auto event_handler_raw = event_handler.get();
    Bind(std::move(client_end), dispatcher, event_handler_raw,
         fidl::internal::AnyTeardownObserver::ByOwning(std::move(event_handler)));
  }

  // Overload of |Bind| that supports custom behavior on teardown completion via
  // |teardown_observer|. Through helpers that return an |AnyTeardownObserver|,
  // users may link the completion of teardown to the invocation of a callback
  // or the lifecycle of related business objects. See for example
  // |fidl::ObserveTeardown| and |fidl::ShareUntilTeardown|.
  //
  // This overload does not demand taking ownership of |event_handler| by
  // |std::unique_ptr|, hence is suitable when the |event_handler| needs to be
  // managed independently of the client lifetime.
  //
  // See |Bind| above for other behavior aspects of the function.
  void Bind(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
            fidl::WireAsyncEventHandler<Protocol>* event_handler,
            fidl::internal::AnyTeardownObserver teardown_observer =
                fidl::internal::AnyTeardownObserver::Noop()) {
    controller_.Bind(std::make_shared<ClientImpl>(), client_end.TakeChannel(), dispatcher,
                     event_handler, std::move(teardown_observer));
  }

  // Overload of |Bind| that omits the |event_handler|, to
  // workaround C++ limitations on default arguments.
  //
  // See |Bind| above for other behavior aspects of the constructor.
  void Bind(fidl::ClientEnd<Protocol> client_end, async_dispatcher_t* dispatcher,
            fidl::internal::AnyTeardownObserver teardown_observer =
                fidl::internal::AnyTeardownObserver::Noop()) {
    Bind(client_end.TakeChannel(), dispatcher, nullptr, std::move(teardown_observer));
  }

  // Initiates asynchronous teardown of the bindings. See the **Lifecycle**
  // section from the class documentation.
  //
  // |Bind| must have been called before this.
  //
  // While it is safe to invoke |AsyncTeardown| from any thread, it is unsafe to
  // wait for teardown to complete from a dispatcher thread, as that will likely
  // deadlock.
  void AsyncTeardown() { controller_.Unbind(); }

  // Returns another |WireSharedClient| instance sharing the same channel.
  //
  // Prefer to |Clone| only when necessary e.g. extending the lifetime of a
  // |WireSharedClient| to a different scope. Any living clone will prevent the
  // cleanup of the channel, unless one explicitly call |WaitForChannel|.
  WireSharedClient Clone() { return WireSharedClient(*this); }

  // Returns the interface for making outgoing FIDL calls. If the client has
  // been unbound, calls on the interface return error with status
  // |ZX_ERR_CANCELED| and reason |fidl::Reason::kUnbind|.
  //
  // Persisting this pointer to a local variable is discouraged, since that
  // results in unsafe borrows. Always prefer making calls directly via the
  // |fidl::WireSharedClient| reference-counting type. A client may be cloned and handed
  // off through the |Clone| method.
  ClientImpl* operator->() const { return get(); }
  ClientImpl& operator*() const { return *get(); }

 private:
  ClientImpl* get() const { return static_cast<ClientImpl*>(controller_.get()); }

  WireSharedClient(const WireSharedClient& other) noexcept = default;
  WireSharedClient& operator=(const WireSharedClient& other) noexcept = default;

  internal::ClientController controller_;
};

template <typename Protocol, typename AsyncEventHandlerReference>
WireSharedClient(fidl::ClientEnd<Protocol>, async_dispatcher_t*, AsyncEventHandlerReference&&,
                 fidl::internal::AnyTeardownObserver) -> WireSharedClient<Protocol>;

template <typename Protocol, typename AsyncEventHandlerReference>
WireSharedClient(fidl::ClientEnd<Protocol>, async_dispatcher_t*, AsyncEventHandlerReference&&)
    -> WireSharedClient<Protocol>;

template <typename Protocol>
WireSharedClient(fidl::ClientEnd<Protocol>, async_dispatcher_t*) -> WireSharedClient<Protocol>;

}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_CLIENT_H_
