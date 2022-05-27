// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_BLOBFS_BLOBFS_INSPECT_TREE_H_
#define SRC_STORAGE_BLOBFS_BLOBFS_INSPECT_TREE_H_

#include <lib/syslog/cpp/macros.h>
#include <zircon/system/public/zircon/compiler.h>

#include <mutex>

#include "src/lib/storage/vfs/cpp/inspect/inspect_tree.h"
#include "src/lib/storage/vfs/cpp/inspect/node_operations.h"
#include "src/storage/blobfs/format.h"

namespace blobfs {

// Encapsulates the state required to make a filesystem inspect tree for Blobfs.
class BlobfsInspectTree final {
 public:
  BlobfsInspectTree();
  ~BlobfsInspectTree() = default;

  // Set general filesystem information.
  void SetInfo(const fs_inspect::InfoData& info) __TA_EXCLUDES(info_mutex_);

  // Update resource usage values that change when certain fields in the superblock are modified.
  void UpdateSuperblock(const Superblock& superblock) __TA_EXCLUDES(usage_mutex_);

  // Update FVM volume information and record any out of space events.
  void UpdateVolumeData(const block_client::BlockDevice& device, bool out_of_space = false)
      __TA_EXCLUDES(volume_mutex_);

  // Reference to the Inspector this object owns.
  const inspect::Inspector& Inspector() { return inspector_; }

  // Obtain node-level operation trackers.
  fs_inspect::NodeOperations* GetNodeOperations() { return &node_operations_; }

 private:
  // Helper function to create and return all required callbacks to create an fs_inspect tree.
  fs_inspect::NodeCallbacks CreateCallbacks();

  //
  // Generic fs_inspect Properties
  //

  mutable std::mutex info_mutex_{};
  fs_inspect::InfoData info_ __TA_GUARDED(info_mutex_){};

  mutable std::mutex usage_mutex_{};
  fs_inspect::UsageData usage_ __TA_GUARDED(usage_mutex_){};

  mutable std::mutex volume_mutex_{};
  fs_inspect::VolumeData volume_ __TA_GUARDED(volume_mutex_){};

  // The Inspector to which the tree is attached.
  inspect::Inspector inspector_;

  // In order to distinguish filesystem instances, we must attach the InspectTree to a uniquely
  // named child node instead of the Inspect root. This is because fshost currently serves all
  // filesystem inspect trees, and is not be required when filesystems are componentized (the tree
  // can be attached directly to the inspect root in that case).
  inspect::Node tree_root_;

  // Node to which operational statistics (latency/error counters) are added.
  inspect::Node opstats_node_;

  // All common filesystem node operation trackers.
  fs_inspect::NodeOperations node_operations_;

  // Filesystem inspect tree nodes.
  // **MUST be declared last**, as the callbacks passed to this object use the above properties.
  // This ensures that the callbacks are destroyed before any properties that they may reference.
  fs_inspect::FilesystemNodes fs_inspect_nodes_;
};

}  // namespace blobfs

#endif  // SRC_STORAGE_BLOBFS_BLOBFS_INSPECT_TREE_H_
