// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_CLIENT_BASE_H_
#define LIB_FIDL_LLCPP_CLIENT_BASE_H_

#include <lib/async/dispatcher.h>
#include <lib/fidl/llcpp/async_binding.h>
#include <lib/fidl/llcpp/extract_resource_on_destruction.h>
#include <lib/fidl/llcpp/internal/client_details.h>
#include <lib/fidl/llcpp/internal/intrusive_container/wavl_tree.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/zx/channel.h>
#include <zircon/fidl.h>
#include <zircon/listnode.h>
#include <zircon/types.h>

#include <memory>
#include <mutex>

namespace fidl {
namespace internal {

// |ResponseContext| contains information about an outstanding asynchronous
// method call. It inherits from an intrusive container node so that
// |ClientBase| can track it without requiring heap allocation.
//
// The generated code will define type-specific response contexts e.g.
// `FooMethodResponseContext`, that inherits from |ResponseContext| and
// interprets the bytes passed to the |OnReply| call appropriately.
// Users should interact with those subclasses; the notes here on lifecycle
// apply to those subclasses.
//
// ## Lifecycle
//
// The bindings runtime has no opinions about how |ResponseContext|s are
// allocated.
//
// Once a |ResponseContext| is passed to the bindings runtime, ownership
// is transferred to the bindings (in particular, the |ClientBase| object).
// Ownership is returned back to the caller when either |OnReply| or |OnError|
// is invoked. This means that the user or generated code must keep the
// response context object alive for the duration of the async method call.
//
// NOTE: |ResponseContext| are additionally referenced with a |list_node_t|
// in order to safely iterate over outstanding transactions on |ClientBase|
// destruction, invoking |OnError| on each outstanding response context.
class ResponseContext : public fidl::internal_wavl::WAVLTreeContainable<ResponseContext*>,
                        private list_node_t {
 public:
  explicit ResponseContext(uint64_t ordinal)
      : fidl::internal_wavl::WAVLTreeContainable<ResponseContext*>(),
        list_node_t(LIST_INITIAL_CLEARED_VALUE),
        ordinal_(ordinal) {}
  virtual ~ResponseContext() = default;

  // |ResponseContext| objects are "pinned" in memory.
  ResponseContext(const ResponseContext& other) = delete;
  ResponseContext& operator=(const ResponseContext& other) = delete;
  ResponseContext(ResponseContext&& other) = delete;
  ResponseContext& operator=(ResponseContext&& other) = delete;

  uint64_t ordinal() const { return ordinal_; }
  zx_txid_t Txid() const { return txid_; }

  // Invoked if a response has been received for this context.
  //
  // |msg| references the incoming message in encoded form.
  //
  // Ownership of bytes referenced by |msg| stays with the caller.
  // The callee should not access the bytes in |msg| once this method returns.
  //
  // Ownership of handles referenced by |msg| is transferred to the callee.
  //
  // If |OnRawReply| returns |ZX_OK|, that indicates decoding was successful,
  // and |OnRawReply| has invoked the user response handler. Ownership of this
  // object has been transferred to the user.
  //
  // If |OnRawReply| returns an error, that indicates decoding failure, and
  // the caller should invoke |OnError| to propagate the error and give up
  // ownership.
  virtual zx_status_t OnRawReply(::fidl::IncomingMessage&& msg) = 0;

  // Invoked if an error occurs handling the response message prior to invoking
  // the user-specified callback or if the ClientBase is destroyed with the
  // transaction outstanding. Note that |OnError| may be invoked within
  // ~ClientBase(), so the user must ensure that a FIDL client is not
  // destroyed while holding any locks which |OnError| would take.
  //
  // |OnError| is allowed to consume the current object.
  virtual void OnError() = 0;

 private:
  friend class ClientBase;

  // For use with |fidl::internal_wavl::WAVLTree|.
  struct Traits {
    static zx_txid_t GetKey(const ResponseContext& context) { return context.txid_; }
    static bool LessThan(const zx_txid_t& key1, const zx_txid_t& key2) { return key1 < key2; }
    static bool EqualTo(const zx_txid_t& key1, const zx_txid_t& key2) { return key1 == key2; }
  };

  const uint64_t ordinal_;  // Expected ordinal for the response.
  zx_txid_t txid_ = 0;      // Zircon txid of outstanding transaction.
};

// ChannelRef takes ownership of a channel. It can transfer the channel
// ownership on destruction with the use of |DestroyAndExtract|.
// Otherwise, the channel is closed.
class ChannelRef {
 public:
  explicit ChannelRef(zx::channel channel) : channel_(ExtractedOnDestruction(std::move(channel))) {}
  zx_handle_t handle() const { return channel_.get().get(); }

