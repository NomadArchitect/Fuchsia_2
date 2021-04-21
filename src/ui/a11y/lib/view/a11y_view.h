// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_A11Y_LIB_VIEW_A11Y_VIEW_H_
#define SRC_UI_A11Y_LIB_VIEW_A11Y_VIEW_H_

#include <fuchsia/ui/accessibility/view/cpp/fidl.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/ui/scenic/cpp/commands.h>
#include <lib/ui/scenic/cpp/resources.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>

#include <memory>
#include <optional>

namespace a11y {

// The AccessibilityView class represents the accessibility-owned view
// directly below the root view in the scene graph.
//
// This view is used to vend capabilities to the accessibility manager
// that a view confers, e.g. ability to request focus, consume and
// respond to input events, annotate underlying views, and apply
// coordinate transforms to its subtree.
class AccessibilityView {
 public:
  AccessibilityView(sys::ComponentContext* component_context,
                    fuchsia::ui::scenic::ScenicPtr scenic);
  ~AccessibilityView() = default;

 private:
  void OnScenicEvent(std::vector<fuchsia::ui::scenic::Event> events);

  // Connection to scenic.
  fuchsia::ui::scenic::ScenicPtr scenic_;

  // Scenic session interface.
  scenic::Session session_;

  // Resources below must be declared after scenic session, because they
  // must be destroyed before the session is destroyed.

  // Holds the a11y view resource.
  // If not present, this view does not exist in the view tree.
  std::optional<scenic::View> a11y_view_;

  // Holds the client view holder.
  // If not present, this view does not exist in the view tree.
  std::optional<scenic::ViewHolder> client_view_holder_;

  // Interface between the accessibility view and the scenic service
  // that inserts it into the scene graph.
  fuchsia::ui::accessibility::view::RegistryPtr accessibility_view_registry_;
};

}  // namespace a11y

#endif  // SRC_UI_A11Y_LIB_VIEW_A11Y_VIEW_H_
