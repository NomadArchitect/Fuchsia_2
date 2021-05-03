// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/tests/enclosed_guest.h"

#include <fcntl.h>
#include <fuchsia/kernel/cpp/fidl.h>
#include <fuchsia/netstack/cpp/fidl.h>
#include <fuchsia/sysinfo/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fit/single_threaded_executor.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <string.h>
#include <sys/mount.h>
#include <unistd.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include <algorithm>
#include <optional>
#include <string>

#include "src/lib/fxl/strings/string_printf.h"
#include "src/virtualization/lib/grpc/grpc_vsock_stub.h"
#include "src/virtualization/tests/logger.h"
#include "src/virtualization/tests/periodic_logger.h"

static constexpr char kGuestManagerUrl[] =
    "fuchsia-pkg://fuchsia.com/guest_manager#meta/guest_manager.cmx";
static constexpr char kRealm[] = "realmguestintegrationtest";
// TODO(fxbug.dev/12589): Use consistent naming for the test utils here.
static constexpr char kFuchsiaTestUtilsUrl[] =
    "fuchsia-pkg://fuchsia.com/virtualization-test-utils";
static constexpr char kDebianTestUtilDir[] = "/test_utils";
static constexpr zx::duration kLoopTimeout = zx::sec(300);
static constexpr zx::duration kLoopConditionStep = zx::msec(10);
static constexpr size_t kNumRetries = 40;
static constexpr zx::duration kRetryStep = zx::msec(200);
static constexpr uint32_t kTerminaStartupListenerPort = 7777;
static constexpr uint32_t kTerminaMaitredPort = 8888;

static bool RunLoopUntil(async::Loop* loop, fit::function<bool()> condition) {
  const zx::time deadline = zx::deadline_after(kLoopTimeout);
  while (zx::clock::get_monotonic() < deadline) {
    // Check our condition.
    if (condition()) {
      return true;
    }

    // Wait until next polling interval.
    loop->Run(zx::deadline_after(kLoopConditionStep));
    loop->ResetQuit();
  }

  return condition();
}

static std::string JoinArgVector(const std::vector<std::string>& argv) {
  std::string result;
  for (const auto& arg : argv) {
    result += arg;
    result += " ";
  }
  return result;
}

// Execute |command| on the guest serial and wait for the |result|.
zx_status_t EnclosedGuest::Execute(const std::vector<std::string>& argv,
                                   const std::unordered_map<std::string, std::string>& env,
                                   std::string* result, int32_t* return_code) {
  if (env.size() > 0) {
    FX_LOGS(ERROR) << "Only TerminaEnclosedGuest::Execute accepts environment variables.";
    return ZX_ERR_NOT_SUPPORTED;
  }
  auto command = JoinArgVector(argv);
  return console_->ExecuteBlocking(command, ShellPrompt(), result);
}

