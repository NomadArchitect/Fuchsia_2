// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fshost/fshost_integration_test.h"

#include <sys/statfs.h>

namespace devmgr {

void FshostIntegrationTest::SetUp() {
  std::string service_name = std::string("/svc/") + fuchsia::sys2::Realm::Name_;
  auto status = zx::make_status(
      fdio_service_connect(service_name.c_str(), realm_.NewRequest().TakeChannel().get()));
  ASSERT_TRUE(status.is_ok());

  fuchsia::sys2::Realm_CreateChild_Result create_result;
  fuchsia::sys2::ChildDecl child_decl;
  child_decl.set_name("test-fshost")
      .set_url("fuchsia-pkg://fuchsia.com/fshost-tests#meta/test-fshost.cm")
      .set_startup(fuchsia::sys2::StartupMode::LAZY);
  status =
      zx::make_status(realm_->CreateChild(fuchsia::sys2::CollectionRef{.name = "fshost-collection"},
                                          std::move(child_decl), &create_result));
  ASSERT_TRUE(status.is_ok() && !create_result.is_err());

  fuchsia::sys2::Realm_BindChild_Result bind_result;
  status = zx::make_status(realm_->BindChild(
      fuchsia::sys2::ChildRef{.name = "test-fshost", .collection = "fshost-collection"},
      exposed_dir_.NewRequest(), &bind_result));
  ASSERT_TRUE(status.is_ok() && !bind_result.is_err());

  // Describe so that we discover errors early.
  fuchsia::io::NodeInfo info;
  ASSERT_EQ(exposed_dir_->Describe(&info), ZX_OK);

  zx::channel request;
  status = zx::make_status(zx::channel::create(0, &watcher_channel_, &request));
  ASSERT_EQ(status.status_value(), ZX_OK);
  status = zx::make_status(
      exposed_dir_->Open(fuchsia::io::OPEN_RIGHT_READABLE | fuchsia::io::OPEN_RIGHT_WRITABLE, 0,
                         fidl::DiscoverableProtocolName<fuchsia_fshost::BlockWatcher>,
                         fidl::InterfaceRequest<fuchsia::io::Node>(std::move(request))));
  ASSERT_EQ(status.status_value(), ZX_OK);
}

void FshostIntegrationTest::TearDown() {
  fuchsia::sys2::Realm_DestroyChild_Result destroy_result;
  auto status = zx::make_status(realm_->DestroyChild(
      fuchsia::sys2::ChildRef{.name = "test-fshost", .collection = "fshost-collection"},
      &destroy_result));
  ASSERT_TRUE(status.is_ok() && !destroy_result.is_err());
}

void FshostIntegrationTest::PauseWatcher() const {
  auto result = fidl::WireCall<fuchsia_fshost::BlockWatcher>(watcher_channel_.borrow()).Pause();
  ASSERT_EQ(result.status(), ZX_OK);
  ASSERT_EQ(result->status, ZX_OK);
}

void FshostIntegrationTest::ResumeWatcher() const {
  auto result = fidl::WireCall<fuchsia_fshost::BlockWatcher>(watcher_channel_.borrow()).Resume();
  ASSERT_EQ(result.status(), ZX_OK);
  ASSERT_EQ(result->status, ZX_OK);
}

fbl::unique_fd FshostIntegrationTest::WaitForMount(const std::string& name,
                                                   uint64_t expected_fs_type) {
  // The mount point will always exist so we expect open() to work regardless of whether the device
  // is actually mounted. We retry until the mount point has the expected filesystem type. Retry 20
  // times and then give up.
  for (int i = 0; i < 20; i++) {
    fidl::SynchronousInterfacePtr<fuchsia::io::Node> root;
    zx_status_t status =
        exposed_dir()->Open(fuchsia::io::OPEN_RIGHT_READABLE, 0, name, root.NewRequest());
    EXPECT_EQ(ZX_OK, status);
    if (status != ZX_OK)
      return fbl::unique_fd();

    fbl::unique_fd fd;
    status = fdio_fd_create(root.Unbind().TakeChannel().release(), fd.reset_and_get_address());
    EXPECT_EQ(ZX_OK, status);
    if (status != ZX_OK)
      return fbl::unique_fd();

    struct statfs buf;
    EXPECT_EQ(fstatfs(fd.get(), &buf), 0);
    if (buf.f_type == expected_fs_type)
      return fd;

    sleep(1);
  }

  return fbl::unique_fd();
}

}  // namespace devmgr
