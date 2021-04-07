// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <fuchsia/gpu/magma/llcpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/zx/channel.h>

#include <thread>

#include <gtest/gtest.h>

#include "helper/test_device_helper.h"
#include "magma.h"
#include "magma_common_defs.h"
#include "magma_util/macros.h"

namespace {

#if defined(NO_HARDWARE)
constexpr const char* kDevicePath = "/dev/test/msd-img-rgx-no-hardware";
#endif

class TestBase : public magma::TestDeviceBase {
 public:
#if defined(NO_HARDWARE)
  TestBase() : magma::TestDeviceBase(kDevicePath) {}
#else
  TestBase() : magma::TestDeviceBase(0x1010) {}
#endif
};

bool ErrorIsExpected(int32_t result) {
  return (result == MAGMA_STATUS_CONNECTION_LOST || result == MAGMA_STATUS_INTERNAL_ERROR);
}

class TestConnection : public TestBase {
 public:
  TestConnection() { magma_create_connection2(device(), &connection_); }

  ~TestConnection() {
    if (connection_)
      magma_release_connection(connection_);
  }

  int32_t Test() {
    DASSERT(connection_);

    int32_t result = magma_get_error(connection_);
    if (ErrorIsExpected(result))
      return result;
    return DRET(result);
  }

 private:
  magma_connection_t connection_;
};

constexpr uint32_t kMaxCount = 100;
constexpr uint32_t kRestartCount = kMaxCount / 10;

static std::atomic_uint complete_count;

static void looper_thread_entry() {
  std::unique_ptr<TestConnection> test(new TestConnection());
  while (complete_count < kMaxCount) {
    int32_t result = test->Test();
    if (result == 0) {
      complete_count++;
    } else {
      EXPECT_TRUE(ErrorIsExpected(result));
      test.reset(new TestConnection());
    }
    std::this_thread::yield();
  }
}

static void test_shutdown(uint32_t iters) {
  for (uint32_t i = 0; i < iters; i++) {
    complete_count = 0;

    TestBase test_base;

    std::thread looper(looper_thread_entry);
    std::thread looper2(looper_thread_entry);

    uint32_t count = kRestartCount;
    while (complete_count < kMaxCount) {
      if (complete_count > count) {
        // Should replace this with a request to devmgr to restart the driver
        auto result =
            fidl::WireCall<fuchsia_gpu_magma::Device>(test_base.channel()->borrow()).TestRestart();
        EXPECT_EQ(ZX_OK, result.status());

        count += kRestartCount;
      }
      std::this_thread::yield();
    }

    looper.join();
    looper2.join();
  }
}

}  // namespace

TEST(Shutdown, DISABLED_Test) { test_shutdown(1); }
