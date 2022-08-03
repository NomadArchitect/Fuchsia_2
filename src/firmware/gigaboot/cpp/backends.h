// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_FIRMWARE_GIGABOOT_CPP_BACKENDS_H_
#define SRC_FIRMWARE_GIGABOOT_CPP_BACKENDS_H_

namespace gigaboot {

enum class RebootMode {
  kBootloader,
  kNormal,
  kRecovery,
};

// Set reboot mode. Returns true if succeeds, false otherwise.
bool SetRebootMode(RebootMode mode);

// Get reboot mode.
RebootMode GetRebootMode();

// Adds verified boot backends.

}  // namespace gigaboot

#endif  // SRC_FIRMWARE_GIGABOOT_CPP_BACKENDS_H_
