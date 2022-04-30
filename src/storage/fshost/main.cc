// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <fidl/fuchsia.fshost/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <fuchsia/boot/c/fidl.h>
#include <fuchsia/ldsvc/c/fidl.h>
#include <getopt.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/namespace.h>
#include <lib/fdio/watcher.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <zircon/boot/image.h>
#include <zircon/dlfcn.h>
#include <zircon/processargs.h>
#include <zircon/status.h>

#include <fstream>
#include <iostream>
#include <memory>
#include <ostream>
#include <thread>

#include <fbl/unique_fd.h>
#include <fshost_config/config.h>
#include <ramdevice-client/ramdisk.h>
#include <zstd/zstd.h>

#include "block-watcher.h"
#include "config.h"
#include "fs-manager.h"
#include "metrics.h"
#include "src/lib/storage/vfs/cpp/remote_dir.h"
#include "src/lib/storage/vfs/cpp/service.h"

namespace fio = fuchsia_io;

namespace fshost {
namespace {

constexpr char kItemsPath[] = "/svc/" fuchsia_boot_Items_Name;

zx_status_t DecompressZstd(zx::vmo& input, uint64_t input_offset, size_t input_size,
                           zx::vmo& output, uint64_t output_offset, size_t output_size) {
  auto input_buffer = std::make_unique<uint8_t[]>(input_size);
  zx_status_t status = input.read(input_buffer.get(), input_offset, input_size);
  if (status != ZX_OK) {
    return status;
  }

  auto output_buffer = std::make_unique<uint8_t[]>(output_size);

  auto rc = ZSTD_decompress(output_buffer.get(), output_size, input_buffer.get(), input_size);
  if (ZSTD_isError(rc) || rc != output_size) {
    return ZX_ERR_IO_DATA_INTEGRITY;
  }

  return output.write(output_buffer.get(), output_offset, output_size);
}

// Get ramdisk from the boot items service.
zx_status_t get_ramdisk(zx::vmo* ramdisk_vmo) {
  zx::channel local, remote;
  zx_status_t status = zx::channel::create(0, &local, &remote);
  if (status != ZX_OK) {
    return status;
  }
  status = fdio_service_connect(kItemsPath, remote.release());
  if (status != ZX_OK) {
    return status;
  }
  uint32_t length;
  return fuchsia_boot_ItemsGet(local.get(), ZBI_TYPE_STORAGE_RAMDISK, 0,
                               ramdisk_vmo->reset_and_get_address(), &length);
}

int RamctlWatcher(void* arg) {
  zx_status_t status = wait_for_device("/dev/sys/platform/00:00:2d/ramctl", ZX_TIME_INFINITE);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to open /dev/sys/platform/00:00:2d/ramctl: " << strerror(errno);
    return -1;
  }

  zx::vmo ramdisk_vmo(static_cast<zx_handle_t>(reinterpret_cast<uintptr_t>(arg)));

  zbi_header_t header;
  status = ramdisk_vmo.read(&header, 0, sizeof(header));
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "cannot read ZBI_TYPE_STORAGE_RAMDISK item header: "
                   << zx_status_get_string(status);
    return -1;
  }
  if (!(header.flags & ZBI_FLAG_VERSION) || header.magic != ZBI_ITEM_MAGIC ||
      header.type != ZBI_TYPE_STORAGE_RAMDISK) {
    FX_LOGS(ERROR) << "invalid ZBI_TYPE_STORAGE_RAMDISK item header";
    return -1;
  }

  zx::vmo vmo;
  if (header.flags & ZBI_FLAG_STORAGE_COMPRESSED) {
    status = zx::vmo::create(header.extra, 0, &vmo);
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "cannot create VMO for uncompressed RAMDISK: "
                     << zx_status_get_string(status);
      return -1;
    }
    status = DecompressZstd(ramdisk_vmo, sizeof(zbi_header_t), header.length, vmo, 0, header.extra);
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "failed to decompress RAMDISK: " << zx_status_get_string(status);
      return -1;
    }
  } else {
    // TODO(fxbug.dev/34597): The old code ignored uncompressed items too, and
    // silently.  Really the protocol should be cleaned up so the VMO arrives
    // without the header in it and then it could just be used here directly
    // if uncompressed (or maybe bootsvc deals with decompression in the first
    // place so the uncompressed VMO is always what we get).
    FX_LOGS(ERROR) << "ignoring uncompressed RAMDISK item in ZBI";
    return -1;
  }

  ramdisk_client* client;
  status = ramdisk_create_from_vmo(vmo.release(), &client);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to create ramdisk from ZBI_TYPE_STORAGE_RAMDISK";
  } else {
    FX_LOGS(INFO) << "ZBI_TYPE_STORAGE_RAMDISK attached";
  }
  return 0;
}

