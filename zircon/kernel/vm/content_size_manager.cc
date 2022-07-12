// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/content_size_manager.h"

#include <ktl/limits.h>

uint64_t ContentSizeManager::Operation::GetSizeLocked() const {
  DEBUG_ASSERT(IsValid());

  return size_;
}

void ContentSizeManager::Operation::ShrinkSizeLocked(uint64_t new_size) {
  DEBUG_ASSERT(IsValid());
  // This function may only be called on expanding content write operations.
  ASSERT(type_ == OperationType::Append || type_ == OperationType::Write);
  ASSERT(new_size <= size_);

  size_ = new_size;
}

void ContentSizeManager::Operation::CommitLocked() {
  DEBUG_ASSERT(IsValid());

  parent()->CommitAndDequeueOperationLocked(this);
}

void ContentSizeManager::Operation::CancelLocked() {
  DEBUG_ASSERT(IsValid());

  parent()->DequeueOperationLocked(this);
}

void ContentSizeManager::Operation::Initialize(ContentSizeManager* parent, uint64_t size,
                                               OperationType type) {
  DEBUG_ASSERT(!IsValid());
  DEBUG_ASSERT(parent != nullptr);

  parent_ = parent;
  size_ = size;
  type_ = type;
}

zx_status_t ContentSizeManager::BeginAppendLocked(uint64_t append_size, Guard<Mutex>* lock_guard,
                                                  uint64_t* out_new_content_size,
                                                  Operation* out_op) {
  DEBUG_ASSERT(out_new_content_size);
  DEBUG_ASSERT(out_op);

  out_op->Initialize(this, append_size, OperationType::Append);
  write_q_.push_back(out_op);

  // Block until head if there are any of the following operations preceding this one:
  //   * Appends or writes that exceed the current content size.
  //   * Set size
  bool should_block = false;
  auto iter = --write_q_.make_iterator(*out_op);
  while (iter.IsValid()) {
    iter->AssertParentLockHeld();
    if (iter->GetType() == OperationType::SetSize || iter->GetType() == OperationType::Append ||
        (iter->GetType() == OperationType::Write && iter->GetSizeLocked() > content_size_)) {
      should_block = true;
      break;
    }
    --iter;
  }

  if (should_block) {
    BlockUntilHeadLocked(out_op, lock_guard);
  }

  if (add_overflow(content_size_, append_size, out_new_content_size)) {
    // Dequeue operation since this change should not be committed.
    DequeueOperationLocked(out_op);
    return ZX_ERR_OUT_OF_RANGE;
  }

  return ZX_OK;
}

void ContentSizeManager::BeginWriteLocked(uint64_t target_size, Guard<Mutex>* lock_guard,
                                          ktl::optional<uint64_t>* out_prev_content_size,
                                          Operation* out_op) {
  DEBUG_ASSERT(out_prev_content_size);
  DEBUG_ASSERT(out_op);

  *out_prev_content_size = ktl::nullopt;

  out_op->Initialize(this, target_size, OperationType::Write);
  write_q_.push_back(out_op);

  // Check if there are any set size operations in front of this that sets the content size smaller
  // than `target_size`.
  bool block_due_to_set = false;
  auto iter = --write_q_.make_iterator(*out_op);
  while (iter.IsValid()) {
    iter->AssertParentLockHeld();
    if (iter->GetType() == OperationType::SetSize && iter->GetSizeLocked() < target_size) {
      block_due_to_set = true;
      break;
    }
    --iter;
  }

  // If this write can potentially create a scenario where it expands content, block until it is the
  // head of the queue.
  if (block_due_to_set || target_size > content_size_) {
    BlockUntilHeadLocked(out_op, lock_guard);

    if (target_size > content_size_) {
      *out_prev_content_size = content_size_;
    }
  }
}

void ContentSizeManager::BeginReadLocked(uint64_t target_size, uint64_t* out_content_size_limit,
                                         Operation* out_op) {
  DEBUG_ASSERT(out_content_size_limit);
  DEBUG_ASSERT(out_op);

  // Allow reads up to the smallest outstanding size.
  // Other concurrent, in-flight operations may or may not complete before this read, so it is okay
  // to be more conservative here and only read up to the guaranteed valid region.
  *out_content_size_limit = content_size_;
  for (auto& op : read_q_) {
    if (op.GetType() != OperationType::SetSize) {
      continue;
    }

    op.AssertParentLockHeld();
    *out_content_size_limit = ktl::min(op.GetSizeLocked(), *out_content_size_limit);
  }
  *out_content_size_limit = ktl::min(target_size, *out_content_size_limit);

  out_op->Initialize(this, *out_content_size_limit, OperationType::Read);
  read_q_.push_back(out_op);
}

