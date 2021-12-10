// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_FACTORY_FACTORYFS_MOUNT_H_
#define SRC_STORAGE_FACTORY_FACTORYFS_MOUNT_H_

#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/default.h>

#include <block-client/cpp/block-device.h>
#include <fbl/function.h>

namespace factoryfs {

using block_client::BlockDevice;

#define FS_HANDLE_DIAGNOSTICS_DIR PA_HND(PA_USER0, 2)

// Determines the kind of directory layout the filesystem server should expose to the outside world.
// TODO(fxbug.dev/34531): When all users migrate to the export directory, delete this enum, since
// only |kExportDirectory| would be used.
enum class ServeLayout {
  // The root of the filesystem is exposed directly
  kDataRootOnly,

  // Expose a pseudo-directory with the filesystem root located at "/root".
  // TODO(fxbug.dev/34531): Also expose an administration service under "/svc/fuchsia.fs.Admin".
  kExportDirectory
};

// Toggles that may be set on factoryfs during initialization.
struct MountOptions {
  bool verbose = false;
  bool metrics = false;  // TODO(manalib)
};

// Begins serving requests to the filesystem by parsing the on-disk format using |device|. If
// |ServeLayout| is |kDataRootOnly|, |root| serves the root of the filesystem. If it's
// |kExportDirectory|, |root| serves an outgoing directory.
//
// This function blocks until the filesystem terminates.
zx_status_t Mount(std::unique_ptr<BlockDevice> device, MountOptions* options,
                  fidl::ServerEnd<fuchsia_io::Directory> root, ServeLayout layout);

}  // namespace factoryfs

#endif  // SRC_STORAGE_FACTORY_FACTORYFS_MOUNT_H_
