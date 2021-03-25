// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_OS_H_
#define SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_OS_H_

#include <unordered_map>
#include <vector>

#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

namespace harvester {

const size_t kNumExtraSlop = 10;

class OS {
 public:
  virtual ~OS() = default;

  // Convenience methods.

  virtual zx_duration_t HighResolutionNow() = 0;

  // Wrapper around GetInfo for fetching singular info objects.
  template <typename T>
  zx_status_t GetInfo(zx_handle_t parent, zx_koid_t parent_koid,
                      unsigned int kind, const char* kind_name,
                      T& info_object) {
    zx_status_t status = GetInfo(parent, kind, &info_object, sizeof(T), nullptr,
                                 nullptr);

    if (status != ZX_OK) {
      // ZX_ERR_BAD_STATE is returned when a process is already destroyed. This
      // is not exceptional; pass through the error code but don't spam logs.
      if (status != ZX_ERR_BAD_STATE) {
        FX_LOGS(ERROR) << "zx_object_get_info(" << parent_koid << ", "
                       << kind_name << ", ...) failed: "
                       << zx_status_get_string(status) << " (" << status << ")";
      }
    }

    return status;
  }

  // Wrapper around GetInfo for fetching vectors of children.
  template <typename T = zx_koid_t>
  zx_status_t GetChildren(zx_handle_t parent, zx_koid_t parent_koid,
                          unsigned int children_kind, const char* kind_name,
                          std::vector<T>& children) {
    zx_status_t status;

    // Fetch the number of children available.
    size_t num_children;
    status = GetInfo(
        parent, children_kind, nullptr, 0, nullptr, &num_children);

    if (status != ZX_OK) {
      // ZX_ERR_BAD_STATE is returned when a process is already destroyed. This
      // is not exceptional; pass through the error code but don't spam logs.
      if (status != ZX_ERR_BAD_STATE) {
        FX_LOGS(ERROR) << "zx_object_get_info(" << parent_koid << ", "
                       << kind_name << ", ...) failed: "
                       << zx_status_get_string(status) << " (" << status << ")";
      }
      return status;
    }

    // This is inherently racy (TOCTTOU race condition). Add a bit of slop space
    // in case children have been added.
    children.resize(num_children + kNumExtraSlop);

    // Fetch the actual child objects.
    size_t actual = 0;
    size_t available = 0;
    status = GetInfo(parent, children_kind, children.data(),
                     children.capacity() * sizeof(T), &actual, &available);

    if (status != ZX_OK) {
      // ZX_ERR_BAD_STATE is returned when a process is already destroyed. This
      // is not exceptional; pass through the error code but don't spam logs.
      if (status != ZX_ERR_BAD_STATE) {
        FX_LOGS(ERROR) << "zx_object_get_info(" << parent_koid << ", "
                       << kind_name << ", ...) failed: "
                       << zx_status_get_string(status) << " (" << status << ")";
      }
      // On error, empty children so we don't pass through invalid information.
      children.clear();
      return status;
    }

    // If we're still too small at least warn the user.
    if (actual < available) {
      FX_LOGS(WARNING) <<  "zx_object_get_info(" << parent_koid << ", "
                       << kind_name << ", ...) truncated " << (available - actual)
                       << "/" << available << " results";
    }

    children.resize(actual);

    return ZX_OK;
  }

 protected:

  // Thin wrappers around OS calls. Allows for mocking.

  virtual zx_status_t GetInfo(zx_handle_t parent, unsigned int children_kind,
                              void* out_buffer, size_t buffer_size,
                              size_t* actual, size_t* avail) = 0;
};

class OSImpl : public OS {
 public:
  ~OSImpl() = default;

  virtual zx_status_t GetInfo(zx_handle_t parent, unsigned int children_kind,
                              void* out_buffer, size_t buffer_size,
                              size_t* actual, size_t* avail) override {
    return zx_object_get_info(parent, children_kind, out_buffer, buffer_size,
                              actual, avail);
  }

  virtual zx_duration_t HighResolutionNow() override {
    auto now = std::chrono::high_resolution_clock::now();
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
        now.time_since_epoch())
        .count();
  }
};

}  // harvester

#endif  // SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_OS_H_

