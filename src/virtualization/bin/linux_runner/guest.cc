// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/linux_runner/guest.h"

#include <arpa/inet.h>
#include <fuchsia/device/cpp/fidl.h>
#include <fuchsia/hardware/block/volume/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/status.h>
#include <netinet/in.h>
#include <sys/mount.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/status.h>

#include <memory>

#include <fbl/unique_fd.h>

#include "src/virtualization/bin/linux_runner/block_devices.h"
#include "src/virtualization/bin/linux_runner/ports.h"
#include "src/virtualization/lib/grpc/grpc_vsock_stub.h"
#include "src/virtualization/lib/guest_config/guest_config.h"
#include "src/virtualization/third_party/vm_tools/vm_guest.grpc.pb.h"

#include <grpc++/grpc++.h>
#include <grpc++/server_posix.h>

namespace {
[[maybe_unused]] constexpr const char kLinuxGuestPackage[] =
    "fuchsia-pkg://fuchsia.com/termina_guest#meta/termina_guest.cmx";
constexpr const char kContainerName[] = "penguin";
constexpr const char kContainerImageAlias[] = "debian/bullseye";
constexpr const char kContainerImageServer[] = "https://storage.googleapis.com/cros-containers/96";
constexpr const char kDefaultContainerUser[] = "machina";

// Return the given IPv4 address as a packed uint32_t in network byte
// order (i.e., big endian).
//
// `Ipv4Addr(127, 0, 0, 1)` will generate the loopback address "127.0.0.1".
constexpr uint32_t Ipv4Addr(uint8_t a, uint8_t b, uint8_t c, uint8_t d) {
  return htonl((static_cast<uint32_t>(a) << 24) | (static_cast<uint32_t>(b) << 16) |
               (static_cast<uint32_t>(c) << 8) | (static_cast<uint32_t>(d) << 0));
}

// Run the given command in the guest as a daemon (i.e., in the background and
// automatically restarted on failure).
void MaitredStartDaemon(vm_tools::Maitred::Stub& maitred, std::vector<std::string> args,
                        std::vector<std::pair<std::string, std::string>> env) {
  grpc::ClientContext context;
  vm_tools::LaunchProcessRequest request;
  vm_tools::LaunchProcessResponse response;

  // Set up args / environment.
  request.mutable_argv()->Assign(args.begin(), args.end());
  request.mutable_env()->insert(env.begin(), env.end());

  // Set up as a daemon.
  request.set_use_console(true);
  request.set_respawn(true);
  request.set_wait_for_exit(false);

  TRACE_DURATION("linux_runner", "LaunchProcessRPC");
  grpc::Status status = maitred.LaunchProcess(&context, request, &response);
  FX_CHECK(status.ok()) << "Failed to start daemon in guest: " << status.error_message() << "\n"
                        << "Command run: " << request.DebugString();
  FX_CHECK(response.status() == vm_tools::ProcessStatus::LAUNCHED)
      << "Process failed to launch, with launch status: "
      << vm_tools::ProcessStatus_Name(response.status());
}

// Run the given command in the guest, blocking until finished.
void MaitredRunCommandSync(vm_tools::Maitred::Stub& maitred, std::vector<std::string> args,
                           std::vector<std::pair<std::string, std::string>> env) {
  grpc::ClientContext context;
  vm_tools::LaunchProcessRequest request;
  vm_tools::LaunchProcessResponse response;

  // Set up args / environment.
  request.mutable_argv()->Assign(args.begin(), args.end());
  request.mutable_env()->insert(env.begin(), env.end());

  // Set the command as synchronous.
  request.set_use_console(true);
  request.set_respawn(false);
  request.set_wait_for_exit(true);

  TRACE_DURATION("linux_runner", "LaunchProcessRPC");
  grpc::Status status = maitred.LaunchProcess(&context, request, &response);
  FX_CHECK(status.ok()) << "Guest command failed: " << status.error_message();
}

// Ask maitre'd to enable the network in the guest.
void MaitredBringUpNetwork(vm_tools::Maitred::Stub& maitred, uint32_t address, uint32_t gateway,
                           uint32_t netmask) {
  grpc::ClientContext context;
  vm_tools::NetworkConfigRequest request;
  vm_tools::EmptyMessage response;

  vm_tools::IPv4Config* config = request.mutable_ipv4_config();
  config->set_address(Ipv4Addr(100, 64, 1, 1));       // 100.64.1.1, RFC-6598 address
  config->set_gateway(Ipv4Addr(100, 64, 1, 2));       // 100.64.1.2, RFC-6598 address
  config->set_netmask(Ipv4Addr(255, 255, 255, 252));  // 30-bit netmask

  TRACE_DURATION("linux_runner", "ConfigureNetworkRPC");
  grpc::Status status = maitred.ConfigureNetwork(&context, request, &response);
  FX_CHECK(status.ok()) << "Failed to configure guest network: " << status.error_message();
}

}  // namespace

