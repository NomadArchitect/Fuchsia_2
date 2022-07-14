// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_TOUCH_SYSTEM_H_
#define SRC_UI_SCENIC_LIB_INPUT_TOUCH_SYSTEM_H_

#include <fuchsia/ui/input/accessibility/cpp/fidl.h>
#include <fuchsia/ui/input/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <lib/sys/cpp/component_context.h>

#include <map>
#include <optional>

#include "src/ui/scenic/lib/gfx/engine/scene_graph.h"
#include "src/ui/scenic/lib/input/a11y_legacy_contender.h"
#include "src/ui/scenic/lib/input/a11y_registry.h"
#include "src/ui/scenic/lib/input/constants.h"
#include "src/ui/scenic/lib/input/gesture_arena.h"
#include "src/ui/scenic/lib/input/gfx_legacy_contender.h"
#include "src/ui/scenic/lib/input/helper.h"
#include "src/ui/scenic/lib/input/hit_tester.h"
#include "src/ui/scenic/lib/input/touch_source.h"
#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

namespace scenic_impl::input {

// Tracks input APIs.
class TouchSystem : public fuchsia::ui::input::PointerCaptureListenerRegistry {
 public:
  struct PointerCaptureListener {
    fuchsia::ui::input::PointerCaptureListenerPtr listener_ptr;
    fuchsia::ui::views::ViewRef view_ref;
  };

  explicit TouchSystem(sys::ComponentContext* context,
                       std::shared_ptr<const view_tree::Snapshot>& view_tree_snapshot,
                       HitTester& hit_tester, inspect::Node& parent_node,
                       RequestFocusFunc request_focus, fxl::WeakPtr<gfx::SceneGraph> scene_graph);
  ~TouchSystem() = default;

  fuchsia::ui::input::accessibility::PointerEventListenerPtr&
  accessibility_pointer_event_listener() {
    return a11y_pointer_event_registry_->accessibility_pointer_event_listener();
  }

  void RegisterTouchSource(
      fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> touch_source_request,
      zx_koid_t client_view_ref_koid);

  // |fuchsia.ui.pointercapture.ListenerRegistry|
  void RegisterListener(
      fidl::InterfaceHandle<fuchsia::ui::input::PointerCaptureListener> listener_handle,
      fuchsia::ui::views::ViewRef view_ref, RegisterListenerCallback success_callback) override;

  // For tests.
  // TODO(fxbug.dev/72919): Remove when integration tests are properly separated out.
  void RegisterA11yListener(
      fidl::InterfaceHandle<fuchsia::ui::input::accessibility::PointerEventListener> listener,
      A11yPointerEventRegistry::RegisterCallback callback) {
    a11y_pointer_event_registry_->Register(std::move(listener), std::move(callback));
  }

  void DispatchPointerCommand(const fuchsia::ui::input::SendPointerInputCmd& command,
                              scheduling::SessionId session_id);

  // Injects a touch event directly to the View with koid |event.target|.
  void InjectTouchEventExclusive(const InternalTouchEvent& event, StreamId stream_id);
  // Injects a touch event by hit testing for appropriate targets.
  void InjectTouchEventHitTested(const InternalTouchEvent& event, StreamId stream_id);

  // Injects a mouse event using the GFX legacy API. Deprecated.
  void LegacyInjectMouseEventHitTested(const InternalTouchEvent& event);

 private:
  // Send a copy of the event to the singleton listener of the pointer capture API if there is one.
  // TODO(fxbug.dev/48150): Delete when we delete the PointerCapture functionality.
  void ReportPointerEventToPointerCaptureListener(const InternalTouchEvent& event) const;

  // Enqueue the pointer event into the EventReporter of a View.
  void ReportPointerEventToGfxLegacyView(const InternalTouchEvent& event, zx_koid_t view_ref_koid,
                                         fuchsia::ui::input::PointerEventType type);

  // Takes a ViewRef koid and creates a GfxLegacyContender that delivers events to the corresponding
  // SessionListener on contest victory.
  ContenderId AddGfxLegacyContender(StreamId stream_id, zx_koid_t view_ref_koid);

  fuchsia::ui::input::accessibility::PointerEvent CreateAccessibilityEvent(
      const InternalTouchEvent& event);

  // Collects all the GestureContenders for a new touch event stream.
  std::vector<ContenderId> CollectContenders(StreamId stream_id, const InternalTouchEvent& event);

  // Updates the gesture arena and all contenders for stream |stream_id| with a new event.
  void UpdateGestureContest(const InternalTouchEvent& event, StreamId stream_id);

  // Records a set of responses from a gesture disambiguation contender.
  void RecordGestureDisambiguationResponse(StreamId stream_id, ContenderId contender_id,
                                           const std::vector<GestureResponse>& responses);

