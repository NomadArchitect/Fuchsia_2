// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/focus/focus_manager.h"

#include <lib/syslog/cpp/macros.h>

namespace focus {

FocusManager::FocusManager(inspect::Node inspect_node)
    : focus_chain_listener_registry_(this), inspect_node_(std::move(inspect_node)) {
  // Track the focus chain in inspect.
  lazy_ = inspect_node_.CreateLazyValues("values", [this] {
    inspect::Inspector inspector;

    auto array = inspector.GetRoot().CreateUintArray("focus_chain", focus_chain_.size());
    for (size_t i = 0; i < focus_chain_.size(); i++) {
      array.Set(i, focus_chain_[i]);
    }
    inspector.emplace(std::move(array));

    return fit::make_ok_promise(std::move(inspector));
  });
}

void FocusManager::Publish(sys::ComponentContext& component_context) {
  component_context.outgoing()->AddPublicService<FocusChainListenerRegistry>(
      [this](fidl::InterfaceRequest<FocusChainListenerRegistry> request) {
        focus_chain_listener_registry_.Bind(std::move(request));
      });
}

FocusChangeStatus FocusManager::RequestFocus(zx_koid_t requestor, zx_koid_t request) {
  // Invalid requestor.
  if (snapshot_->view_tree.count(requestor) == 0) {
    return FocusChangeStatus::kErrorRequestorInvalid;
  }

  // Invalid request.
  if (snapshot_->view_tree.count(request) == 0) {
    return FocusChangeStatus::kErrorRequestInvalid;
  }

  // Transfer policy: requestor must be authorized.
  if (std::find(focus_chain_.begin(), focus_chain_.end(), requestor) == focus_chain_.end()) {
    return FocusChangeStatus::kErrorRequestorNotAuthorized;
  }

  // Transfer policy: requestor must be ancestor of request
  if (!snapshot_->IsDescendant(/*descendant_koid*/ request, /*ancestor_koid*/ requestor) &&
      request != requestor) {
    return FocusChangeStatus::kErrorRequestorNotRequestAncestor;
  }

  // Transfer policy: request must be focusable
  if (!snapshot_->view_tree.at(request).is_focusable) {
    return FocusChangeStatus::kErrorRequestCannotReceiveFocus;
  }

  // It's a valid request for a change to focus chain.
  SetFocus(request);
  FX_DCHECK(focus_chain_.at(0) == snapshot_->root);
  return FocusChangeStatus::kAccept;
}

void FocusManager::OnNewViewTreeSnapshot(std::shared_ptr<const view_tree::Snapshot> snapshot) {
  FX_DCHECK(snapshot);
  snapshot_ = std::move(snapshot);
  RepairFocus();
}

void FocusManager::Register(
    fidl::InterfaceHandle<fuchsia::ui::focus::FocusChainListener> focus_chain_listener) {
  const uint64_t id = next_focus_chain_listener_id_++;
  fuchsia::ui::focus::FocusChainListenerPtr new_listener;
  new_listener.Bind(std::move(focus_chain_listener));
  new_listener.set_error_handler([this, id](zx_status_t) { focus_chain_listeners_.erase(id); });
  const auto [_, success] = focus_chain_listeners_.emplace(id, std::move(new_listener));
  FX_DCHECK(success);

  // Dispatch current chain on register.
  DispatchFocusChainTo(focus_chain_listeners_.at(id));
}

void FocusManager::DispatchFocusChainTo(const fuchsia::ui::focus::FocusChainListenerPtr& listener) {
  listener->OnFocusChange(CloneFocusChain(), [] { /* No flow control yet. */ });
}

void FocusManager::DispatchFocusChain() {
  for (auto& [_, listener] : focus_chain_listeners_) {
    DispatchFocusChainTo(listener);
  }
}

fuchsia::ui::views::ViewRef FocusManager::CloneViewRefOf(zx_koid_t koid) const {
  FX_DCHECK(snapshot_->view_tree.count(koid) != 0)
      << "all views in the focus chain must exist in the view tree";
  fuchsia::ui::views::ViewRef clone;
  fidl::Clone(snapshot_->view_tree.at(koid).view_ref, &clone);
  return clone;
}

fuchsia::ui::focus::FocusChain FocusManager::CloneFocusChain() const {
  fuchsia::ui::focus::FocusChain full_copy{};
  for (const zx_koid_t koid : focus_chain_) {
    full_copy.mutable_focus_chain()->push_back(CloneViewRefOf(koid));
  }
  return full_copy;
}

void FocusManager::RepairFocus() {
  // Old root no longer valid -> move focus to new root.
  if (focus_chain_.empty() || snapshot_->root != focus_chain_.front()) {
    SetFocus(snapshot_->root);
    return;
  }

  std::vector<zx_koid_t> new_focus_chain = focus_chain_;

  // See if there's any place where the old focus chain breaks a parent-child relationship, and
  // truncate from there.
  // Note: Start at i = 1 so we can compare with i - 1.
  for (size_t child_index = 1; child_index < new_focus_chain.size(); ++child_index) {
    const zx_koid_t child = new_focus_chain.at(child_index);
    const zx_koid_t parent = new_focus_chain.at(child_index - 1);
    if (snapshot_->view_tree.count(child) == 0 || snapshot_->view_tree.at(child).parent != parent) {
      new_focus_chain.erase(new_focus_chain.begin() + child_index, new_focus_chain.end());
      break;
    }
  }

  SetFocusChain(std::move(new_focus_chain));
}

void FocusManager::SetFocus(zx_koid_t koid) {
  FX_DCHECK(koid != ZX_KOID_INVALID || koid == snapshot_->root);
  if (koid != ZX_KOID_INVALID) {
    FX_DCHECK(snapshot_->view_tree.count(koid) != 0);
    FX_DCHECK(snapshot_->view_tree.at(koid).is_focusable);
  }

  std::vector<zx_koid_t> new_focus_chain;

  // Regenerate chain.
  while (koid != ZX_KOID_INVALID) {
    new_focus_chain.emplace_back(koid);
    koid = snapshot_->view_tree.at(koid).parent;
  }
  std::reverse(new_focus_chain.begin(), new_focus_chain.end());

  SetFocusChain(std::move(new_focus_chain));
}

void FocusManager::SetFocusChain(std::vector<zx_koid_t> new_focus_chain) {
  if (new_focus_chain != focus_chain_) {
    focus_chain_ = std::move(new_focus_chain);
    DispatchFocusChain();
  }
}

}  // namespace focus
