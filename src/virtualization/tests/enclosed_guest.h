// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_TESTS_ENCLOSED_GUEST_H_
#define SRC_VIRTUALIZATION_TESTS_ENCLOSED_GUEST_H_

#include <fuchsia/virtualization/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/executor.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/sys/cpp/testing/test_with_environment_fixture.h>

#include <memory>

#include "lib/async/dispatcher.h"
#include "lib/sys/cpp/testing/enclosing_environment.h"
#include "src/virtualization/lib/grpc/grpc_vsock_server.h"
#include "src/virtualization/lib/vsh/command_runner.h"
#include "src/virtualization/tests/fake_netstack.h"
#include "src/virtualization/tests/fake_scenic.h"
#include "src/virtualization/tests/guest_console.h"
#include "src/virtualization/tests/socket_logger.h"
#include "src/virtualization/third_party/vm_tools/vm_guest.grpc.pb.h"
#include "src/virtualization/third_party/vm_tools/vm_host.grpc.pb.h"

enum class GuestKernel {
  ZIRCON,
  LINUX,
};

class LocalGuestConfigProvider : public fuchsia::virtualization::GuestConfigProvider,
                                 public component_testing::LocalComponent {
 public:
  LocalGuestConfigProvider(async_dispatcher_t* dispatcher, std::string package_dir_name,
                           fuchsia::virtualization::GuestConfig&& config);

  // |fuchsia::virtualization::GuestConfigProvider|
  void Get(GetCallback callback) override;
  // |component_testing::LocalComponent
  void Start(std::unique_ptr<component_testing::LocalComponentHandles> handles) override;

 private:
  async_dispatcher_t* dispatcher_;
  fidl::BindingSet<fuchsia::virtualization::GuestConfigProvider> binding_set_;
  std::unique_ptr<component_testing::LocalComponentHandles> handles_;
  fuchsia::virtualization::GuestConfig config_;
  std::string package_dir_name_;
};

// EnclosedGuest is a base class that defines an guest environment and instance
// encapsulated in an EnclosingEnvironment. A derived class must define the
// |LaunchInfo| to send to the guest environment controller, as well as methods
// for waiting for the guest to be ready and running test utilities. Most tests
// will derive from either ZirconEnclosedGuest or DebianEnclosedGuest below and
// override LaunchInfo only. EnclosedGuest is designed to be used with
// GuestTest.
class EnclosedGuest {
 public:
  explicit EnclosedGuest(async::Loop& loop) : loop_(loop) {}
  virtual ~EnclosedGuest() {}

  // Start the guest.
  //
  // Abort with ZX_ERR_TIMED_OUT if we reach `deadline` first.
  // This is the preferred way to start up the guest, which creates an enclosing environment
  // internally, and launches the guest by calling `Install` and `Launch` respectively.
  zx_status_t Start(zx::time deadline);

  // Attempt to gracefully stop the guest.
  //
  // Abort with ZX_ERR_TIMED_OUT if we reach `deadline` first.
  zx_status_t Stop(zx::time deadline);

  // TODO(fxbug.dev/72386)
  // Remove once audio test framework is migrated to RealmBuilder and virtio sound tests is using
  // CFv2
  zx_status_t InstallV1(sys::testing::EnvironmentServices& services);
  zx_status_t LaunchV1(sys::testing::EnclosingEnvironment& environment, const std::string& realm,
                       zx::time deadline);
  void GetHostVsockEndpointV1(
      fidl::InterfaceRequest<fuchsia::virtualization::HostVsockEndpoint> endpoint) {
    realm_->GetHostVsockEndpoint(std::move(endpoint));
  }
  virtual bool UsingCFv1() const { return false; }

  bool Ready() const { return ready_; }

  // Execute |command| on the guest serial and wait for the |result|.
  virtual zx_status_t Execute(const std::vector<std::string>& argv,
                              const std::unordered_map<std::string, std::string>& env,
                              zx::time deadline, std::string* result = nullptr,
                              int32_t* return_code = nullptr);

  // Run a test util named |util| with |argv| in the guest and wait for the
  // |result|.
  zx_status_t RunUtil(const std::string& util, const std::vector<std::string>& argv,
                      zx::time deadline, std::string* result = nullptr);

  // Return a shell command for a test utility named |util| with the given
  // |argv| in the guest. The result may be passed directly to |Execute|
  // to actually run the command.
  virtual std::vector<std::string> GetTestUtilCommand(const std::string& util,
                                                      const std::vector<std::string>& argv) = 0;

  virtual GuestKernel GetGuestKernel() = 0;

  template <typename T>
  ::fidl::SynchronousInterfacePtr<T> ConnectToRealmSync() {
    return realm_root_->ConnectSync<T>();
  }

  template <typename T>
  ::fidl::InterfacePtr<T> ConnectToRealm() {
    return realm_root_->Connect<T>();
  }

  uint32_t GetGuestCid() const { return guest_cid_; }

  FakeNetstack* GetNetstack() { return &fake_netstack_; }

  FakeScenic* GetScenic() { return &fake_scenic_; }

  std::optional<GuestConsole>& GetConsole() { return console_; }

 protected:
  // Provides guest specific |url| and |cfg|, called by Start.
  virtual zx_status_t LaunchInfo(std::string* url, fuchsia::virtualization::GuestConfig* cfg) = 0;

  // Waits until the guest is ready to run test utilities, called by Start.
  virtual zx_status_t WaitForSystemReady(zx::time deadline) = 0;

