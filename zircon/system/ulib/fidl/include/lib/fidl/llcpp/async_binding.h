// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_ASYNC_BINDING_H_
#define LIB_FIDL_LLCPP_ASYNC_BINDING_H_

#include <lib/async/dispatcher.h>
#include <lib/async/task.h>
#include <lib/async/wait.h>
#include <lib/fidl/epitaph.h>
#include <lib/fidl/llcpp/extract_resource_on_destruction.h>
#include <lib/fidl/llcpp/internal/client_details.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/result.h>
#include <lib/fidl/llcpp/server_end.h>
#include <lib/fidl/llcpp/transaction.h>
#include <lib/fidl/llcpp/wire_messaging.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <lib/sync/completion.h>
#include <lib/zx/channel.h>
#include <zircon/fidl.h>

#include <mutex>
#include <optional>

namespace fidl {

// The return value of various Dispatch, TryDispatch, or
// |IncomingMessageDispatcher::dispatch_message| functions, which call into the
// appropriate server message handlers based on the method ordinal.
enum class __attribute__((enum_extensibility(closed))) DispatchResult {
  // The FIDL method ordinal was not recognized by the dispatch function.
  kNotFound = false,

  // The FIDL method ordinal matched one of the handlers.
  // Note that this does not necessarily mean the message was handled successfully.
  // For example, the message could fail to decode.
  kFound = true
};

namespace internal {

class IncomingMessageDispatcher;

// A generic callback type handling the completion of server unbinding.
// Note that the first parameter is a pointer to |IncomingMessageDispatcher|,
// which is the common base interface implemented by all server protocol
// message handling interfaces.
//
// The bindings runtime need to convert this pointer to the specific server
// implementation type before invoking the public unbinding completion callback
// that is |fidl::OnUnboundFn<ServerImpl>|.
using AnyOnUnboundFn = fit::callback<void(IncomingMessageDispatcher*, UnbindInfo, zx::channel)>;

class AsyncTransaction;
class ChannelRef;
class ClientBase;

// |AsyncBinding| objects implement the common logic for registering waits
// on channels, and unbinding. |AsyncBinding| itself composes |async_wait_t|
// which borrows the channel to wait for messages. The actual responsibilities
// of managing channel ownership falls on the various subclasses, which must
// ensure the channel is not destroyed while there are outstanding waits.
class AsyncBinding : private async_wait_t {
 public:
  ~AsyncBinding() __TA_EXCLUDES(lock_) = default;

  void BeginWait() __TA_EXCLUDES(lock_);

  zx_status_t EnableNextDispatch() __TA_EXCLUDES(lock_);

  // |UnbindInternal| attempts to post exactly one task to drive the unbinding process.
  // This enum reflects the result of posting the task.
  enum class UnboundNotificationPostingResult {
    kOk,

    // The unbind task is already running, so we should not post another.
    kRacedWithInProgressUnbind,

    // Failed to post the task to the dispatcher. This is usually due to
    // the dispatcher already shutting down.
    //
    // If the user shuts down the dispatcher when the binding is already
    // established and monitoring incoming messages, then whichever thread
    // that was monitoring incoming messages would drive the unbinding
    // process.
    //
    // If the user calls |BindServer| on a shut-down dispatcher, there is
    // no available thread to drive the unbinding process and report errors.
    // We consider it a programming error, and panic right away. Note that
    // this is inherently racy i.e. shutting down dispatchers while trying
    // to also bind new channels to the same dispatcher, so we may want to
    // reevaluate whether shutting down the dispatcher is an error whenever
    // there is any active binding (fxbug.dev/NNNNN).
    kDispatcherError,
  };

  void Unbind(std::shared_ptr<AsyncBinding>&& calling_ref) __TA_EXCLUDES(lock_) {
    UnbindInternal(std::move(calling_ref), ::fidl::UnbindInfo::Unbind());
  }

  UnboundNotificationPostingResult InternalError(std::shared_ptr<AsyncBinding>&& calling_ref,
                                                 UnbindInfo error) __TA_EXCLUDES(lock_) {
    return UnbindInternal(std::move(calling_ref), error);
  }

  zx::unowned_channel channel() const { return zx::unowned_channel(handle()); }
  zx_handle_t handle() const { return async_wait_t::object; }

 protected:
  struct UnbindTask {
    async_task_t task;
    std::weak_ptr<AsyncBinding> binding;
  };

  static void OnMessage(async_dispatcher_t* dispatcher, async_wait_t* wait, zx_status_t status,
                        const zx_packet_signal_t* signal) {
    static_cast<AsyncBinding*>(wait)->MessageHandler(status, signal);
  }

