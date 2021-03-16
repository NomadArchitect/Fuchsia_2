// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/hardware/platform/device/c/banjo.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

extern zx_status_t test_amlogic_video_bind(void* ctx, zx_device_t* parent);

static zx_driver_ops_t amlogic_video_driver_ops = {
    .version = DRIVER_OPS_VERSION, .init = NULL, .bind = test_amlogic_video_bind,
    // .release is not critical for this driver because dedicated devhost
    // process
};

// clang-format off
ZIRCON_DRIVER_BEGIN(amlogic_video, amlogic_video_driver_ops, "zircon", "0.1", 4)
// This driver will never be autobound at boot, but only when the test harness
// specifically asks for it to be bound.
    BI_ABORT_IF_AUTOBIND,
    BI_ABORT_IF(NE, BIND_COMPOSITE, 1),
    BI_ABORT_IF(NE, BIND_PLATFORM_DEV_VID, PDEV_VID_AMLOGIC),
    BI_MATCH_IF(EQ, BIND_PLATFORM_DEV_DID, PDEV_DID_AMLOGIC_VIDEO),
ZIRCON_DRIVER_END(amlogic_video);
// clang-format on
