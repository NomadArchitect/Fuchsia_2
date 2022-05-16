// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_RUNTIME_INCLUDE_LIB_FDF_CPP_DISPATCHER_H_
#define LIB_DRIVER_RUNTIME_INCLUDE_LIB_FDF_CPP_DISPATCHER_H_

#include <lib/async/dispatcher.h>
#include <lib/fdf/cpp/unowned.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/stdcompat/string_view.h>
#include <lib/zx/status.h>

#include <string>

namespace fdf {

// Usage Notes:
//
// C++ wrapper for a dispatcher, with RAII semantics. Automatically shuts down
// the dispatcher when it goes out of scope.
//
// Example:
//
//  void Driver::OnDispatcherShutdown(fdf_dispatcher_t* dispatcher) {
//    // Handle dispatcher shutdown.
//    // It is now safe to destroy |dispatcher|.
//  }
//
//  void Driver::Start() {
//    // TODO(fxb/85946): update this once scheduler_role is supported.
//    const char* scheduler_role = "";
//
//    auto shutdown_handler = [&]() {
//      OnDispatcherShutdown();
//    };
//    auto dispatcher = fdf::Dispatcher::Create(0, shutdown_handler, scheduler_role);
//
//    fdf::ChannelRead channel_read;
//    ...
//     zx_status_t status = channel_read->Begin(dispatcher.get());
//
//    // The dispatcher will call the channel_read handler when ready.
//  }
//
class Dispatcher {
 public:
  using HandleType = fdf_dispatcher_t*;

  // Called when the asynchronous shutdown for |dispatcher| has completed.
  using ShutdownHandler = fit::callback<void(fdf_dispatcher_t* dispatcher)>;

  // Creates a dispatcher.
  //
  // |options| provides configuration for the dispatcher.
  // See also |FDF_DISPATCHER_OPTION_UNSYNCHRONIZED| and |FDF_DISPATCHER_OPTION_ALLOW_SYNC_CALLS|.
  //
  // |scheduler_role| is a hint. It may or not impact the priority the work scheduler against the
  // dispatcher is handled at. It may or may not impact the ability for other drivers to share
  // zircon threads with the dispatcher.
  //
  // |shutdown_handler| will be called when the dispatcher's asynchronous shutdown
  // has completed. The client is responsible for retaining this structure in memory
  // (and unmodified) until the handler runs.
  //
  // This must be called from a thread managed by the driver runtime.
  static zx::status<Dispatcher> Create(uint32_t options, ShutdownHandler shutdown_handler,
                                       cpp17::string_view scheduler_role = {}) {
    // We need to create an additional shutdown context in addition to the fdf::Dispatcher
    // object, as the fdf::Dispatcher may be destructed before the shutdown handler
    // is called. This can happen if the raw pointer is released from the fdf::Dispatcher.
    auto dispatcher_shutdown_context =
        std::make_unique<DispatcherShutdownContext>(std::move(shutdown_handler));
    fdf_dispatcher_t* dispatcher;
    zx_status_t status =
        fdf_dispatcher_create(options, scheduler_role.data(), scheduler_role.size(),
                              dispatcher_shutdown_context->observer(), &dispatcher);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    dispatcher_shutdown_context.release();
    return zx::ok(Dispatcher(dispatcher));
  }

  // Returns the current thread's dispatcher.
  // This will return NULL if not called from a dispatcher managed thread.
  static Unowned<Dispatcher> GetCurrent() {
    return Unowned<Dispatcher>(fdf_dispatcher_get_current_dispatcher());
  }

  // Returns an unowned dispatcher provided an async dispatcher. If |async_dispatcher| was not
  // retrieved via `fdf_dispatcher_get_async_dispatcher`, the call will result in a crash.
  static Unowned<Dispatcher> From(async_dispatcher_t* async_dispatcher) {
    return Unowned<Dispatcher>(fdf_dispatcher_from_async_dispatcher(async_dispatcher));
  }

  explicit Dispatcher(fdf_dispatcher_t* dispatcher = nullptr) : dispatcher_(dispatcher) {}

  Dispatcher(const Dispatcher& to_copy) = delete;
  Dispatcher& operator=(const Dispatcher& other) = delete;

  Dispatcher(Dispatcher&& other) noexcept : Dispatcher(other.release()) {}
  Dispatcher& operator=(Dispatcher&& other) noexcept {
    reset(other.release());
    return *this;
  }

  // Shutting down a dispatcher is an asynchronous operation.
  //
  // Once |Dispatcher::ShutdownAsync| is called, the dispatcher will no longer
  // accept queueing new async_dispatcher_t operations or ChannelRead callbacks.
  //
  // The dispatcher will asynchronously wait for all pending async_dispatcher_t
  // and ChannelRead callbacks to complete. Then it will serially cancel all
  // remaining callbacks with ZX_ERR_CANCELED and call the shutdown handler set
  // in |Dispatcher::Create|.
  //
  // If the dispatcher is already shutdown, this will do nothing.
  void ShutdownAsync() {
    if (dispatcher_) {
      fdf_dispatcher_shutdown_async(dispatcher_);
    }
  }

  // The dispatcher must be completely shutdown before the dispatcher can be closed.
  // It is safe to call this from the shutdown handler set in |Dispatcher::Create|.
  ~Dispatcher() { close(); }

  fdf_dispatcher_t* get() const { return dispatcher_; }

  void reset(fdf_dispatcher_t* dispatcher = nullptr) {
    close();
    dispatcher_ = dispatcher;
  }

  void close() {
    if (dispatcher_) {
      fdf_dispatcher_destroy(dispatcher_);
      dispatcher_ = nullptr;
    }
  }

  fdf_dispatcher_t* release() {
    fdf_dispatcher_t* ret = dispatcher_;
    dispatcher_ = nullptr;
    return ret;
  }

  // Gets the dispatcher's asynchronous dispatch interface.
  async_dispatcher_t* async_dispatcher() const {
    return dispatcher_ ? fdf_dispatcher_get_async_dispatcher(dispatcher_) : nullptr;
  }

  // Returns the options set for this dispatcher.
  std::optional<uint32_t> options() const {
    return dispatcher_ ? std::optional(fdf_dispatcher_get_options(dispatcher_)) : std::nullopt;
  }

  Unowned<Dispatcher> borrow() const { return Unowned<Dispatcher>(dispatcher_); }

 protected:
  fdf_dispatcher_t* dispatcher_;

 private:
  class DispatcherShutdownContext {
   public:
    explicit DispatcherShutdownContext(ShutdownHandler handler)
        : observer_{CallHandler}, handler_(std::move(handler)) {}

    fdf_dispatcher_shutdown_observer_t* observer() { return &observer_; }

   private:
    static void CallHandler(fdf_dispatcher_t* dispatcher,
                            fdf_dispatcher_shutdown_observer_t* observer) {
      static_assert(offsetof(DispatcherShutdownContext, observer_) == 0);
      auto self = reinterpret_cast<DispatcherShutdownContext*>(observer);
      self->handler_(dispatcher);
      // Delete the pointer allocated in |Dispatcher::Create|.
      delete self;
    }

    fdf_dispatcher_shutdown_observer_t observer_;
    ShutdownHandler handler_;
  };
};

using UnownedDispatcher = Unowned<Dispatcher>;

}  // namespace fdf

#endif  // LIB_DRIVER_RUNTIME_INCLUDE_LIB_FDF_CPP_DISPATCHER_H_
