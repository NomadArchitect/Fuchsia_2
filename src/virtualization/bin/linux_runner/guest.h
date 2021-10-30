// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_GUEST_H_
#define SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_GUEST_H_

#include <fuchsia/virtualization/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/trace/event.h>
#include <lib/virtualization/scenic_wayland_dispatcher.h>
#include <zircon/types.h>

#include <deque>
#include <memory>

#include "src/virtualization/bin/linux_runner/crash_listener.h"
#include "src/virtualization/bin/linux_runner/linux_component.h"
#include "src/virtualization/bin/linux_runner/log_collector.h"
#include "src/virtualization/lib/grpc/grpc_vsock_server.h"
#include "src/virtualization/third_party/vm_tools/container_guest.grpc.pb.h"
#include "src/virtualization/third_party/vm_tools/container_host.grpc.pb.h"
#include "src/virtualization/third_party/vm_tools/tremplin.grpc.pb.h"
#include "src/virtualization/third_party/vm_tools/vm_guest.grpc.pb.h"

#include <grpc++/grpc++.h>

namespace linux_runner {

struct AppLaunchRequest {
 public:
  AppLaunchRequest(const AppLaunchRequest&) = delete;
  AppLaunchRequest& operator=(const AppLaunchRequest&) = delete;

  AppLaunchRequest(AppLaunchRequest&&) = default;
  AppLaunchRequest& operator=(AppLaunchRequest&&) = default;

