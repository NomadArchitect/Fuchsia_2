// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    super::{
        args, click, inspect_keys, motion, one_finger_drag, primary_tap, scroll, secondary_tap,
    },
    crate::{input_device, input_handler::InputHandler, mouse_binding, touch_binding},
    anyhow::{format_err, Context, Error},
    async_trait::async_trait,
    core::cell::RefCell,
    fidl_fuchsia_input_report as fidl_input_report,
    fuchsia_inspect::{ArrayProperty, Node as InspectNode},
    fuchsia_inspect_contrib::nodes::BoundedListNode,
    fuchsia_syslog::{fx_log_debug, fx_log_err, fx_log_info},
    fuchsia_zircon as zx,
    std::any::Any,
    std::fmt::Debug,
};

pub fn make_input_handler(
    inspect_node: &InspectNode,
) -> std::rc::Rc<dyn crate::input_handler::InputHandler> {
    // TODO(https://fxbug.dev/105092): Remove log message.
    fx_log_info!("touchpad: created input handler");
    std::rc::Rc::new(GestureArena::new_internal(
        || {
            vec![
                Box::new(click::InitialContender {
                    max_finger_displacement_in_mm:
                        args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                }),
                Box::new(motion::InitialContender {
                    min_movement_in_mm: args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                }),
                Box::new(primary_tap::InitialContender {
                    max_finger_displacement_in_mm:
                        args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                    max_time_elapsed: args::TAP_TIMEOUT,
                }),
                Box::new(secondary_tap::InitialContender {
                    max_finger_displacement_in_mm:
                        args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                    max_time_elapsed: args::TAP_TIMEOUT,
                }),
                Box::new(one_finger_drag::InitialContender {
                    min_movement_in_mm: args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                }),
                Box::new(scroll::InitialContender {
                    min_movement_in_mm: args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM,
                    max_movement_in_mm: args::MAX_SPURIOUS_TO_INTENTIONAL_SCROLL_THRESHOLD_MM,
                    limit_tangent_for_direction: args::MAX_SCROLL_DIRECTION_SKEW_DEGREES
                        .to_radians()
                        .tan(),
                }),
            ]
        },
        inspect_node,
        MAX_TOUCHPAD_EVENT_LOG_ENTRIES,
    ))
}

pub(super) const PRIMARY_BUTTON: mouse_binding::MouseButton = 1;
pub(super) const SECONDARY_BUTTON: mouse_binding::MouseButton = 2;

// TODO(https://fxbug.dev/102654): check that we've removed all leading `_` from types
// and variables in this file.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TouchpadEvent {
    pub(super) timestamp: zx::Time,
    // TODO(https://fxbug.dev/102655): replace these fields with a field that embeds
    // `touch_data: super::touch_binding::TouchpadEvent`.
    pub(super) pressed_buttons: Vec<u8>,
    pub(super) contacts: Vec<touch_binding::TouchContact>,
}

#[derive(Debug, PartialEq)]
pub(super) struct MouseEvent {
    pub(super) timestamp: zx::Time,
    pub(super) mouse_data: mouse_binding::MouseEvent,
}

#[derive(Debug)]
pub(super) enum ExamineEventResult {
    Contender(Box<dyn Contender>),
    MatchedContender(Box<dyn MatchedContender>),
    Mismatch(&'static str),
}

pub(super) trait Contender: std::fmt::Debug + AsAny {
    /// Examines `event`, to determine whether or not the gesture
    /// is relevant to this `Recognizer`.
    ///
    /// Returns
    /// * `ExamineEventResult::MatchedContender` if this recognizer wants
    ///   to send (or start sending) events downstream, OR
    /// * `ExamineEventResult::Contender` if this recognizer is not yet
    ///   ready to send events downstream, but wants to continue
    ///   contending for the gesture, OR
    /// * `ExamineEventResult::Mismatch` if this recognizer no longer
    ///   wants to contend for this gesture
    fn examine_event(self: Box<Self>, event: &TouchpadEvent) -> ExamineEventResult;

