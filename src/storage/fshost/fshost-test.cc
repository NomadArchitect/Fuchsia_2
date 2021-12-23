// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.fshost/cpp/wire.h>
#include <fidl/fuchsia.io.admin/cpp/wire_test_base.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.io2/cpp/wire.h>
#include <fidl/fuchsia.process.lifecycle/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/namespace.h>
#include <lib/fidl-async/bind.h>
#include <lib/fidl/llcpp/server.h>
#include <lib/zx/channel.h>
#include <zircon/errors.h>
#include <zircon/fidl.h>

#include <memory>

#include <cobalt-client/cpp/collector.h>
#include <cobalt-client/cpp/in_memory_logger.h>
#include <fbl/algorithm.h>
#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "fs-manager.h"
#include "fshost-fs-provider.h"
#include "src/lib/storage/vfs/cpp/pseudo_dir.h"
#include "src/lib/storage/vfs/cpp/synchronous_vfs.h"
#include "src/storage/fshost/block-watcher.h"
#include "src/storage/fshost/metrics_cobalt.h"

namespace fshost {
namespace {

namespace fio = fuchsia_io;

std::unique_ptr<cobalt_client::Collector> MakeCollector() {
  return std::make_unique<cobalt_client::Collector>(
      std::make_unique<cobalt_client::InMemoryLogger>());
}

class FakeDriverManagerAdmin final
    : public fidl::WireServer<fuchsia_device_manager::Administrator> {
 public:
  void Suspend(SuspendRequestView request, SuspendCompleter::Sync& completer) override {
    completer.Reply(ZX_OK);
  }

  void UnregisterSystemStorageForShutdown(
      UnregisterSystemStorageForShutdownRequestView request,
      UnregisterSystemStorageForShutdownCompleter::Sync& completer) override {
    unregister_was_called_ = true;
    completer.Reply(ZX_OK);
  }

  bool UnregisterWasCalled() { return unregister_was_called_; }

 private:
  std::atomic<bool> unregister_was_called_ = false;
};

// Test that the manager performs the shutdown procedure correctly with respect to externally
// observable behaviors.
TEST(FsManagerTestCase, ShutdownSignalsCompletion) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  FakeDriverManagerAdmin driver_admin;
  auto admin_endpoints = fidl::CreateEndpoints<fuchsia_device_manager::Administrator>();
  ASSERT_TRUE(admin_endpoints.is_ok());
  fidl::BindServer(loop.dispatcher(), std::move(admin_endpoints->server), &driver_admin);

  zx::channel dir_request, lifecycle_request;
  FsManager manager(nullptr, std::make_unique<FsHostMetricsCobalt>(MakeCollector()));
  Config config;
  BlockWatcher watcher(manager, &config);
  ASSERT_EQ(manager.Initialize(std::move(dir_request), std::move(lifecycle_request),
                               std::move(admin_endpoints->client), nullptr, watcher),
            ZX_OK);

  // The manager should not have exited yet: No one has asked for the shutdown.
  EXPECT_FALSE(manager.IsShutdown());

  // Once we trigger shutdown, we expect a shutdown signal.
  sync_completion_t callback_called;
  manager.Shutdown([callback_called = &callback_called](zx_status_t status) {
    EXPECT_EQ(status, ZX_OK);
    sync_completion_signal(callback_called);
  });
  manager.WaitForShutdown();
  EXPECT_EQ(sync_completion_wait(&callback_called, ZX_TIME_INFINITE), ZX_OK);
  EXPECT_TRUE(driver_admin.UnregisterWasCalled());