 private:
  template <typename Callback>
  friend void DestroyAndExtract(std::shared_ptr<ChannelRef>&& object, Callback&& callback);

  ExtractedOnDestruction<zx::channel> channel_;
};

template <typename Callback>
void DestroyAndExtract(std::shared_ptr<ChannelRef>&& object, Callback&& callback) {
  DestroyAndExtract(std::move(object), &ChannelRef::channel_, std::forward<Callback>(callback));
}

// ChannelRefTracker takes ownership of a channel, wrapping it in a ChannelRef. It is used to create
// and track one or more strong references to the channel, and supports extracting out its owned
// channel in a thread-safe manner.
class ChannelRefTracker {
 public:
  // Set the given channel as the owned channel.
  void Init(zx::channel channel) __TA_EXCLUDES(lock_);

  // If the |ChannelRef| is still alive, returns a strong reference to it.
  std::shared_ptr<ChannelRef> Get() { return channel_weak_.lock(); }

  // Blocks on the release of any outstanding strong references to the channel and returns it. Only
  // one caller will be able to retrieve the channel. Other calls will return immediately with a
  // null channel.
  zx::channel WaitForChannel() __TA_EXCLUDES(lock_);

 private:
  std::mutex lock_;
  std::shared_ptr<ChannelRef> channel_ __TA_GUARDED(lock_);

  // Weak reference used to access channel without taking locks.
  std::weak_ptr<ChannelRef> channel_weak_;
};

// Base LLCPP client class supporting use with a multithreaded asynchronous dispatcher, safe error
// handling and unbinding, and asynchronous transaction tracking. Users should not directly interact
// with this class. |ClientBase| objects must be managed via std::shared_ptr.
class ClientBase {
 public:
  // Creates an unbound ClientBase. Bind() must be called before any other APIs are invoked.
  ClientBase() = default;
  virtual ~ClientBase() = default;

  // Neither copyable nor movable.
  ClientBase(const ClientBase& other) = delete;
  ClientBase& operator=(const ClientBase& other) = delete;
  ClientBase(ClientBase&& other) = delete;
  ClientBase& operator=(ClientBase&& other) = delete;

  // Bind the channel to the dispatcher. Invoke on_unbound on error or unbinding.
  // NOTE: This is not thread-safe and must be called exactly once, before any other APIs.
  void Bind(std::shared_ptr<ClientBase> client, zx::channel channel, async_dispatcher_t* dispatcher,
            std::shared_ptr<AsyncEventHandler>&& event_handler);

  // Asynchronously unbind the client from the dispatcher. on_unbound will be invoked on a
  // dispatcher thread if provided.
  void Unbind();

  // Waits for all strong references to the channel to be released, then returns it. This
  // necessarily triggers unbinding first in order to release the binding's reference.
  //
  // NOTE: As this returns a zx::channel which owns the handle, only a single call is expected to
  // succeed. Additional calls will simply return an empty zx::channel.
  zx::channel WaitForChannel();

  // Stores the given asynchronous transaction response context, setting the txid field.
  void PrepareAsyncTxn(ResponseContext* context);

  // Forget the transaction associated with the given context. Used when zx_channel_write() fails.
  void ForgetAsyncTxn(ResponseContext* context);

  // Releases all outstanding `ResponseContext`s. Invoked after the ClientBase is unbound.
  void ReleaseResponseContextsWithError();

  // Returns a strong reference to the channel to prevent its destruction during a |zx_channel_call|
  // or |zx_channel_write|. The caller must release the reference after making the call/write,
  // so as not to indefinitely block operations such as |WaitForChannel|.
  //
  // If the client has been unbound, returns |nullptr|.
  std::shared_ptr<ChannelRef> GetChannel() {
    if (auto binding = binding_.lock()) {
      return binding->GetChannel();
    }
    return nullptr;
  }

  // For debugging.
  size_t GetTransactionCount() {
    std::scoped_lock lock(lock_);
    return contexts_.size();
  }

  // Dispatches a generic incoming message.
  //
  // ## Handling events
  //
  // If the incoming message is an event, the implementation should dispatch it
  // using the optional |maybe_event_handler|.
  //
  // If |maybe_event_handler| is null, the implementation should perform all the
  // checks that the message is valid and a recognized event, but not
  // actually invoke the event handler.
  //
  // If |maybe_event_handler| is present, it should point to a event handler
  // subclass which corresponds to the protocol of |ClientImpl|. This constraint
  // is typically enforced when creating the client.
  //
  // ## Message ownership
  //
  // If a matching response handler or event handler is found, |msg| is then
  // consumed, regardless of decoding error. Otherwise, |msg| is not consumed.
  //
  // ## Return value
  //
  // If errors occur during dispatching, the function will return an
  // |UnbindInfo| describing the error. Otherwise, it will return
  // |std::nullopt|.
  std::optional<UnbindInfo> Dispatch(fidl::IncomingMessage& msg,
                                     AsyncEventHandler* maybe_event_handler);

