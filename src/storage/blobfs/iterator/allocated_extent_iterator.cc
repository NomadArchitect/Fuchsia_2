// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/iterator/allocated_extent_iterator.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/status.h>
#include <stdint.h>
#include <zircon/types.h>

#include "src/storage/blobfs/format.h"
#include "src/storage/blobfs/iterator/allocated_node_iterator.h"
#include "src/storage/blobfs/iterator/extent_iterator.h"

namespace blobfs {

AllocatedExtentIterator::AllocatedExtentIterator(NodeFinder* finder, InodePtr inode,
                                                 uint32_t node_index)
    : inode_(std::move(inode)), node_index_(node_index), node_iterator_(finder, inode_.get()) {}

zx::status<AllocatedExtentIterator> AllocatedExtentIterator::Create(NodeFinder* finder,
                                                                    uint32_t node_index) {
  auto inode = finder->GetNode(node_index);
  if (inode.is_error()) {
    return inode.take_error();
  }
  if (!inode->header.IsAllocated() || !inode->header.IsInode()) {
    FX_LOGS(ERROR) << "node_index " << node_index << " isn't a valid inode: " << *(inode.value());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(AllocatedExtentIterator(finder, std::move(inode.value()), node_index));
}

bool AllocatedExtentIterator::Done() const { return ExtentIndex() == inode_->extent_count; }

zx_status_t AllocatedExtentIterator::Next(const Extent** out) {
  ZX_DEBUG_ASSERT(!Done());
  zx_status_t status = ValidateExtentCount();
  if (status != ZX_OK) {
    return status;
  }

  const Extent* extent = GetExtent();
  UpdateIndices(*extent);
  if (!Done() && local_index_ == (IsInode() ? kInlineMaxExtents : extent_node_->extent_count)) {
    zx_status_t status = NextContainer();
    if (status != ZX_OK) {
      return status;
    }
  }

  *out = extent;
  return ZX_OK;
}

uint64_t AllocatedExtentIterator::BlockIndex() const { return block_index_; }

uint32_t AllocatedExtentIterator::ExtentIndex() const {
  return local_index_ + node_iterator_.ExtentIndex();
}

uint32_t AllocatedExtentIterator::NodeIndex() const {
  ZX_DEBUG_ASSERT(!Done());
  return node_index_;
}

zx_status_t AllocatedExtentIterator::VerifyIteration(NodeFinder* finder, Inode* inode) {
  uint32_t container_count = 0;
  AllocatedNodeIterator fast(finder, inode);
  AllocatedNodeIterator slow(finder, inode);
  while (!fast.Done()) {
    zx::status<ExtentContainer*> status = fast.Next();
    if (status.is_error()) {
      return status.status_value();
    }
    ExtentContainer* current = status.value();

    // Verify the correct iterability of the current node.
    if (fast.Done()) {
      if (inode->extent_count != fast.ExtentIndex() + current->extent_count) {
        FX_LOGS(ERROR) << "Final extent count " << fast.ExtentIndex() + current->extent_count
                       << " does not match inode extent count " << inode->extent_count << " .";
        return ZX_ERR_OUT_OF_RANGE;
      }
    } else if (fast.NextNodeIndex() == slow.NextNodeIndex()) {
      FX_LOGS(ERROR) << "node cycle detected.";
      return ZX_ERR_IO_DATA_INTEGRITY;
    } else if (current->extent_count != kContainerMaxExtents) {
      FX_LOGS(ERROR) << "non-packed extent container found.";
      return ZX_ERR_BAD_STATE;
    }

    // Advance the slow pointer to detct cycles.
    if (++container_count % 2 == 0) {
      zx::status<ExtentContainer*> status = slow.Next();
      if (status.is_error()) {
        return status.status_value();
      }
      if (!fast.Done() && fast.NextNodeIndex() == slow.NextNodeIndex()) {
        FX_LOGS(ERROR) << "Node cycle detected.";
        return ZX_ERR_IO_DATA_INTEGRITY;
      }
    }
  }
  return ZX_OK;
}

bool AllocatedExtentIterator::IsInode() const { return extent_node_ == nullptr; }

zx_status_t AllocatedExtentIterator::ValidateExtentCount() const {
  ZX_ASSERT(local_index_ < (IsInode() ? kInlineMaxExtents : kContainerMaxExtents));
  if (!IsInode() && local_index_ > extent_node_->extent_count) {
    // This container doesn't recognize this extent as valid.
    FX_LOGS(ERROR) << "Extent is invalid: " << local_index_;
    return ZX_ERR_IO_DATA_INTEGRITY;
  }
  return ZX_OK;
}

void AllocatedExtentIterator::UpdateIndices(const Extent& extent) {
  block_index_ += extent.Length();
  local_index_++;
}

const Extent* AllocatedExtentIterator::GetExtent() const {
  if (IsInode()) {
    return &inode_->extents[local_index_];
  } else {
    return &extent_node_->extents[local_index_];
  }
}

zx_status_t AllocatedExtentIterator::NextContainer() {
  ZX_DEBUG_ASSERT(!node_iterator_.Done());
  uint32_t node_index = node_iterator_.NextNodeIndex();
  // Our implementation uses 0xffffffffu as an end of list indicator to spot
  // attempts to iterate past the end of the list. This value is technically
  // valid but not in any existing practical or debugging use cases.
  ZX_DEBUG_ASSERT(node_index != kMaxNodeId);

  zx::status<ExtentContainer*> status = node_iterator_.Next();
  if (status.is_error()) {
    return status.status_value();
  }
  extent_node_ = status.value();
  local_index_ = 0;
  if (extent_node_->previous_node != node_index_) {
    return ZX_ERR_IO_DATA_INTEGRITY;
  }
  node_index_ = node_index;

  return ZX_OK;
}

}  // namespace blobfs
