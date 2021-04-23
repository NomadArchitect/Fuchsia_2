// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/guest/balloon.h"

#include <fuchsia/virtualization/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <iostream>

#include <virtio/balloon.h>

void handle_balloon(uint32_t env_id, uint32_t cid, uint32_t num_pages,
                    sys::ComponentContext* context) {
  // Connect to environment.
  fuchsia::virtualization::ManagerSyncPtr manager;
  context->svc()->Connect(manager.NewRequest());
  fuchsia::virtualization::RealmSyncPtr env_ptr;
  manager->Connect(env_id, env_ptr.NewRequest());

  fuchsia::virtualization::BalloonControllerSyncPtr balloon_controller;
  env_ptr->ConnectToBalloon(cid, balloon_controller.NewRequest());

  balloon_controller->RequestNumPages(num_pages);
  std::cout << "Resizing the memory balloon to " << num_pages << " pages\n";
}

static const char* tag_name(uint16_t tag) {
  switch (tag) {
    case VIRTIO_BALLOON_S_SWAP_IN:
      return "swap-in:             ";
    case VIRTIO_BALLOON_S_SWAP_OUT:
      return "swap-out:            ";
    case VIRTIO_BALLOON_S_MAJFLT:
      return "major-faults:        ";
    case VIRTIO_BALLOON_S_MINFLT:
      return "minor-faults:        ";
    case VIRTIO_BALLOON_S_MEMFREE:
      return "free-memory:         ";
    case VIRTIO_BALLOON_S_MEMTOT:
      return "total-memory:        ";
    case VIRTIO_BALLOON_S_AVAIL:
      return "available-memory:    ";
    case VIRTIO_BALLOON_S_CACHES:
      return "disk-caches:         ";
    case VIRTIO_BALLOON_S_HTLB_PGALLOC:
      return "hugetlb-allocations: ";
    case VIRTIO_BALLOON_S_HTLB_PGFAIL:
      return "hugetlb-failures:    ";
    default:
      return "unknown:             ";
  }
}

void handle_balloon_stats(uint32_t env_id, uint32_t cid, sys::ComponentContext* context) {
  // Connect to environment.
  fuchsia::virtualization::ManagerSyncPtr manager;
  context->svc()->Connect(manager.NewRequest());
  fuchsia::virtualization::RealmSyncPtr env_ptr;
  manager->Connect(env_id, env_ptr.NewRequest());

  fuchsia::virtualization::BalloonControllerSyncPtr balloon_controller;
  env_ptr->ConnectToBalloon(cid, balloon_controller.NewRequest());

  zx_status_t status;
  fidl::VectorPtr<fuchsia::virtualization::MemStat> mem_stats;
  balloon_controller->GetMemStats(&status, &mem_stats);
  if (status != ZX_OK) {
    std::cerr << "Failed to get memory statistics " << status << '\n';
    return;
  }
  for (auto& mem_stat : *mem_stats) {
    std::cout << tag_name(mem_stat.tag) << mem_stat.val << '\n';
  }
}