  // It's an error if shutdown gets called twice, but we expect the callback to still get called
  // with the appropriate error message since the shutdown function has no return value.
  sync_completion_reset(&callback_called);
  manager.Shutdown([callback_called = &callback_called](zx_status_t status) {
    EXPECT_EQ(status, ZX_ERR_INTERNAL);
    sync_completion_signal(callback_called);
  });
  EXPECT_EQ(sync_completion_wait(&callback_called, ZX_TIME_INFINITE), ZX_OK);
}

// Test that the manager shuts down the filesystems given a call on the lifecycle channel
TEST(FsManagerTestCase, LifecycleStop) {
  zx::channel dir_request, lifecycle_request, lifecycle;
  zx_status_t status = zx::channel::create(0, &lifecycle_request, &lifecycle);
  ASSERT_EQ(status, ZX_OK);

  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  FakeDriverManagerAdmin driver_admin;
  auto admin_endpoints = fidl::CreateEndpoints<fuchsia_device_manager::Administrator>();
  ASSERT_TRUE(admin_endpoints.is_ok());
  fidl::BindServer(loop.dispatcher(), std::move(admin_endpoints->server), &driver_admin);

  FsManager manager(nullptr, std::make_unique<FsHostMetricsCobalt>(MakeCollector()));
  Config config;
  BlockWatcher watcher(manager, &config);
  ASSERT_EQ(manager.Initialize(std::move(dir_request), std::move(lifecycle_request),
                               std::move(admin_endpoints->client), nullptr, watcher),
            ZX_OK);

  // The manager should not have exited yet: No one has asked for an unmount.
  EXPECT_FALSE(manager.IsShutdown());

  // Call stop on the lifecycle channel
  fidl::WireSyncClient<fuchsia_process_lifecycle::Lifecycle> client(std::move(lifecycle));
  auto result = client->Stop();
  ASSERT_EQ(result.status(), ZX_OK);

  // the lifecycle channel should be closed now
  zx_signals_t pending;
  EXPECT_EQ(client.client_end().channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(),
                                                   &pending),
            ZX_OK);
  EXPECT_TRUE(pending & ZX_CHANNEL_PEER_CLOSED);

  // Now we expect a shutdown signal.
  manager.WaitForShutdown();
  EXPECT_TRUE(driver_admin.UnregisterWasCalled());
}

class MockDirectoryAdminOpener : public fuchsia_io_admin::testing::DirectoryAdmin_TestBase {
 public:
  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Unexpected call to MockDirectoryAdminOpener: " << name;
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Open(OpenRequestView request, OpenCompleter::Sync& completer) override {
    saved_open_flags_ = request->flags;
    saved_open_count_ += 1;
    saved_path_ = request->path.get();
  }

  uint32_t saved_open_flags() const { return saved_open_flags_; }
  uint32_t saved_open_count() const { return saved_open_count_; }
  const std::string saved_path() const { return saved_path_; }

 private:
  // Test fields used for validation.
  uint32_t saved_open_flags_ = 0;
  uint32_t saved_open_count_ = 0;
  std::string saved_path_;
};

// Test that asking FshostFsProvider for blobexec opens /fs/blob from the
// current installed namespace with the EXEC right
TEST(FshostFsProviderTestCase, CloneBlobExec) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  fdio_ns_t* ns;
  ASSERT_EQ(fdio_ns_get_installed(&ns), ZX_OK);

  // Mock out an object that implements DirectoryOpen and records some state;
  // bind it to the server handle.  Install it at /fs.
  auto admin = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(admin.status_value(), ZX_OK);

  auto server = std::make_shared<MockDirectoryAdminOpener>();
  fidl::BindServer(loop.dispatcher(), std::move(admin->server), server);

  fdio_ns_bind(ns, "/fs", admin->client.channel().release());

  // Verify that requesting blobexec gets you the handle at /fs/blob, with the
  // permissions expected.
  FshostFsProvider provider;
  zx::channel blobexec = provider.CloneFs("blobexec");

  // Force a describe call on the target of the Open, to resolve the Open.  We
  // expect this to fail because our mock just closes the channel after Open.
  int fd;
  EXPECT_EQ(ZX_ERR_PEER_CLOSED, fdio_fd_create(blobexec.release(), &fd));

  EXPECT_EQ(server->saved_open_count(), 1u);
  uint32_t expected_flags = ZX_FS_RIGHT_READABLE | ZX_FS_RIGHT_WRITABLE | ZX_FS_RIGHT_EXECUTABLE |
                            ZX_FS_RIGHT_ADMIN | ZX_FS_FLAG_DIRECTORY | ZX_FS_FLAG_NOREMOTE;
  EXPECT_EQ(expected_flags, server->saved_open_flags());
  EXPECT_EQ("blob", server->saved_path());

  // Tear down.
  ASSERT_EQ(fdio_ns_unbind(ns, "/fs"), ZX_OK);
}

