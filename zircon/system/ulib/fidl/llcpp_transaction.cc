// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/llcpp/result.h>
#include <lib/fidl/llcpp/transaction.h>

namespace fidl {

CompleterBase& CompleterBase::operator=(CompleterBase&& other) noexcept {
  if (this != &other) {
    DropTransaction();
    transaction_ = other.transaction_;
    owned_ = other.owned_;
    needs_to_reply_ = other.needs_to_reply_;
    other.transaction_ = nullptr;
    other.owned_ = false;
    other.needs_to_reply_ = false;
  }
  return *this;
}

void CompleterBase::Close(zx_status_t status) {
  ScopedLock lock(lock_);
  EnsureHasTransaction(&lock);
  transaction_->Close(status);
  DropTransaction();
}

void CompleterBase::EnableNextDispatch() {
  ScopedLock lock(lock_);
  EnsureHasTransaction(&lock);
  transaction_->EnableNextDispatch();
}

CompleterBase::CompleterBase(CompleterBase&& other) noexcept
    : transaction_(other.transaction_),
      owned_(other.owned_),
      needs_to_reply_(other.needs_to_reply_) {
  other.transaction_ = nullptr;
  other.owned_ = false;
  other.needs_to_reply_ = false;
}

CompleterBase::~CompleterBase() {
  ScopedLock lock(lock_);
  ZX_ASSERT_MSG(!needs_to_reply_ || (transaction_ && transaction_->IsUnbound()),
                "Completer expected a Reply to be sent.");
  DropTransaction();
}

std::unique_ptr<Transaction> CompleterBase::TakeOwnership() {
  ScopedLock lock(lock_);
  EnsureHasTransaction(&lock);
  std::unique_ptr<Transaction> clone = transaction_->TakeOwnership();
  DropTransaction();
  return clone;
}

fidl::Result CompleterBase::SendReply(::fidl::OutgoingMessage* message) {
  ScopedLock lock(lock_);
  EnsureHasTransaction(&lock);
  if (unlikely(!needs_to_reply_)) {
    lock.release();  // Avoid crashing on death tests.
    ZX_PANIC("Repeated or unexpected Reply.");
  }
  // At this point we are either replying or calling InternalError, so no need for
  // further replies.
  needs_to_reply_ = false;
  if (!message->ok()) {
    transaction_->InternalError(fidl::UnbindInfo{*message});
    return *message;
  }
  zx_status_t status = transaction_->Reply(message);
  if (status != ZX_OK) {
    auto error = fidl::Result::TransportError(status);
    transaction_->InternalError(fidl::UnbindInfo{error});
    return error;
  }
  return fidl::Result::Ok();
}

void CompleterBase::InternalError(UnbindInfo error) {
  ScopedLock lock(lock_);
  EnsureHasTransaction(&lock);
  transaction_->InternalError(error);
  // NOTE: The transaction is not dropped as the user has not explicitly Close()d the completer.
  // As such, Drop() would panic if invoked here.
}

void CompleterBase::EnsureHasTransaction(ScopedLock* lock) {
  if (unlikely(!transaction_)) {
    lock->release();  // Avoid crashing on death tests.
    ZX_PANIC("ToAsync() was already called.");
  }
}

void CompleterBase::DropTransaction() {
  if (owned_) {
    owned_ = false;
    delete transaction_;
  }
  transaction_ = nullptr;
  needs_to_reply_ = false;
}

}  // namespace fidl