    /// Returns a string that uniquely identifies the concrete type
    /// of this implementation of the `Contender` trait.
    fn get_type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// If the gesture need to start from idle(no finger contacting the surface).
    fn start_from_idle(&self) -> bool {
        false
    }
}

pub trait AsAny {
    fn as_any(&self) -> &dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub(super) enum VerifyEventResult {
    MatchedContender(Box<dyn MatchedContender>),
    Mismatch(&'static str),
}

#[derive(Debug, PartialEq)]
pub(super) enum RecognizedGesture {
    /// Contains one variant for each recognizer, and the
    /// special value `Unrecognized` for when no recognizer
    /// claims the gesture.
    _Unrecognized,
    Click,
    #[allow(dead_code)] // Only used in tests until we implement palm recognizer.
    Palm,
    PrimaryTap,
    SecondaryTap,
    Motion,
    Scroll,
    OneFingerDrag,
}

#[derive(Debug)]
pub(super) struct ProcessBufferedEventsResult {
    pub(super) generated_events: Vec<MouseEvent>,
    pub(super) winner: Option<Box<dyn Winner>>,
    pub(super) recognized_gesture: RecognizedGesture, // for latency breakdown
}

pub(super) trait MatchedContender: std::fmt::Debug + AsAny {
    /// Verifies that `event` still matches the gesture that is relevant
    /// to this `Recognizer`.
    ///
    /// Returns
    /// * `VerifyEventResult::MatchedContender` if this recognizer wants
    ///   to send (or start sending) events downstream, OR
    /// * `VerifyEventResult::Mismatch` if this recognizer no longer
    ///   wants to contend for this gesture
    fn verify_event(self: Box<Self>, event: &TouchpadEvent) -> VerifyEventResult;

    /// Takes `events`, and generates corresponding `MouseEvent`s.
    ///
    /// Returns `ProcessBufferedEventsResult` with fields:
    /// * `generated_events`: the sequence of `MouseEvent`s needed
    ///   to effect the gesture downstream.
    /// * `winner`:
    ///   * `None` if the gesture is complete
    ///   * `Some` otherwise
    ///
    /// Note:
    /// * `generated_events` MAY be empty; for example, a palm
    ///   recognizer wants to discard unintended events
    /// * `events` is guaranteed to contains exactly the sequence of
    ///   `TouchpadEvent`s that this recognizer has already examined
    ///   and verified.
    /// * recognizers MAY choose to ignore `events`
    ///   e.g.:
    ///   * a one-finger-tap recognizer does not need to inspect
    ///     `events` to generate the `MouseEvent`s for the button click
    ///   * a motion recognizer, in contrast, needs the details in
    ///     `events` to generate the appropriate motion
    fn process_buffered_events(
        self: Box<Self>,
        events: Vec<TouchpadEvent>,
    ) -> ProcessBufferedEventsResult;

    /// Returns a string that uniquely identifies the concrete type
    /// of this implementation of the `MatchedContender` trait.
    fn get_type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum EndGestureEvent {
    GeneratedEvent(MouseEvent),
    UnconsumedEvent(TouchpadEvent),
    NoEvent,
}

#[derive(Debug)]
pub(super) enum ProcessNewEventResult {
    ContinueGesture(Option<MouseEvent>, Box<dyn Winner>),
    EndGesture(EndGestureEvent, &'static str),
}

pub(super) trait Winner: std::fmt::Debug {
    /// Takes `event`, and generates corresponding `MouseEvent`s.
    ///
    /// Returns:
    /// * `ContinueGesture(Some, …)` if the gesture is still
    ///   in progress, and a `MouseEvent` should be sent downstream;
    ///   might be used, e.g., by a motion recognizer
    /// * `ContinueGesutre(None, …)` if the gesture is still
    ///   in progress, and no `MouseEvent` should be sent downstream;
    ///   might be used, e.g., by a palm recognizer
    /// * `EndGesture(UnconsumedEvent, …)` if the gesture has ended because
    ///   `event` did not match; might be used, e.g., if the user
    ///    presses the touchpad down after a motion gesture
    /// * `EndGesture(GeneratedEvent, …)` if the gesture has ended because
    ///   `event` did end the gesture; might be used, e.g., if the user
    ///    release the touchpad down after a drag gesture
    /// * `EndGesture(NoEvent, …)` if `event` matches a normal end
    ///   of the gesture; might be used, e.g., if the user lifts
    ///   their finger off the touchpad after a motion gesture
    fn process_new_event(self: Box<Self>, event: TouchpadEvent) -> ProcessNewEventResult;

    /// Returns a string that uniquely defines the concrete type
    /// of this implementation of the `Winner` trait.
    fn get_type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

const MAX_TOUCHPAD_EVENT_LOG_ENTRIES: usize = 1250; // 125 Hz * 10 seconds

#[derive(Debug)]
enum MutableState {
    /// Looking for the start of any gesture.
    Idle,

    /// Looking for the start of a gesture that can be chained after
    /// a previous gesture. "Chaining" is using two or more gestures
    /// in sequence, while always having at least one finger in contact
    /// with the pad.
    ///
    /// Note that this does _not_ require any particular finger to
    /// be continually in contact with the pad. For example, the
    /// following is a valid chain:
    ///    1. Index finger
    ///    2. Index finger + middle finger
    ///    3. Middle finger
    Chain,

    /// Disambiguating the current gesture.
    Matching {
        contenders: Vec<Box<dyn Contender>>,
        matched_contenders: Vec<Box<dyn MatchedContender>>,
        buffered_events: Vec<TouchpadEvent>,
    },

    /// The matching gesture has been identified, and is still in progress.
    Forwarding { winner: Box<dyn Winner> },

    /// A transient state during the processing of a single `InputEvent`.
    Invalid,
}

/// Names for the `MutableState`s. Exists to support the state machine debug log.
#[derive(Debug, PartialEq)]
enum StateName {
    Idle,
    Chain,
    Matching,
    Forwarding,
    Invalid,
}

struct GestureArena<ContenderFactory: Fn() -> Vec<Box<dyn Contender>>> {
    contender_factory: ContenderFactory,
    mutable_state: RefCell<MutableState>,
    inspect_log: RefCell<BoundedListNode>,
}

impl<ContenderFactory: Fn() -> Vec<Box<dyn Contender>>> GestureArena<ContenderFactory> {
    #[cfg(test)]
    fn new_for_test(
        contender_factory: ContenderFactory,
        inspector: &fuchsia_inspect::Inspector,
        max_inspect_log_entries: usize,
    ) -> GestureArena<ContenderFactory> {
        Self::new_internal(contender_factory, inspector.root(), max_inspect_log_entries)
    }

    fn new_internal(
        contender_factory: ContenderFactory,
        inspect_node: &InspectNode,
        max_inspect_log_entries: usize,
    ) -> GestureArena<ContenderFactory> {
        GestureArena {
            contender_factory,
            mutable_state: RefCell::new(MutableState::Idle),
            inspect_log: RefCell::new(BoundedListNode::new(
                inspect_node.create_child(inspect_keys::ARENA_LOG_ROOT),
                max_inspect_log_entries,
            )),
        }
    }
}

impl TouchpadEvent {
    fn log_inspect(&self, log_entry_node: &InspectNode) {
        use inspect_keys::*;
        log_entry_node.atomic_update(|log_entry_node| {
            let touchpad_event_node = log_entry_node.create_child(&*TOUCHPAD_EVENT_NODE);

            // Create an inspect array from the pressed buttons.
            let pressed_buttons_node = touchpad_event_node
                .create_uint_array(&*PRESSED_BUTTONS_PROP, self.pressed_buttons.len());
            self.pressed_buttons.iter().enumerate().for_each(|(i, &button_id)| {
                pressed_buttons_node.set(i, button_id);
            });

            // Populate the touchpad event details
            log_common(&touchpad_event_node, self.timestamp);
            touchpad_event_node.record(pressed_buttons_node);
            touchpad_event_node.record_child(&*CONTACT_STATE_PROP, |contact_set_node| {
                self.contacts.iter().for_each(|contact| {
                    contact_set_node.record_child(contact.id.to_string(), |contact_node| {
                        contact_node.record_double(&*X_POS_PROP, f64::from(contact.position.x));
                        contact_node.record_double(&*Y_POS_PROP, f64::from(contact.position.y));
                        if let Some(contact_size) = contact.contact_size {
                            contact_node.record_double(&*WIDTH_PROP, f64::from(contact_size.width));
                            contact_node
                                .record_double(&*HEIGHT_PROP, f64::from(contact_size.height));
                        }
                    })
                })
            });

            // Pass ownership of the touchpad event node to the log.
            log_entry_node.record(touchpad_event_node);
        })
    }
}

fn log_common(inspect_node: &InspectNode, driver_timestamp: zx::Time) {
    use inspect_keys::*;
    inspect_node.record_int(&*EVENT_TIME_PROP, driver_timestamp.into_nanos());
    inspect_node.record_int(
        &*ENTRY_LATENCY_PROP,
        // Use lower precision for latency, to minimize space.
        (fuchsia_async::Time::now().into_zx() - driver_timestamp).into_micros(),
    );
}

impl MutableState {
    fn get_state_name(&self) -> StateName {
        match self {
            Self::Idle => StateName::Idle,
            Self::Chain => StateName::Chain,
            Self::Matching { .. } => StateName::Matching,
            Self::Forwarding { .. } => StateName::Forwarding,
            Self::Invalid => StateName::Invalid,
        }
    }
}

fn parse_touchpad_event(
    event_time: &zx::Time,
    touchpad_event: &touch_binding::TouchpadEvent,
    touchpad_descriptor: &touch_binding::TouchpadDeviceDescriptor,
) -> Result<TouchpadEvent, Error> {
    let position_divisor =
        get_position_divisor_to_mm(touchpad_descriptor).context("failed to compute divisor")?;
    Ok(TouchpadEvent {
        timestamp: *event_time,
        pressed_buttons: touchpad_event.pressed_buttons.iter().copied().collect::<Vec<_>>(),
        contacts: touchpad_event
            .injector_contacts
            .iter()
            .map(|contact| touch_binding::TouchContact {
                position: contact.position / position_divisor,
                ..*contact
            })
            .collect(),
    })
}

impl std::convert::From<MouseEvent> for input_device::InputEvent {
    fn from(mouse_event: MouseEvent) -> input_device::InputEvent {
        input_device::InputEvent {
            // TODO(https://fxbug.dev/104890): Convert `mouse_event.mouse_data.wheel_delta_v`
            // and `mouse_event.mouse_data.wheel_delta_h` from micrometers to counts.
            device_event: input_device::InputDeviceEvent::Mouse(mouse_event.mouse_data),
            device_descriptor: input_device::InputDeviceDescriptor::Mouse(
                mouse_binding::MouseDeviceDescriptor {
                    // Use a large number for the `device_id` for the gesture arena's
                    // virtual mouse, to avoid conflicts with real devices (which start
                    // at ID 0).
                    //
                    // However, don't use u32::MAX, since the `InputDeviceRegistry`
                    // FIDL handling code uses that for the first registered device
                    // (and works downwards).
                    device_id: u32::MAX / 2,
                    absolute_x_range: Some(fidl_input_report::Range { min: -127, max: 127 }),
                    absolute_y_range: Some(fidl_input_report::Range { min: -127, max: 127 }),
                    wheel_v_range: Some(fidl_input_report::Axis {
                        // Set range based on the range of `position_y` values for the
                        // Atlas touchpad. The touchpad is 2.48 inches tall, and the
                        // position is reported in micrometers. That gives `position_y`
                        // a range of (0, 62992).
                        //
                        // Note that this range is likely larger than needed, since users
                        // probably won't swipe all the way up/down the pad in a single
                        // sampling interval (8msec).
                        //
                        // TODO(https://fxbug.dev/104890): Adjust this range to reflect
                        // the range in counts instead of micrometers.
                        range: fidl_input_report::Range { min: -65535, max: 65535 },
                        unit: fidl_input_report::Unit {
                            type_: fidl_fuchsia_input_report::UnitType::Other,
                            exponent: 0,
                        },
                    }),
                    wheel_h_range: None,
                    buttons: Some(vec![PRIMARY_BUTTON, SECONDARY_BUTTON]),
                    counts_per_mm: mouse_binding::DEFAULT_COUNTS_PER_MM,
                },
            ),
            event_time: mouse_event.timestamp,
            handled: input_device::Handled::No,
            trace_id: None,
        }
    }
}

impl<ContenderFactory: Fn() -> Vec<Box<dyn Contender>>> GestureArena<ContenderFactory> {
    /// Interprets `TouchpadEvent`s, and sends corresponding `MouseEvent`s downstream.
    fn handle_touchpad_event(
        self: std::rc::Rc<Self>,
        event_time: &zx::Time,
        touchpad_event: &touch_binding::TouchpadEvent,
        device_descriptor: &touch_binding::TouchpadDeviceDescriptor,
    ) -> Result<Vec<input_device::InputEvent>, Error> {
        let touchpad_event = parse_touchpad_event(event_time, touchpad_event, device_descriptor)
            .context("dropping touchpad event")?;
        touchpad_event.log_inspect(self.inspect_log.borrow_mut().create_entry());

        let old_state_name = self.mutable_state.borrow().get_state_name();
        let (new_state, generated_events) = match self.mutable_state.replace(MutableState::Invalid)
        {
            MutableState::Idle => self.handle_event_while_idle(touchpad_event),
            MutableState::Chain => self.handle_event_while_chain(touchpad_event),
            MutableState::Matching { contenders, matched_contenders, buffered_events } => self
                .handle_event_while_matching(
                    contenders,
                    matched_contenders,
                    buffered_events,
                    touchpad_event,
                ),
            MutableState::Forwarding { winner } => {
                self.handle_event_while_forwarding(winner, touchpad_event)
            }
            MutableState::Invalid => {
                unreachable!();
            }
        };
        fx_log_debug!("gesture_arena: {:?} -> {:?}", old_state_name, new_state.get_state_name());
        self.mutable_state.replace(new_state);
        // self.log_mutable_state();  // uncomment to log contender set
        Ok(generated_events.into_iter().map(input_device::InputEvent::from).collect())
    }

    fn handle_event_while_idle(&self, new_event: TouchpadEvent) -> (MutableState, Vec<MouseEvent>) {
        let (contenders, matched_contenders) = (self.contender_factory)()
            .into_iter()
            .map(|contender| (contender.get_type_name(), contender.examine_event(&new_event)))
            .fold(
                (vec![], vec![]),
                |(mut contenders, mut matched_contenders), (type_name, examine_result)| {
                    match examine_result {
                        ExamineEventResult::Contender(contender) => {
                            contenders.push(contender);
                        }
                        ExamineEventResult::MatchedContender(matched_contender) => {
                            matched_contenders.push(matched_contender);
                        }
                        ExamineEventResult::Mismatch(reason) => {
                            // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                            fx_log_info!("touchpad: {} mismatched: {}", type_name, reason);
                        }
                    }
                    (contenders, matched_contenders)
                },
            );
        if contenders.is_empty() && matched_contenders.is_empty() {
            // No `Contender`s or `MatchedContender`s. The event does not
            // match the start of any gesture:
            // * Transition based on number of contacts.
            // * Drop `new_event`, since we have no way to convert that event
            //   into something clients downstream of the input pipeline can
            //   understand.
            if new_event.contacts.len() > 0 {
                // Enter chain if finger contacting. It is required if finger
                // placed on touchpad but Mismatch on every contender, Chain
                // state can prevent tap gesture when finger lift.
                (MutableState::Chain, vec![])
            } else {
                (MutableState::Idle, vec![])
            }
        } else {
            // At least one `Contender` or `MatchedContender`. Continue the contest.
            (
                MutableState::Matching {
                    contenders,
                    matched_contenders,
                    buffered_events: vec![new_event],
                },
                vec![],
            )
        }
    }

    fn handle_event_while_chain(
        &self,
        new_event: TouchpadEvent,
    ) -> (MutableState, Vec<MouseEvent>) {
        let (contenders, matched_contenders) = (self.contender_factory)()
            .into_iter()
            .filter(|contender| !contender.start_from_idle())
            .map(|contender| (contender.get_type_name(), contender.examine_event(&new_event)))
            .fold(
                (vec![], vec![]),
                |(mut contenders, mut matched_contenders), (type_name, examine_result)| {
                    match examine_result {
                        ExamineEventResult::Contender(contender) => {
                            contenders.push(contender);
                        }
                        ExamineEventResult::MatchedContender(matched_contender) => {
                            matched_contenders.push(matched_contender);
                        }
                        ExamineEventResult::Mismatch(reason) => {
                            // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                            fx_log_info!("touchpad: {} mismatched: {}", type_name, reason);
                        }
                    }
                    (contenders, matched_contenders)
                },
            );
        if contenders.is_empty() && matched_contenders.is_empty() {
            // No `Contender`s or `MatchedContender`s. The event does not
            // match the start of any gesture:
            // * Transition based on number of contacts.
            // * Drop `new_event`, since we have no way to convert that event
            //   into something clients downstream of the input pipeline can
            //   understand.
            if new_event.contacts.len() > 0 {
                // One more contacts continues the chain.
                (MutableState::Chain, vec![])
            } else {
                // Zero contacts ends the chain.
                (MutableState::Idle, vec![])
            }
        } else {
            // At least one `Contender` or `MatchedContender`. Continue the contest.
            (
                MutableState::Matching {
                    contenders,
                    matched_contenders,
                    buffered_events: vec![new_event],
                },
                vec![],
            )
        }
    }

    fn handle_event_while_matching(
        &self,
        contenders: Vec<Box<dyn Contender>>,
        matched_contenders: Vec<Box<dyn MatchedContender>>,
        buffered_events: Vec<TouchpadEvent>,
        new_event: TouchpadEvent,
    ) -> (MutableState, Vec<MouseEvent>) {
        // Note: This function processes `contenders` after `matched_contenders` to ensure
        // that a recognizer doesn't get invoked twice with the same event. Otherwise, it
        // would be possible for the following sequence of events to occur:
        //
        // 1. This function calls some_recognizer::Contender.examine_event(&new_event).
        // 2. The call returns some_recognizer::MatchedContender.
        // 3. This function calls some_recognizer::MatchedContender.verify_event(&new_event).
        //
        // See also: `does_not_repeat_event_to_matched_contender_returned_by_examine_event` test.
        let matched_contenders = matched_contenders
            .into_iter()
            .map(|matched_contender| {
                (matched_contender.get_type_name(), matched_contender.verify_event(&new_event))
            })
            .fold(vec![], |mut matched_contenders, (type_name, verify_result)| {
                match verify_result {
                    VerifyEventResult::MatchedContender(m) => {
                        matched_contenders.push(m);
                    }
                    VerifyEventResult::Mismatch(reason) => {
                        // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                        fx_log_info!("touchpad: {} mismatched: {}", type_name, reason);
                    }
                }
                matched_contenders
            });
        let (contenders, matched_contenders) = contenders
            .into_iter()
            .map(|contender| (contender.get_type_name(), contender.examine_event(&new_event)))
            .fold(
                (vec![], matched_contenders),
                |(mut contenders, mut matched_contenders), (type_name, examine_result)| {
                    match examine_result {
                        ExamineEventResult::Contender(contender) => {
                            contenders.push(contender);
                        }
                        ExamineEventResult::MatchedContender(matched_contender) => {
                            matched_contenders.push(matched_contender);
                        }
                        ExamineEventResult::Mismatch(reason) => {
                            // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                            fx_log_info!("touchpad: {} mismatched: {}", type_name, reason);
                        }
                    }
                    (contenders, matched_contenders)
                },
            );

        let has_finger_contact = new_event.contacts.len() > 0;

        match (
            u8::try_from(contenders.len()).unwrap_or(u8::MAX),
            u8::try_from(matched_contenders.len()).unwrap_or(u8::MAX),
        ) {
            // No `Contender`s or `MatchedContender`s. The contest has ended without
            // identifying a gesture.
            (0, 0) => {
                if has_finger_contact {
                    // One more contacts transit to chain. This is required when user
                    // give an unknown gesture for example a 2 fingers swipe in no
                    // direction and movement larger than threshold. Then finger lift,
                    // Chain state will prevent it to be recognized as secondary tap.
                    (MutableState::Chain, vec![])
                } else {
                    (MutableState::Idle, vec![])
                }
            }
            // No `Contender`s, and exactly one `MatchedContender`. The contest has ended
            // with a recognized gesture.
            (0, 1) => {
                let mut buffered_events = buffered_events;
                buffered_events.push(new_event);

                let mut matched_contenders = matched_contenders;
                let ProcessBufferedEventsResult { generated_events, winner, recognized_gesture } =
                    matched_contenders.remove(0).process_buffered_events(buffered_events);
                // TODO(https://fxbug.dev/105092): Remove log message.
                fx_log_info!("touchpad: recognized {:?}", recognized_gesture);

                match winner {
                    Some(winner) => (MutableState::Forwarding { winner }, generated_events),
                    None => {
                        if has_finger_contact {
                            (MutableState::Chain, generated_events)
                        } else {
                            (MutableState::Idle, generated_events)
                        }
                    }
                }
            }
            // At least 1 `Contender`, or 2 `MatchedContender`s; continue the contest.
            (1.., _) | (_, 2..) => {
                let mut buffered_events = buffered_events;
                buffered_events.push(new_event);
                (MutableState::Matching { contenders, matched_contenders, buffered_events }, vec![])
            }
        }
    }

    fn handle_event_while_forwarding(
        &self,
        winner: Box<dyn Winner>,
        new_event: TouchpadEvent,
    ) -> (MutableState, Vec<MouseEvent>) {
        let type_name = winner.get_type_name();
        let has_finger_contact = new_event.contacts.len() > 0;

        match winner.process_new_event(new_event) {
            ProcessNewEventResult::ContinueGesture(generated_event, winner) => {
                (MutableState::Forwarding { winner }, generated_event.into_iter().collect())
            }
            ProcessNewEventResult::EndGesture(
                EndGestureEvent::GeneratedEvent(generated_event),
                reason,
            ) => {
                // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                fx_log_info!("touchpad: {} ended: {}", type_name, reason);
                if has_finger_contact {
                    (MutableState::Chain, vec![generated_event])
                } else {
                    (MutableState::Idle, vec![generated_event])
                }
            }
            ProcessNewEventResult::EndGesture(
                EndGestureEvent::UnconsumedEvent(unconsumed_event),
                reason,
            ) => {
                // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                fx_log_info!("touchpad: {} ended: {}", type_name, reason);
                if unconsumed_event.contacts.len() > 0 {
                    self.handle_event_while_chain(unconsumed_event)
                } else {
                    self.handle_event_while_idle(unconsumed_event)
                }
            }
            ProcessNewEventResult::EndGesture(EndGestureEvent::NoEvent, reason) => {
                // TODO(https://fxbug.dev/105588): Gate log message on dynamic opt-in.
                fx_log_info!("touchpad: {} ended: {}", type_name, reason);
                if has_finger_contact {
                    (MutableState::Chain, vec![])
                } else {
                    (MutableState::Idle, vec![])
                }
            }
        }
    }

    #[allow(dead_code)] // only called in developer debug builds
    fn log_mutable_state(&self) {
        match &*self.mutable_state.borrow() {
            MutableState::Idle => fx_log_info!("touchpad: Idle"),
            MutableState::Chain => fx_log_info!("touchpad: Chain"),
            MutableState::Matching { contenders, matched_contenders, buffered_events } => {
                fx_log_info!(
                    "touchpad: Matching {{ \
                                contenders: [ {} ], \
                                matched_contenders: [ {} ], \
                                n_buffered_events: {} \
                              }}",
                    contenders.iter().fold(String::new(), |accum, item| {
                        accum + &format!("{}, ", item.get_type_name())
                    }),
                    matched_contenders.iter().fold(String::new(), |accum, item| {
                        accum + &format!("{}, ", item.get_type_name())
                    }),
                    buffered_events.len()
                )
            }
            MutableState::Forwarding { winner } => {
                fx_log_info!("touchpad: Forwarding {{ winner: {} }}", winner.get_type_name());
            }
            MutableState::Invalid => fx_log_info!("touchpad: Invalid"),
        }
    }
}

#[async_trait(?Send)]
impl<ContenderFactory: Fn() -> Vec<Box<dyn Contender>>> InputHandler
    for GestureArena<ContenderFactory>
{
    /// Handle `input_event`:
    /// * For an unhandled touchpad event, try to interpret as a touchpad gesture.
    /// * For a keyboard event, log the timestamp, and pass the event onwards.
    /// * For any other event, pass onwards without logging.
    async fn handle_input_event(
        self: std::rc::Rc<Self>,
        input_event: input_device::InputEvent,
    ) -> Vec<input_device::InputEvent> {
        match input_event {
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Touchpad(ref touchpad_event),
                ref event_time,
                device_descriptor:
                    input_device::InputDeviceDescriptor::Touchpad(ref touchpad_descriptor),
                handled: input_device::Handled::No,
                ..
            } => {
                match self.handle_touchpad_event(event_time, touchpad_event, touchpad_descriptor) {
                    Ok(r) => r,
                    Err(e) => {
                        fx_log_err!("{}", e);
                        vec![input_event]
                    }
                }
            }
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Keyboard(_),
                event_time,
                ..
            } => {
                log_keyboard_event_timestamp(
                    self.inspect_log.borrow_mut().create_entry(),
                    event_time,
                );
                vec![input_event]
            }
            _ => {
                vec![input_event]
            }
        }
    }
}

/// Returns the multiplier to translate position data for the device described by
/// `device_descriptor`, from the units in the corresponding `TouchpadEvent`, to
/// millimeters.
///
/// For example, if this function returns 1000, then the original data are in
/// micrometers, and dividing by 1000 will yield millimeters.
fn get_position_divisor_to_mm(
    touchpad_descriptor: &touch_binding::TouchpadDeviceDescriptor,
) -> Result<f32, Error> {
    const EXPONENT_MILLIS: i32 = -3;
    let divisors: Vec<_> = touchpad_descriptor
        .contacts
        .iter()
        .enumerate()
        .map(|(i, contact_descriptor)| {
            match (contact_descriptor.x_unit, contact_descriptor.y_unit) {
                (
                    fidl_input_report::Unit {
                        type_: fidl_input_report::UnitType::Meters,
                        exponent: exponent_x,
                    },
                    fidl_input_report::Unit {
                        type_: fidl_input_report::UnitType::Meters,
                        exponent: exponent_y,
                    },
                ) => {
                    if exponent_x == exponent_y {
                        Ok(f32::powi(10.0, EXPONENT_MILLIS - exponent_x))
                    } else {
                        Err(format!(
                            "contact {}: mismatched exponents x={}, y={}",
                            i, exponent_x, exponent_y
                        ))
                    }
                }
                (
                    fidl_input_report::Unit { type_: x_unit_type, .. },
                    fidl_input_report::Unit { type_: fidl_input_report::UnitType::Meters, .. },
                ) => Err(format!(
                    "contact {}: expected x-unit-type of Meters but got {:?}",
                    i, x_unit_type
                )),
                (
                    fidl_input_report::Unit { type_: fidl_input_report::UnitType::Meters, .. },
                    fidl_input_report::Unit { type_: y_unit_type, .. },
                ) => Err(format!(
                    "contact {}: expected y-unit-type of Meters but got {:?}",
                    i, y_unit_type
                )),
                (
                    fidl_input_report::Unit { type_: x_unit_type, .. },
                    fidl_input_report::Unit { type_: y_unit_type, .. },
                ) => Err(format!(
                    "contact {}: expected x and y unit-types of Meters but got x={:?} and y={:?}",
                    i, x_unit_type, y_unit_type
                )),
            }
        })
        .collect();

    let (divisors, errors): (Vec<_>, Vec<_>) =
        divisors.into_iter().fold((vec![], vec![]), |(mut divisors, mut errors), divisor| {
            match divisor {
                Ok(d) => divisors.push(d),
                Err(e) => errors.push(e),
            };
            (divisors, errors)
        });

    if !errors.is_empty() {
        return Err(format_err!(errors
            .into_iter()
            .fold(String::new(), |prev_err_msgs, this_err_msg| prev_err_msgs
                + &this_err_msg
                + ", ")));
    }

    let first_divisor = match divisors.first() {
        Some(&divisor) => divisor,
        None => return Err(format_err!("no contact descriptors!")),
    };

    if divisors.iter().any(|&divisor| divisor != first_divisor) {
        return Err(format_err!(divisors
            .iter()
            .enumerate()
            .filter(|(_i, &divisor)| divisor != first_divisor)
            .map(|(i, divisor)| format!(
                "contact {} has a different divisor than the first contact ({:?} != {:?})",
                i, divisor, first_divisor,
            ))
            .fold(String::new(), |prev_err_msgs, this_err_msg| prev_err_msgs
                + &this_err_msg
                + ", ")));
    }

    Ok(first_divisor)
}

fn log_keyboard_event_timestamp(log_entry_node: &InspectNode, driver_timestamp: zx::Time) {
    use inspect_keys::*;
    log_entry_node.atomic_update(|log_entry_node| {
        log_entry_node.record_child(&*KEY_EVENT_NODE, |key_event_node| {
            log_common(key_event_node, driver_timestamp);
        })
    });
}

#[cfg(test)]
mod tests {
    mod utils {
        use {
            super::super::{
                Contender, ExamineEventResult, MatchedContender, ProcessBufferedEventsResult,
                ProcessNewEventResult, TouchpadEvent, VerifyEventResult, Winner, PRIMARY_BUTTON,
            },
            crate::{input_device, keyboard_binding, mouse_binding, touch_binding, Position},
            assert_matches::assert_matches,
            fidl_fuchsia_input_report as fidl_input_report, fuchsia_zircon as zx,
            maplit::hashset,
            std::{
                cell::{Cell, RefCell},
                rc::Rc,
            },
        };

        /// The gesture arena is mostly agnostic to the event details. Consequently, most
        /// tests can use the same lightly populated touchpad event.
        pub(super) fn make_unhandled_touchpad_event() -> input_device::InputEvent {
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                event_time: zx::Time::ZERO,
                trace_id: None,
                handled: input_device::Handled::No,
            }
        }

        /// The gesture arena is mostly agnostic to the event details. Consequently, most
        /// tests can use the same lightly populated mouse event.
        pub(super) fn make_unhandled_mouse_event() -> input_device::InputEvent {
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Mouse(mouse_binding::MouseEvent {
                    location: mouse_binding::MouseLocation::Relative(
                        mouse_binding::RelativeLocation {
                            counts: Position::zero(),
                            millimeters: Position::zero(),
                        },
                    ),
                    wheel_delta_h: None,
                    wheel_delta_v: None,
                    phase: mouse_binding::MousePhase::Move,
                    affected_buttons: hashset! {},
                    pressed_buttons: hashset! {},
                    is_precision_scroll: None,
                }),
                device_descriptor: make_mouse_descriptor(),
                event_time: zx::Time::ZERO,
                trace_id: None,
                handled: input_device::Handled::No,
            }
        }

        /// The gesture arena is mostly agnostic to the event details. Consequently, most
        /// tests can use the same lightly populated keyboard event.
        pub(super) fn make_unhandled_keyboard_event() -> input_device::InputEvent {
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Keyboard(
                    keyboard_binding::KeyboardEvent::new(
                        fidl_fuchsia_input::Key::A,
                        fidl_fuchsia_ui_input3::KeyEventType::Pressed,
                    ),
                ),
                device_descriptor: make_keyboard_descriptor(),
                event_time: zx::Time::ZERO,
                trace_id: None,
                handled: input_device::Handled::No,
            }
        }

        pub(super) fn make_touchpad_descriptor() -> input_device::InputDeviceDescriptor {
            input_device::InputDeviceDescriptor::Touchpad(touch_binding::TouchpadDeviceDescriptor {
                device_id: 1,
                contacts: vec![touch_binding::ContactDeviceDescriptor {
                    x_range: fidl_input_report::Range { min: 0, max: 10_000 },
                    y_range: fidl_input_report::Range { min: 0, max: 10_000 },
                    x_unit: fidl_input_report::Unit {
                        type_: fidl_input_report::UnitType::Meters,
                        exponent: -6,
                    },
                    y_unit: fidl_input_report::Unit {
                        type_: fidl_input_report::UnitType::Meters,
                        exponent: -6,
                    },
                    pressure_range: None,
                    width_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                    height_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                }],
            })
        }

        pub(super) fn make_mouse_descriptor() -> input_device::InputDeviceDescriptor {
            input_device::InputDeviceDescriptor::Mouse(mouse_binding::MouseDeviceDescriptor {
                device_id: 2,
                absolute_x_range: Some(fidl_input_report::Range { min: -127, max: 127 }),
                absolute_y_range: Some(fidl_input_report::Range { min: -127, max: 127 }),
                wheel_v_range: None,
                wheel_h_range: None,
                buttons: Some(vec![PRIMARY_BUTTON]),
                counts_per_mm: mouse_binding::DEFAULT_COUNTS_PER_MM,
            })
        }

        fn make_keyboard_descriptor() -> input_device::InputDeviceDescriptor {
            input_device::InputDeviceDescriptor::Keyboard(
                keyboard_binding::KeyboardDeviceDescriptor {
                    device_id: 3,
                    device_info: fidl_fuchsia_input_report::DeviceInfo {
                        product_id: 0,
                        vendor_id: 0,
                        version: 0,
                    },
                    keys: vec![fidl_fuchsia_input::Key::A],
                },
            )
        }

        #[derive(Clone, Debug)]
        /// Provides the ability to
        ///
        /// 1. Plumb a fake `Contender` into a `GestureArena`
        /// 2. Fake interactions between the two
        /// 3. Inspect interactions between the two
        ///
        /// To plumb the fake, pass a `Clone` of the `StubContender` to
        /// `ContenderFactoryOnce`. To fake or inspect interactions, call the
        /// inherent methods on the struct.
        pub(super) struct StubContender {
            inner: Rc<RefCell<StubContenderInner>>,
            start_from_idle: bool,
        }

        impl StubContender {
            pub(super) fn new() -> Self {
                Self {
                    inner: Rc::new(RefCell::new(StubContenderInner {
                        next_result: None,
                        calls_received: 0,
                        last_touchpad_event: None,
                    })),
                    start_from_idle: false,
                }
            }

            pub(super) fn new_start_from_idle() -> Self {
                Self {
                    inner: Rc::new(RefCell::new(StubContenderInner {
                        next_result: None,
                        calls_received: 0,
                        last_touchpad_event: None,
                    })),
                    start_from_idle: true,
                }
            }

            /// Set the value to be returned on the next call to `examine_event()`.
            /// Aborts if a value is already set, since that suggests that a previously
            /// expected call was never made.
            pub(super) fn set_next_result(&self, next_result: ExamineEventResult) {
                self.assert_next_result_is_none();
                self.inner.borrow_mut().next_result = Some(next_result);
            }

            pub(super) fn assert_next_result_is_none(&self) {
                assert_matches!(self.inner.borrow().next_result, None);
            }

            pub(super) fn calls_received(&self) -> usize {
                self.inner.borrow().calls_received
            }

            pub(super) fn ref_count(&self) -> usize {
                Rc::strong_count(&self.inner)
            }

            pub(super) fn get_last_touchpad_event(&self) -> Option<TouchpadEvent> {
                self.inner.borrow_mut().last_touchpad_event.take()
            }
        }

        #[derive(Debug)]
        struct StubContenderInner {
            next_result: Option<ExamineEventResult>,
            calls_received: usize,
            last_touchpad_event: Option<TouchpadEvent>,
        }

        /// A factory that returns `Vec<Box<dyn Contender>>` from `contenders` on the
        /// first call, and `panic()`-s on the second call.
        ///
        /// Useful because
        /// a) `GestureArena` requires that the factory be invocable multiple times, BUT
        /// b) most of the gesture arena tests don't expect that to happen, SO
        /// c) the tests don't have logic to handle that case.
        ///
        /// To use: pass a lambda which invokes `make_contenders()` to
        /// `GestureArena::new_for_test()`.
        pub(super) struct ContenderFactoryOnce {
            contenders: Cell<Option<Vec<StubContender>>>,
        }

        impl ContenderFactoryOnce {
            pub(super) fn new(contenders: Vec<StubContender>) -> Self {
                Self { contenders: Cell::new(Some(contenders)) }
            }

            pub(super) fn make_contenders(&self) -> Vec<Box<dyn Contender>> {
                self.contenders
                    .take()
                    .expect("`contenders` has been consumed")
                    .into_iter()
                    .map(std::convert::From::<StubContender>::from)
                    .collect()
            }
        }

        impl Contender for StubContender {
            fn examine_event(self: Box<Self>, event: &TouchpadEvent) -> ExamineEventResult {
                let mut inner = self.inner.borrow_mut();
                inner.calls_received += 1;
                inner.last_touchpad_event = Some(event.clone());
                inner.next_result.take().unwrap_or_else(|| {
                    panic!("missing `next_result` on call {}", inner.calls_received)
                })
            }

            fn start_from_idle(&self) -> bool {
                self.start_from_idle
            }
        }

        // A fake contender that always returns itself. Useful for keeping the
        // gesture arena from exiting the matching state. Keeping the arena in
        // the matching state eliminates the need for some tests to provide
        // return values for `process_buffered_events()`.
        #[derive(Debug)]
        pub(super) struct ContenderForever {}

        impl Contender for ContenderForever {
            fn examine_event(self: Box<Self>, _event: &TouchpadEvent) -> ExamineEventResult {
                ExamineEventResult::Contender(self)
            }
        }

        #[derive(Clone, Debug)]
        pub(super) struct StubMatchedContender {
            inner: Rc<RefCell<StubMatchedContenderInner>>,
        }

        impl StubMatchedContender {
            pub(super) fn new() -> Self {
                Self {
                    inner: Rc::new(RefCell::new(StubMatchedContenderInner {
                        next_verify_event_result: None,
                        next_process_buffered_events_result: None,
                        verify_event_calls_received: 0,
                        process_buffered_events_calls_received: 0,
                        last_process_buffered_events_args: None,
                    })),
                }
            }

            /// Set the value to be returned on the next call to `verify_event()`.
            /// Aborts if a value is already set, since that suggests that a previously
            /// expected call was never made.
            pub(super) fn set_next_verify_event_result(&self, next_result: VerifyEventResult) {
                self.assert_next_verify_event_result_is_none();
                self.inner.borrow_mut().next_verify_event_result = Some(next_result);
            }

            fn assert_next_verify_event_result_is_none(&self) {
                assert_matches!(self.inner.borrow().next_verify_event_result, None);
            }

            pub(super) fn verify_event_calls_received(&self) -> usize {
                self.inner.borrow().verify_event_calls_received
            }

            /// Set the value to be returned on the next call to `process_buffered_events()`.
            /// Aborts if a value is already set, since that suggests that a previously
            /// expected call was never made.
            pub(super) fn set_next_process_buffered_events_result(
                &self,
                next_result: ProcessBufferedEventsResult,
            ) {
                self.assert_next_process_buffered_events_result_is_none();
                self.inner.borrow_mut().next_process_buffered_events_result = Some(next_result);
            }

            pub(super) fn get_last_processed_buffered_events_args(
                &self,
            ) -> Option<Vec<TouchpadEvent>> {
                self.inner.borrow_mut().last_process_buffered_events_args.take()
            }

            fn assert_next_process_buffered_events_result_is_none(&self) {
                assert_matches!(self.inner.borrow().next_process_buffered_events_result, None);
            }

            pub(super) fn ref_count(&self) -> usize {
                Rc::strong_count(&self.inner)
            }
        }

        #[derive(Debug)]
        struct StubMatchedContenderInner {
            next_verify_event_result: Option<VerifyEventResult>,
            next_process_buffered_events_result: Option<ProcessBufferedEventsResult>,
            verify_event_calls_received: usize,
            process_buffered_events_calls_received: usize,
            last_process_buffered_events_args: Option<Vec<TouchpadEvent>>,
        }

        impl MatchedContender for StubMatchedContender {
            fn verify_event(self: Box<Self>, _event: &TouchpadEvent) -> VerifyEventResult {
                let mut inner = self.inner.borrow_mut();
                inner.verify_event_calls_received += 1;
                inner.next_verify_event_result.take().unwrap_or_else(|| {
                    panic!(
                        "missing `next_verify_event_result` on call {}",
                        inner.verify_event_calls_received
                    )
                })
            }

            fn process_buffered_events(
                self: Box<Self>,
                events: Vec<TouchpadEvent>,
            ) -> ProcessBufferedEventsResult {
                let mut inner = self.inner.borrow_mut();
                inner.last_process_buffered_events_args = Some(events);
                inner.process_buffered_events_calls_received += 1;
                inner.next_process_buffered_events_result.take().unwrap_or_else(|| {
                    panic!(
                        "missing `next_process_buffered_events_result` on call {}",
                        inner.process_buffered_events_calls_received
                    )
                })
            }
        }

        #[derive(Clone, Debug)]
        pub(super) struct StubWinner {
            inner: Rc<RefCell<StubWinnerInner>>,
        }

        impl StubWinner {
            pub(super) fn new() -> Self {
                Self {
                    inner: Rc::new(RefCell::new(StubWinnerInner {
                        next_result: None,
                        calls_received: 0,
                    })),
                }
            }

            /// Set the value to be returned on the next call to `examine_event()`.
            pub(super) fn set_next_result(&self, next_result: ProcessNewEventResult) {
                self.inner.borrow_mut().next_result = Some(next_result);
            }

            pub(super) fn calls_received(&self) -> usize {
                self.inner.borrow().calls_received
            }
        }

        #[derive(Debug)]
        struct StubWinnerInner {
            next_result: Option<ProcessNewEventResult>,
            calls_received: usize,
        }

        impl Winner for StubWinner {
            fn process_new_event(self: Box<Self>, _event: TouchpadEvent) -> ProcessNewEventResult {
                let mut inner = self.inner.borrow_mut();
                inner.calls_received += 1;
                inner.next_result.take().unwrap_or_else(|| {
                    panic!("missing `next_result` on call {}", inner.calls_received)
                })
            }
        }

        impl From<StubContender> for Box<dyn Contender> {
            fn from(stub_contender: StubContender) -> Box<dyn Contender> {
                Box::new(stub_contender)
            }
        }

        impl From<ContenderForever> for Box<dyn Contender> {
            fn from(contender_forever: ContenderForever) -> Box<dyn Contender> {
                Box::new(contender_forever)
            }
        }

        impl From<StubMatchedContender> for Box<dyn MatchedContender> {
            fn from(stub_matched_contender: StubMatchedContender) -> Box<dyn MatchedContender> {
                Box::new(stub_matched_contender)
            }
        }

        impl From<StubWinner> for Box<dyn Winner> {
            fn from(stub_winner: StubWinner) -> Box<dyn Winner> {
                Box::new(stub_winner)
            }
        }
    }

