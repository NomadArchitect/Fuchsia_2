// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_BLOCK_DEVICES_H_
#define SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_BLOCK_DEVICES_H_

#include <fuchsia/hardware/block/volume/cpp/fidl.h>
#include <fuchsia/virtualization/cpp/fidl.h>
#include <lib/fitx/result.h>
#include <lib/zx/status.h>
#include <zircon/hw/gpt.h>

#include <vector>

constexpr const char kGuestPartitionName[] = "guest";

constexpr std::array<uint8_t, fuchsia::hardware::block::partition::GUID_LENGTH>
    kGuestPartitionGuid = {
        0x9a, 0x17, 0x7d, 0x2d, 0x8b, 0x24, 0x4a, 0x4c,
        0x87, 0x11, 0x1f, 0x99, 0x05, 0xb7, 0x6e, 0xd1,
};

fitx::result<std::string, std::vector<fuchsia::virtualization::BlockSpec>> GetBlockDevices(
    size_t stateful_image_size);

void DropDevNamespace();

zx::status<> WipeStatefulPartition(size_t bytes_to_zero, uint8_t value = 0);

#endif  // SRC_VIRTUALIZATION_BIN_LINUX_RUNNER_BLOCK_DEVICES_H_