zx_status_t EnclosedGuest::Start() {
  Logger::Get().Reset();
  PeriodicLogger logger;

  logger.Start("Creating guest environment", zx::sec(5));
  real_services_->Connect(real_env_.NewRequest());
  auto services = sys::testing::EnvironmentServices::Create(real_env_, loop_.dispatcher());

  fuchsia::sys::LaunchInfo launch_info;
  launch_info.url = kGuestManagerUrl;
  zx_status_t status = services->AddServiceWithLaunchInfo(std::move(launch_info),
                                                          fuchsia::virtualization::Manager::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure launching virtualization manager: " << zx_status_get_string(status);
    return status;
  }

  status = services->AddService(fake_netstack_.GetHandler(), fuchsia::netstack::Netstack::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure launching mock netstack: " << zx_status_get_string(status);
    return status;
  }

  status = services->AddService(fake_scenic_.GetHandler(), fuchsia::ui::scenic::Scenic::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure launching fake scenic service: " << zx_status_get_string(status);
    return status;
  }

  status = services->AllowParentService(fuchsia::sysinfo::SysInfo::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure adding sysinfo service: " << zx_status_get_string(status);
    return status;
  }

  status = services->AllowParentService(fuchsia::kernel::HypervisorResource::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure adding hypervisor resource service: "
                   << zx_status_get_string(status);
    return status;
  }

  status = services->AllowParentService(fuchsia::kernel::VmexResource::Name_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure adding vmex resource service: " << zx_status_get_string(status);
    return status;
  }

  logger.Start("Creating guest sandbox", zx::sec(5));
  enclosing_environment_ =
      sys::testing::EnclosingEnvironment::Create(kRealm, real_env_, std::move(services));
  bool environment_running =
      RunLoopUntil(GetLoop(), [this] { return enclosing_environment_->is_running(); });
  if (!environment_running) {
    FX_LOGS(ERROR) << "Timed out waiting for guest sandbox environment to become ready.";
    return ZX_ERR_TIMED_OUT;
  }

  std::string url;
  fuchsia::virtualization::GuestConfig cfg;
  status = LaunchInfo(&url, &cfg);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure launching guest image: " << zx_status_get_string(status);
    return status;
  }

  enclosing_environment_->ConnectToService(manager_.NewRequest());
  manager_->Create("EnclosedGuest", realm_.NewRequest());

  status = SetupVsockServices();
  if (status != ZX_OK) {
    return status;
  }

  // Launch the guest.
  logger.Start("Launching guest", zx::sec(5));
  bool launch_complete = false;
  realm_->LaunchInstance(url, cpp17::nullopt, std::move(cfg), guest_.NewRequest(),
                         [this, &launch_complete](uint32_t cid) {
                           guest_cid_ = cid;
                           launch_complete = true;
                         });
  RunLoopUntil(GetLoop(), [&launch_complete]() { return launch_complete; });

  logger.Start("Connecting to guest serial", zx::sec(10));
  zx::socket serial_socket;
  guest_->GetSerial([&serial_socket](zx::socket s) { serial_socket = std::move(s); });
  bool socket_valid =
      RunLoopUntil(GetLoop(), [&serial_socket] { return serial_socket.is_valid(); });
  if (!socket_valid) {
    FX_LOGS(ERROR) << "Timed out waiting to connect to guest's serial.";
    return ZX_ERR_TIMED_OUT;
  }
  console_ = std::make_unique<GuestConsole>(std::make_unique<ZxSocket>(std::move(serial_socket)));
  status = console_->Start();
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Error connecting to guest's console: " << zx_status_get_string(status);
    return status;
  }

  logger.Start("Waiting for system to become ready", zx::sec(10));
  status = WaitForSystemReady();
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failure while waiting for guest system to become ready: "
                   << zx_status_get_string(status);
    return status;
  }

  ready_ = true;
  return ZX_OK;
}

zx_status_t EnclosedGuest::Stop() {
  zx_status_t status = ShutdownAndWait();
  if (status != ZX_OK) {
    return status;
  }
  loop_.Quit();
  return ZX_OK;
}

zx_status_t EnclosedGuest::RunUtil(const std::string& util, const std::vector<std::string>& argv,
                                   std::string* result) {
  return Execute(GetTestUtilCommand(util, argv), {}, result);
}

zx_status_t ZirconEnclosedGuest::LaunchInfo(std::string* url,
                                            fuchsia::virtualization::GuestConfig* cfg) {
  *url = kZirconGuestUrl;
  cfg->mutable_cmdline_add()->push_back("kernel.serial=none");
  return ZX_OK;
}

zx_status_t ZirconEnclosedGuest::WaitForSystemReady() {
  std::string ps;
  for (size_t i = 0; i != kNumRetries; i++) {
    zx_status_t status = Execute({"ps"}, {}, &ps);
    if (status != ZX_OK) {
      continue;
    }
    auto appmgr = ps.find("appmgr");
    auto virtcon = ps.find("virtual-console");
    if (appmgr == std::string::npos || virtcon == std::string::npos) {
      zx::nanosleep(zx::deadline_after(kRetryStep));
      continue;
    }
    return ZX_OK;
  }
  FX_LOGS(ERROR) << "Failed to wait for appmgr and virtual-console";

  auto appmgr = ps.find("appmgr");
  if (appmgr == std::string::npos) {
    FX_LOGS(ERROR) << "'appmgr' cannot be found in 'ps' output";
  }
  auto virtcon = ps.find("virtual-console");
  if (virtcon == std::string::npos) {
    FX_LOGS(ERROR) << "'virtual-console' cannot be found in 'ps' output";
  }
  return ZX_ERR_TIMED_OUT;
}

zx_status_t ZirconEnclosedGuest::ShutdownAndWait() {
  zx_status_t status = GetConsole()->SendBlocking("dm shutdown\n");
  if (status != ZX_OK) {
    return status;
  }
  return GetConsole()->WaitForSocketClosed();
}