TEST(FsManagerTestCase, InstallFsAfterShutdownWillFail) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  FakeDriverManagerAdmin driver_admin;
  auto admin_endpoints = fidl::CreateEndpoints<fuchsia_device_manager::Administrator>();
  ASSERT_TRUE(admin_endpoints.is_ok());
  fidl::BindServer(loop.dispatcher(), std::move(admin_endpoints->server), &driver_admin);

  zx::channel dir_request, lifecycle_request;
  FsManager manager(nullptr, std::make_unique<FsHostMetricsCobalt>(MakeCollector()));
  Config config;
  BlockWatcher watcher(manager, &config);
  ASSERT_EQ(manager.Initialize(std::move(dir_request), std::move(lifecycle_request),
                               std::move(admin_endpoints->client), nullptr, watcher),
            ZX_OK);

  manager.Shutdown([](zx_status_t status) { EXPECT_EQ(status, ZX_OK); });
  manager.WaitForShutdown();

  auto export_root = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(export_root.status_value(), ZX_OK);

  auto export_root_server = std::make_shared<MockDirectoryAdminOpener>();
  fidl::BindServer(loop.dispatcher(), std::move(export_root->server), export_root_server);

  auto root = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(root.status_value(), ZX_OK);

  auto root_server = std::make_shared<MockDirectoryAdminOpener>();
  fidl::BindServer(loop.dispatcher(), std::move(root->server), root_server);

  EXPECT_EQ(manager
                .InstallFs(FsManager::MountPoint::kData, export_root->client.TakeChannel(),
                           root->client.TakeChannel())
                .status_value(),
            ZX_ERR_BAD_STATE);
}

TEST(FsManagerTestCase, ReportFailureOnUncleanUnmount) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  FakeDriverManagerAdmin driver_admin;
  auto admin_endpoints = fidl::CreateEndpoints<fuchsia_device_manager::Administrator>();
  ASSERT_TRUE(admin_endpoints.is_ok());
  fidl::BindServer(loop.dispatcher(), std::move(admin_endpoints->server), &driver_admin);

  zx::channel dir_request, lifecycle_request;
  FsManager manager(nullptr, std::make_unique<FsHostMetricsCobalt>(MakeCollector()));
  Config config;
  BlockWatcher watcher(manager, &config);
  ASSERT_EQ(manager.Initialize(std::move(dir_request), std::move(lifecycle_request),
                               std::move(admin_endpoints->client), nullptr, watcher),
            ZX_OK);

  auto export_root = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(export_root.status_value(), ZX_OK);

  auto export_root_server = std::make_shared<MockDirectoryAdminOpener>();
  fidl::BindServer(loop.dispatcher(), std::move(export_root->server), export_root_server);

  auto root = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(root.status_value(), ZX_OK);

  auto root_server = std::make_shared<MockDirectoryAdminOpener>();
  fidl::BindServer(loop.dispatcher(), std::move(root->server), root_server);

  auto admin = fidl::CreateEndpoints<fuchsia_io_admin::DirectoryAdmin>();
  ASSERT_EQ(admin.status_value(), ZX_OK);

  EXPECT_EQ(manager
                .InstallFs(FsManager::MountPoint::kData, export_root->client.TakeChannel(),
                           root->client.TakeChannel())
                .status_value(),
            ZX_OK);

  zx_status_t shutdown_status = ZX_OK;
  manager.Shutdown([&shutdown_status](zx_status_t status) { shutdown_status = status; });
  manager.WaitForShutdown();

  // MockDirectoryAdminOpener doesn't handle the attempt to open the admin service (which is used to
  // shut down the filesystem) which should result in the channel being closed.
  ASSERT_EQ(shutdown_status, ZX_ERR_PEER_CLOSED);
}

}  // namespace
}  // namespace fshost