  fuchsia::sys::Package application;
  fuchsia::sys::StartupInfo startup_info;
  fidl::InterfaceRequest<fuchsia::sys::ComponentController> controller_request;
};

struct GuestConfig {
  size_t stateful_image_size;
};

class Guest : public vm_tools::StartupListener::Service,
              public vm_tools::tremplin::TremplinListener::Service,
              public vm_tools::container::ContainerListener::Service {
 public:
  // Creates a new |Guest|
  static zx_status_t CreateAndStart(sys::ComponentContext* context, GuestConfig config,
                                    std::unique_ptr<Guest>* guest);

  Guest(sys::ComponentContext* context, GuestConfig config, fuchsia::virtualization::RealmPtr env);
  ~Guest();

  void Launch(AppLaunchRequest request);

 private:
  fpromise::promise<> Start();
  fpromise::promise<std::unique_ptr<GrpcVsockServer>, zx_status_t> StartGrpcServer();
  void StartGuest();
  void MountExtrasPartition();
  void MountVmTools();
  void ConfigureNetwork();
  void StartTermina();
  void LaunchContainerShell();
  void AddMagmaDeviceToContainer();
  void CreateContainer();
  void StartContainer();
  void SetupUser();
  void DumpContainerDebugInfo();

  // |vm_tools::StartupListener::Service|
  grpc::Status VmReady(grpc::ServerContext* context, const vm_tools::EmptyMessage* request,
                       vm_tools::EmptyMessage* response) override;

  // |vm_tools::tremplin::TremplinListener::Service|
  grpc::Status TremplinReady(grpc::ServerContext* context,
                             const ::vm_tools::tremplin::TremplinStartupInfo* request,
                             vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status UpdateCreateStatus(grpc::ServerContext* context,
                                  const vm_tools::tremplin::ContainerCreationProgress* request,
                                  vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status UpdateDeletionStatus(::grpc::ServerContext* context,
                                    const ::vm_tools::tremplin::ContainerDeletionProgress* request,
                                    ::vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status UpdateStartStatus(::grpc::ServerContext* context,
                                 const ::vm_tools::tremplin::ContainerStartProgress* request,
                                 ::vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status UpdateExportStatus(::grpc::ServerContext* context,
                                  const ::vm_tools::tremplin::ContainerExportProgress* request,
                                  ::vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status UpdateImportStatus(::grpc::ServerContext* context,
                                  const ::vm_tools::tremplin::ContainerImportProgress* request,
                                  ::vm_tools::tremplin::EmptyMessage* response) override;
  grpc::Status ContainerShutdown(::grpc::ServerContext* context,
                                 const ::vm_tools::tremplin::ContainerShutdownInfo* request,
                                 ::vm_tools::tremplin::EmptyMessage* response) override;

  // |vm_tools::container::ContainerListener::Service|
  grpc::Status ContainerReady(grpc::ServerContext* context,
                              const vm_tools::container::ContainerStartupInfo* request,
                              vm_tools::EmptyMessage* response) override;
  grpc::Status ContainerShutdown(grpc::ServerContext* context,
                                 const vm_tools::container::ContainerShutdownInfo* request,
                                 vm_tools::EmptyMessage* response) override;
  grpc::Status UpdateApplicationList(
      grpc::ServerContext* context,
      const vm_tools::container::UpdateApplicationListRequest* request,
      vm_tools::EmptyMessage* response) override;
  grpc::Status OpenUrl(grpc::ServerContext* context,
                       const vm_tools::container::OpenUrlRequest* request,
                       vm_tools::EmptyMessage* response) override;
  grpc::Status InstallLinuxPackageProgress(
      grpc::ServerContext* context,
      const vm_tools::container::InstallLinuxPackageProgressInfo* request,
      vm_tools::EmptyMessage* response) override;
  grpc::Status UninstallPackageProgress(
      grpc::ServerContext* context,
      const vm_tools::container::UninstallPackageProgressInfo* request,
      vm_tools::EmptyMessage* response) override;
  grpc::Status OpenTerminal(grpc::ServerContext* context,
                            const vm_tools::container::OpenTerminalRequest* request,
                            vm_tools::EmptyMessage* response) override;
  grpc::Status UpdateMimeTypes(grpc::ServerContext* context,
                               const vm_tools::container::UpdateMimeTypesRequest* request,
                               vm_tools::EmptyMessage* response) override;

  void LaunchApplication(AppLaunchRequest request);
  void OnNewView(fidl::InterfaceHandle<fuchsia::ui::app::ViewProvider> view, uint32_t id);
  void OnShutdownView(uint32_t id);
  void CreateComponent(AppLaunchRequest request,
                       fidl::InterfaceHandle<fuchsia::ui::app::ViewProvider> view, uint32_t id);
  void OnComponentTerminated(uint32_t id);
  void CreateTerminalComponent(AppLaunchRequest app);

  async_dispatcher_t* async_;
  async::Executor executor_;
  GuestConfig config_;
  std::unique_ptr<GrpcVsockServer> grpc_server_;
  fuchsia::virtualization::HostVsockEndpointPtr socket_endpoint_;
  fuchsia::virtualization::RealmPtr guest_env_;
  fuchsia::virtualization::GuestPtr guest_controller_;
  uint32_t guest_cid_ = 0;
  std::unique_ptr<vm_tools::Maitred::Stub> maitred_;
  std::unique_ptr<vm_tools::tremplin::Tremplin::Stub> tremplin_;
  std::unique_ptr<vm_tools::container::Garcon::Stub> garcon_;
  CrashListener crash_listener_;
  LogCollector log_collector_;
  guest::ScenicWaylandDispatcher wayland_dispatcher_;
  // Requests queued up waiting for the guest to fully boot.
  std::deque<AppLaunchRequest> pending_requests_;
  // Requests that have been dispatched to the container, but have not yet been
  // associated with a wayland ViewProvider.
  std::deque<AppLaunchRequest> dispatched_requests_;
  // Views launched in the background (ex: not using garcon). These can be
  // returned by requesting a null app URI (linux://).
  using BackgroundView = std::pair<uint32_t, fidl::InterfaceHandle<fuchsia::ui::app::ViewProvider>>;
  std::deque<BackgroundView> background_views_;
  std::unordered_map<uint32_t, std::unique_ptr<LinuxComponent>> components_;
  std::unordered_map<uint32_t, std::unique_ptr<LinuxComponent>> terminals_;
  fuchsia::sys::LauncherPtr launcher_;

  // A flow ID used to track the time from the time the VM is created until
  // the time the guest has reported itself as ready via the VmReady RPC in the
  // vm_tools::StartupListener::Service.
  const trace_async_id_t vm_ready_nonce_ = TRACE_NONCE();
};
}  // namespace linux_runner

#endif  // SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_GUEST_H_
