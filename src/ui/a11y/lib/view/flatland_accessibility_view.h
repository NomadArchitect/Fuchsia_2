// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_A11Y_LIB_VIEW_FLATLAND_ACCESSIBILITY_VIEW_H_
#define SRC_UI_A11Y_LIB_VIEW_FLATLAND_ACCESSIBILITY_VIEW_H_

#include <fuchsia/accessibility/scene/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/ui/scenic/cpp/commands.h>
#include <lib/ui/scenic/cpp/resources.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>

#include <memory>
#include <optional>

#include "src/ui/a11y/lib/view/accessibility_view.h"

namespace a11y {

// Implements the AccessibilityViewInterface using the flatland graphics
// composition API.
class FlatlandAccessibilityView : public AccessibilityViewInterface,
                                  public fuchsia::accessibility::scene::Provider {
 public:
  explicit FlatlandAccessibilityView(fuchsia::ui::composition::FlatlandPtr flatland);
  ~FlatlandAccessibilityView() override = default;

  // |AccessibilityViewInterface|
  void add_view_properties_changed_callback(ViewPropertiesChangedCallback callback) override;

  // |AccessibilityViewInterface|
  std::optional<fuchsia::ui::views::ViewRef> view_ref() override;

  // |AccessibilityViewInterface|
  void add_scene_ready_callback(SceneReadyCallback callback) override;

  // |AccessibilityViewInterface|
  void RequestFocus(fuchsia::ui::views::ViewRef view_ref, RequestFocusCallback callback) override;

  // |fuchsia::accessibility::scene::Provider|
  void CreateView(fuchsia::ui::views::ViewCreationToken a11y_view_token,
                  fuchsia::ui::views::ViewportCreationToken proxy_viewport_token) override;

  fidl::InterfaceRequestHandler<fuchsia::accessibility::scene::Provider> GetHandler();

 private:
  // Interface for the a11y view's flatland instance.
  fuchsia::ui::composition::FlatlandPtr flatland_;

  // Scenic focuser used to request focus chain updates in the a11y view's subtree.
  fuchsia::ui::views::FocuserPtr focuser_;

  // Used to retrieve a11y view layout info. These should not change over the
  // lifetime of the view.
  fuchsia::ui::composition::ParentViewportWatcherPtr parent_watcher_;

  // True if the a11y view has been attached to the scene.
  bool is_initialized_ = false;

  // Holds a copy of the view ref of the a11y view.
  // If not present, the a11y view has not yet been connected to the scene.
  std::optional<fuchsia::ui::views::ViewRef> view_ref_;

  // Layout info for the a11y view. If std::nullopt, then layout info has not yet
  // been received.
  std::optional<fuchsia::ui::composition::LayoutInfo> layout_info_;

  // If set, gets invoked whenever the view properties for the a11y view change.
  std::vector<ViewPropertiesChangedCallback> view_properties_changed_callbacks_;

  // If set, gets invoked when the scene becomes ready.
  std::vector<SceneReadyCallback> scene_ready_callbacks_;

  fidl::BindingSet<fuchsia::accessibility::scene::Provider> view_bindings_;
};

}  // namespace a11y

#endif  // SRC_UI_A11Y_LIB_VIEW_FLATLAND_ACCESSIBILITY_VIEW_H_