  // Waits for the guest to perform a graceful shutdown.
  virtual zx_status_t ShutdownAndWait(zx::time deadline) = 0;

  virtual std::string ShellPrompt() = 0;

  // Invoked after the guest |Realm| has been created but before the guest
  // has been launched.
  //
  // Any vsock ports that are listened on here are guaranteed to be ready to
  // accept connections before the guest attempts to connect to them.
  virtual zx_status_t SetupVsockServices(zx::time deadline) { return ZX_OK; }

  async::Loop* GetLoop() { return &loop_; }

 private:
  async::Loop& loop_;
  std::unique_ptr<component_testing::RealmRoot> realm_root_;

  std::unique_ptr<LocalGuestConfigProvider> local_guest_config_provider_;

  fuchsia::virtualization::GuestPtr guest_;
  FakeScenic fake_scenic_;
  FakeNetstack fake_netstack_;

  std::optional<SocketLogger> serial_logger_;
  std::optional<GuestConsole> console_;
  uint32_t guest_cid_;
  bool ready_ = false;

  // TODO(fxbug.dev/72386)
  // Remove once audio test framework is migrated to RealmBuilder and sound test is using CFv2
  fuchsia::sys::EnvironmentPtr real_env_;
  std::unique_ptr<sys::testing::EnclosingEnvironment> enclosing_environment_;
  fuchsia::virtualization::ManagerPtr manager_;
  fuchsia::virtualization::RealmPtr realm_;
};

class ZirconEnclosedGuest : public EnclosedGuest {
 public:
  explicit ZirconEnclosedGuest(async::Loop& loop) : EnclosedGuest(loop) {}

  std::vector<std::string> GetTestUtilCommand(const std::string& util,
                                              const std::vector<std::string>& argv) override;

  GuestKernel GetGuestKernel() override { return GuestKernel::ZIRCON; }

 protected:
  zx_status_t LaunchInfo(std::string* url, fuchsia::virtualization::GuestConfig* cfg) override;
  zx_status_t WaitForSystemReady(zx::time deadline) override;
  zx_status_t ShutdownAndWait(zx::time deadline) override;
  std::string ShellPrompt() override { return "$ "; }
};

class DebianEnclosedGuest : public EnclosedGuest {
 public:
  explicit DebianEnclosedGuest(async::Loop& loop) : EnclosedGuest(loop) {}

  std::vector<std::string> GetTestUtilCommand(const std::string& util,
                                              const std::vector<std::string>& argv) override;

  GuestKernel GetGuestKernel() override { return GuestKernel::LINUX; }

 protected:
  zx_status_t LaunchInfo(std::string* url, fuchsia::virtualization::GuestConfig* cfg) override;
  zx_status_t WaitForSystemReady(zx::time deadline) override;
  zx_status_t ShutdownAndWait(zx::time deadline) override;
  std::string ShellPrompt() override { return "$ "; }
};

class TerminaEnclosedGuest : public EnclosedGuest, public vm_tools::StartupListener::Service {
 public:
  explicit TerminaEnclosedGuest(async::Loop& loop)
      : EnclosedGuest(loop), executor_(loop.dispatcher()) {}

  GuestKernel GetGuestKernel() override { return GuestKernel::LINUX; }

  std::vector<std::string> GetTestUtilCommand(const std::string& util,
                                              const std::vector<std::string>& argv) override;
  zx_status_t Execute(const std::vector<std::string>& argv,
                      const std::unordered_map<std::string, std::string>& env, zx::time deadline,
                      std::string* result, int32_t* return_code) override;

 protected:
  zx_status_t LaunchInfo(std::string* url, fuchsia::virtualization::GuestConfig* cfg) override;
  zx_status_t WaitForSystemReady(zx::time deadline) override;
  zx_status_t ShutdownAndWait(zx::time deadline) override;
  std::string ShellPrompt() override { return "$ "; }

 private:
  zx_status_t SetupVsockServices(zx::time deadline) override;

  // |vm_tools::StartupListener::Service|
  grpc::Status VmReady(grpc::ServerContext* context, const vm_tools::EmptyMessage* request,
                       vm_tools::EmptyMessage* response) override;

  std::unique_ptr<vsh::BlockingCommandRunner> command_runner_;
  async::Executor executor_;
  std::unique_ptr<GrpcVsockServer> server_;
  std::unique_ptr<vm_tools::Maitred::Stub> maitred_;
  fuchsia::virtualization::HostVsockEndpointPtr vsock_;
};

using AllGuestTypes =
    ::testing::Types<ZirconEnclosedGuest, DebianEnclosedGuest, TerminaEnclosedGuest>;

class GuestTestNameGenerator {
 public:
  template <typename T>
  static std::string GetName(int idx) {
    // Use is_base_of because some tests will use sub-classes. By default gtest will just use
    // idx to string, so we just suffix the actual enclosed guest type.
    if (std::is_base_of<ZirconEnclosedGuest, T>())
      return std::to_string(idx) + "_ZirconGuest";
    if (std::is_base_of<DebianEnclosedGuest, T>())
      return std::to_string(idx) + "_DebianGuest";
    if (std::is_base_of<TerminaEnclosedGuest, T>())
      return std::to_string(idx) + "_TerminaGuest";
  }
};

#endif  // SRC_VIRTUALIZATION_TESTS_ENCLOSED_GUEST_H_
