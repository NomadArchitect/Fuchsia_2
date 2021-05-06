// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/gfx/engine/scene_graph.h"

#include <lib/fostr/fidl/fuchsia/ui/input/formatting.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <zircon/status.h>

#include <sstream>

#include "src/ui/scenic/lib/gfx/engine/session.h"
#include "src/ui/scenic/lib/gfx/util/time.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace scenic_impl {
namespace gfx {

using fuchsia::ui::focus::FocusChainListener;
using fuchsia::ui::focus::FocusChainListenerRegistry;
using fuchsia::ui::views::Error;
using fuchsia::ui::views::ViewRef;
using ViewFocuser = fuchsia::ui::views::Focuser;

CompositorWeakPtr SceneGraph::GetCompositor(GlobalId compositor_id) const {
  for (const CompositorWeakPtr& compositor : compositors_) {
    if (compositor && compositor->global_id() == compositor_id) {
      return compositor;
    }
  }
  return Compositor::kNullWeakPtr;
}

SceneGraph::SceneGraph(RequestFocusFunc request_focus)
    : request_focus_(std::move(request_focus)), weak_factory_(this) {}

void SceneGraph::AddCompositor(const CompositorWeakPtr& compositor) {
  FX_DCHECK(compositor);
  compositors_.push_back(compositor);
}

void SceneGraph::RemoveCompositor(const CompositorWeakPtr& compositor) {
  FX_DCHECK(compositor);
  auto it =
      std::find_if(compositors_.begin(), compositors_.end(),
                   [compositor](const auto& c) -> bool { return c.get() == compositor.get(); });
  FX_DCHECK(it != compositors_.end());
  compositors_.erase(it);
}

void SceneGraph::InvalidateAnnotationViewHolder(zx_koid_t koid) {
  view_tree_.InvalidateAnnotationViewHolder(koid);
}

// To avoid unnecessary complexity or cost of maintaining state, view_tree_ modifications are
// destructive.  This operation must preserve any needed state before applying updates.
void SceneGraph::ProcessViewTreeUpdates(ViewTreeUpdates view_tree_updates) {
  // Process all updates.
  for (auto& update : view_tree_updates) {
    if (auto ptr = std::get_if<ViewTreeNewRefNode>(&update)) {
      view_tree_.NewRefNode(std::move(*ptr));
    } else if (const auto ptr = std::get_if<ViewTreeNewAttachNode>(&update)) {
      view_tree_.NewAttachNode(ptr->koid);
    } else if (const auto ptr = std::get_if<ViewTreeDeleteNode>(&update)) {
      view_tree_.DeleteNode(ptr->koid);
    } else if (const auto ptr = std::get_if<ViewTreeMakeGlobalRoot>(&update)) {
      view_tree_.MakeGlobalRoot(ptr->koid);
    } else if (const auto ptr = std::get_if<ViewTreeConnectToParent>(&update)) {
      view_tree_.ConnectToParent(ptr->child, ptr->parent);
    } else if (const auto ptr = std::get_if<ViewTreeDisconnectFromParent>(&update)) {
      view_tree_.DisconnectFromParent(ptr->koid);
    } else {
      FX_NOTREACHED() << "Encountered unknown type of view tree update; variant index is: "
                      << update.index();
    }
  }
}

void SceneGraph::RegisterViewFocuser(SessionId session_id,
                                     fidl::InterfaceRequest<ViewFocuser> view_focuser) {
  FX_DCHECK(session_id != 0u) << "precondition";
  FX_DCHECK(view_focuser_endpoints_.count(session_id) == 0u) << "precondition";

  fit::function<void(ViewRef, ViewFocuser::RequestFocusCallback)> request_focus_handler =
      [this, session_id](ViewRef view_ref, ViewFocuser::RequestFocusCallback response) {
        std::optional<zx_koid_t> requestor = this->view_tree().ConnectedViewRefKoidOf(session_id);
        if (requestor.has_value() &&
            request_focus_(requestor.value(), utils::ExtractKoid(view_ref))) {
          response(fit::ok());  // Request received, and honored.
          return;
        }

        response(fit::error(Error::DENIED));  // Report a problem.
      };

  view_focuser_endpoints_.emplace(
      session_id, ViewFocuserEndpoint(std::move(view_focuser), std::move(request_focus_handler)));
}

void SceneGraph::UnregisterViewFocuser(SessionId session_id) {
  view_focuser_endpoints_.erase(session_id);
}

void SceneGraph::OnNewFocusedView(const zx_koid_t old_focus, const zx_koid_t new_focus) {
  FX_DCHECK(old_focus != new_focus);

  const zx_time_t focus_time = dispatcher_clock_now();
  if (old_focus != ZX_KOID_INVALID) {
    fuchsia::ui::input::FocusEvent focus;
    focus.event_time = focus_time;
    focus.focused = false;

    if (view_tree_.EventReporterOf(old_focus)) {
      fuchsia::ui::input::InputEvent input;
      input.set_focus(std::move(focus));
      view_tree_.EventReporterOf(old_focus)->EnqueueEvent(std::move(input));
    } else {
      FX_VLOGS(1) << "Old focus event; could not enqueue. No reporter. Event was: " << focus;
    }
  }

  if (new_focus != ZX_KOID_INVALID) {
    fuchsia::ui::input::FocusEvent focus;
    focus.event_time = focus_time;
    focus.focused = true;

    if (view_tree_.EventReporterOf(new_focus)) {
      fuchsia::ui::input::InputEvent input;
      input.set_focus(std::move(focus));
      view_tree_.EventReporterOf(new_focus)->EnqueueEvent(std::move(input));
    } else {
      FX_VLOGS(1) << "New focus event; could not enqueue. No reporter. Event was: " << focus;
    }
  }
}

SceneGraph::ViewFocuserEndpoint::ViewFocuserEndpoint(
    fidl::InterfaceRequest<ViewFocuser> view_focuser,
    fit::function<void(ViewRef, RequestFocusCallback)> request_focus_handler)
    : request_focus_handler_(std::move(request_focus_handler)),
      endpoint_(this, std::move(view_focuser)) {
  FX_DCHECK(request_focus_handler_) << "invariant";
}

SceneGraph::ViewFocuserEndpoint::ViewFocuserEndpoint(ViewFocuserEndpoint&& original)
    : request_focus_handler_(std::move(original.request_focus_handler_)),
      endpoint_(this, original.endpoint_.Unbind()) {
  FX_DCHECK(request_focus_handler_) << "invariant";
}

void SceneGraph::ViewFocuserEndpoint::RequestFocus(ViewRef view_ref,
                                                   RequestFocusCallback response) {
  request_focus_handler_(std::move(view_ref), std::move(response));
}

}  // namespace gfx
}  // namespace scenic_impl