    mod idle_chain_states {
        use {
            super::{
                super::{
                    Contender, ExamineEventResult, GestureArena, InputHandler, MutableState,
                    TouchpadEvent,
                },
                utils::{
                    make_touchpad_descriptor, make_unhandled_keyboard_event,
                    make_unhandled_mouse_event, make_unhandled_touchpad_event,
                    ContenderFactoryOnce, StubContender, StubMatchedContender,
                },
            },
            crate::{input_device, touch_binding, Position},
            assert_matches::assert_matches,
            fuchsia_zircon as zx,
            maplit::hashset,
            std::{cell::Cell, cell::RefCell, rc::Rc},
            test_case::test_case,
        };

        fn make_gesture_arena_with_state(
            contender_factory: ContenderFactoryOnce,
            state: MutableState,
        ) -> Rc<GestureArena<impl Fn() -> Vec<Box<dyn Contender>>>> {
            Rc::new(GestureArena {
                contender_factory: move || contender_factory.make_contenders(),
                mutable_state: RefCell::new(state),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            })
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_contender_factory_on_touchpad_event(state: MutableState) {
            let contender_factory_called = Cell::new(false);
            let contender_factory = || {
                contender_factory_called.set(true);
                Vec::new()
            };
            let arena = Rc::new(GestureArena {
                contender_factory,
                mutable_state: RefCell::new(state),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            });
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            assert!(contender_factory_called.get());
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_contender_factory_on_mouse_event(state: MutableState) {
            let contender_factory_called = Cell::new(false);
            let contender_factory = || {
                contender_factory_called.set(true);
                Vec::new()
            };

            let arena = Rc::new(GestureArena {
                contender_factory,
                mutable_state: RefCell::new(state),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            });
            arena.handle_input_event(make_unhandled_mouse_event()).await;
            assert!(!contender_factory_called.get());
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_contender_factory_on_keyboard_event(state: MutableState) {
            let contender_factory_called = Cell::new(false);
            let contender_factory = || {
                contender_factory_called.set(true);
                Vec::new()
            };

            let arena = Rc::new(GestureArena {
                contender_factory,
                mutable_state: RefCell::new(state),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            });
            arena.handle_input_event(make_unhandled_keyboard_event()).await;
            assert!(!contender_factory_called.get());
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn calls_examine_event_on_contender(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            pretty_assertions::assert_eq!(contender.calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn calls_examine_event_on_idle_only_contender_while_idle() {
            let contender = StubContender::new_start_from_idle();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, MutableState::Idle);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            pretty_assertions::assert_eq!(contender.calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_calls_examine_event_on_idle_only_contender_while_chain() {
            let contender = StubContender::new_start_from_idle();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, MutableState::Chain);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            pretty_assertions::assert_eq!(contender.calls_received(), 0);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn calls_examine_event_on_all_contenders_even_if_first_matches(state: MutableState) {
            let first_contender = StubContender::new();
            let second_contender = StubContender::new();
            let contender_factory =
                ContenderFactoryOnce::new(vec![first_contender.clone(), second_contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            first_contender.set_next_result(ExamineEventResult::MatchedContender(
                StubMatchedContender::new().into(),
            ));
            second_contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            pretty_assertions::assert_eq!(first_contender.calls_received(), 1);
            pretty_assertions::assert_eq!(second_contender.calls_received(), 1);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn retains_reference_to_replacement_contender(state: MutableState) {
            // Create a gesture arena which will instantiate a `StubContender`.
            let initial_contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![initial_contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);

            // Configure `initial_contender` to return a new `StubContender` when
            // `examine_event()` is called.
            let replacement_contender = StubContender::new();
            initial_contender.set_next_result(ExamineEventResult::Contender(
                replacement_contender.clone().into(),
            ));

            // Process a touchpad event. This should cause `arena` to consume the
            // `ExamineEventResult` set above.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify that the `ExamineEventResult` was, in fact, consumed.
            initial_contender.assert_next_result_is_none();

            // Finally, verify that `replacement_contender` has two references.
            //
            // Note that:
            // * One of the references is from `replacement_contender`.
            // * The second one cannot be from the `ExamineEventResult` above,
            //   because the `ExamineEventResult` was consumed.
            //
            // Hence: the second reference (if it exists) must be in the gesture arena.
            pretty_assertions::assert_eq!(replacement_contender.ref_count(), 2);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn retains_reference_to_matched_contender(state: MutableState) {
            // Create a gesture arena which will instantiate a `StubContender`.
            let initial_contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![initial_contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            // Configure `initial_contender` to return a `StubMatchedContender` when
            // `examine_event()` is called.
            let replacement_contender = StubMatchedContender::new();
            initial_contender.set_next_result(ExamineEventResult::MatchedContender(
                replacement_contender.clone().into(),
            ));

            // Process a touchpad event. This should cause `arena` to consume the
            // `ExamineEventResult` set above.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify that the `ExamineEventResult` was, in fact, consumed.
            initial_contender.assert_next_result_is_none();

            // Finally, verify that `replacement_contender` has two references.
            //
            // Note that:
            // * One of the references is from `replacement_contender`.
            // * The second one cannot be from the `ExamineEventResult` above,
            //   because the `ExamineEventResult` was consumed.
            //
            // Hence: the second reference (if it exists) must be in the gesture arena.
            pretty_assertions::assert_eq!(replacement_contender.ref_count(), 2);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn retains_touchpad_event_when_entering_matching(state: MutableState) {
            // Create a gesture arena which will instantiate a `StubContender`.
            let initial_contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![initial_contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            // Create the event which will be sent to the arena.
            let touchpad_event = input_device::InputEvent {
                event_time: zx::Time::from_nanos(123456),
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![],
                        pressed_buttons: hashset! {0},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                trace_id: None,
                handled: input_device::Handled::No,
            };

            // Configure `initial_contender` to return a `StubMatchedContender` when
            // `examine_event()` is called.
            initial_contender.set_next_result(ExamineEventResult::MatchedContender(
                StubMatchedContender::new().into(),
            ));

            // Process `touchpad_event`. Because `initial_contender` returns
            // `ExamineEventResult::MatchedContender`, the gesture arena will enter
            // the `Matching` state.
            arena.clone().handle_input_event(touchpad_event).await;

            // Verify that `arena` retained the details of `touchpad_event`.
            assert_matches!(
                &*arena.mutable_state.borrow(),
                MutableState::Matching {
                    contenders: _,
                    matched_contenders: _,
                    buffered_events
                } => pretty_assertions::assert_eq!(
                    buffered_events.as_slice(),
                    [TouchpadEvent {
                        timestamp: zx::Time::from_nanos(123456),
                        pressed_buttons: vec![0],
                        contacts: vec![],
                    }]
                )
            );
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_mismatch_entering_chain(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));

            let touchpad_event = input_device::InputEvent {
                event_time: zx::Time::from_nanos(123456),
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![touch_binding::TouchContact {
                            id: 1,
                            position: Position { x: 0.0, y: 0.0 },
                            pressure: None,
                            contact_size: None,
                        }],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                trace_id: None,
                handled: input_device::Handled::No,
            };

            pretty_assertions::assert_eq!(
                arena.clone().handle_input_event(touchpad_event).await,
                vec![]
            );

            assert_matches!(*arena.mutable_state.borrow(), MutableState::Chain);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_mismatch_entering_idle(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            pretty_assertions::assert_eq!(
                arena.clone().handle_input_event(make_unhandled_touchpad_event()).await,
                vec![]
            );

            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_when_entering_matching(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Contender(StubContender::new().into()));
            pretty_assertions::assert_eq!(
                arena.handle_input_event(make_unhandled_touchpad_event()).await,
                vec![]
            );
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn enters_idle_on_mismatch(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn enters_matching_on_contender_result(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::Contender(StubContender::new().into()));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Matching { .. });
        }

        #[test_case(MutableState::Idle; "idle")]
        #[test_case(MutableState::Chain; "chain")]
        #[fuchsia::test(allow_stalls = false)]
        async fn enters_matching_on_matched_contender_result(state: MutableState) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = make_gesture_arena_with_state(contender_factory, state);
            contender.set_next_result(ExamineEventResult::MatchedContender(
                StubMatchedContender::new().into(),
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Matching { .. });
        }
    }

    mod matching_state {
        use {
            super::{
                super::{
                    Contender, ExamineEventResult, GestureArena, InputHandler, MouseEvent,
                    MutableState, ProcessBufferedEventsResult, RecognizedGesture, TouchpadEvent,
                    VerifyEventResult, PRIMARY_BUTTON,
                },
                utils::{
                    make_touchpad_descriptor, make_unhandled_keyboard_event,
                    make_unhandled_mouse_event, make_unhandled_touchpad_event, ContenderForever,
                    StubContender, StubMatchedContender, StubWinner,
                },
            },
            crate::{input_device, mouse_binding, touch_binding, Position},
            assert_matches::assert_matches,
            fuchsia_zircon as zx,
            maplit::hashset,
            pretty_assertions::assert_eq,
            std::{cell::RefCell, rc::Rc},
            test_case::test_case,
        };

        fn make_matching_arena(
            contenders: Vec<StubContender>,
            matched_contenders: Vec<StubMatchedContender>,
            buffered_events: Vec<TouchpadEvent>,
            contender_forever: Option<ContenderForever>,
        ) -> Rc<GestureArena<impl Fn() -> Vec<Box<dyn Contender>>>> {
            Rc::new(GestureArena {
                contender_factory: || {
                    // Note: printing instead of panic()-ing here yields better
                    // failure messages from the tests.
                    eprintln!("factory invoked in matching state");
                    vec![]
                },
                mutable_state: RefCell::new(MutableState::Matching {
                    contenders: {
                        contenders
                            .into_iter()
                            .map(std::convert::From::<StubContender>::from)
                            .chain(
                                contender_forever
                                    .into_iter()
                                    .map(std::convert::From::<ContenderForever>::from),
                            )
                            .collect()
                    },
                    matched_contenders: {
                        matched_contenders
                            .into_iter()
                            .map(std::convert::From::<StubMatchedContender>::from)
                            .collect()
                    },
                    buffered_events,
                }),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            })
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_examine_and_verify_event_on_touchpad_event() {
            let contender = StubContender::new();
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![contender.clone()],
                vec![matched_contender.clone()],
                vec![],
                None,
            );
            contender.set_next_result(ExamineEventResult::Contender(contender.clone().into()));
            matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            assert_eq!(contender.calls_received(), 1);
            assert_eq!(matched_contender.verify_event_calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_examine_or_verify_event_on_mouse_event() {
            let contender = StubContender::new();
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![contender.clone()],
                vec![matched_contender.clone()],
                vec![],
                None,
            );
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_mouse_event()).await;
            assert_eq!(contender.calls_received(), 0);
            assert_eq!(matched_contender.verify_event_calls_received(), 0);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_examine_or_verify_event_on_keyboard_event() {
            let contender = StubContender::new();
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![contender.clone()],
                vec![matched_contender.clone()],
                vec![],
                None,
            );
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_keyboard_event()).await;
            assert_eq!(contender.calls_received(), 0);
            assert_eq!(matched_contender.verify_event_calls_received(), 0);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_repeat_event_to_matched_contender_returned_by_examine_event() {
            let contender = StubContender::new();
            let arena = make_matching_arena(
                vec![contender.clone()],
                vec![],
                vec![],
                // Make sure that `arena` does not progress to the forwarding state,
                // even if the logic _does_ repeat the event.
                Some(ContenderForever {}),
            );

            // Configure `contender` to give a `MatchContender` to `arena`.
            let matched_contender = StubMatchedContender::new();
            contender.set_next_result(ExamineEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));

            // Set the return value for `matched_contender`. If the implementation
            // is buggy, and `verify_event()` is called, having a return value for
            // that call makes this test fail at the final assertion, which is easier
            // to understand.
            matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));

            // Send the touchpad event, and validate that the arena did not call
            // `verify_event()` on the newly returned `MatchedContender`.
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            assert_eq!(matched_contender.verify_event_calls_received(), 0);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_examine_event_for_new_event_with_contender_replaced_by_contender() {
            // Set up an arena with a `StubContender` that will continue the
            // matching process with a new `StubContender`.
            let initial_contender = StubContender::new();
            let arena = make_matching_arena(vec![initial_contender.clone()], vec![], vec![], None);
            let replacement_contender = StubContender::new();
            initial_contender.set_next_result(ExamineEventResult::Contender(
                replacement_contender.clone().into(),
            ));

            // Process a touchpad event. This should cause `arena` to replace
            // `initial_contender` with `replacement_contender`.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Process another touchpad event. This should cause `arena` to invoke
            // `examine_event()` on `replacement_contender`.
            replacement_contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify that `replacement_contender` was called.
            assert_eq!(replacement_contender.calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_verify_event_for_new_event_with_contender_replaced_by_matched_contender() {
            // Set up an arena with
            // * a `StubContender` that will continue the matching process with
            //   a new `StubMatchedContender`
            // * a `ContenderForever`, which will keep the arena in the matching
            //   state
            let initial_contender = StubContender::new();
            let arena = make_matching_arena(
                vec![initial_contender.clone()],
                vec![],
                vec![],
                Some(ContenderForever {}),
            );
            let replacement_contender = StubMatchedContender::new();
            initial_contender.set_next_result(ExamineEventResult::MatchedContender(
                replacement_contender.clone().into(),
            ));

            // Process a touchpad event. This should cause `arena` to replace
            // `initial_contender` with `replacement_contender`.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Process another touchpad event. This should cause `arena` to invoke
            // `examine_event()` on `replacement_contender`.
            replacement_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify that `replacement_contender` was called.
            assert_eq!(replacement_contender.verify_event_calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_verify_event_for_new_event_with_matched_contender_replaced_by_matched_contender(
        ) {
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![],
                vec![matched_contender.clone()],
                vec![],
                // Ensure that `arena` remains in the matching state. This simplifies
                // the test by eliminating the need to provide a response to a
                // potential `process_buffered_events()` call.
                Some(ContenderForever {}),
            );

            // Configure `matched_contender` to replace itself with
            // `replacement_matched_contender`.
            let replacement_matched_contender = StubMatchedContender::new();
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                replacement_matched_contender.clone().into(),
            ));

            // Process a touchpad event. This should cause `arena` to retain
            // replace `contender` with `replacement_contender`.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Set a return value for the expected call on `replacement_contender`.
            replacement_matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));

            // Process another touchpad event, and verify that `replacement_contender`
            // is called.
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_eq!(replacement_matched_contender.verify_event_calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_mismatch_entering_idle() {
            let contender = StubContender::new();
            let arena = make_matching_arena(vec![contender.clone()], vec![], vec![], None);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            assert_eq!(
                arena.clone().handle_input_event(make_unhandled_touchpad_event()).await,
                vec![]
            );
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_mismatch_entering_chain() {
            let contender = StubContender::new();
            let arena = make_matching_arena(vec![contender.clone()], vec![], vec![], None);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));

            let touchpad_event = input_device::InputEvent {
                event_time: zx::Time::from_nanos(123456),
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![touch_binding::TouchContact {
                            id: 1,
                            position: Position { x: 0.0, y: 0.0 },
                            pressure: None,
                            contact_size: None,
                        }],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                trace_id: None,
                handled: input_device::Handled::No,
            };

            pretty_assertions::assert_eq!(
                arena.clone().handle_input_event(touchpad_event).await,
                vec![]
            );

            assert_matches!(*arena.mutable_state.borrow(), MutableState::Chain);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_contender() {
            let contender = StubContender::new();
            let arena = make_matching_arena(vec![contender.clone()], vec![], vec![], None);
            contender.set_next_result(ExamineEventResult::Contender(contender.clone().into()));
            assert_eq!(arena.handle_input_event(make_unhandled_touchpad_event()).await, vec![]);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_multiple_matched_contenders() {
            let first_matched_contender = StubMatchedContender::new();
            let second_matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![],
                vec![first_matched_contender.clone(), second_matched_contender.clone()],
                vec![],
                None,
            );
            first_matched_contender.set_next_verify_event_result(
                VerifyEventResult::MatchedContender(first_matched_contender.clone().into()),
            );
            second_matched_contender.set_next_verify_event_result(
                VerifyEventResult::MatchedContender(second_matched_contender.clone().into()),
            );
            assert_eq!(arena.handle_input_event(make_unhandled_touchpad_event()).await, vec![]);
        }

        #[test_case(Some(StubWinner::new()); "with_winner")]
        #[test_case(None; "without_winner")]
        #[fuchsia::test(allow_stalls = false)]
        async fn generates_events_from_process_buffered_events_on_single_matched_contender(
            winner: Option<StubWinner>,
        ) {
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(vec![], vec![matched_contender.clone()], vec![], None);
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));
            matched_contender.set_next_process_buffered_events_result(
                ProcessBufferedEventsResult {
                    generated_events: vec![
                        MouseEvent {
                            timestamp: zx::Time::from_nanos(123),
                            mouse_data: mouse_binding::MouseEvent {
                                location: mouse_binding::MouseLocation::Relative(
                                    mouse_binding::RelativeLocation {
                                        counts: Position::zero(),
                                        millimeters: Position::zero(),
                                    },
                                ),
                                wheel_delta_v: None,
                                wheel_delta_h: None,
                                phase: mouse_binding::MousePhase::Down,
                                affected_buttons: hashset! { PRIMARY_BUTTON },
                                pressed_buttons: hashset! { PRIMARY_BUTTON },
                                is_precision_scroll: None,
                            },
                        },
                        MouseEvent {
                            timestamp: zx::Time::from_nanos(456),
                            mouse_data: mouse_binding::MouseEvent {
                                location: mouse_binding::MouseLocation::Relative(
                                    mouse_binding::RelativeLocation {
                                        counts: Position::zero(),
                                        millimeters: Position::zero(),
                                    },
                                ),
                                wheel_delta_v: None,
                                wheel_delta_h: None,
                                phase: mouse_binding::MousePhase::Up,
                                affected_buttons: hashset! { PRIMARY_BUTTON },
                                pressed_buttons: hashset! {},
                                is_precision_scroll: None,
                            },
                        },
                    ],
                    winner: winner.map(std::convert::From::<StubWinner>::from),
                    recognized_gesture: RecognizedGesture::Click,
                },
            );
            assert_matches!(
                arena
                    .handle_input_event(make_unhandled_touchpad_event())
                    .await
                    .as_slice(),
                [
                    input_device::InputEvent {
                        handled: input_device::Handled::No,
                        device_event: input_device::InputDeviceEvent::Mouse(
                            mouse_binding::MouseEvent {
                                pressed_buttons: first_pressed_buttons, ..
                            }
                        ),
                        ..
                    },
                    input_device::InputEvent {
                        handled: input_device::Handled::No,
                        device_event: input_device::InputDeviceEvent::Mouse(
                            mouse_binding::MouseEvent {
                                pressed_buttons: second_pressed_buttons, ..
                            }
                        ),
                        ..
                    },
                ] => {
                    pretty_assertions::assert_eq!(*first_pressed_buttons, hashset! { PRIMARY_BUTTON });
                    pretty_assertions::assert_eq!(*second_pressed_buttons, hashset! {});
                }
            );
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn passes_all_buffered_events_to_process_buffered_events() {
            // Create an arena, and seed it a buffered event (emulating what happens
            // as the arena transitions from Idle to Matching).
            let contender = StubContender::new();
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![contender.clone()],
                vec![matched_contender.clone()],
                vec![TouchpadEvent {
                    timestamp: zx::Time::from_nanos(123),
                    contacts: vec![],
                    pressed_buttons: vec![],
                }],
                None,
            );

            // Send another event to the arena, to exercise the case of an event
            // being buffered because the contest continued.
            contender.set_next_result(ExamineEventResult::Contender(contender.clone().into()));
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));
            arena
                .clone()
                .handle_input_event(input_device::InputEvent {
                    event_time: zx::Time::from_nanos(456),
                    device_event: input_device::InputDeviceEvent::Touchpad(
                        touch_binding::TouchpadEvent {
                            injector_contacts: vec![],
                            pressed_buttons: hashset! {},
                        },
                    ),
                    device_descriptor: make_touchpad_descriptor(),
                    trace_id: None,
                    handled: input_device::Handled::No,
                })
                .await;

            // Send the event that concludes the contest.
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));
            matched_contender.set_next_process_buffered_events_result(
                ProcessBufferedEventsResult {
                    generated_events: vec![],
                    winner: None,
                    recognized_gesture: RecognizedGesture::Palm,
                },
            );
            arena
                .handle_input_event(input_device::InputEvent {
                    event_time: zx::Time::from_nanos(789),
                    device_event: input_device::InputDeviceEvent::Touchpad(
                        touch_binding::TouchpadEvent {
                            injector_contacts: vec![],
                            pressed_buttons: hashset! {},
                        },
                    ),
                    device_descriptor: make_touchpad_descriptor(),
                    trace_id: None,
                    handled: input_device::Handled::No,
                })
                .await;

            // Verify that the contender received all three events.
            assert_eq!(
                matched_contender
                    .get_last_processed_buffered_events_args()
                    .map(|vec| vec
                        .into_iter()
                        .map(|event| event.timestamp.into_nanos())
                        .collect::<Vec<_>>())
                    .as_deref(),
                Some([123, 456, 789].as_slice())
            );
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_idle_when_sole_contender_does_not_match() {
            let contender = StubContender::new();
            let arena = make_matching_arena(vec![contender.clone()], vec![], vec![], None);
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_idle_when_sole_matched_contender_does_not_match() {
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(vec![], vec![matched_contender.clone()], vec![], None);
            matched_contender
                .set_next_verify_event_result(VerifyEventResult::Mismatch("some reason"));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn remains_in_matching_when_a_contender_remains() {
            let contender = StubContender::new();
            let arena = make_matching_arena(vec![contender.clone()], vec![], vec![], None);
            contender.set_next_result(ExamineEventResult::Contender(contender.clone().into()));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Matching { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn remains_in_matching_when_multiple_matched_contenders_remain() {
            let matched_contender_a = StubMatchedContender::new();
            let matched_contender_b = StubMatchedContender::new();
            let arena = make_matching_arena(
                vec![],
                vec![matched_contender_a.clone(), matched_contender_b.clone()],
                vec![],
                None,
            );
            matched_contender_a.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender_a.clone().into(),
            ));
            matched_contender_b.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender_b.clone().into(),
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Matching { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_idle_when_sole_matched_contender_returns_no_winner() {
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(vec![], vec![matched_contender.clone()], vec![], None);
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));
            matched_contender.set_next_process_buffered_events_result(
                ProcessBufferedEventsResult {
                    generated_events: vec![],
                    winner: None,
                    recognized_gesture: RecognizedGesture::Palm,
                },
            );
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_forwarding_when_sole_matched_contender_returns_a_winner() {
            let matched_contender = StubMatchedContender::new();
            let arena = make_matching_arena(vec![], vec![matched_contender.clone()], vec![], None);
            matched_contender.set_next_verify_event_result(VerifyEventResult::MatchedContender(
                matched_contender.clone().into(),
            ));
            matched_contender.set_next_process_buffered_events_result(
                ProcessBufferedEventsResult {
                    generated_events: vec![],
                    winner: Some(StubWinner::new().into()),
                    recognized_gesture: RecognizedGesture::Palm,
                },
            );
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Forwarding { .. });
        }
    }

    mod forwarding_state {
        use {
            super::{
                super::{
                    Contender, EndGestureEvent, ExamineEventResult, GestureArena, InputHandler,
                    MouseEvent, MutableState, ProcessNewEventResult, TouchpadEvent,
                },
                utils::{
                    make_touchpad_descriptor, make_unhandled_keyboard_event,
                    make_unhandled_mouse_event, make_unhandled_touchpad_event, ContenderForever,
                    StubContender, StubWinner,
                },
            },
            crate::{input_device, mouse_binding, touch_binding, Position},
            assert_matches::assert_matches,
            fuchsia_zircon as zx,
            maplit::hashset,
            pretty_assertions::assert_eq,
            std::{
                cell::{Cell, RefCell},
                rc::Rc,
            },
            test_case::test_case,
        };

        /// Creates an arena in the forwarding state, with
        /// a) the given `winner`, and
        /// b) an appropriate contender factory
        ///
        /// If `contender` is `None`, the contender factory will abort on the first
        /// call.
        ///
        /// If `contender` is `Some`, the contender factory will return the content
        /// of the `Option` on the first call, and abort on the second call.
        ///
        /// The former is the common case for the tests. The latter is useful for
        /// tests that exercise the arena's handling of an `EndGesture(Some)`.
        fn make_forwarding_arena(
            winner: StubWinner,
            contender: Option<Box<dyn Contender>>,
        ) -> Rc<GestureArena<impl Fn() -> Vec<Box<dyn Contender>>>> {
            let contender = Cell::new(contender);
            Rc::new(GestureArena {
                contender_factory: move || vec![contender.take().expect("`contender` is None")],
                mutable_state: RefCell::new(MutableState::Forwarding { winner: winner.into() }),
                inspect_log: RefCell::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                    fuchsia_inspect::Inspector::new().root().create_child("some_key"),
                    1,
                )),
            })
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_process_new_event_on_touchpad_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::NoEvent,
                "some reason",
            ));
            arena.handle_input_event(make_unhandled_touchpad_event()).await;
            assert_eq!(winner.calls_received(), 1);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_process_new_event_on_mouse_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::NoEvent,
                "some reason",
            ));
            arena.handle_input_event(make_unhandled_mouse_event()).await;
            assert_eq!(winner.calls_received(), 0);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn does_not_invoke_process_new_event_on_keyboard_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::NoEvent,
                "some reason",
            ));
            arena.handle_input_event(make_unhandled_keyboard_event()).await;
            assert_eq!(winner.calls_received(), 0);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn invokes_process_new_event_for_multiple_new_events() {
            // Create `arena` with `winner` and a `ContenderForever`. The latter
            // makes the test fail in a more useful way if the `GestureArena`
            // is buggy.
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), Some(ContenderForever {}.into()));

            // Send two events to `arena`.
            winner.set_next_result(ProcessNewEventResult::ContinueGesture(
                None,
                winner.clone().into(),
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            winner.set_next_result(ProcessNewEventResult::ContinueGesture(
                None,
                winner.clone().into(),
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify `winner` was called as expected.
            assert_eq!(winner.calls_received(), 2);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_event_on_continue_gesture_with_mouse_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::ContinueGesture(
                Some(MouseEvent {
                    timestamp: zx::Time::from_nanos(123),
                    mouse_data: mouse_binding::MouseEvent {
                        location: mouse_binding::MouseLocation::Relative(
                            mouse_binding::RelativeLocation {
                                counts: Position::zero(),
                                millimeters: Position::zero(),
                            },
                        ),
                        wheel_delta_v: None,
                        wheel_delta_h: None,
                        phase: mouse_binding::MousePhase::Move,
                        affected_buttons: hashset! {},
                        pressed_buttons: hashset! {},
                        is_precision_scroll: None,
                    },
                }),
                winner.clone().into(),
            ));
            assert_matches!(
                arena.handle_input_event(make_unhandled_touchpad_event()).await.as_slice(),
                [
                    input_device::InputEvent {
                        event_time,
                        handled: input_device::Handled::No,
                        device_event: input_device::InputDeviceEvent::Mouse(_),
                        ..
                    },
                ] => pretty_assertions::assert_eq!(*event_time, zx::Time::from_nanos(123))
            );
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_continue_gesture_without_mouse_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::ContinueGesture(
                None,
                winner.clone().into(),
            ));
            pretty_assertions::assert_eq!(
                arena.handle_input_event(make_unhandled_touchpad_event()).await.as_slice(),
                vec![]
            );
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_end_gesture_without_touchpad_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::NoEvent,
                "some reason",
            ));
            pretty_assertions::assert_eq!(
                arena.handle_input_event(make_unhandled_touchpad_event()).await.as_slice(),
                vec![]
            );
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_end_gesture_with_unconsumed_touchpad_event_entering_idle() {
            // Create `arena` with a `StubContender` for processing the unconsumed
            // `TouchpadEvent`.
            let winner = StubWinner::new();
            let contender = StubContender::new();
            let arena = make_forwarding_arena(winner.clone(), Some(contender.clone().into()));
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::UnconsumedEvent(TouchpadEvent {
                    contacts: vec![],
                    pressed_buttons: vec![],
                    timestamp: zx::Time::ZERO,
                }),
                "some reason",
            ));

            // Set a return value for the `examine_event()` call.
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));