// Initialize the fshost namespace.
//
// |fs_root_client| is mapped to "/fs", and represents the filesystem of devmgr.
zx_status_t BindNamespace(fidl::ClientEnd<fio::Directory> fs_root_client) {
  fdio_ns_t* ns;
  zx_status_t status;
  if ((status = fdio_ns_get_installed(&ns)) != ZX_OK) {
    FX_LOGS(ERROR) << "cannot get namespace: " << status;
    return status;
  }

  // Bind "/fs".
  if ((status = fdio_ns_bind(ns, "/fs", fs_root_client.TakeChannel().release())) != ZX_OK) {
    FX_LOGS(ERROR) << "cannot bind /fs to namespace: " << status;
    return status;
  }
  return ZX_OK;
}

int Main(bool disable_block_watcher, bool ignore_component_config) {
  auto boot_args = FshostBootArgs::Create();
  auto config = DefaultConfig();
  if (!ignore_component_config) {
    config = fshost_config::Config::from_args();
  }
  ApplyBootArgsToConfig(config, *boot_args);

  FX_LOGS(INFO) << "Config: " << config;

  // Initialize the local filesystem in isolation.
  fidl::ServerEnd<fio::Directory> dir_request{
      zx::channel{zx_take_startup_handle(PA_DIRECTORY_REQUEST)}};
  fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> lifecycle_request{
      zx::channel{zx_take_startup_handle(PA_LIFECYCLE)}};

  auto metrics = DefaultMetrics();
  FsManager fs_manager(boot_args, std::move(metrics));

  if (config.netboot) {
    FX_LOGS(INFO) << "disabling automount";
  }

  BlockWatcher watcher(fs_manager, &config);

  zx_status_t status =
      fs_manager.Initialize(std::move(dir_request), std::move(lifecycle_request), config, watcher);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Cannot initialize FsManager: " << zx_status_get_string(status);
    return EXIT_FAILURE;
  }

  // Serve the root filesystems in our own namespace.
  zx::status fs_dir_or = fs_manager.GetFsDir();
  if (fs_dir_or.is_error()) {
    FX_PLOGS(ERROR, fs_dir_or.status_value()) << "Cannot serve root filesystems";
    return EXIT_FAILURE;
  }

  // Initialize namespace, and begin monitoring for a termination event.
  status = BindNamespace(std::move(*fs_dir_or));
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "cannot bind namespace";
    return EXIT_FAILURE;
  }

  fs_manager.ReadyForShutdown();

  // If there is a ramdisk, setup the ramctl filesystems.
  zx::vmo ramdisk_vmo;
  status = get_ramdisk(&ramdisk_vmo);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "failed to get ramdisk" << zx_status_get_string(status);
  } else if (ramdisk_vmo.is_valid()) {
    thrd_t t;

    int err = thrd_create_with_name(
        &t, &RamctlWatcher, reinterpret_cast<void*>(static_cast<uintptr_t>(ramdisk_vmo.release())),
        "ramctl-filesystems");
    if (err != thrd_success) {
      FX_LOGS(ERROR) << "failed to start ramctl-filesystems: " << err;
    }
    thrd_detach(t);
  }

  status = watcher.mounter().MaybeInitCryptClient().status_value();
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "cannot init crypt client";
    return EXIT_FAILURE;
  }

  if (disable_block_watcher) {
    FX_LOGS(INFO) << "block-watcher disabled";
  } else {
    watcher.Run();
  }

  fs_manager.WaitForShutdown();
  FX_LOGS(INFO) << "terminating";
  return EXIT_SUCCESS;
}

}  // namespace
}  // namespace fshost

int main(int argc, char** argv) {
  int disable_block_watcher = false;
  int ignore_component_config = false;
  option options[] = {
      {"disable-block-watcher", no_argument, &disable_block_watcher, true},
      // TODO(https://fxbug.dev/95600) delete, needed for isolated_devmgr to launch as a bare binary
      {"ignore-component-config", no_argument, &ignore_component_config, true},
  };
  while (getopt_long(argc, argv, "", options, nullptr) != -1) {
  }

  return fshost::Main(disable_block_watcher, ignore_component_config);
}