std::vector<std::string> ZirconEnclosedGuest::GetTestUtilCommand(
    const std::string& util, const std::vector<std::string>& argv) {
  std::string fuchsia_url = fxl::StringPrintf("%s#meta/%s.cmx", kFuchsiaTestUtilsUrl, util.c_str());
  std::vector<std::string> exec_argv = {"/bin/run", fuchsia_url};
  exec_argv.insert(exec_argv.end(), argv.begin(), argv.end());
  return exec_argv;
}

zx_status_t DebianEnclosedGuest::LaunchInfo(std::string* url,
                                            fuchsia::virtualization::GuestConfig* cfg) {
  *url = kDebianGuestUrl;
  return ZX_OK;
}

zx_status_t DebianEnclosedGuest::WaitForSystemReady() {
  for (size_t i = 0; i != kNumRetries; i++) {
    std::string response;
    zx_status_t status = Execute({"echo", "guest ready"}, {}, &response);
    if (status != ZX_OK) {
      continue;
    }
    auto ready = response.find("guest ready");
    if (ready == std::string::npos) {
      zx::nanosleep(zx::deadline_after(kRetryStep));
      continue;
    }
    return ZX_OK;
  }
  FX_LOGS(ERROR) << "Failed to wait for shell";
  return ZX_ERR_TIMED_OUT;
}

zx_status_t DebianEnclosedGuest::ShutdownAndWait() {
  PeriodicLogger logger("Attempting to shut down guest", zx::sec(10));
  zx_status_t status = GetConsole()->SendBlocking("shutdown now\n");
  if (status != ZX_OK) {
    return status;
  }
  return GetConsole()->WaitForSocketClosed();
}

std::vector<std::string> DebianEnclosedGuest::GetTestUtilCommand(
    const std::string& util, const std::vector<std::string>& argv) {
  std::string bin_path = fxl::StringPrintf("%s/%s", kDebianTestUtilDir, util.c_str());

  std::vector<std::string> exec_argv = {bin_path};
  exec_argv.insert(exec_argv.end(), argv.begin(), argv.end());
  return exec_argv;
}

zx_status_t TerminaEnclosedGuest::LaunchInfo(std::string* url,
                                             fuchsia::virtualization::GuestConfig* cfg) {
  *url = kTerminaGuestUrl;
  cfg->set_virtio_gpu(false);

  // Add the block device that contains the test binaries.
  int fd = open("/pkg/data/linux_tests.img", O_RDONLY);
  if (fd < 0) {
    return ZX_ERR_BAD_STATE;
  }
  zx::channel channel;
  zx_status_t status = fdio_get_service_handle(fd, channel.reset_and_get_address());
  if (status != ZX_OK) {
    return status;
  }
  cfg->mutable_block_devices()->push_back({
      "linux_tests",
      fuchsia::virtualization::BlockMode::READ_ONLY,
      fuchsia::virtualization::BlockFormat::RAW,
      fidl::InterfaceHandle<fuchsia::io::File>(std::move(channel)),
  });
  // Add non-prebuilt test extras.
  fd = open("/pkg/data/extras.img", O_RDONLY);
  if (fd < 0) {
    return ZX_ERR_BAD_STATE;
  }
  status = fdio_get_service_handle(fd, channel.reset_and_get_address());
  if (status != ZX_OK) {
    return status;
  }
  cfg->mutable_block_devices()->push_back({
      "extras",
      fuchsia::virtualization::BlockMode::READ_ONLY,
      fuchsia::virtualization::BlockFormat::RAW,
      fidl::InterfaceHandle<fuchsia::io::File>(std::move(channel)),
  });
  return ZX_OK;
}

zx_status_t TerminaEnclosedGuest::SetupVsockServices() {
  fuchsia::virtualization::HostVsockEndpointPtr grpc_endpoint;
  GetHostVsockEndpoint(vsock_.NewRequest());
  GetHostVsockEndpoint(grpc_endpoint.NewRequest());

  GrpcVsockServerBuilder builder(std::move(grpc_endpoint));
  builder.AddListenPort(kTerminaStartupListenerPort);
  builder.RegisterService(this);

  executor_.schedule_task(
      builder.Build().and_then([this](std::unique_ptr<GrpcVsockServer>& result) mutable {
        server_ = std::move(result);
        return fit::ok();
      }));
  if (!RunLoopUntil(GetLoop(), [this] { return server_ != nullptr; })) {
    return ZX_ERR_TIMED_OUT;
  }

  return ZX_OK;
}

