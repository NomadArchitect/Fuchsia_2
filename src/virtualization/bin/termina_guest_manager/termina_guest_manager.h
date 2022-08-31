// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_TERMINA_GUEST_MANAGER_TERMINA_GUEST_MANAGER_H_
#define SRC_VIRTUALIZATION_BIN_TERMINA_GUEST_MANAGER_TERMINA_GUEST_MANAGER_H_

#include <fuchsia/sys/cpp/fidl.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>

#include "src/virtualization/bin/guest_manager/guest_manager.h"
#include "src/virtualization/bin/termina_guest_manager/guest.h"
#include "src/virtualization/bin/termina_guest_manager/termina_config.h"

namespace termina_guest_manager {

class TerminaGuestManager : GuestManager, public fuchsia::virtualization::LinuxManager {
 public:
  explicit TerminaGuestManager(async_dispatcher_t* dispatcher);
  TerminaGuestManager(async_dispatcher_t* dispatcher,
                      std::unique_ptr<sys::ComponentContext> context);

  zx_status_t Init();

  TerminaGuestManager(const TerminaGuestManager&) = delete;
  TerminaGuestManager& operator=(const TerminaGuestManager&) = delete;

 private:
  // |fuchsia::virtualization::LinuxManager|
  void StartAndGetLinuxGuestInfo(std::string label,
                                 StartAndGetLinuxGuestInfoCallback callback) override;
  void WipeData(WipeDataCallback callback) override;

  void OnGuestInfoChanged(GuestInfo info);

  std::unique_ptr<sys::ComponentContext> context_;
  termina_config::Config structured_config_;
  fidl::BindingSet<fuchsia::virtualization::LinuxManager> manager_bindings_;
  std::unique_ptr<termina_guest_manager::Guest> guest_;
  std::deque<StartAndGetLinuxGuestInfoCallback> callbacks_;
  std::optional<GuestInfo> info_;
};

}  // namespace termina_guest_manager

#endif  // SRC_VIRTUALIZATION_BIN_TERMINA_GUEST_MANAGER_TERMINA_GUEST_MANAGER_H_