  static void OnUnbindTask(async_dispatcher_t*, async_task_t* task, zx_status_t status) {
    static_assert(std::is_standard_layout<UnbindTask>::value, "Need offsetof.");
    static_assert(offsetof(UnbindTask, task) == 0, "Cast async_task_t* to UnbindTask*.");
    auto* unbind_task = reinterpret_cast<UnbindTask*>(task);
    if (auto binding = unbind_task->binding.lock()) {
      // Backup the |binding| pointer before moving it into |OnUnbind|.
      // Using a raw pointer to ensure that there is only one strong reference
      // to the binding object during unbinding.
      auto* binding_ptr = binding.get();
      binding_ptr->OnUnbind(std::move(binding), ::fidl::UnbindInfo::Unbind(), true);
    }
    delete unbind_task;
  }

  AsyncBinding(async_dispatcher_t* dispatcher, const zx::unowned_channel& borrowed_channel);

  // Common message handling entrypoint shared by both client and server bindings.
  void MessageHandler(zx_status_t status, const zx_packet_signal_t* signal) __TA_EXCLUDES(lock_);

  // Dispatches a generic incoming message.
  //
  // ## Message ownership
  //
  // The client async binding should invoke the matching response handler or
  // event handler, if one is found. |msg| is then consumed, regardless of
  // decoding error.
  //
  // The server async binding should invoke the matching request handler if
  // one is found. |msg| is then consumed, regardless of decoding error.
  //
  // In other cases (e.g. unknown message, epitaph), |msg| is not consumed.
  //
  // The caller should simply ignore the |fidl::IncomingMessage| object once
  // it is passed to this function, letting RAII clean up handles as needed.
  //
  // ## Return value
  //
  // If errors occur during dispatching, the function will return an
  // |UnbindInfo| describing the error. Otherwise, it will return
  // |std::nullopt|.
  //
  // If `*binding_released` is set, the calling code no longer has ownership of
  // this |AsyncBinding| object and so must not access its state.
  virtual std::optional<UnbindInfo> Dispatch(fidl::IncomingMessage& msg,
                                             bool* binding_released) = 0;

  // Used by both Close() and Unbind().
  UnboundNotificationPostingResult UnbindInternal(std::shared_ptr<AsyncBinding>&& calling_ref,
                                                  UnbindInfo info) __TA_EXCLUDES(lock_);

  // Triggered by explicit Unbind(), channel peer closed, or other channel/dispatcher error in the
  // context of a dispatcher thread with exclusive ownership of the internal binding reference. If
  // the binding is still bound, waits for all other references to be released, sends the epitaph
  // (except for Unbind()), and invokes the error handler if specified. `calling_ref` is released.
  void OnUnbind(std::shared_ptr<AsyncBinding>&& calling_ref, UnbindInfo info,
                bool is_unbind_task = false) __TA_EXCLUDES(lock_);

  // Waits for all references to the binding to be released. Sends epitaph and invokes
  // on_unbound_fn_ as required. Behavior differs between server and client.
  // |FinishUnbind| will be invoked on a dispatcher thread if the dispatcher
  // is running, and will be invoked on the thread that is calling shutdown
  // if the dispatcher is shutting down.
  virtual void FinishUnbind(std::shared_ptr<AsyncBinding>&& calling_ref, UnbindInfo info) = 0;

  async_dispatcher_t* dispatcher_ = nullptr;
  std::shared_ptr<AsyncBinding> keep_alive_ = {};

  std::mutex lock_;
  std::optional<UnbindInfo> unbind_info_ __TA_GUARDED(lock_) = {};
  bool begun_ __TA_GUARDED(lock_) = false;
  bool sync_unbind_ __TA_GUARDED(lock_) = false;
  bool canceled_ __TA_GUARDED(lock_) = false;
};

// Base implementation shared by various specializations of
// |AsyncServerBinding<Protocol>|.
class AnyAsyncServerBinding : public AsyncBinding {
 public:
  AnyAsyncServerBinding(async_dispatcher_t* dispatcher, const zx::unowned_channel& borrowed_channel,
                        IncomingMessageDispatcher* interface)
      : AsyncBinding(dispatcher, borrowed_channel), interface_(interface) {}

  std::optional<UnbindInfo> Dispatch(fidl::IncomingMessage& msg, bool* binding_released) override;

  void Close(std::shared_ptr<AsyncBinding>&& calling_ref, zx_status_t epitaph)
      __TA_EXCLUDES(lock_) {
    UnbindInternal(std::move(calling_ref), fidl::UnbindInfo::Close(epitaph));
  }

 protected:
  friend fidl::internal::AsyncTransaction;

  IncomingMessageDispatcher* interface() const { return interface_; }

 private:
  friend fidl::internal::AsyncTransaction;

