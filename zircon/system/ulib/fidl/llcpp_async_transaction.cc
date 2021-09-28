// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/llcpp/async_binding.h>
#include <lib/fidl/llcpp/async_transaction.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/server.h>
#include <zircon/assert.h>

namespace fidl {

namespace internal {

std::optional<UnbindInfo> AsyncTransaction::Dispatch(std::shared_ptr<AsyncBinding>&& binding,
                                                     fidl::IncomingMessage&& msg) {
  ZX_ASSERT(!owned_binding_);
  ZX_ASSERT(!moved_);
  bool moved = false;
  moved_ = &moved;
  // Take ownership of the internal (dispatcher) reference to the AsyncBinding. Until code executed
  // in this scope releases ownership, no other thread may access the binding via keep_alive_.
  owned_binding_ = std::move(binding);
  // Avoid static_pointer_cast for now since it results in atomic inc/dec.
  auto* binding_raw = static_cast<AsyncServerBinding*>(owned_binding_.get());
  fidl::DispatchResult dispatch_result =
      binding_raw->interface_->dispatch_message(std::move(msg), this);
  if (moved)
    return {};  // Return if `this` is no longer valid.
  moved_ = nullptr;
  // Transfer ownership of the binding back to the dispatcher if we still have it.
  if (owned_binding_)
    binding_raw->keep_alive_ = std::move(owned_binding_);

  switch (dispatch_result) {
    case fidl::DispatchResult::kFound:
      // Propagate any error that happened during the message handling.
      return unbind_info_;
    case fidl::DispatchResult::kNotFound:
      // The message was not recognized by the |dispatch_fn_|.
      return fidl::UnbindInfo::UnknownOrdinal();
  }
}

zx_status_t AsyncTransaction::Reply(fidl::OutgoingMessage* message) {
  ZX_ASSERT(txid_ != 0);
  auto txid = txid_;
  txid_ = 0;

  // Get a strong reference to the binding. Avoid unnecessarily copying the reference if
  // owned_binding_ is valid. On error, the reference will be consumed by Close().
  std::shared_ptr<AsyncBinding> tmp = owned_binding_ ? nullptr : unowned_binding_.lock();
  auto& binding = owned_binding_ ? owned_binding_ : tmp;
  if (!binding)
    return ZX_ERR_CANCELED;

  message->set_txid(txid);
  message->Write(binding->channel());
  return message->status();
}

void AsyncTransaction::EnableNextDispatch() {
  if (!owned_binding_)
    return;  // Has no effect if the Transaction does not own the binding.
  auto* binding_raw = static_cast<AsyncServerBinding*>(owned_binding_.get());
  unowned_binding_ = owned_binding_;  // Preserve a weak reference to the binding.
  binding_raw->keep_alive_ = std::move(owned_binding_);
  if (binding_raw->CheckForTeardownAndBeginNextWait() == ZX_OK) {
    *binding_released_ = true;
  } else {
    // Propagate a placeholder error, such that the message handler will
    // terminate dispatch right after the processing of this transaction.
    unbind_info_ = UnbindInfo::Unbind();
  }
}

void AsyncTransaction::Close(zx_status_t epitaph) {
  if (!owned_binding_) {
    if (auto binding = unowned_binding_.lock()) {
      auto* binding_raw = static_cast<AsyncServerBinding*>(binding.get());
      binding_raw->Close(std::move(binding), epitaph);
    }
    return;
  }
  unbind_info_ = UnbindInfo::Close(epitaph);
  // Return ownership of the binding to the dispatcher.
  auto* binding_raw = static_cast<AsyncServerBinding*>(owned_binding_.get());
  binding_raw->keep_alive_ = std::move(owned_binding_);
}

void AsyncTransaction::InternalError(UnbindInfo error) {
  if (!owned_binding_) {
    if (auto binding = unowned_binding_.lock()) {
      auto* binding_raw = static_cast<AsyncServerBinding*>(binding.get());
      binding_raw->StartTeardownWithInfo(std::move(binding), error);
    }
    return;
  }
  unbind_info_ = error;
  // Return ownership of the binding to the dispatcher.
  auto* binding_raw = static_cast<AsyncServerBinding*>(owned_binding_.get());
  binding_raw->keep_alive_ = std::move(owned_binding_);
}

std::unique_ptr<Transaction> AsyncTransaction::TakeOwnership() {
  ZX_ASSERT(owned_binding_);
  ZX_ASSERT(moved_);
  *moved_ = true;
  moved_ = nullptr;                   // This should only ever be called once.
  unowned_binding_ = owned_binding_;  // Preserve a weak reference to the binding.
  auto* binding_raw = static_cast<AsyncServerBinding*>(owned_binding_.get());
  binding_raw->keep_alive_ = std::move(owned_binding_);
  return std::make_unique<AsyncTransaction>(std::move(*this));
}

bool AsyncTransaction::IsUnbound() {
  // The channel is unbound if this transaction neither owns the binding nor can get a strong
  // reference to it.
  return !owned_binding_ && !unowned_binding_.lock();
}

}  // namespace internal

}  // namespace fidl
