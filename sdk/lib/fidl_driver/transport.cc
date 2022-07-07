// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// clang-format off
// The MakeAnyTransport overloads need to be defined before including
// message.h, which uses them.
#include <lib/fidl_driver/cpp/transport.h>
// clang-format on

#include <lib/fdf/cpp/channel_read.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/message_storage.h>
#include <lib/fidl/llcpp/status.h>
#include <zircon/errors.h>

#include <optional>

namespace fidl {
namespace internal {

namespace {

zx_status_t driver_write(fidl_handle_t handle, WriteOptions write_options, const WriteArgs& args) {
  // Note: in order to force the encoder to only output one iovec, only provide an iovec buffer of
  // 1 element to the encoder.
  ZX_ASSERT(args.data_count == 1);

  const zx_channel_iovec_t& iovec = static_cast<const zx_channel_iovec_t*>(args.data)[0];
  fdf_arena_t* arena =
      write_options.outgoing_transport_context.release<internal::DriverTransport>();
  void* arena_handles = fdf_arena_allocate(arena, args.handles_count * sizeof(fdf_handle_t));
  memcpy(arena_handles, args.handles, args.handles_count * sizeof(fdf_handle_t));

  zx_status_t status =
      fdf_channel_write(handle, 0, arena, const_cast<void*>(iovec.buffer), iovec.capacity,
                        static_cast<fdf_handle_t*>(arena_handles), args.handles_count);
  return status;
}

zx_status_t driver_read(fidl_handle_t handle, const ReadOptions& read_options,
                        const ReadArgs& args) {
  ZX_DEBUG_ASSERT(args.storage_view != nullptr);
  ZX_DEBUG_ASSERT(args.out_data != nullptr);
  DriverMessageStorageView* rd_view = static_cast<DriverMessageStorageView*>(args.storage_view);

  fdf_arena_t* out_arena;
  zx_status_t status =
      fdf_channel_read(handle, 0, &out_arena, args.out_data, args.out_data_actual_count,
                       args.out_handles, args.out_handles_actual_count);
  if (status != ZX_OK) {
    return status;
  }

  *rd_view->arena = fdf::Arena(out_arena);
  return ZX_OK;
}

zx_status_t driver_call(fidl_handle_t handle, CallOptions call_options,
                        const CallMethodArgs& args) {
  ZX_DEBUG_ASSERT(args.rd.storage_view != nullptr);
  ZX_DEBUG_ASSERT(args.rd.out_data != nullptr);
  DriverMessageStorageView* rd_view = static_cast<DriverMessageStorageView*>(args.rd.storage_view);

  // Note: in order to force the encoder to only output one iovec, only provide an iovec buffer of
  // 1 element to the encoder.
  ZX_ASSERT(args.wr.data_count == 1);
  const zx_channel_iovec_t& iovec = static_cast<const zx_channel_iovec_t*>(args.wr.data)[0];
  fdf_arena_t* arena = call_options.outgoing_transport_context.release<DriverTransport>();
  void* arena_handles = fdf_arena_allocate(arena, args.wr.handles_count * sizeof(fdf_handle_t));
  memcpy(arena_handles, args.wr.handles, args.wr.handles_count * sizeof(fdf_handle_t));

  fdf_arena_t* rd_arena = nullptr;
  fdf_channel_call_args fdf_args = {
      .wr_arena = arena,
      .wr_data = const_cast<void*>(iovec.buffer),
      .wr_num_bytes = iovec.capacity,
      .wr_handles = static_cast<fdf_handle_t*>(arena_handles),
      .wr_num_handles = args.wr.handles_count,

      .rd_arena = &rd_arena,
      .rd_data = args.rd.out_data,
      .rd_num_bytes = args.rd.out_data_actual_count,
      .rd_handles = args.rd.out_handles,
      .rd_num_handles = args.rd.out_handles_actual_count,
  };
  zx_status_t status = fdf_channel_call(handle, 0, ZX_TIME_INFINITE, &fdf_args);
  if (status != ZX_OK) {
    return status;
  }

  *rd_view->arena = fdf::Arena(rd_arena);
  return ZX_OK;
}

zx_status_t driver_create_waiter(fidl_handle_t handle, async_dispatcher_t* dispatcher,
                                 TransportWaitSuccessHandler success_handler,
                                 TransportWaitFailureHandler failure_handler,
                                 AnyTransportWaiter& any_transport_waiter) {
  any_transport_waiter.emplace<DriverWaiter>(handle, dispatcher, std::move(success_handler),
                                             std::move(failure_handler));
  return ZX_OK;
}

void driver_create_thread_checker(async_dispatcher_t* dispatcher, ThreadingPolicy threading_policy,
                                  AnyThreadChecker& any_thread_checker) {
  class __TA_CAPABILITY("mutex") DriverThreadChecker final : public ThreadChecker {
   public:
    explicit DriverThreadChecker(async_dispatcher_t* dispatcher, ThreadingPolicy policy)
        : ThreadChecker(policy),
          initial_dispatcher_(fdf_dispatcher_from_async_dispatcher(dispatcher)) {
      if (policy == ThreadingPolicy::kCreateAndTeardownFromDispatcherThread) {
        uint32_t options = fdf_dispatcher_get_options(initial_dispatcher_);
        if (options & FDF_DISPATCHER_OPTION_UNSYNCHRONIZED) {
          // This error indicates that the user is using a synchronized FIDL
          // binding (e.g. |fdf::WireClient|) over an unsynchronized dispatcher.
          // This is not allowed, as it leads to thread safety issues.
          resumable_panic(
              "A synchronized fdf_dispatcher_t is required. "
              "Ensure the fdf_dispatcher_t does not have the "
              "|FDF_DISPATCHER_OPTION_UNSYNCHRONIZED| option.");
        }
      }
    }

    // Checks for exclusive access by checking that the current thread is the
    // same as the constructing thread.
    void check() const __TA_ACQUIRE() override {
      if (policy() == ThreadingPolicy::kCreateAndTeardownFromDispatcherThread) {
        fdf_dispatcher_t* current_dispatcher = fdf_dispatcher_get_current_dispatcher();
        if (current_dispatcher == nullptr) {
          // This error indicates that the user is destroying a synchronized
          // FIDL binding (e.g. |fdf::WireClient|) on a thread that is not
          // managed by a driver dispatcher. This is not allowed, as it leads to
          // thread safety issues.
          resumable_panic(
              "Current thread is not managed by a driver dispatcher. "
              "Ensure binding and teardown occur on a dispatcher managed thread.");
          return;
        }
        if (initial_dispatcher_ != current_dispatcher) {
          // This error indicates that the user is destroying a synchronized
          // FIDL binding (e.g. |fdf::WireClient|) on a thread whose dispatcher
          // is not the same as the one it is bound to. This is not allowed, as
          // it leads to thread safety issues.
          resumable_panic(
              "Currently executing on a different dispatcher than the FIDL binding was bound on. "
              "Ensure binding and teardown occur from the same dispatcher.");
          return;
        }
      }
    }

    // Generates an exception that could be caught in unit testing, then recovered.
    //
    // By comparison, `ZX_PANIC` would put the current thread under an infinite loop
    // of crashing.
    static void resumable_panic(const char* msg) {
      fprintf(stderr, "%s\n", msg);
      fflush(nullptr);
      // The following logic is similar to `backtrace_request`.
      // See zircon/system/ulib/backtrace-request/include/lib/backtrace-request/backtrace-request.h
#if defined(__aarch64__)
      __asm__("brk 0");
#elif defined(__x86_64__)
      __asm__("int3");
#else
#error "what machine?"
#endif
    }

   private:
    fdf_dispatcher_t* initial_dispatcher_;
  };
  any_thread_checker.emplace<DriverThreadChecker>(dispatcher, threading_policy);
}

void driver_close(fidl_handle_t handle) { fdf_handle_close(handle); }

void driver_close_many(const fidl_handle_t* handles, size_t num_handles) {
  for (size_t i = 0; i < num_handles; i++) {
    fdf_handle_close(handles[i]);
  }
}

}  // namespace

const TransportVTable DriverTransport::VTable = {
    .type = FIDL_TRANSPORT_TYPE_DRIVER,
    .encoding_configuration = &DriverTransport::EncodingConfiguration,
    .write = driver_write,
    .read = driver_read,
    .call = driver_call,
    .create_waiter = driver_create_waiter,
    .create_thread_checker = driver_create_thread_checker,
};

zx_status_t DriverWaiter::Begin() {
  state_.channel_read.emplace(
      state_.handle, 0 /* options */,
      [&state = state_](fdf_dispatcher_t* dispatcher, fdf::ChannelRead* channel_read,
                        fdf_status_t status) {
        if (status != ZX_OK) {
          fidl::UnbindInfo unbind_info;
          if (status == ZX_ERR_PEER_CLOSED) {
            unbind_info = fidl::UnbindInfo::PeerClosed(status);
          } else {
            unbind_info = fidl::UnbindInfo::DispatcherError(status);
          }
          return state.failure_handler(unbind_info);
        }

        fdf::Arena arena;
        DriverMessageStorageView storage_view{.arena = &arena};
        IncomingMessage msg = fidl::MessageRead(fdf::UnownedChannel(state.handle), storage_view);
        if (!msg.ok()) {
          return state.failure_handler(fidl::UnbindInfo{msg});
        }
        state.channel_read = std::nullopt;
        return state.success_handler(msg, &storage_view);
      });
  zx_status_t status =
      state_.channel_read->Begin(fdf_dispatcher_from_async_dispatcher(state_.dispatcher));
  if (status == ZX_ERR_UNAVAILABLE) {
    // Begin() is called when the dispatcher is shutting down.
    return ZX_ERR_CANCELED;
  }
  return status;
}

fidl::internal::DriverWaiter::CancellationResult DriverWaiter::Cancel() {
  fdf_dispatcher_t* dispatcher = fdf_dispatcher_from_async_dispatcher(state_.dispatcher);
  uint32_t options = fdf_dispatcher_get_options(dispatcher);
  ZX_ASSERT(state_.channel_read.has_value());

  if (options & FDF_DISPATCHER_OPTION_UNSYNCHRONIZED) {
    // Unsynchronized dispatcher.
    fdf_status_t status = state_.channel_read->Cancel();
    ZX_ASSERT(status == ZX_OK || status == ZX_ERR_NOT_FOUND);

    // When the dispatcher is unsynchronized, our |ChannelRead| handler will
    // always be called (sometimes with a ZX_OK status and other times with a
    // ZX_ERR_CANCELED status). For the purpose of determining which code should
    // finish teardown of the |AsyncBinding|, it is as if the cancellation
    // failed.
    return CancellationResult::kNotFound;
  }

  // Synchronized dispatcher.
  fdf_dispatcher_t* current_dispatcher = fdf_dispatcher_get_current_dispatcher();
  if (current_dispatcher == dispatcher) {
    // The binding is being torn down from a dispatcher thread.
    fdf_status_t status = state_.channel_read->Cancel();
    // If the status is not |ZX_OK|, then the FIDL runtime has gotten out of
    // sync with the state of the driver runtime.
    ZX_ASSERT(status == ZX_OK);
    return CancellationResult::kOk;
  }

  // The binding is being torn down from a foreign thread.
  // The only way this could happen is when the user is using a shared client
  // or a server binding. In both cases, the contract is that the teardown
  // will happen asynchronously. We can implement that behavior by indicating
  // that synchronous cancellation failed.
  return CancellationResult::kDispatcherContextNeeded;
}

const CodingConfig DriverTransport::EncodingConfiguration = {
    .max_iovecs_write = 1,
    .handle_metadata_stride = 0,
    .close = driver_close,
    .close_many = driver_close_many,
};

}  // namespace internal
}  // namespace fidl
