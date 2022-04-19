// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fshost/config.h"

namespace fshost {

fshost_config::Config DefaultConfig() {
  auto config = EmptyConfig();
  config.blobfs = true;
  config.bootpart = true;
  config.check_filesystems = true;
  config.fvm = true;
  config.gpt = true;
  config.data = true;
  config.format_data_on_corruption = true;
  config.allow_legacy_data_partition_names = false;
  return config;
}

fshost_config::Config EmptyConfig() {
  fshost_config::Config config{
      .allow_legacy_data_partition_names = false,
      .apply_limits_to_ramdisk = false,
      .blobfs = false,
      .blobfs_max_bytes = 0,
      .bootpart = false,
      .check_filesystems = false,
      .data = false,
      .data_filesystem_binary_path = "",
      .data_max_bytes = 0,
      .durable = false,
      .factory = false,
      .format_data_on_corruption = false,
      .fs_switch = false,
      .fvm = false,
      .fvm_ramdisk = false,
      .gpt = false,
      .gpt_all = false,
      .mbr = false,
      .nand = false,
      .netboot = false,
      .no_zxcrypt = false,
      .sandbox_decompression = false,
      .zxcrypt_non_ramdisk = false,
  };
  return config;
}

void ApplyBootArgsToConfig(fshost_config::Config& config, const FshostBootArgs& boot_args) {
  if (boot_args.netboot()) {
    config.netboot = true;
  }
  if (boot_args.check_filesystems()) {
    config.check_filesystems = true;
  }
}

}  // namespace fshost
