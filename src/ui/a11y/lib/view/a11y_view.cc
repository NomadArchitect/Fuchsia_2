// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/a11y/lib/view/a11y_view.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/commands.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>
#include <zircon/status.h>

namespace a11y {
namespace {

void InvokeViewPropertiesChangedCallbacks(
    const fuchsia::ui::gfx::ViewProperties& properties,
    std::vector<AccessibilityView::ViewPropertiesChangedCallback>* callbacks) {
  auto it = callbacks->begin();
  while (it != callbacks->end()) {
    if ((*it)(properties)) {
      it++;
    } else {
      callbacks->erase(it);
    }
  }
}

void InvokeSceneReadyCallbacks(std::vector<AccessibilityView::SceneReadyCallback>* callbacks) {
  auto it = callbacks->begin();
  while (it != callbacks->end()) {
    if ((*it)()) {
      it++;
    } else {
      callbacks->erase(it);
    }
  }
}

}  // namespace

AccessibilityView::AccessibilityView(
    fuchsia::ui::accessibility::view::RegistryPtr accessibility_view_registry,
    fuchsia::ui::scenic::ScenicPtr scenic)
    : accessibility_view_registry_(std::move(accessibility_view_registry)),
      scenic_(std::move(scenic)) {
  // Set up scenic session endpoints.
  fuchsia::ui::scenic::SessionEndpoints endpoints;
  fuchsia::ui::scenic::SessionPtr session;
  endpoints.set_session(session.NewRequest());
  endpoints.set_view_focuser(focuser_.NewRequest());
  fidl::InterfaceHandle<fuchsia::ui::scenic::SessionListener> session_listener;

  // Wrap session for convenience and create valid session listener.
  // NOTE: We need access to the session and session listener handles to set up
  // the session endpoints (which are required to associate the focuser with the
  // a11y view's ViewRef). The scenic::Session wrapper class does not give us access to both
  // handles, so we need to instantiate the session and listener separately, and then transfer
  // ownership to the session wrapper. Since the scenic::Session wrapper class does not have
  // a default constructor, we need to hold it in a nullable type like unique_ptr.
  session_ = std::make_unique<scenic::Session>(std::move(session), session_listener.NewRequest());

  // Add session listener to endpoints.
  endpoints.set_session_listener(session_listener.Bind());

  // Create scenic session.
  scenic_->CreateSessionT(std::move(endpoints), /* unused */ [] {});

  // Set up session listener event handler.
  session_->set_event_handler(
      [this](std::vector<fuchsia::ui::scenic::Event> events) { OnScenicEvent(std::move(events)); });

  accessibility_view_registry_.set_error_handler([](zx_status_t status) {
    FX_LOGS(ERROR) << "Error from fuchsia::ui::accessibility::view::Registry"
                   << zx_status_get_string(status);
  });

  // Set up focuser error handler.
  focuser_.set_error_handler([](zx_status_t error) {
    FX_LOGS(WARNING) << "Focuser died with error " << zx_status_get_string(error);
  });

  // Create view token and view ref pairs for a11y view.
  auto [a11y_view_token, a11y_view_holder_token] = scenic::ViewTokenPair::New();
  auto [a11y_control_ref, a11y_view_ref] = scenic::ViewRefPair::New();

  // Make a copy of the a11y view ref. We need an extra copy to send to root
  // presenter so that it can route input through the a11y view.
  fuchsia::ui::views::ViewRef a11y_view_ref_copy;
  fidl::Clone(a11y_view_ref, &a11y_view_ref_copy);

  view_ref_.emplace();
  fidl::Clone(a11y_view_ref, &(*view_ref_));

  // Create a11y view. We need to do this step before we ask root presenter to
  // add our view holder to the scene, because root presenter will try to route
  // input to the a11y view at that time. If the a11y view does not yet exist,
  // that step will fail.
  a11y_view_.emplace(session_.get(), std::move(a11y_view_token), std::move(a11y_control_ref),
                     std::move(a11y_view_ref), "A11y View");

  // TODO(fxbug.dev/77045): Switch to use SafePresenter.
  // Apply changes.
  session_->Present(
      /* presentation_time = */ 0,
      /* presentation_callback = */ [this, a11y_view_ref_copy = std::move(a11y_view_ref_copy),
                                     a11y_view_holder_token = std::move(a11y_view_holder_token)](
                                        fuchsia::images::PresentationInfo info) mutable {
        // Insert a11y view into root presenter.
        accessibility_view_registry_->CreateAccessibilityViewHolder(
            std::move(a11y_view_ref_copy), std::move(a11y_view_holder_token),
            [this](fuchsia::ui::views::ViewHolderToken proxy_view_holder_token) {
              // Create the proxy view holder and attach it to the scene.
              proxy_view_holder_.emplace(session_.get(), std::move(proxy_view_holder_token),
                                         "Proxy View Holder");
              a11y_view_->AddChild(proxy_view_holder_.value());

              // If the a11y view is already attached to the scene, use its properties
              // to set the properties of the proxy view holder. Otherwise, let
              // |OnScenicEvent| set the proxy view holder properties when the a11y
              // view is attached.
              if (a11y_view_properties_) {
                session_->Enqueue(scenic::NewSetViewPropertiesCmd(proxy_view_holder_->id(),
                                                                  *a11y_view_properties_));
              }

              // Apply changes.
              session_->Present(
                  /* presentation_time = */ 0,
                  /* presentation_callback = */ [this](fuchsia::images::PresentationInfo info) {
                    const bool old = is_initialized();
                    proxy_view_holder_attached_ = true;
                    if (a11y_view_properties_) {
                      proxy_view_holder_properties_set_ = true;
                    }

                    if (is_initialized() && !old) {
                      // The scene just became ready.
                      InvokeSceneReadyCallbacks(&scene_ready_callbacks_);
                    }
                  });
            });
      });
}

void AccessibilityView::OnScenicEvent(std::vector<fuchsia::ui::scenic::Event> events) {
  bool changes_to_present = false;
  bool view_properties_changed = false;
  for (const auto& event : events) {
    if (event.Which() == fuchsia::ui::scenic::Event::Tag::kGfx) {
      const auto& gfx_event = event.gfx();
      if (gfx_event.Which() == fuchsia::ui::gfx::Event::Tag::kViewAttachedToScene) {
        const auto& view_attached_event = gfx_event.view_attached_to_scene();
        if (view_attached_event.view_id == a11y_view_->id()) {
          a11y_view_properties_ = view_attached_event.properties;
          view_properties_changed = true;
          // If the client view holder was already created, then we need to set
          // its properties.
          if (proxy_view_holder_) {
            session_->Enqueue(
                scenic::NewSetViewPropertiesCmd(proxy_view_holder_->id(), *a11y_view_properties_));
            changes_to_present = true;
          }
        }
      } else if (gfx_event.Which() == fuchsia::ui::gfx::Event::Tag::kViewPropertiesChanged) {
        const auto& view_properties_changed_event = gfx_event.view_properties_changed();
        if (view_properties_changed_event.view_id == a11y_view_->id()) {
          a11y_view_properties_ = view_properties_changed_event.properties;
          view_properties_changed = true;
          // If the client view holder was already created, then we need to set
          // its properties.
          if (proxy_view_holder_) {
            session_->Enqueue(
                scenic::NewSetViewPropertiesCmd(proxy_view_holder_->id(), *a11y_view_properties_));
            changes_to_present = true;
          }
        }
      } else if (gfx_event.Which() == fuchsia::ui::gfx::Event::Tag::kViewConnected) {
        const auto& view_connected_event = gfx_event.view_connected();
        if (view_connected_event.view_holder_id == proxy_view_holder_->id()) {
          const bool old = is_initialized();
          proxy_view_connected_ = true;
          if (is_initialized() && !old) {
            InvokeSceneReadyCallbacks(&scene_ready_callbacks_);
          }
        }
      }
    }
  }

  if (changes_to_present) {
    session_->Present(
        /* presentation_time = */ 0,
        /* presentation_callback = */ [this](fuchsia::images::PresentationInfo info) {
          const bool old = is_initialized();
          proxy_view_holder_properties_set_ = true;
          if (is_initialized() && !old) {
            // The scene just became ready.
            InvokeSceneReadyCallbacks(&scene_ready_callbacks_);
          }
        });
  }

  if (view_properties_changed && !view_properties_changed_callbacks_.empty()) {
    InvokeViewPropertiesChangedCallbacks(*a11y_view_properties_,
                                         &view_properties_changed_callbacks_);
  }
}

std::optional<fuchsia::ui::views::ViewRef> AccessibilityView::view_ref() {
  if (!view_ref_) {
    return std::nullopt;
  }
  fuchsia::ui::views::ViewRef copy;
  fidl::Clone(*view_ref_, &copy);
  return std::move(copy);
}

void AccessibilityView::add_view_properties_changed_callback(
    ViewPropertiesChangedCallback callback) {
  view_properties_changed_callbacks_.push_back(std::move(callback));
  if (a11y_view_properties_) {
    view_properties_changed_callbacks_.back()(*a11y_view_properties_);
  }
}

void AccessibilityView::add_scene_ready_callback(SceneReadyCallback callback) {
  scene_ready_callbacks_.push_back(std::move(callback));
  if (is_initialized()) {
    scene_ready_callbacks_.back()();
  }
}

void AccessibilityView::RequestFocus(fuchsia::ui::views::ViewRef view_ref,
                                     fit::function<void(bool)> callback) {
  FX_DCHECK(focuser_);
  focuser_->RequestFocus(std::move(view_ref), [callback = std::move(callback)](auto result) {
    if (result.is_err()) {
      callback(false);
    } else {
      callback(true);
    }
  });
}

}  // namespace a11y
