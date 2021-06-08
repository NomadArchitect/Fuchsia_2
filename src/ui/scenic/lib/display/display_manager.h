// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_MANAGER_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_MANAGER_H_

#include <fuchsia/hardware/display/cpp/fidl.h>
#include <lib/fit/function.h>

#include "src/lib/fxl/macros.h"
#include "src/ui/scenic/lib/display/display.h"
#include "src/ui/scenic/lib/display/display_controller_listener.h"

namespace scenic_impl {
namespace display {

// Discovers and owns the default display controller, and waits for and exposes the default display.
class DisplayManager {
 public:
  // |display_available_cb| is a one-shot callback that is triggered when the first display is
  // observed, and cleared immediately afterward.
  explicit DisplayManager(fit::closure display_available_cb);
  ~DisplayManager() = default;

  void BindDefaultDisplayController(
      fidl::InterfaceHandle<fuchsia::hardware::display::Controller> controller,
      zx::channel dc_device);

  // Gets information about the default display.
  // May return null if there isn't one.
  Display* default_display() const { return default_display_.get(); }

  // Only use this during Scenic initialization to pass a reference to FrameScheduler.
  std::shared_ptr<Display> default_display_shared() const { return default_display_; }

  std::shared_ptr<fuchsia::hardware::display::ControllerSyncPtr> default_display_controller() {
    return default_display_controller_;
  }

  std::shared_ptr<display::DisplayControllerListener> default_display_controller_listener() {
    return default_display_controller_listener_;
  }

  // For testing.
  void SetDefaultDisplayForTests(std::shared_ptr<Display> display) {
    default_display_ = std::move(display);
  }

  // TODO(fxbug.dev/76640): we may want to have multiple clients of this, so a single setter that
  // stomps previous callbacks may not be what we want.
  using VsyncCallback = fit::function<void(uint64_t display_id, zx::time timestamp,
                                           const std::vector<uint64_t>& image_ids)>;
  void SetVsyncCallback(VsyncCallback callback);

 private:
  VsyncCallback vsync_callback_;

  void OnDisplaysChanged(std::vector<fuchsia::hardware::display::Info> added,
                         std::vector<uint64_t> removed);
  void OnClientOwnershipChange(bool has_ownership);
  void OnVsync(uint64_t display_id, uint64_t timestamp, std::vector<uint64_t> image_ids,
               uint64_t cookie);

  std::shared_ptr<fuchsia::hardware::display::ControllerSyncPtr> default_display_controller_;
  std::shared_ptr<display::DisplayControllerListener> default_display_controller_listener_;

  std::shared_ptr<Display> default_display_;

  fit::closure display_available_cb_;
  // A boolean indicating whether or not we have ownership of the display
  // controller (not just individual displays). The default is no.
  bool owns_display_controller_ = false;

  FXL_DISALLOW_COPY_AND_ASSIGN(DisplayManager);
};

}  // namespace display
}  // namespace scenic_impl

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_MANAGER_H_