grpc::Status TerminaEnclosedGuest::VmReady(grpc::ServerContext* context,
                                           const vm_tools::EmptyMessage* request,
                                           vm_tools::EmptyMessage* response) {
  auto p = NewGrpcVsockStub<vm_tools::Maitred>(vsock_, GetGuestCid(), kTerminaMaitredPort);
  auto result = fit::run_single_threaded(std::move(p));
  if (result.is_ok()) {
    maitred_ = std::move(result.value());
  } else {
    FX_PLOGS(ERROR, result.error()) << "Failed to connect to maitred";
  }
  return grpc::Status::OK;
}

// Use Maitred to mount the given block device at the given location.
//
// The destination directory will be created if required.
zx_status_t MountDeviceInGuest(vm_tools::Maitred::Stub& maitred, std::string_view block_device,
                               std::string_view mount_point, std::string_view fs_type,
                               uint64_t mount_flags) {
  grpc::ClientContext context;
  vm_tools::MountRequest request;
  vm_tools::MountResponse response;

  request.mutable_source()->assign(block_device);
  request.mutable_target()->assign(mount_point);
  request.mutable_fstype()->assign(fs_type);
  request.set_mountflags(mount_flags);
  request.set_create_target(true);

  auto grpc_status = maitred.Mount(&context, request, &response);
  if (!grpc_status.ok()) {
    FX_LOGS(ERROR) << "Request to mount block device '" << block_device
                   << "' failed: " << grpc_status.error_message();
    return ZX_ERR_IO;
  }
  if (response.error() != 0) {
    FX_LOGS(ERROR) << "Mounting block device '" << block_device << "' failed: " << response.error();
    return ZX_ERR_IO;
  }

  return ZX_OK;
}

zx_status_t TerminaEnclosedGuest::WaitForSystemReady() {
  // The VM will connect to the StartupListener port when it's ready and we'll
  // create the maitred stub in |VmReady|.
  {
    PeriodicLogger logger("Wait for maitred", zx::sec(1));
    if (!RunLoopUntil(GetLoop(), [this] { return maitred_ != nullptr; })) {
      return ZX_ERR_TIMED_OUT;
    }
  }
  FX_CHECK(maitred_) << "No maitred connection";

  // Connect to vshd.
  fuchsia::virtualization::HostVsockEndpointPtr endpoint;
  GetHostVsockEndpoint(endpoint.NewRequest());
  command_runner_ =
      std::make_unique<vsh::BlockingCommandRunner>(std::move(endpoint), GetGuestCid());

  // Create mountpoints for test utils and extras. The root filesystem is read only so we
  // put these under /tmp.
  zx_status_t status;
  status = MountDeviceInGuest(*maitred_, "/dev/vdc", "/tmp/test_utils", "ext2", MS_RDONLY);
  if (status != ZX_OK) {
    return status;
  }
  status = MountDeviceInGuest(*maitred_, "/dev/vdd", "/tmp/extras", "romfs", MS_RDONLY);
  if (status != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t TerminaEnclosedGuest::ShutdownAndWait() {
  if (server_) {
    server_->inner()->Shutdown();
    server_->inner()->Wait();
  }
  return ZX_OK;
}

zx_status_t TerminaEnclosedGuest::Execute(const std::vector<std::string>& argv,
                                          const std::unordered_map<std::string, std::string>& env,
                                          std::string* result, int32_t* return_code) {
  auto command_result = command_runner_->Execute({argv, env});
  if (command_result.is_error()) {
    return command_result.error();
  }
  if (result) {
    *result = std::move(command_result.value().out);
    if (!command_result.value().err.empty()) {
      *result += "\n";
      *result += std::move(command_result.value().err);
    }
  }
  if (return_code) {
    *return_code = command_result.value().return_code;
  }
  return ZX_OK;
}

std::vector<std::string> TerminaEnclosedGuest::GetTestUtilCommand(
    const std::string& util, const std::vector<std::string>& args) {
  std::vector<std::string> argv;
  argv.emplace_back("/tmp/test_utils/" + util);
  argv.insert(argv.end(), args.begin(), args.end());
  return argv;
}
