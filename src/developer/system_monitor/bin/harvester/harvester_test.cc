// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "harvester.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/loop.h>
#include <lib/async-testing/dispatcher_stub.h>
#include <lib/sys/cpp/testing/test_with_environment_fixture.h>

#include <gtest/gtest.h>

#include "dockyard_proxy_fake.h"
#include "info_resource.h"
#include "os.h"

namespace {

class AsyncDispatcherFake : public async::DispatcherStub {
 public:
  zx::time Now() override { return current_time_; }
  void SetTime(zx::time t) { current_time_ = t; }

 private:
  zx::time current_time_;
};

}  // namespace

class SystemMonitorHarvesterTest : public ::testing::Test {
 public:
  void SetUp() override {
    // Create a test harvester.
    std::unique_ptr<harvester::DockyardProxyFake> dockyard_proxy =
        std::make_unique<harvester::DockyardProxyFake>();
    std::unique_ptr<harvester::OS> os = std::make_unique<harvester::OSImpl>();

    EXPECT_EQ(harvester::GetInfoResource(&info_resource), ZX_OK);
    test_harvester = std::make_unique<harvester::Harvester>(
        info_resource, std::move(dockyard_proxy), std::move(os));
  }

  zx_handle_t GetHarvesterInfoResource() const {
    return test_harvester->info_resource_;
  }
  zx::duration GetGatherThreadsAndCpuPeriod() const {
    return test_harvester->gather_threads_and_cpu_.update_period_;
  }
  zx::duration GetGatherMemoryPeriod() const {
    return test_harvester->gather_memory_.update_period_;
  }
  zx::duration GetGatherProcessesAndMemoryPeriod() const {
    return test_harvester->gather_processes_and_memory_.update_period_;
  }

  std::unique_ptr<harvester::Harvester> test_harvester;
  async::Loop loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  zx_handle_t info_resource;
};

TEST_F(SystemMonitorHarvesterTest, CreateHarvester) {
  AsyncDispatcherFake fast_dispatcher;
  AsyncDispatcherFake slow_dispatcher;
  EXPECT_EQ(info_resource, GetHarvesterInfoResource());

  test_harvester->GatherFastData(&fast_dispatcher);
  EXPECT_EQ(zx::msec(100), GetGatherThreadsAndCpuPeriod());

  test_harvester->GatherSlowData(&slow_dispatcher);
  // TODO(fxbug.dev/40872): re-enable once we need this data.
  // EXPECT_EQ(zx::sec(3), GetGatherInspectablePeriod());
  // EXPECT_EQ(zx::sec(10), GetGatherIntrospectionPeriod());
  EXPECT_EQ(zx::sec(2), GetGatherProcessesAndMemoryPeriod());
}

class SystemMonitorHarvesterIntegrationTest
    : public gtest::TestWithEnvironmentFixture {
 public:
  void SetUp() override {
    // Create a test harvester.
    std::unique_ptr<harvester::DockyardProxyFake> dockyard_proxy_ptr =
        std::make_unique<harvester::DockyardProxyFake>();
    dockyard_proxy = dockyard_proxy_ptr.get();
    std::unique_ptr<harvester::OS> os = std::make_unique<harvester::OSImpl>();

    EXPECT_EQ(harvester::GetInfoResource(&info_resource), ZX_OK);
    test_harvester = std::make_unique<harvester::Harvester>(
        info_resource, std::move(dockyard_proxy_ptr), std::move(os));
  }

  std::unique_ptr<harvester::Harvester> test_harvester;
  zx_handle_t info_resource;
  harvester::DockyardProxyFake* dockyard_proxy;
};

TEST_F(SystemMonitorHarvesterIntegrationTest, GatherLogs) {
  auto message = "test-harvester-log-message";
  FX_LOGS(INFO) << message;

  test_harvester->GatherLogs();

  RunLoopUntil([&] { return dockyard_proxy->CheckLogSubstringSent(message); });
}
