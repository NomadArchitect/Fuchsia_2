// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_A11Y_LIB_MAGNIFIER_MAGNIFIER_H_
#define SRC_UI_A11Y_LIB_MAGNIFIER_MAGNIFIER_H_

#include <fuchsia/accessibility/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/zx/time.h>

#include <memory>

#include "src/lib/callback/scoped_task_runner.h"
#include "src/lib/fxl/memory/weak_ptr.h"
#include "src/lib/ui/input/gesture_detector.h"
#include "src/ui/a11y/lib/gesture_manager/arena/contest_member.h"
#include "src/ui/a11y/lib/gesture_manager/arena/recognizer.h"
#include "src/ui/a11y/lib/gesture_manager/gesture_handler.h"
#include "src/ui/lib/glm_workaround/glm_workaround.h"

namespace a11y {

class Magnifier : public fuchsia::accessibility::Magnifier,
                  public GestureRecognizer,
                  input::GestureDetector::Delegate {
 public:
  // Max time between tap begins in a trigger gesture.
  static constexpr zx::duration kTriggerMaxDelay = zx::msec(400);
  // Time a trigger needs to be held in place before it signifies temporary zoom
  // rather than a toggle. Moving the pointer also transitions to a temporary
  // zoom.
  static constexpr zx::duration kTemporaryZoomHold = zx::msec(500);
  // Transition over .2 s @ 60 fps.
  static constexpr zx::duration kTransitionPeriod = zx::msec(200);
  static constexpr float kTransitionRate = 1 / (kTransitionPeriod.to_msecs() * .060f);
  static constexpr float kDragThreshold = 1.f / 16;  // NDC
  static constexpr float kMinScale = 2, kMaxScale = 20;
  static constexpr float kDefaultScale = 4;

  Magnifier();
  ~Magnifier() override;

  // |fuchsia::accessibility::Magnifier|
  void RegisterHandler(
      fidl::InterfaceHandle<fuchsia::accessibility::MagnificationHandler> handler) override;

  // Method to register recognizers in a gesture recognition arena.
  void BindGestures(a11y::GestureHandler* gesture_handler);

  // Used when magnification is toggled off, to restore the presentation to an umagnified state.
  void ZoomOutIfMagnified();

  // |GestureRecognizer|
  void OnWin() override;
  // |GestureRecognizer|
  void OnDefeat() override;
  // |GestureRecognizer|
  void OnContestStarted(std::unique_ptr<ContestMember> contest_member) override;
  // |GestureRecognizer|
  void HandleEvent(const fuchsia::ui::input::accessibility::PointerEvent& pointer_event) override;
  // |GestureRecognizer|
  std::string DebugName() const override;

 private:
  // Magnification is enabled by a triple 1-finger tap or a double 3-finger tap.
  // Once it is enabled, zoom can be adjusted by pinching, and the view can be
  // dragged to pan (with at least two fingers to start, after which a single
  // finger will do).
  //
  // Alternately, magnification can be temporary if the last tap is held down,
  // in which case panning focuses on the area of the display that would be
  // under the finger in an unmagnified view.
  class Trigger {
   public:
    // This does not update the primer type, which is only updated on commit.
    // This should be checked on tap begin and update.
    bool ShouldTrigger(input::GestureDetector::TapType tap_type) const;

    // Tests whether the given tap type could be part of a trigger gesture, to support early defeat
    // declaration in the gesture arena.
    bool CanTrigger(input::GestureDetector::TapType tap_type) const;

    // Only taps can prime this gesture. When a tap is committed, update the
    // primer.
    void OnTapCommit(input::GestureDetector::TapType tap_type);

    // Cancels the trigger, on move or final commit.
    void Reset();

    bool is_primed() const { return primer_type_ != PrimerType::kNotPrimed; }

   private:
    // The most wonderful thing about triggers is I'm not the only one!
    enum class PrimerType {
      kNotPrimed,
      // 2 3-finger taps
      k2x3,
      // 3 1-finger taps - first tap
      k3x1_1,
      // 3 1-finger taps - second tap
      // They're bouncy trouncy flouncy pouncy fun fun fun fun fun.
      k3x1_2
    };

    PrimerType primer_type_ = PrimerType::kNotPrimed;
  };

  class Interaction;

  // Represents current and pending state resulting from control gestures (not including animation
  // progress). We may choose to remove this structure after Magnifier is broken into component
  // recognizers with their own post-win event streaming.
  struct ControlState {
    float transition_rate = 0;
    float magnified_scale = kDefaultScale;
    glm::vec2 magnified_translation = {0, 0};

    bool operator==(const ControlState& o) const;
    bool operator!=(const ControlState& o) const;

    // Helper that sets the magnified translation to focus on the given screen coordinate. This does
    // not call |UpdateTransform|.
    void FocusOn(const glm::vec2& focus);
  };

  // |input::GestureDetector::Delegate|
  std::unique_ptr<input::GestureDetector::Interaction> BeginInteraction(
      const input::Gesture* gesture) override;

  // Resets the gesture detector and trigger, and cancels the tap timeout if scheduled.
  void ResetRecognizer();

  // Rejects unfulfilled multitap gestures on timeout. The determination of when to post this task
  // is governed by the |Interaction|, but the timeout itself can outlive the |Interaction| (but not
  // the |Magnifier|).
  void ResetTaps();

  // Sends the updated transform to the handler.
  void UpdateTransform();
  // Sends the updated transform if it is the |current_transform_|.
  void UpdateIfActive(const ControlState* state);
  void TransitionIntoZoom(ControlState* state);
  void TransitionOutOfZoom(ControlState* state);

  bool is_magnified(const ControlState* state) const;

  fuchsia::accessibility::MagnificationHandlerPtr handler_;

  input::GestureDetector gesture_detector_;
  fxl::WeakPtr<Interaction> interaction_;

  float transition_progress_ = 0;
  // Double-buffered state allows us to defer updates from gestures until after we've won.
  ControlState buffered_state_[2];
  // Represents committed control state resulting from winning gestures. This is a pointer to allow
  // ongoing interactions to route updates independent from when the gesture is awarded a win.
  ControlState* current_state_ = &buffered_state_[0];
  // Represents pending control state accumulated by contending gestures.
  ControlState* pending_state_ = &buffered_state_[1];
  bool update_in_progress_ = false, update_pending_ = false;

  Trigger trigger_;

  callback::ScopedTaskRunner handler_scope_;
  // Task that handles timeouts to reject unfulfilled multitap gestures.
  async::TaskClosureMethod<Magnifier, &Magnifier::ResetTaps> reset_taps_;

  // This should be last as destroying it can trigger cleanup actions that depend on other state.
  std::unique_ptr<ContestMember> contest_member_;
};

}  // namespace a11y

#endif  // SRC_UI_A11Y_LIB_MAGNIFIER_MAGNIFIER_H_