  // Dispatches an incoming event.
  //
  // This should be implemented by the generated messaging layer.
  //
  // ## Handling events
  //
  // If |maybe_event_handler| is null, the implementation should perform all the
  // checks that the message is valid and a recognized event, but not
  // actually invoke the event handler.
  //
  // If |maybe_event_handler| is present, it should point to a event handler
  // subclass which corresponds to the protocol of |ClientImpl|. This constraint
  // is typically enforced when creating the client.
  //
  // ## Message ownership
  //
  // If a matching event handler is found, |msg| is then consumed, regardless of
  // decoding error. Otherwise, |msg| is not consumed.
  //
  // ## Return value
  //
  // If errors occur during dispatching, the function will return an
  // |UnbindInfo| describing the error. Otherwise, it will return
  // |std::nullopt|.
  virtual std::optional<UnbindInfo> DispatchEvent(fidl::IncomingMessage& msg,
                                                  AsyncEventHandler* maybe_event_handler) = 0;

 private:
  ChannelRefTracker channel_tracker_;

  // Weak reference to the internal binding state.
  std::weak_ptr<AsyncClientBinding> binding_;

  // State for tracking outstanding transactions.
  std::mutex lock_;
  // The base node of an intrusive container of ResponseContexts corresponding to outstanding
  // asynchronous transactions.
  fidl::internal_wavl::WAVLTree<zx_txid_t, ResponseContext*, ResponseContext::Traits> contexts_
      __TA_GUARDED(lock_);
  // Mirror list used to safely invoke OnError() on outstanding ResponseContexts in ~ClientBase().
  list_node_t delete_list_ __TA_GUARDED(lock_) = LIST_INITIAL_VALUE(delete_list_);
  zx_txid_t txid_base_ __TA_GUARDED(lock_) = 0;  // Value used to compute the next txid.
};

// |ClientController| manages the lifetime of a |ClientImpl| instance.
// The |ClientImpl| class needs to inherit from |fidl::internal::ClientBase|.
//
// |ClientImpl|s are created when binding a client endpoint to a message
// dispatcher, via |Bind|. The destruction of |ClientImpl|s is initiated when
// this |ClientController| class destructs, or when |Unbind| is explicitly
// invoked.
class ClientController {
 public:
  ClientController() = default;
  ~ClientController() = default;

  ClientController(ClientController&& other) noexcept = default;
  ClientController& operator=(ClientController&& other) noexcept = default;
  ClientController(const ClientController& other) = default;
  ClientController& operator=(const ClientController& other) = default;

  // Binds the client implementation to the |dispatcher| and |client_end|.
  // Takes ownership of |client_impl| and starts managing its lifetime.
  void Bind(ClientBase* client_impl, zx::channel client_end, async_dispatcher_t* dispatcher,
            std::shared_ptr<AsyncEventHandler>&& event_handler);

  // Begins to unbind the channel from the dispatcher. In particular, it
  // triggers the asynchronous destruction of the bound |ClientImpl|. May be
  // called from any thread. If provided, the |AsyncEventHandler::Unbound| is
  // invoked asynchronously on a dispatcher thread.
  //
  // |Bind| must have been called before this.
  void Unbind();

  // Blocks the current thread until no nothing is bound to the channel (and no
  // in-flight uses of the channel exist), then returns the underlying channel.
  // Unbinds from the dispatcher automatically.
  //
  // |Bind| must have been called before this.
  zx::channel WaitForChannel();

  bool is_valid() const { return static_cast<bool>(client_impl_); }
  explicit operator bool() const { return is_valid(); }

  ClientBase* get() const { return client_impl_.get(); }

 private:
  // |ControlBlock| controls the lifecycle of a client binding, such that
  // unbinding will only happen after all clones of a |Client| managing
  // the same channel goes out of scope.
  //
  // Specifically, all clones of a |Client| will share the same |ControlBlock|
  // instance, which in turn references the |ClientImpl|, and is responsible
  // for its unbinding via RAII.
  class ControlBlock final {
   public:
    explicit ControlBlock(std::shared_ptr<ClientBase> client) : client_impl_(std::move(client)) {}

    // Triggers unbinding, which will cause any strong references to the
    // |ClientBase| to be released.
    ~ControlBlock() {
      if (client_impl_) {
        client_impl_->Unbind();
      }
    }

   private:
    std::shared_ptr<ClientBase> client_impl_;
  };

  std::shared_ptr<ClientBase> client_impl_;
  std::shared_ptr<ControlBlock> control_;
};

}  // namespace internal
}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_CLIENT_BASE_H_