  IncomingMessageDispatcher* interface_ = nullptr;
};

// The async server binding for |Protocol|.
// Contains an event sender for that protocol, which directly owns the channel.
template <typename Protocol>
class AsyncServerBinding final : public AnyAsyncServerBinding {
  struct ConstructionKey {};

 public:
  using EventSender = typename fidl::WireEventSender<Protocol>;

  static std::shared_ptr<AsyncServerBinding> Create(async_dispatcher_t* dispatcher,
                                                    fidl::ServerEnd<Protocol>&& server_end,
                                                    IncomingMessageDispatcher* interface,
                                                    AnyOnUnboundFn&& on_unbound_fn) {
    auto ret = std::make_shared<AsyncServerBinding>(dispatcher, std::move(server_end), interface,
                                                    std::move(on_unbound_fn), ConstructionKey{});
    // We keep the binding alive until somebody decides to close the channel.
    ret->keep_alive_ = ret;
    return ret;
  }

  virtual ~AsyncServerBinding() = default;

  const EventSender& event_sender() const { return event_sender_.get(); }

  zx::unowned_channel channel() const { return zx::unowned_channel(event_sender_.get().channel()); }

  // Do not construct this object outside of this class. This constructor takes
  // a private type following the pass-key idiom.
  AsyncServerBinding(async_dispatcher_t* dispatcher, fidl::ServerEnd<Protocol>&& server_end,
                     IncomingMessageDispatcher* interface, AnyOnUnboundFn&& on_unbound_fn,
                     ConstructionKey key)
      : AnyAsyncServerBinding(dispatcher, server_end.channel().borrow(), interface),
        event_sender_(EventSender(std::move(server_end))),
        on_unbound_fn_(std::move(on_unbound_fn)) {}

 private:
  void FinishUnbind(std::shared_ptr<AsyncBinding>&& calling_ref, UnbindInfo info) override {
    // Stash required state after deleting the binding, since the binding
    // will be destroyed as part of this function.
    auto* the_interface = interface();
    auto on_unbound_fn = std::move(on_unbound_fn_);

    // Downcast to our class.
    std::shared_ptr<AsyncServerBinding> server_binding =
        std::static_pointer_cast<AsyncServerBinding>(calling_ref);
    calling_ref.reset();

    // Delete the calling reference.
    // Wait for any transient references to be released.
    DestroyAndExtract(std::move(server_binding), &AsyncServerBinding::event_sender_,
                      [&info, the_interface, &on_unbound_fn](EventSender event_sender) {
                        // `this` is no longer valid.

                        // If required, send the epitaph.
                        zx::channel channel = std::move(event_sender.channel());
                        if (info.reason() == Reason::kClose) {
                          info =
                              UnbindInfo::Close(fidl_epitaph_write(channel.get(), info.status()));
                        }

                        // Execute the unbound hook if specified.
                        if (on_unbound_fn)
                          on_unbound_fn(the_interface, info, std::move(channel));
                      });
  }

  // The channel is owned by AsyncServerBinding.
  ExtractedOnDestruction<EventSender> event_sender_;

  // The user callback to invoke after unbinding has completed.
  AnyOnUnboundFn on_unbound_fn_ = {};
};

// The async client binding. The client supports both synchronous and
// asynchronous calls. Because the channel lifetime must outlast the duration
// of any synchronous calls, and that synchronous calls do not yet support
// cancellation, the client binding does not own the channel directly.
// Rather, it co-owns the channel between itself and any in-flight sync
// calls, using shared pointers.
class AsyncClientBinding final : public AsyncBinding {
 public:
  static std::shared_ptr<AsyncClientBinding> Create(async_dispatcher_t* dispatcher,
                                                    std::shared_ptr<ChannelRef> channel,
                                                    std::shared_ptr<ClientBase> client,
                                                    AsyncEventHandler* event_handler,
                                                    AnyTeardownObserver&& teardown_observer);

  virtual ~AsyncClientBinding() = default;

  std::shared_ptr<ChannelRef> GetChannel() const { return channel_; }

 private:
  AsyncClientBinding(async_dispatcher_t* dispatcher, std::shared_ptr<ChannelRef> channel,
                     std::shared_ptr<ClientBase> client, AsyncEventHandler* event_handler,
                     AnyTeardownObserver&& teardown_observer);

  std::optional<UnbindInfo> Dispatch(fidl::IncomingMessage& msg, bool* binding_released) override;

  void FinishUnbind(std::shared_ptr<AsyncBinding>&& calling_ref, UnbindInfo info) override;

  std::shared_ptr<ChannelRef> channel_ = nullptr;  // Strong reference to the channel.
  std::shared_ptr<ClientBase> client_;
  AsyncEventHandler* event_handler_;
  AnyTeardownObserver teardown_observer_;
};

}  // namespace internal
}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_ASYNC_BINDING_H_
