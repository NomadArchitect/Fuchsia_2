// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/device-name-provider/args.h"

#include <fuchsia/boot/llcpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/dispatcher.h>
#include <lib/fdio/spawn.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/sync/completion.h>
#include <lib/zx/process.h>
#include <zircon/processargs.h>

#include <fs/pseudo_dir.h>
#include <fs/service.h>
#include <fs/synchronous_vfs.h>
#include <mock-boot-arguments/server.h>
#include <zxtest/zxtest.h>

namespace {
constexpr char kInterface[] = "/dev/whatever/whatever";
constexpr char kNodename[] = "some-four-word-name";
constexpr char kEthDir[] = "/dev";

class FakeSvc {
 public:
  explicit FakeSvc(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher), vfs_(dispatcher) {
    auto root_dir = fbl::MakeRefCounted<fs::PseudoDir>();
    root_dir->AddEntry(::llcpp::fuchsia::boot::Arguments::Name,
                       fbl::MakeRefCounted<fs::Service>([this](zx::channel request) {
                         auto result =
                             fidl::BindServer(dispatcher_, std::move(request), &mock_boot_);
                         if (!result.is_ok()) {
                           return result.error();
                         }
                         return ZX_OK;
                       }));

    zx::channel svc_remote;
    ASSERT_OK(zx::channel::create(0, &svc_local_, &svc_remote));

    vfs_.ServeDirectory(root_dir, std::move(svc_remote));
  }

  mock_boot_arguments::Server& mock_boot() { return mock_boot_; }
  zx::channel& svc_chan() { return svc_local_; }

 private:
  async_dispatcher_t* dispatcher_;
  fs::SynchronousVfs vfs_;
  mock_boot_arguments::Server mock_boot_;
  zx::channel svc_local_;
};

class ArgsTest : public zxtest::Test {
 public:
  ArgsTest() : loop_(&kAsyncLoopConfigNoAttachToCurrentThread), fake_svc_(loop_.dispatcher()) {
    loop_.StartThread("paver-test-loop");
  }

  ~ArgsTest() { loop_.Shutdown(); }

  FakeSvc& fake_svc() { return fake_svc_; }
  const zx::channel& svc_root() { return fake_svc_.svc_chan(); }

 private:
  async::Loop loop_;
  FakeSvc fake_svc_;
};

TEST_F(ArgsTest, DeviceNameProviderNoneProvided) {
  int argc = 1;
  const char* argv[] = {"device-name-provider"};
  const char* error = nullptr;
  DeviceNameProviderArgs args;
  ASSERT_EQ(ParseArgs(argc, const_cast<char**>(argv), svc_root(), &error, &args), 0, "%s", error);
  ASSERT_TRUE(args.interface.empty());
  ASSERT_TRUE(args.nodename.empty());
  ASSERT_EQ(args.namegen, 0);
  ASSERT_EQ(args.ethdir, std::string("/dev/class/ethernet"));
  ASSERT_EQ(error, nullptr);
}

TEST_F(ArgsTest, DeviceNameProviderAllProvided) {
  int argc = 9;
  const char* argv[] = {"device-name-provider",
                        "--nodename",
                        kNodename,
                        "--interface",
                        kInterface,
                        "--ethdir",
                        kEthDir,
                        "--namegen",
                        "1"};
  const char* error = nullptr;
  DeviceNameProviderArgs args;
  ASSERT_EQ(ParseArgs(argc, const_cast<char**>(argv), svc_root(), &error, &args), 0, "%s", error);
  ASSERT_EQ(args.interface, std::string(kInterface));
  ASSERT_EQ(args.nodename, std::string(kNodename));
  ASSERT_EQ(args.ethdir, std::string(kEthDir));
  ASSERT_EQ(args.namegen, 1);
  ASSERT_EQ(error, nullptr);
}

TEST_F(ArgsTest, DeviceNameProviderValidation) {
  int argc = 2;
  const char* argv[] = {
      "device-name-provider",
      "--interface",
  };
  DeviceNameProviderArgs args;
  const char* error = nullptr;
  ASSERT_LT(ParseArgs(argc, const_cast<char**>(argv), svc_root(), &error, &args), 0);
  ASSERT_TRUE(args.interface.empty());
  ASSERT_TRUE(strstr(error, "interface"));

  argc = 2;
  argv[1] = "--nodename";
  args.interface = "";
  args.nodename = "";
  args.namegen = 0;
  error = nullptr;
  ASSERT_LT(ParseArgs(argc, const_cast<char**>(argv), svc_root(), &error, &args), 0);
  ASSERT_TRUE(args.nodename.empty());
  ASSERT_TRUE(strstr(error, "nodename"));

  argc = 2;
  argv[1] = "--namegen";
  args.interface = "";
  args.nodename = "";
  args.namegen = 0;
  error = nullptr;
  ASSERT_LT(ParseArgs(argc, const_cast<char**>(argv), svc_root(), &error, &args), 0);
  ASSERT_EQ(args.namegen, 0);
  ASSERT_TRUE(strstr(error, "namegen"));
}
}  // namespace
