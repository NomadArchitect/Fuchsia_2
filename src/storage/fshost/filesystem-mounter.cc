// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "filesystem-mounter.h"

#include <fuchsia/io/llcpp/fidl.h>
#include <fuchsia/update/verify/llcpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/inspect/service/cpp/service.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/process.h>
#include <zircon/status.h>

#include <fbl/ref_ptr.h>

#include "fdio.h"
#include "fshost-fs-provider.h"
#include "pkgfs-launcher.h"
#include "src/storage/blobfs/mount.h"
#include "src/storage/minfs/minfs.h"

namespace devmgr {

namespace fio = fuchsia_io;

zx_status_t FilesystemMounter::LaunchFs(int argc, const char** argv, zx_handle_t* hnd,
                                        uint32_t* ids, size_t len, uint32_t fs_flags) {
  FshostFsProvider fs_provider;
  DevmgrLauncher launcher(&fs_provider);
  return launcher.Launch(*zx::job::default_job(), argv[0], argv, nullptr, -1,
                         /* TODO(fxbug.dev/32044) */ zx::resource(), hnd, ids, len, nullptr,
                         fs_flags);
}

zx::status<zx::channel> FilesystemMounter::MountFilesystem(FsManager::MountPoint point,
                                                           const char* binary,
                                                           const mount_options_t& options,
                                                           zx::channel block_device_client,
                                                           uint32_t fs_flags) {
  zx::status create_endpoints = fidl::CreateEndpoints<fio::Node>();
  if (create_endpoints.is_error()) {
    return create_endpoints.take_error();
  }
  auto [client_end, server_end] = std::move(create_endpoints.value());

  constexpr size_t num_handles = 2;
  zx_handle_t handles[num_handles] = {server_end.TakeChannel().release(),
                                      block_device_client.release()};
  uint32_t ids[num_handles] = {PA_DIRECTORY_REQUEST, FS_HANDLE_BLOCK_DEVICE_ID};

  fbl::Vector<const char*> argv;
  argv.push_back(binary);
  if (options.readonly) {
    argv.push_back("--readonly");
  }
  if (options.verbose_mount) {
    argv.push_back("--verbose");
  }
  if (options.collect_metrics) {
    argv.push_back("--metrics");
  }
  if (options.write_compression_algorithm != nullptr) {
    argv.push_back("--compression");
    argv.push_back(options.write_compression_algorithm);
  }
  if (options.sandbox_decompression) {
    argv.push_back("--sandbox_decompression");
  }
  if (options.cache_eviction_policy != nullptr) {
    argv.push_back("--eviction_policy");
    argv.push_back(options.cache_eviction_policy);
  }
  argv.push_back("mount");
  argv.push_back(nullptr);
  zx_status_t status =
      LaunchFs(static_cast<int>(argv.size() - 1), argv.data(), handles, ids, num_handles, fs_flags);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  auto result = fidl::WireCall(client_end).Describe();
  if (!result.ok()) {
    return zx::error(result.status());
  }

  zx::channel root;
  status = fs_root_handle(client_end.channel().get(), root.reset_and_get_address());
  if (status != ZX_OK) {
    return zx::error(status);
  }
  status = InstallFs(point, std::move(root));
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return zx::ok(client_end.TakeChannel());
}

zx_status_t FilesystemMounter::MountData(zx::channel block_device, const mount_options_t& options) {
  if (data_mounted_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  zx::status ret = MountFilesystem(FsManager::MountPoint::kData, "/pkg/bin/minfs", options,
                                   std::move(block_device), FS_SVC);
  if (ret.is_error()) {
    return ret.error_value();
  }

  data_mounted_ = true;
  return ZX_OK;
}

zx_status_t FilesystemMounter::MountInstall(zx::channel block_device,
                                            const mount_options_t& options) {
  if (install_mounted_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  zx::status ret = MountFilesystem(FsManager::MountPoint::kInstall, "/pkg/bin/minfs", options,
                                   std::move(block_device), FS_SVC);
  if (ret.is_error()) {
    return ret.error_value();
  }

  install_mounted_ = true;
  return ZX_OK;
}

zx_status_t FilesystemMounter::MountFactoryFs(zx::channel block_device,
                                              const mount_options_t& options) {
  if (factory_mounted_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  zx::status ret = MountFilesystem(FsManager::MountPoint::kFactory, "/pkg/bin/factoryfs", options,
                                   std::move(block_device), FS_SVC);
  if (ret.is_error()) {
    return ret.error_value();
  }

  factory_mounted_ = true;
  return ZX_OK;
}

zx_status_t FilesystemMounter::MountDurable(zx::channel block_device,
                                            const mount_options_t& options) {
  if (durable_mounted_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  zx::status ret = MountFilesystem(FsManager::MountPoint::kDurable, "/pkg/bin/minfs", options,
                                   std::move(block_device), FS_SVC);
  if (ret.is_error()) {
    return ret.error_value();
  }

  durable_mounted_ = true;
  return ZX_OK;
}

zx_status_t FilesystemMounter::MountBlob(zx::channel block_device, const mount_options_t& options) {
  if (blob_mounted_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  zx::channel fs_diagnostics_dir_client, fs_diagnostics_dir_server;
  zx_status_t status =
      zx::channel::create(0, &fs_diagnostics_dir_client, &fs_diagnostics_dir_server);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to create channel for diagnostics dir: "
                   << zx_status_get_string(status);
    return status;
  }

  zx::status ret = MountFilesystem(FsManager::MountPoint::kBlob, "/pkg/bin/blobfs", options,
                                   std::move(block_device), FS_SVC | FS_SVC_BLOBFS);
  if (ret.is_error()) {
    return ret.error_value();
  }
  status = fshost_.SetFsExportRoot(FsManager::MountPoint::kBlob, std::move(ret).value());
  if (status != ZX_OK) {
    return status;
  }

  status = fshost_.ForwardFsDiagnosticsDirectory(FsManager::MountPoint::kBlob, "blobfs");
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to add diagnostic directory for blobfs: "
                   << zx_status_get_string(status);
  }
  status = fshost_.ForwardFsService(FsManager::MountPoint::kBlob,
                                    fuchsia_update_verify::BlobfsVerifier::Name);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to forward BlobfsVerifier service for blobfs: "
                   << zx_status_get_string(status);
  }

  blob_mounted_ = true;
  return ZX_OK;
}

void FilesystemMounter::TryMountPkgfs() {
  // Pkgfs waits for the following to mount before initializing:
  //   - Blobfs. Pkgfs is launched from blobfs, so this is a hard requirement.
  //   - Minfs. Pkgfs and other components want minfs to exist, so although they
  //   could launch and query for it later, this synchronization point means that
  //   subsequent clients will no longer need to query.
  //
  // TODO(fxbug.dev/38621): In the future, this mechanism may be replaced with a feed-forward
  // design to the mounted filesystems.
  if (!pkgfs_mounted_ && blob_mounted_ && (data_mounted_ || !WaitForData())) {
    // Historically we don't retry if pkgfs fails to launch, which seems reasonable since the
    // cause of a launch failure is unlikely to be transient.
    // TODO(fxbug.dev/58363): fshost should handle failures to mount critical filesystems better.
    auto status = LaunchPkgfs(this);
    if (status.is_error()) {
      FX_LOGS(ERROR) << "failed to launch pkgfs: " << status.status_string();
    }
    pkgfs_mounted_ = true;
  }
}

}  // namespace devmgr