            // Verify no events were generated.
            pretty_assertions::assert_eq!(
                arena.clone().handle_input_event(make_unhandled_touchpad_event()).await.as_slice(),
                vec![]
            );

            // Unconsumed event without contact will invoked in handle_event_while_idle,
            // but no contender want this event so stay in Idle state.
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_no_events_on_end_gesture_with_unconsumed_touchpad_event_entering_chain()
        {
            // Create `arena` with a `StubContender` for processing the unconsumed
            // `TouchpadEvent`.
            let winner = StubWinner::new();
            let contender = StubContender::new();
            let arena = make_forwarding_arena(winner.clone(), Some(contender.clone().into()));
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::UnconsumedEvent(TouchpadEvent {
                    contacts: vec![touch_binding::TouchContact {
                        id: 1,
                        position: Position { x: 0.0, y: 0.0 },
                        pressure: None,
                        contact_size: None,
                    }],
                    pressed_buttons: vec![],
                    timestamp: zx::Time::from_nanos(123456),
                }),
                "some reason",
            ));

            // Set a return value for the `examine_event()` call.
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));

            let touchpad_event = input_device::InputEvent {
                event_time: zx::Time::from_nanos(123456),
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![touch_binding::TouchContact {
                            id: 1,
                            position: Position { x: 0.0, y: 0.0 },
                            pressure: None,
                            contact_size: None,
                        }],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                trace_id: None,
                handled: input_device::Handled::No,
            };

            // Verify no events were generated.
            pretty_assertions::assert_eq!(
                arena.clone().handle_input_event(touchpad_event).await.as_slice(),
                vec![]
            );

            // Unconsumed event with contact will invoked in handle_event_while_chain,
            // but no contender want this event so stay in Chain state.
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Chain { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn generates_event_on_end_gesture_with_touchpad_event() {
            // Create `arena` with a `StubContender` for processing the unconsumed
            // `TouchpadEvent`.
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            let mouse_event = MouseEvent {
                timestamp: zx::Time::from_nanos(123),
                mouse_data: mouse_binding::MouseEvent {
                    location: mouse_binding::MouseLocation::Relative(
                        mouse_binding::RelativeLocation {
                            counts: Position::zero(),
                            millimeters: Position::zero(),
                        },
                    ),
                    wheel_delta_v: None,
                    wheel_delta_h: None,
                    phase: mouse_binding::MousePhase::Move,
                    affected_buttons: hashset! {},
                    pressed_buttons: hashset! {},
                    is_precision_scroll: None,
                },
            };
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::GeneratedEvent(mouse_event),
                "some reason",
            ));

            // Verify events were generated.
            assert_matches!(
                arena.handle_input_event(make_unhandled_touchpad_event()).await.as_slice(),
                [
                    input_device::InputEvent {
                        event_time,
                        handled: input_device::Handled::No,
                        device_event: input_device::InputDeviceEvent::Mouse(_),
                        ..
                    },
                ] => pretty_assertions::assert_eq!(*event_time, zx::Time::from_nanos(123))
            );
        }

        #[test_case(Some(MouseEvent{
            timestamp: zx::Time::from_nanos(123),
            mouse_data: mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(
                    mouse_binding::RelativeLocation {
                        counts: Position::zero(),
                        millimeters: Position::zero(),
                    },
                ),
                wheel_delta_v: None,
                wheel_delta_h: None,
                phase: mouse_binding::MousePhase::Move,
                affected_buttons: hashset! {},
                pressed_buttons: hashset! {},
                is_precision_scroll: None,
            },
        }); "with_mouse_event")]
        #[test_case(None; "without_mouse_event")]
        #[fuchsia::test(allow_stalls = false)]
        async fn remains_in_forwarding_on_continue_gesture(mouse_event: Option<MouseEvent>) {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::ContinueGesture(
                mouse_event,
                winner.clone().into(),
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Forwarding { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_idle_on_end_gesture_with_touchpad_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            let mouse_event = MouseEvent {
                timestamp: zx::Time::from_nanos(123),
                mouse_data: mouse_binding::MouseEvent {
                    location: mouse_binding::MouseLocation::Relative(
                        mouse_binding::RelativeLocation {
                            counts: Position::zero(),
                            millimeters: Position::zero(),
                        },
                    ),
                    wheel_delta_v: None,
                    wheel_delta_h: None,
                    phase: mouse_binding::MousePhase::Move,
                    affected_buttons: hashset! {},
                    pressed_buttons: hashset! {},
                    is_precision_scroll: None,
                },
            };
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::GeneratedEvent(mouse_event),
                "some reason",
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle { .. });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_chain_on_end_gesture_with_touchpad_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            let mouse_event = MouseEvent {
                timestamp: zx::Time::from_nanos(123),
                mouse_data: mouse_binding::MouseEvent {
                    location: mouse_binding::MouseLocation::Relative(
                        mouse_binding::RelativeLocation {
                            counts: Position::zero(),
                            millimeters: Position::zero(),
                        },
                    ),
                    wheel_delta_v: None,
                    wheel_delta_h: None,
                    phase: mouse_binding::MousePhase::Move,
                    affected_buttons: hashset! {},
                    pressed_buttons: hashset! {},
                    is_precision_scroll: None,
                },
            };
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::GeneratedEvent(mouse_event),
                "some reason",
            ));
            let touchpad_event = input_device::InputEvent {
                event_time: zx::Time::from_nanos(123456),
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![touch_binding::TouchContact {
                            id: 1,
                            position: Position { x: 0.0, y: 0.0 },
                            pressure: None,
                            contact_size: None,
                        }],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(),
                trace_id: None,
                handled: input_device::Handled::No,
            };
            arena.clone().handle_input_event(touchpad_event).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Chain);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn transitions_to_idle_on_end_gesture_without_touchpad_event() {
            let winner = StubWinner::new();
            let arena = make_forwarding_arena(winner.clone(), None);
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::NoEvent,
                "reason",
            ));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await;
            assert_matches!(*arena.mutable_state.borrow(), MutableState::Idle);
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn starts_new_contest_on_end_gesture_with_touchpad_event() {
            // Set up an arena in the forwarding state with `winner`, and
            // with a factory that will return `contender`. The latter should
            // be called at the start of the new contest.
            let winner = StubWinner::new();
            let contender = StubContender::new();
            let arena = make_forwarding_arena(winner.clone(), Some(contender.clone().into()));

            // Set up `winner` to end the gesture and return an unconsumed event.
            winner.set_next_result(ProcessNewEventResult::EndGesture(
                EndGestureEvent::UnconsumedEvent(TouchpadEvent {
                    timestamp: zx::Time::ZERO,
                    contacts: vec![],
                    pressed_buttons: vec![],
                }),
                "reason",
            ));

            // Set up `contender` to reply to the `examine_event()` call.
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));

            // Send an event into the arena.
            arena.handle_input_event(make_unhandled_touchpad_event()).await;

            // Verify that the arena started a new contest.
            assert_eq!(contender.calls_received(), 1);
        }
    }

    mod touchpad_event_payload {
        use {
            super::{
                super::{ExamineEventResult, GestureArena, InputHandler},
                utils::{ContenderFactoryOnce, StubContender},
            },
            crate::{input_device, touch_binding, Position},
            assert_matches::assert_matches,
            fidl_fuchsia_input_report::{self as fidl_input_report, UnitType},
            fuchsia_zircon as zx,
            maplit::hashset,
            std::rc::Rc,
            test_case::test_case,
            test_util::assert_near,
        };

        fn make_touchpad_descriptor(
            units: Vec<(fidl_input_report::Unit, fidl_input_report::Unit)>,
        ) -> input_device::InputDeviceDescriptor {
            let contacts: Vec<_> = units
                .into_iter()
                .map(|(x_unit, y_unit)| touch_binding::ContactDeviceDescriptor {
                    x_range: fidl_input_report::Range { min: 0, max: 1_000_000 },
                    y_range: fidl_input_report::Range { min: 0, max: 1_000_000 },
                    x_unit,
                    y_unit,
                    pressure_range: None,
                    width_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                    height_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                })
                .collect();
            input_device::InputDeviceDescriptor::Touchpad(touch_binding::TouchpadDeviceDescriptor {
                device_id: 1,
                contacts,
            })
        }

        fn make_unhandled_touchpad_event(
            contact_position_units: Vec<(fidl_input_report::Unit, fidl_input_report::Unit)>,
            positions: Vec<Position>,
        ) -> input_device::InputEvent {
            let injector_contacts: Vec<_> = positions
                .into_iter()
                .enumerate()
                .map(|(i, position)| touch_binding::TouchContact {
                    id: u32::try_from(i).unwrap(),
                    position,
                    contact_size: None,
                    pressure: None,
                })
                .collect();
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts,
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: make_touchpad_descriptor(contact_position_units),
                event_time: zx::Time::ZERO,
                trace_id: None,
                handled: input_device::Handled::No,
            }
        }

        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
            )],
            vec![ Position { x: 200000.0, y: 100000.0 }]; "from_micrometers")]
        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -2 },
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -2 },
            )],
            vec![ Position { x: 20.0, y: 10.0 }]; "from_centimeters")]
        #[fuchsia::test(allow_stalls = false)]
        async fn provides_recognizer_position_in_millimeters(
            contact_position_units: Vec<(fidl_input_report::Unit, fidl_input_report::Unit)>,
            positions: Vec<Position>,
        ) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = Rc::new(GestureArena::new_for_test(
                || contender_factory.make_contenders(),
                &fuchsia_inspect::Inspector::new(),
                1,
            ));
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena
                .handle_input_event(make_unhandled_touchpad_event(
                    contact_position_units,
                    positions,
                ))
                .await;
            assert_matches!(
                contender.get_last_touchpad_event().unwrap().contacts.as_slice(),
                [touch_binding::TouchContact { position, .. }] => {
                    assert_near!(position.x, 200.0, 1.0);
                    assert_near!(position.y, 100.0, 1.0);
                }
            );
        }

        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::None, exponent: -6 },
                fidl_input_report::Unit{ type_: UnitType::None, exponent: -6 },
            )],
            vec![];
            "both units unspecified")]
        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::None, exponent: -6 },
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
            )],
            vec![];
            "x unit unspecified")]
        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
                fidl_input_report::Unit{ type_: UnitType::None, exponent: -6 },
            )],
            vec![];
            "y unit unspecified")]
        #[test_case(
            vec![(
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -3 },
                fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
            )],
            vec![];
            "mismatched exponents")]
        #[test_case(
            vec![
                (
                    fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -3 },
                    fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -3 },
                ),
                (
                    fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
                    fidl_input_report::Unit{ type_: UnitType::Meters, exponent: -6 },
                ),
            ],
            vec![];
            "unequal divisors")]
        #[fuchsia::test(allow_stalls = false)]
        async fn skips_contender_on_bad_descriptor(
            contact_position_units: Vec<(fidl_input_report::Unit, fidl_input_report::Unit)>,
            positions: Vec<Position>,
        ) {
            let contender = StubContender::new();
            let contender_factory = ContenderFactoryOnce::new(vec![contender.clone()]);
            let arena = Rc::new(GestureArena::new_for_test(
                || contender_factory.make_contenders(),
                &fuchsia_inspect::Inspector::new(),
                1,
            ));
            contender.set_next_result(ExamineEventResult::Mismatch("some reason"));
            arena
                .handle_input_event(make_unhandled_touchpad_event(
                    contact_position_units,
                    positions,
                ))
                .await;
            assert_eq!(contender.calls_received(), 0);
        }
    }

    mod inspect {
        use {
            super::{
                super::{GestureArena, InputHandler},
                utils::make_unhandled_touchpad_event,
            },
            crate::{input_device, keyboard_binding, touch_binding, Position, Size},
            assert_matches::assert_matches,
            fidl_fuchsia_input_report as fidl_input_report, fuchsia_async as fasync,
            fuchsia_zircon as zx,
            maplit::hashset,
            std::rc::Rc,
        };

        #[fuchsia::test]
        fn logs_touchpad_events_and_key_event_timestamps_to_inspect() {
            let mut executor =
                fasync::TestExecutor::new_with_fake_time().expect("failed to create executor");
            let inspector = fuchsia_inspect::Inspector::new();
            let arena = Rc::new(GestureArena::new_for_test(|| vec![], &inspector, 100));
            let touchpad_descriptor = input_device::InputDeviceDescriptor::Touchpad(
                touch_binding::TouchpadDeviceDescriptor {
                    device_id: 1,
                    contacts: vec![touch_binding::ContactDeviceDescriptor {
                        x_range: fidl_input_report::Range { min: 0, max: 10_000 },
                        y_range: fidl_input_report::Range { min: 0, max: 10_000 },
                        x_unit: fidl_input_report::Unit {
                            // Use millimeters to avoid floating-point rounding.
                            type_: fidl_input_report::UnitType::Meters,
                            exponent: -3,
                        },
                        y_unit: fidl_input_report::Unit {
                            // Use millimeters to avoid floating-point rounding.
                            type_: fidl_input_report::UnitType::Meters,
                            exponent: -3,
                        },
                        pressure_range: None,
                        width_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                        height_range: Some(fidl_input_report::Range { min: 0, max: 10_000 }),
                    }],
                },
            );
            let keyboard_descriptor = input_device::InputDeviceDescriptor::Keyboard(
                keyboard_binding::KeyboardDeviceDescriptor {
                    device_id: 2,
                    device_info: fidl_fuchsia_input_report::DeviceInfo {
                        product_id: 0,
                        vendor_id: 0,
                        version: 0,
                    },
                    keys: vec![fidl_fuchsia_input::Key::A, fidl_fuchsia_input::Key::B],
                },
            );

            // Process a touchpad event without width/height.
            let mut handle_event_fut = arena.clone().handle_input_event(input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![
                            touch_binding::TouchContact {
                                id: 1u32,
                                position: Position { x: 2.0, y: 3.0 },
                                contact_size: None,
                                pressure: None,
                            },
                            touch_binding::TouchContact {
                                id: 2u32,
                                position: Position { x: 40.0, y: 50.0 },
                                contact_size: None,
                                pressure: None,
                            },
                        ],
                        pressed_buttons: hashset! {1},
                    },
                ),
                device_descriptor: touchpad_descriptor.clone(),
                event_time: zx::Time::from_nanos(12_300),
                trace_id: None,
                handled: input_device::Handled::No,
            });
            executor.set_fake_time(fasync::Time::from_nanos(10_000_000));
            assert_matches!(
                executor.run_until_stalled(&mut handle_event_fut),
                std::task::Poll::Ready(_)
            );

            // Process a handled key event.
            let mut handle_event_fut = arena.clone().handle_input_event(input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Keyboard(
                    keyboard_binding::KeyboardEvent::new(
                        fidl_fuchsia_input::Key::A,
                        fidl_fuchsia_ui_input3::KeyEventType::Pressed,
                    ),
                ),
                device_descriptor: keyboard_descriptor.clone(),
                event_time: zx::Time::from_nanos(11_000_000),
                trace_id: None,
                handled: input_device::Handled::Yes,
            });
            executor.set_fake_time(fasync::Time::from_nanos(12_000_000));
            assert_matches!(
                executor.run_until_stalled(&mut handle_event_fut),
                std::task::Poll::Ready(_)
            );

            // Process an unhandled key event.
            let mut handle_event_fut = arena.clone().handle_input_event(input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Keyboard(
                    keyboard_binding::KeyboardEvent::new(
                        fidl_fuchsia_input::Key::B,
                        fidl_fuchsia_ui_input3::KeyEventType::Pressed,
                    ),
                ),
                device_descriptor: keyboard_descriptor,
                event_time: zx::Time::from_nanos(13_000_000),
                trace_id: None,
                handled: input_device::Handled::No,
            });
            executor.set_fake_time(fasync::Time::from_nanos(14_000_000));
            assert_matches!(
                executor.run_until_stalled(&mut handle_event_fut),
                std::task::Poll::Ready(_)
            );

            // Process a touchpad event with width/height.
            let mut handle_event_fut = arena.clone().handle_input_event(input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::Touchpad(
                    touch_binding::TouchpadEvent {
                        injector_contacts: vec![touch_binding::TouchContact {
                            id: 1u32,
                            position: Position { x: 2.0, y: 3.0 },
                            contact_size: Some(Size { width: 30.0, height: 40.0 }),
                            pressure: None,
                        }],
                        pressed_buttons: hashset! {},
                    },
                ),
                device_descriptor: touchpad_descriptor,
                event_time: zx::Time::from_nanos(18_000_000),
                trace_id: None,
                handled: input_device::Handled::No,
            });
            executor.set_fake_time(fasync::Time::from_nanos(19_000_000));
            assert_matches!(
                executor.run_until_stalled(&mut handle_event_fut),
                std::task::Poll::Ready(_)
            );

            // Uncomment this block to generate a new example for the
            // documentation in [`inspect_keys`].
            /*
            {
                use fuchsia_inspect::hierarchy::testing::JsonGetter;
                println!("{}", inspector.get_pretty_json());
            }
            */

            fuchsia_inspect::assert_data_tree!(inspector, root: {
                gestures_event_log: {
                    "0": {
                        touchpad_event: {
                            driver_monotonic_nanos: 12_300i64,
                            entry_latency_micros: 9987i64,  // 10_000_000 - 12_300 = 9_987_700 nsec
                            pressed_buttons: vec![ 1u64 ],
                            contacts: {
                                "1": {
                                    pos_x_mm: 2.0,
                                    pos_y_mm: 3.0,
                                },
                                "2": {
                                    pos_x_mm: 40.0,
                                    pos_y_mm: 50.0,
                                },
                            }
                        }
                    },
                    "1": {
                        key_event: {
                            driver_monotonic_nanos: 11_000_000i64,
                            entry_latency_micros: 1_000i64,  // 12_000_000 - 11_000_000 = 1_000_00 nsec
                        }
                    },
                    "2": {
                        key_event: {
                            driver_monotonic_nanos: 13_000_000i64,
                            entry_latency_micros: 1_000i64,  // 14_000_000 - 13_000_000 = 1_000_00 nsec
                        }
                    },
                    "3": {
                        touchpad_event: {
                            driver_monotonic_nanos: 18_000_000i64,
                            entry_latency_micros: 1_000i64,  // 19_000_000 - 18_000_000 = 1_000_00 nsec
                            pressed_buttons: Vec::<u64>::new(),
                            contacts: {
                                "1": {
                                    pos_x_mm: 2.0,
                                    pos_y_mm: 3.0,
                                    width_raw: 30.0,
                                    height_raw: 40.0,
                                },
                            }
                        }
                    },
                }
            });
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn retains_latest_events_up_to_cap() {
            let inspector = fuchsia_inspect::Inspector::new();
            let arena = Rc::new(GestureArena::new_for_test(|| vec![], &inspector, 2));
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await; // 0
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await; // 1
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await; // 2
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await; // 3
            arena.clone().handle_input_event(make_unhandled_touchpad_event()).await; // 4
            fuchsia_inspect::assert_data_tree!(inspector, root: {
                gestures_event_log: {
                    "3": contains {},
                    "4": contains {},
                }
            })
        }
    }
}