namespace linux_runner {

using ::fuchsia::virtualization::Listener;

// static
zx_status_t Guest::CreateAndStart(sys::ComponentContext* context, GuestConfig config,
                                  fuchsia::virtualization::GuestManager& guest_manager,
                                  GuestInfoCallback callback, std::unique_ptr<Guest>* guest) {
  TRACE_DURATION("linux_runner", "Guest::CreateAndStart");
  *guest = std::make_unique<Guest>(context, config, std::move(callback), guest_manager);
  return ZX_OK;
}

Guest::Guest(sys::ComponentContext* context, GuestConfig config, GuestInfoCallback callback,
             fuchsia::virtualization::GuestManager& guest_manager)
    : async_(async_get_default_dispatcher()),
      executor_(async_),
      context_(context),
      config_(config),
      callback_(std::move(callback)),
      guest_manager_(guest_manager) {
  auto result = Start();
  if (!result.is_ok()) {
    FX_PLOGS(ERROR, result.status_value()) << "Failed to start guest";
  }
}

Guest::~Guest() {
  if (grpc_server_) {
    grpc_server_->inner()->Shutdown();
    grpc_server_->inner()->Wait();
  }
}

zx::status<> Guest::Start() {
  TRACE_DURATION("linux_runner", "Guest::Start");
  auto result = StartGrpcServer();
  if (!result.is_ok()) {
    return result.take_error();
  }
  grpc_server_ = std::move(result->first);
  StartGuest(std::move(result->second));
  return zx::ok();
}

zx::status<std::pair<std::unique_ptr<GrpcVsockServer>, std::vector<Listener>>>
Guest::StartGrpcServer() {
  TRACE_DURATION("linux_runner", "Guest::StartGrpcServer");
  fuchsia::virtualization::HostVsockEndpointPtr socket_endpoint;

  GrpcVsockServerBuilder builder;

  // CrashListener
  builder.AddListenPort(kCrashListenerPort);
  builder.RegisterService(&crash_listener_);

  // LogCollector
  builder.AddListenPort(kLogCollectorPort);
  builder.RegisterService(&log_collector_);

  // StartupListener
  builder.AddListenPort(kStartupListenerPort);
  builder.RegisterService(static_cast<vm_tools::StartupListener::Service*>(this));

  // TremplinListener
  builder.AddListenPort(kTremplinListenerPort);
  builder.RegisterService(static_cast<vm_tools::tremplin::TremplinListener::Service*>(this));

  // ContainerListener
  builder.AddListenPort(kGarconPort);
  builder.RegisterService(static_cast<vm_tools::container::ContainerListener::Service*>(this));
  return builder.Build();
}

void Guest::StartGuest(std::vector<Listener> vsock_listeners) {
  TRACE_DURATION("linux_runner", "Guest::StartGuest");
  FX_CHECK(!guest_controller_) << "Called StartGuest with an existing instance";
  FX_LOGS(INFO) << "Launching guest...";

  auto block_devices_result = GetBlockDevices(config_.stateful_image_size);
  if (block_devices_result.is_error()) {
    PostContainerFailure(block_devices_result.error_value());
    FX_LOGS(ERROR) << "Failed to start guest: missing block device";
    return;
  }

  // Drop /dev from our local namespace. We no longer need this capability so we go ahead and
  // release it.
  DropDevNamespace();

  fuchsia::virtualization::GuestConfig cfg;
  *cfg.mutable_vsock_listeners() = std::move(vsock_listeners);
  cfg.set_virtio_gpu(false);
  cfg.set_block_devices(std::move(block_devices_result.value()));
  cfg.set_magma_device(fuchsia::virtualization::MagmaDevice());

  // Connect to the wayland bridge afresh, restarting it if it has crashed.
  fuchsia::wayland::ServerPtr server_proxy;
  context_->svc()->Connect(server_proxy.NewRequest());
  cfg.mutable_wayland_device()->server = std::move(server_proxy);

  auto vm_create_nonce = TRACE_NONCE();
  TRACE_FLOW_BEGIN("linux_runner", "LaunchInstance", vm_create_nonce);

  guest_manager_.LaunchGuest(
      std::move(cfg), guest_controller_.NewRequest(), [this, vm_create_nonce](auto res) {
        if (res.is_err()) {
          FX_PLOGS(ERROR, res.err()) << "Termina Guest failed to launch";
        } else {
          TRACE_DURATION("linux_runner", "LaunchInstance Callback");
          TRACE_FLOW_END("linux_runner", "LaunchInstance", vm_create_nonce);
          FX_LOGS(INFO) << "Termina Guest launched";
          guest_controller_->GetHostVsockEndpoint(socket_endpoint_.NewRequest(), [this](auto res) {
            if (res.is_err()) {
              PostContainerFailure("Termina Guest not launched with mandatory vsock support");
            } else {
              PostContainerStatus(fuchsia::virtualization::ContainerStatus::LAUNCHING_GUEST);
              TRACE_FLOW_BEGIN("linux_runner", "TerminaBoot", vm_ready_nonce_);
            }
          });
        }
      });
}

void Guest::MountVmTools() {
  TRACE_DURATION("linux_runner", "Guest::MountVmTools");
  FX_CHECK(maitred_) << "Called MountVmTools without a maitre'd connection";
  FX_LOGS(INFO) << "Mounting vm_tools";

  grpc::ClientContext context;
  vm_tools::MountRequest request;
  vm_tools::MountResponse response;

  request.mutable_source()->assign("/dev/vdb");
  request.mutable_target()->assign("/opt/google/cros-containers");
  request.mutable_fstype()->assign("ext4");
  request.mutable_options()->assign("");
  request.set_mountflags(MS_RDONLY);

  {
    TRACE_DURATION("linux_runner", "MountRPC");
    auto grpc_status = maitred_->Mount(&context, request, &response);
    FX_CHECK(grpc_status.ok()) << "Failed to mount vm_tools partition: "
                               << grpc_status.error_message();
  }
  FX_LOGS(INFO) << "Mounted Filesystem: " << response.error();
}

void Guest::MountExtrasPartition() {
  TRACE_DURATION("linux_runner", "Guest::MountExtrasPartition");
  FX_CHECK(maitred_) << "Called MountExtrasPartition without a maitre'd connection";
  FX_LOGS(INFO) << "Mounting Extras Partition";

  grpc::ClientContext context;
  vm_tools::MountRequest request;
  vm_tools::MountResponse response;

  request.mutable_source()->assign("/dev/vdd");
  request.mutable_target()->assign("/mnt/shared");
  request.mutable_fstype()->assign("romfs");
  request.mutable_options()->assign("");
  request.set_mountflags(0);

  {
    TRACE_DURATION("linux_runner", "MountRPC");
    auto grpc_status = maitred_->Mount(&context, request, &response);
    FX_CHECK(grpc_status.ok()) << "Failed to mount extras filesystem: "
                               << grpc_status.error_message();
  }
  FX_LOGS(INFO) << "Mounted Filesystem: " << response.error();
}

void Guest::ConfigureNetwork() {
  TRACE_DURATION("linux_runner", "Guest::ConfigureNetwork");
  FX_CHECK(maitred_) << "Called ConfigureNetwork without a maitre'd connection";

  FX_LOGS(INFO) << "Configuring Guest Network...";

  // Perform basic network bring up.
  //
  // To bring up the network, maitre'd requires an IPv4 address to use for the
  // guest's external NIC (even though we are going to replace it with
  // a DHCP-acquired address in just a moment).
  //
  // We use an RFC-6598 (carrier-grade NAT) IP address distinct from the LXD
  // subnet, but expect it to be overridden by DHCP later.
  MaitredBringUpNetwork(*maitred_,
                        /*address=*/Ipv4Addr(100, 64, 1, 1),      // 100.64.1.1, RFC-6598 address
                        /*gateway=*/Ipv4Addr(100, 64, 1, 2),      // 100.64.1.2, RFC-6598 address
                        /*netmask=*/Ipv4Addr(255, 255, 255, 252)  // 30-bit netmask
  );

  // Remove the configured IPv4 address from eth0.
  MaitredRunCommandSync(*maitred_, /*args=*/{"/bin/ip", "address", "flush", "eth0"}, /*env=*/{});

  // Run dhclient.
  MaitredStartDaemon(*maitred_,
                     /*args=*/
                     {
                         "/sbin/dhclient",
                         // Lease file
                         "-lf",
                         "/run/dhclient.leases",
                         // PID file
                         "-pf",
                         "/run/dhclient.pid",
                         // Do not detach, but remain in foreground so maitre'd can monitor.
                         "-d",
                         // Interface
                         "eth0",
                     },
                     /*env=*/{{"HOME", "/tmp"}, {"PATH", "/sbin:/bin"}});

  FX_LOGS(INFO) << "Network configured.";
}

void Guest::StartTermina() {
  TRACE_DURATION("linux_runner", "Guest::StartTermina");
  FX_CHECK(maitred_) << "Called StartTermina without a maitre'd connection";
  FX_LOGS(INFO) << "Starting Termina...";

  PostContainerStatus(fuchsia::virtualization::ContainerStatus::STARTING_VM);

  grpc::ClientContext context;
  vm_tools::StartTerminaRequest request;
  vm_tools::StartTerminaResponse response;
  std::string lxd_subnet = "100.115.92.1/24";
  request.mutable_lxd_ipv4_subnet()->swap(lxd_subnet);
  request.set_stateful_device("/dev/vdc");

  {
    TRACE_DURATION("linux_runner", "StartTerminaRPC");
    auto grpc_status = maitred_->StartTermina(&context, request, &response);
    FX_CHECK(grpc_status.ok()) << "Failed to start Termina: " << grpc_status.error_message();
  }
}

// This exposes a shell on /dev/hvc0 that can be used to interact with the
// VM.
void Guest::LaunchContainerShell() {
  FX_CHECK(maitred_) << "Called LaunchShell without a maitre'd connection";
  FX_LOGS(INFO) << "Launching container shell...";
  MaitredStartDaemon(
      *maitred_,
      {"/usr/bin/lxc", "exec", kContainerName, "--", "/bin/login", "-f", kDefaultContainerUser},
      {
          {"LXD_DIR", "/mnt/stateful/lxd"},
          {"LXD_CONF", "/mnt/stateful/lxd_conf"},
          {"LXD_UNPRIVILEGED_ONLY", "true"},
      });
}

void Guest::AddMagmaDeviceToContainer() {
  FX_CHECK(maitred_) << "Called AddMagma without a maitre'd connection";
  FX_LOGS(INFO) << "Adding magma device to container";
  MaitredRunCommandSync(*maitred_,
                        {"/usr/bin/lxc", "config", "device", "add", kContainerName, "magma0",
                         "unix-char", "source=/dev/magma0", "mode=0666"},
                        {
                            {"LXD_DIR", "/mnt/stateful/lxd"},
                            {"LXD_CONF", "/mnt/stateful/lxd_conf"},
                            {"LXD_UNPRIVILEGED_ONLY", "true"},
                        });
}

void Guest::SetupGPUDriversInContainer() {
  FX_CHECK(maitred_) << "Called SetupGPUDrivers without a maitre'd connection";
  FX_LOGS(INFO) << "Setup GPU drivers in container";
  MaitredRunCommandSync(
      *maitred_,
      {"/usr/bin/lxc", "exec", kContainerName, "--", "sh", "-c",
       "mkdir -p /usr/share/vulkan/icd.d; /usr/bin/update-alternatives --install "
       "/usr/share/vulkan/icd.d/10_magma_intel_icd.x86_64.json vulkan-icd "
       "/opt/google/cros-containers/share/vulkan/icd.d/intel_icd.x86_64.json 20; "
       "/usr/bin/update-alternatives --install "
       "/usr/share/vulkan/icd.d/10_magma_intel_icd.i686.json vulkan-icd32 "
       "/opt/google/cros-containers/share/vulkan/icd.d/intel_icd.i686.json 20; "
       "echo /opt/google/cros-containers/drivers/lib64=libc6 > /etc/ld.so.conf.d/cros.conf;"
       "echo /opt/google/cros-containers/drivers/lib32=libc6 >> /etc/ld.so.conf.d/cros.conf;"
       "/sbin/ldconfig; "},
      {
          {"LXD_DIR", "/mnt/stateful/lxd"},
          {"LXD_CONF", "/mnt/stateful/lxd_conf"},
          {"LXD_UNPRIVILEGED_ONLY", "true"},
      });
}

void Guest::CreateContainer() {
  TRACE_DURATION("linux_runner", "Guest::CreateContainer");
  FX_CHECK(tremplin_) << "CreateContainer called without a Tremplin connection";
  FX_LOGS(INFO) << "Creating Container...";

  grpc::ClientContext context;
  vm_tools::tremplin::CreateContainerRequest request;
  vm_tools::tremplin::CreateContainerResponse response;

  request.mutable_container_name()->assign(kContainerName);
  request.mutable_image_alias()->assign(kContainerImageAlias);
  request.mutable_image_server()->assign(kContainerImageServer);

  {
    TRACE_DURATION("linux_runner", "CreateContainerRPC");
    auto status = tremplin_->CreateContainer(&context, request, &response);
    FX_CHECK(status.ok()) << "Failed to create container: " << status.error_message();
  }
  switch (response.status()) {
    case vm_tools::tremplin::CreateContainerResponse::CREATING:
      break;
    case vm_tools::tremplin::CreateContainerResponse::EXISTS:
      FX_LOGS(INFO) << "Container already exists";
      StartContainer();
      break;
    case vm_tools::tremplin::CreateContainerResponse::FAILED:
      PostContainerFailure("Failed to create container: " + response.failure_reason());
      break;
    case vm_tools::tremplin::CreateContainerResponse::UNKNOWN:
    default:
      PostContainerFailure("Unknown status: " + std::to_string(response.status()));
      break;
  }
}

void Guest::StartContainer() {
  TRACE_DURATION("linux_runner", "Guest::StartContainer");
  FX_CHECK(tremplin_) << "StartContainer called without a Tremplin connection";
  FX_LOGS(INFO) << "Starting Container...";

  PostContainerStatus(fuchsia::virtualization::ContainerStatus::STARTING);

  grpc::ClientContext context;
  vm_tools::tremplin::StartContainerRequest request;
  vm_tools::tremplin::StartContainerResponse response;

  request.mutable_container_name()->assign(kContainerName);
  request.mutable_host_public_key()->assign("");
  request.mutable_container_private_key()->assign("");
  request.mutable_token()->assign("container_token");

  {
    TRACE_DURATION("linux_runner", "StartContainerRPC");
    auto status = tremplin_->StartContainer(&context, request, &response);
    FX_CHECK(status.ok()) << "Failed to start container: " << status.error_message();
  }

  switch (response.status()) {
    case vm_tools::tremplin::StartContainerResponse::RUNNING:
    case vm_tools::tremplin::StartContainerResponse::STARTED:
      FX_LOGS(INFO) << "Container started";
      break;
    case vm_tools::tremplin::StartContainerResponse::STARTING:
      FX_LOGS(INFO) << "Container starting";
      break;
    case vm_tools::tremplin::StartContainerResponse::FAILED:
      PostContainerFailure("Failed to start container: " + response.failure_reason());
      break;
    case vm_tools::tremplin::StartContainerResponse::UNKNOWN:
    default:
      PostContainerFailure("Unknown status: " + std::to_string(response.status()));
      break;
  }
}

void Guest::SetupUser() {
  FX_CHECK(tremplin_) << "SetupUser called without a Tremplin connection";
  FX_LOGS(INFO) << "Creating user '" << kDefaultContainerUser << "'...";

  grpc::ClientContext context;
  vm_tools::tremplin::SetUpUserRequest request;
  vm_tools::tremplin::SetUpUserResponse response;

  request.mutable_container_name()->assign(kContainerName);
  request.mutable_container_username()->assign(kDefaultContainerUser);
  {
    TRACE_DURATION("linux_runner", "SetUpUserRPC");
    auto status = tremplin_->SetUpUser(&context, request, &response);
    FX_CHECK(status.ok()) << "Failed to setup user '" << kDefaultContainerUser
                          << "': " << status.error_message();
  }

  switch (response.status()) {
    case vm_tools::tremplin::SetUpUserResponse::EXISTS:
    case vm_tools::tremplin::SetUpUserResponse::SUCCESS:
      FX_LOGS(INFO) << "User created.";
      StartContainer();
      break;
    case vm_tools::tremplin::SetUpUserResponse::FAILED:
      PostContainerFailure("Failed to create user: " + response.failure_reason());
      break;
    case vm_tools::tremplin::SetUpUserResponse::UNKNOWN:
    default:
      PostContainerFailure("Unknown status: " + std::to_string(response.status()));
      break;
  }
}

grpc::Status Guest::VmReady(grpc::ServerContext* context, const vm_tools::EmptyMessage* request,
                            vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::VmReady");
  TRACE_FLOW_END("linux_runner", "TerminaBoot", vm_ready_nonce_);
  FX_LOGS(INFO) << "VM Ready -- Connecting to Maitre'd...";
  auto start_maitred =
      [this](fpromise::result<std::unique_ptr<vm_tools::Maitred::Stub>, zx_status_t>& result) {
        FX_CHECK(result.is_ok()) << "Failed to connect to Maitre'd";
        this->maitred_ = std::move(result.value());
        MountVmTools();
        MountExtrasPartition();
        ConfigureNetwork();
        StartTermina();
      };
  auto task =
      NewGrpcVsockStub<vm_tools::Maitred>(socket_endpoint_, kMaitredPort).then(start_maitred);
  executor_.schedule_task(std::move(task));
  return grpc::Status::OK;
}

grpc::Status Guest::TremplinReady(grpc::ServerContext* context,
                                  const ::vm_tools::tremplin::TremplinStartupInfo* request,
                                  vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::TremplinReady");
  FX_LOGS(INFO) << "Tremplin Ready.";
  auto start_tremplin =
      [this](fpromise::result<std::unique_ptr<vm_tools::tremplin::Tremplin::Stub>, zx_status_t>&
                 result) mutable -> fpromise::result<> {
    FX_CHECK(result.is_ok()) << "Failed to connect to Tremplin";
    tremplin_ = std::move(result.value());
    CreateContainer();
    return fpromise::ok();
  };
  auto task = NewGrpcVsockStub<vm_tools::tremplin::Tremplin>(socket_endpoint_, kTremplinPort)
                  .then(start_tremplin);
  executor_.schedule_task(std::move(task));
  return grpc::Status::OK;
}

grpc::Status Guest::UpdateCreateStatus(grpc::ServerContext* context,
                                       const vm_tools::tremplin::ContainerCreationProgress* request,
                                       vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateCreateStatus");
  switch (request->status()) {
    case vm_tools::tremplin::ContainerCreationProgress::CREATED:
      FX_LOGS(INFO) << "Container created: " << request->container_name();
      SetupUser();
      break;
    case vm_tools::tremplin::ContainerCreationProgress::DOWNLOADING:
      PostContainerDownloadProgress(request->download_progress());
      FX_LOGS(INFO) << "Downloading " << request->container_name() << ": "
                    << request->download_progress() << "%";
      if (request->download_progress() >= 100) {
        PostContainerStatus(fuchsia::virtualization::ContainerStatus::EXTRACTING);
        FX_LOGS(INFO) << "Extracting " << request->container_name();
      }
      break;
    case vm_tools::tremplin::ContainerCreationProgress::DOWNLOAD_TIMED_OUT:
      PostContainerFailure("Download timed out");
      break;
    case vm_tools::tremplin::ContainerCreationProgress::CANCELLED:
      PostContainerFailure("Download cancelled");
      break;
    case vm_tools::tremplin::ContainerCreationProgress::FAILED:
      PostContainerFailure("Download failed: " + request->failure_reason());
      break;
    case vm_tools::tremplin::ContainerCreationProgress::UNKNOWN:
    default:
      PostContainerFailure("Unknown download status: " + std::to_string(request->status()));
      break;
  }
  return grpc::Status::OK;
}

grpc::Status Guest::UpdateDeletionStatus(
    ::grpc::ServerContext* context, const ::vm_tools::tremplin::ContainerDeletionProgress* request,
    ::vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateDeletionStatus");
  FX_LOGS(INFO) << "Update Deletion Status";
  return grpc::Status::OK;
}
grpc::Status Guest::UpdateStartStatus(::grpc::ServerContext* context,
                                      const ::vm_tools::tremplin::ContainerStartProgress* request,
                                      ::vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateStartStatus");
  FX_LOGS(INFO) << "Update Start Status";
  switch (request->status()) {
    case vm_tools::tremplin::ContainerStartProgress::STARTED:
      FX_LOGS(INFO) << "Container started";
      break;
    default:
      PostContainerFailure("Unknown start status: " + std::to_string(request->status()));
      break;
  }
  return grpc::Status::OK;
}
grpc::Status Guest::UpdateExportStatus(::grpc::ServerContext* context,
                                       const ::vm_tools::tremplin::ContainerExportProgress* request,
                                       ::vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateExportStatus");
  FX_LOGS(INFO) << "Update Export Status";
  return grpc::Status::OK;
}
grpc::Status Guest::UpdateImportStatus(::grpc::ServerContext* context,
                                       const ::vm_tools::tremplin::ContainerImportProgress* request,
                                       ::vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateImportStatus");
  FX_LOGS(INFO) << "Update Import Status";
  return grpc::Status::OK;
}
grpc::Status Guest::ContainerShutdown(::grpc::ServerContext* context,
                                      const ::vm_tools::tremplin::ContainerShutdownInfo* request,
                                      ::vm_tools::tremplin::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::ContainerShutdown");
  FX_LOGS(INFO) << "Container Shutdown";
  return grpc::Status::OK;
}

grpc::Status Guest::ContainerReady(grpc::ServerContext* context,
                                   const vm_tools::container::ContainerStartupInfo* request,
                                   vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::ContainerReady");

  // Add Magma GPU support to container.
  AddMagmaDeviceToContainer();
  SetupGPUDriversInContainer();

  // Start required user services.
  LaunchContainerShell();

  // Connect to Garcon service in the container.
  // TODO(tjdetwiler): validate token.
  auto garcon_port = request->garcon_port();
  FX_LOGS(INFO) << "Container Ready; Garcon listening on port " << garcon_port;
  auto start_garcon = [this](fpromise::result<std::unique_ptr<vm_tools::container::Garcon::Stub>,
                                              zx_status_t>& result) mutable -> fpromise::result<> {
    FX_CHECK(result.is_ok()) << "Failed to connect to Garcon";
    garcon_ = std::move(result.value());
    DumpContainerDebugInfo();

    // Container is now Ready.
    PostContainerStatus(fuchsia::virtualization::ContainerStatus::READY);

    return fpromise::ok();
  };
  auto task = NewGrpcVsockStub<vm_tools::container::Garcon>(socket_endpoint_, garcon_port)
                  .then(start_garcon);
  executor_.schedule_task(std::move(task));

  return grpc::Status::OK;
}

grpc::Status Guest::ContainerShutdown(grpc::ServerContext* context,
                                      const vm_tools::container::ContainerShutdownInfo* request,
                                      vm_tools::EmptyMessage* response) {
  FX_LOGS(INFO) << "Container Shutdown";
  return grpc::Status::OK;
}

grpc::Status Guest::UpdateApplicationList(
    grpc::ServerContext* context, const vm_tools::container::UpdateApplicationListRequest* request,
    vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateApplicationList");
  FX_LOGS(INFO) << "Update Application List";
  for (const auto& application : request->application()) {
    FX_LOGS(INFO) << "ID: " << application.desktop_file_id();
    const auto& name = application.name().values().begin();
    if (name != application.name().values().end()) {
      FX_LOGS(INFO) << "\tname:             " << name->value();
    }
    const auto& comment = application.comment().values().begin();
    if (comment != application.comment().values().end()) {
      FX_LOGS(INFO) << "\tcomment:          " << comment->value();
    }
    FX_LOGS(INFO) << "\tno_display:       " << application.no_display();
    FX_LOGS(INFO) << "\tstartup_wm_class: " << application.startup_wm_class();
    FX_LOGS(INFO) << "\tstartup_notify:   " << application.startup_notify();
    FX_LOGS(INFO) << "\tpackage_id:       " << application.package_id();
  }
  return grpc::Status::OK;
}

grpc::Status Guest::OpenUrl(grpc::ServerContext* context,
                            const vm_tools::container::OpenUrlRequest* request,
                            vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::OpenUrl");
  FX_LOGS(INFO) << "Open URL";
  return grpc::Status::OK;
}

grpc::Status Guest::InstallLinuxPackageProgress(
    grpc::ServerContext* context,
    const vm_tools::container::InstallLinuxPackageProgressInfo* request,
    vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::InstallLinuxPackageProgress");
  FX_LOGS(INFO) << "Install Linux Package Progress";
  return grpc::Status::OK;
}

grpc::Status Guest::UninstallPackageProgress(
    grpc::ServerContext* context, const vm_tools::container::UninstallPackageProgressInfo* request,
    vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UninstallPackageProgress");
  FX_LOGS(INFO) << "Uninstall Package Progress";
  return grpc::Status::OK;
}

grpc::Status Guest::OpenTerminal(grpc::ServerContext* context,
                                 const vm_tools::container::OpenTerminalRequest* request,
                                 vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::OpenTerminal");
  FX_LOGS(INFO) << "Open Terminal";
  return grpc::Status::OK;
}

grpc::Status Guest::UpdateMimeTypes(grpc::ServerContext* context,
                                    const vm_tools::container::UpdateMimeTypesRequest* request,
                                    vm_tools::EmptyMessage* response) {
  TRACE_DURATION("linux_runner", "Guest::UpdateMimeTypes");
  FX_LOGS(INFO) << "Update Mime Types";
  size_t i = 0;
  for (const auto& pair : request->mime_type_mappings()) {
    FX_LOGS(INFO) << "\t" << pair.first << ": " << pair.second;
    if (++i > 10) {
      FX_LOGS(INFO) << "\t..." << (request->mime_type_mappings_size() - i) << " more.";
      break;
    }
  }
  return grpc::Status::OK;
}

void Guest::DumpContainerDebugInfo() {
  FX_CHECK(garcon_) << "Called DumpContainerDebugInfo without a garcon connection";
  FX_LOGS(INFO) << "Dumping Container Debug Info...";

  grpc::ClientContext context;
  vm_tools::container::GetDebugInformationRequest request;
  vm_tools::container::GetDebugInformationResponse response;

  auto grpc_status = garcon_->GetDebugInformation(&context, request, &response);
  if (!grpc_status.ok()) {
    FX_LOGS(ERROR) << "Failed to read container debug information: " << grpc_status.error_message();
    return;
  }

  FX_LOGS(INFO) << "Container debug information:";
  FX_LOGS(INFO) << response.debug_information();
}

void Guest::PostContainerStatus(fuchsia::virtualization::ContainerStatus container_status) {
  callback_(GuestInfo{
      .cid = guest_cid_,
      .container_status = container_status,
  });
}

void Guest::PostContainerDownloadProgress(int32_t download_progress) {
  callback_(GuestInfo{
      .cid = guest_cid_,
      .container_status = fuchsia::virtualization::ContainerStatus::DOWNLOADING,
      .download_percent = download_progress,
  });
}

void Guest::PostContainerFailure(std::string failure_reason) {
  FX_LOGS(ERROR) << failure_reason;
  callback_(GuestInfo{
      .cid = guest_cid_,
      .container_status = fuchsia::virtualization::ContainerStatus::FAILED,
      .failure_reason = failure_reason,
  });
}

}  // namespace linux_runner