  // Destroy the arena if the contest is complete (i.e. no contenders left or contest over and
  // stream ended).
  void DestroyArenaIfComplete(StreamId stream_id);

  // For a view hierarchy where |top| is an ancestor of |bottom|, returns |bottom|'s ancestor
  // hierarchy starting at |top| and ending at |bottom|.
  std::vector<zx_koid_t> GetAncestorChainTopToBottom(zx_koid_t bottom, zx_koid_t top) const;

  /// Construction-time state.
  // Reference to the ViewTreeSnapshot held by InputSystem.
  std::shared_ptr<const view_tree::Snapshot>& view_tree_snapshot_;
  HitTester& hit_tester_;
  const RequestFocusFunc request_focus_;
  // TODO(fxbug.dev/64206): Remove when we no longer have any legacy clients.
  fxl::WeakPtr<gfx::SceneGraph> scene_graph_;
  // An inspector that tracks all GestureContenders, so data can persist past contender lifetimes.
  // Must outlive all contenders.
  GestureContenderInspector contender_inspector_;

  /// FIDL server implementations.
  std::optional<A11yPointerEventRegistry> a11y_pointer_event_registry_;
  fidl::BindingSet<fuchsia::ui::input::PointerCaptureListenerRegistry> pointer_capture_registry_;
  // A singleton listener who wants to be notified when pointer events happen.
  // We honor the first pointer capture listener to register. A call to RegisterListener()
  // above will fail if there is already a registered listener.
  std::optional<PointerCaptureListener> pointer_capture_listener_;

  // Legacy mouse.
  // TODO(fxbug.dev/64206): Remove when we no longer have any legacy clients.
  // Tracks the View each mouse pointer is delivered to; a map from device ID to a ViewRef KOID.
  // This is used to ensure consistent delivery of mouse events for a given device. A focus change
  // triggered by other pointer events should *not* affect delivery of events to existing mice.
  std::unordered_map<uint32_t, std::vector</*view_ref_koids*/ zx_koid_t>> mouse_targets_;

  //// Gesture disambiguation state
  // Whenever a new touch event stream is started (by the injection of an ADD event) we create a
  // GestureArena to track that stream, and select a number of contenders to participate in the
  // contest. All contenders are tracked in the |contenders_| map for the duration of their
  // lifetime. The |contenders_| map is relied upon by the |gesture_arenas_| to deliver events.

  // Ties each TouchSource instance to its contender id.
  struct TouchContender {
    ContenderId contender_id;
    TouchSource touch_source;
    TouchContender(zx_koid_t view_ref_koid, ContenderId id,
                   fidl::InterfaceRequest<fuchsia::ui::pointer::TouchSource> event_provider,
                   fit::function<void(StreamId, const std::vector<GestureResponse>&)> respond,
                   fit::function<void()> error_handler, GestureContenderInspector& inspector)
        : contender_id(id),
          touch_source(view_ref_koid, std::move(event_provider), std::move(respond),
                       std::move(error_handler), inspector) {}
  };

  // Each gesture arena tracks one touch event stream and a set of contenders.
  std::unordered_map<StreamId, GestureArena> gesture_arenas_;

  // Map of all active contenders. If any contender is deleted, it must be removed from this map or
  // we risk use-after-free errors.
  std::unordered_map<ContenderId, GestureContender*> contenders_;

  // Mapping of ViewRef koids to TouchContenders
  // Invariant: |touch_contenders_| tracks regular GestureContenders.
  // Note: Legacy GestureContenders are tracked in separate fields.
  // Upon destruction, each member of |touch_contenders_| calls its |respond| closure, which calls
  // back into TouchSystem and relies upon the state of |gesture_arenas_| and |contenders_|.
  // |touch_contenders_| must therefore be destroyed first. To guarantee that, it must be placed
  // textually after these members.
  std::unordered_map<zx_koid_t, TouchContender> touch_contenders_;

  // GestureContender for the accessibility client. Valid while accessibility is connected, null
  // otherwise.
  std::unique_ptr<A11yLegacyContender> a11y_legacy_contender_;

  // Mapping of {device_id, pointer_id} to stream id for gfx legacy injection.
  std::map<std::pair<uint32_t, uint32_t>, StreamId> gfx_legacy_streams_;
  std::unordered_map<ContenderId, GfxLegacyContender> gfx_legacy_contenders_;

  const ContenderId a11y_contender_id_ = 1;
  ContenderId next_contender_id_ = 2;
};

}  // namespace scenic_impl::input

#endif  // SRC_UI_SCENIC_LIB_INPUT_TOUCH_SYSTEM_H_