void ContentSizeManager::BeginSetContentSizeLocked(uint64_t target_size, Operation* out_op,
                                                   Guard<Mutex>* lock_guard) {
  out_op->Initialize(this, target_size, OperationType::SetSize);

  write_q_.push_back(out_op);
  read_q_.push_back(out_op);

  // Block until head if there are any of the following operations preceding this one:
  //   * Appends or writes that exceed either the current content size or the target size.
  //      - If it exceeds the current content size, the overlap is in the region in which the set
  //        size will zero content and the write will commit data.
  //      - If it exceeds the target size, the overlap is in the region in which the set size will
  //        invalidate pages/data and the write will commit data.
  //   * Reads that are reading at or beyond target size.
  //   * Set size
  bool should_block = false;
  auto write_iter = --write_q_.make_iterator(*out_op);
  while (write_iter.IsValid()) {
    write_iter->AssertParentLockHeld();
    if (write_iter->GetType() == OperationType::SetSize ||
        write_iter->GetType() == OperationType::Append ||
        (write_iter->GetType() == OperationType::Write &&
         write_iter->GetSizeLocked() > ktl::min(content_size_, target_size))) {
      should_block = true;
      break;
    }
    --write_iter;
  }

  auto read_iter = --read_q_.make_iterator(*out_op);
  while (read_iter.IsValid() && !should_block) {
    read_iter->AssertParentLockHeld();
    if (read_iter->GetType() == OperationType::Read && read_iter->GetSizeLocked() > target_size) {
      should_block = true;
      break;
    }
    --read_iter;
  }

  if (should_block) {
    BlockUntilHeadLocked(out_op, lock_guard);
  }
}

void ContentSizeManager::BlockUntilHeadLocked(Operation* op, Guard<Mutex>* lock_guard) {
  DEBUG_ASSERT(op->parent_ == this);

  if (fbl::InContainer<WriteQueueTag>(*op)) {
    while (op->IsValid() && &write_q_.front() != op) {
      lock_guard->CallUnlocked([op] { op->ready_event_.Wait(); });
    }
  }

  if (fbl::InContainer<ReadQueueTag>(*op)) {
    while (op->IsValid() && &read_q_.front() != op) {
      lock_guard->CallUnlocked([op] { op->ready_event_.Wait(); });
    }
  }
}

void ContentSizeManager::CommitAndDequeueOperationLocked(Operation* op) {
  if (!op->IsValid()) {
    DEBUG_ASSERT(!fbl::InContainer<WriteQueueTag>(*op));
    DEBUG_ASSERT(!fbl::InContainer<ReadQueueTag>(*op));
    return;
  }

  op->AssertParentLockHeld();
  switch (op->type_) {
    case OperationType::Write:
      content_size_ = ktl::max(op->GetSizeLocked(), content_size_);
      break;
    case OperationType::Append:
      content_size_ += op->GetSizeLocked();
      break;
    case OperationType::SetSize:
      content_size_ = op->GetSizeLocked();
      break;
    case OperationType::Read:
      // No-op
      break;
  }

  DequeueOperationLocked(op);
}

void ContentSizeManager::DequeueOperationLocked(Operation* op) {
  DEBUG_ASSERT(op->IsValid());
  DEBUG_ASSERT(op->parent_ == this);

  auto dequeue_from_list = [op](auto& list) {
    const bool is_head = &list.front() == op;
    auto next = ++list.make_iterator(*op);

    list.erase(*op);

    // If the current operation is now at the head of the list, signal to the next operation that it
    // should wake to complete its task after this one finishes dequeueing.
    if (is_head && next.IsValid()) {
      next->ready_event_.Signal();
    }
  };

  switch (op->type_) {
    case OperationType::Write:
    case OperationType::Append:
      DEBUG_ASSERT(fbl::InContainer<WriteQueueTag>(*op));
      dequeue_from_list(write_q_);
      break;
    case OperationType::Read:
      DEBUG_ASSERT(fbl::InContainer<ReadQueueTag>(*op));
      dequeue_from_list(read_q_);
      break;
    case OperationType::SetSize:
      DEBUG_ASSERT(fbl::InContainer<ReadQueueTag>(*op));
      DEBUG_ASSERT(fbl::InContainer<WriteQueueTag>(*op));
      dequeue_from_list(write_q_);
      dequeue_from_list(read_q_);
      break;
  }

  // Just in case, signal the ready event of |op| in case another thread is blocking on it.
  //
  // Note that this should never usually occur, since only the owning thread of the operation should
  // be blocking or dequeueing.
  op->ready_event_.Signal();

  op->Reset();
}
