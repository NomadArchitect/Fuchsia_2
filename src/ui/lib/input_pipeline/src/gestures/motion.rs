// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    super::gesture_arena::{
        self, ExamineEventResult, MouseEvent, ProcessBufferedEventsResult, ProcessNewEventResult,
        RecognizedGesture, TouchpadEvent, VerifyEventResult,
    },
    crate::mouse_binding,
    crate::utils::{euclidean_distance, Position},
    maplit::hashset,
};

/// The initial state of this recognizer, before a finger contact has been detected.
#[derive(Debug)]
struct Contender {
    /// The minimum movement in millimeters on surface to recognize as a motion.
    min_movement_in_mm: f32,
}

/// The state when this recognizer has detected a finger contact, before finger movement > threshold.
#[derive(Debug)]
struct FingerContactContender {
    /// The minimum movement in millimeters on surface to recognize as a motion.
    min_movement_in_mm: f32,

    /// The initial contact position on touchpad surface.
    initial_position: Position,
}

/// The state when this recognizer has detected a finger contact and a movement > threshold, but the
/// gesture arena has not declared this recognizer the winner.
#[derive(Debug)]
struct MatchedContender {}

/// The state when this recognizer has won the contest.
#[derive(Debug)]
struct Winner {
    /// The last contact position on touchpad surface.
    last_position: Position,
}

impl Contender {
    fn into_finger_contact_contender(
        self: Box<Self>,
        initial_position: Position,
    ) -> Box<dyn gesture_arena::Contender> {
        Box::new(FingerContactContender {
            min_movement_in_mm: self.min_movement_in_mm,
            initial_position,
        })
    }
}

impl gesture_arena::Contender for Contender {
    fn examine_event(self: Box<Self>, event: &TouchpadEvent) -> ExamineEventResult {
        if event.contacts.len() != 1 {
            return ExamineEventResult::Mismatch;
        }

        if event.pressed_buttons.len() > 0 {
            return ExamineEventResult::Mismatch;
        }

        ExamineEventResult::Contender(
            self.into_finger_contact_contender(event.contacts[0].position),
        )
    }
}

impl FingerContactContender {
    fn into_matched_contender(self: Box<Self>) -> Box<dyn gesture_arena::MatchedContender> {
        Box::new(MatchedContender {})
    }
}

impl gesture_arena::Contender for FingerContactContender {
    fn examine_event(self: Box<Self>, event: &TouchpadEvent) -> ExamineEventResult {
        if event.contacts.len() != 1 {
            return ExamineEventResult::Mismatch;
        }

        if event.pressed_buttons.len() > 0 {
            return ExamineEventResult::Mismatch;
        }

        let distance = euclidean_distance(event.contacts[0].position, self.initial_position);
        if distance > self.min_movement_in_mm {
            return ExamineEventResult::MatchedContender(self.into_matched_contender());
        }

        ExamineEventResult::Contender(self)
    }
}

impl MatchedContender {
    fn into_winner(self: Box<Self>, last_position: Position) -> Box<dyn gesture_arena::Winner> {
        Box::new(Winner { last_position })
    }
}

impl gesture_arena::MatchedContender for MatchedContender {
    fn verify_event(self: Box<Self>, event: &TouchpadEvent) -> VerifyEventResult {
        if event.contacts.len() != 1 {
            return VerifyEventResult::Mismatch;
        }

        if event.pressed_buttons.len() > 0 {
            return VerifyEventResult::Mismatch;
        }

        VerifyEventResult::MatchedContender(self)
    }

    fn process_buffered_events(
        self: Box<Self>,
        events: Vec<TouchpadEvent>,
    ) -> ProcessBufferedEventsResult {
        let mut mouse_events: Vec<MouseEvent> = Vec::new();
        let last_position = events[events.len() - 1].contacts[0].position.clone();

        for pair in events.windows(2) {
            mouse_events.push(touchpad_event_to_mouse_motion_event(
                &pair[0].contacts[0].position,
                &pair[1],
            ));
        }

        ProcessBufferedEventsResult {
            generated_events: mouse_events,
            winner: Some(self.into_winner(last_position)),
            recognized_gesture: RecognizedGesture::Motion,
        }
    }
}

impl gesture_arena::Winner for Winner {
    fn process_new_event(self: Box<Self>, event: TouchpadEvent) -> ProcessNewEventResult {
        match u8::try_from(event.contacts.len()).unwrap_or(u8::MAX) {
            0 => ProcessNewEventResult::EndGesture(None),
            1 => {
                if event.pressed_buttons.len() > 0 {
                    ProcessNewEventResult::EndGesture(Some(event))
                } else {
                    let last_position = event.contacts[0].position.clone();
                    ProcessNewEventResult::ContinueGesture(
                        Some(touchpad_event_to_mouse_motion_event(&self.last_position, &event)),
                        Box::new(Winner { last_position }),
                    )
                }
            }
            2.. => ProcessNewEventResult::EndGesture(Some(event)),
        }
    }
}

fn touchpad_event_to_mouse_motion_event(
    last_position: &Position,
    event: &TouchpadEvent,
) -> MouseEvent {
    MouseEvent {
        timestamp: event.timestamp,
        mouse_data: mouse_binding::MouseEvent::new(
            mouse_binding::MouseLocation::Relative(mouse_binding::RelativeLocation {
                counts: Position { x: 0.0, y: 0.0 },
                millimeters: Position {
                    x: event.contacts[0].position.x - last_position.x,
                    y: event.contacts[0].position.y - last_position.y,
                },
            }),
            /* wheel_delta_v= */ None,
            /* wheel_delta_h= */ None,
            mouse_binding::MousePhase::Move,
            /* affected_buttons= */ hashset! {},
            /* pressed_buttons= */ hashset! {},
        ),
    }
}

#[cfg(test)]
mod test {
    use {
        super::*, crate::touch_binding, assert_matches::assert_matches, fuchsia_zircon as zx,
        pretty_assertions::assert_eq, test_case::test_case,
    };

    fn touch_contact(id: u32, position: Position) -> touch_binding::TouchContact {
        touch_binding::TouchContact { id, position, pressure: None, contact_size: None }
    }

    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![1],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"button down")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![],
    };"0 fingers")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![
            touch_contact(1, Position{x: 1.0, y: 1.0}),
            touch_contact(2, Position{x: 5.0, y: 5.0}),
        ],
    };"2 fingers")]
    #[fuchsia::test]
    fn initial_contender_examine_event_mismatch(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::Contender> =
            Box::new(Contender { min_movement_in_mm: 10.0 });

        let got = contender.examine_event(&event);
        assert_matches!(got, ExamineEventResult::Mismatch);
    }

    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"finger hold")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 5.0, y: 5.0})],
    };"finger moved")]
    #[fuchsia::test]
    fn initial_contender_examine_event_finger_contact_contender(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::Contender> =
            Box::new(Contender { min_movement_in_mm: 10.0 });

        let got = contender.examine_event(&event);
        assert_matches!(got, ExamineEventResult::Contender(_));
    }

    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![1],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"button down")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![],
    };"0 fingers")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![
            touch_contact(1, Position{x: 1.0, y: 1.0}),
            touch_contact(2, Position{x: 5.0, y: 5.0}),
            ],
    };"2 fingers")]
    #[fuchsia::test]
    fn finger_contact_contender_examine_event_mismatch(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::Contender> = Box::new(FingerContactContender {
            min_movement_in_mm: 10.0,
            initial_position: Position { x: 1.0, y: 1.0 },
        });

        let got = contender.examine_event(&event);
        assert_matches!(got, ExamineEventResult::Mismatch);
    }

    #[test_case(TouchpadEvent{timestamp: zx::Time::ZERO,
         pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"finger hold")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 5.0, y: 5.0})],
    };"finger move less than threshold")]
    #[fuchsia::test]
    fn finger_contact_contender_examine_event_finger_contact_contender(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::Contender> = Box::new(FingerContactContender {
            min_movement_in_mm: 10.0,
            initial_position: Position { x: 1.0, y: 1.0 },
        });

        let got = contender.examine_event(&event);
        assert_matches!(got, ExamineEventResult::Contender(_));
    }

    #[fuchsia::test]
    fn finger_contact_contender_examine_event_matched_contender() {
        let contender: Box<dyn gesture_arena::Contender> = Box::new(FingerContactContender {
            min_movement_in_mm: 10.0,
            initial_position: Position { x: 1.0, y: 1.0 },
        });
        let event = TouchpadEvent {
            timestamp: zx::Time::ZERO,
            pressed_buttons: vec![],
            contacts: vec![touch_contact(1, Position { x: 11.0, y: 12.0 })],
        };
        let got = contender.examine_event(&event);
        assert_matches!(got, ExamineEventResult::MatchedContender(_));
    }

    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![1],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"button down")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![],
    };"0 fingers")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![
            touch_contact(1, Position{x: 1.0, y: 1.0}),
            touch_contact(2, Position{x: 5.0, y: 5.0}),
        ],
    };"2 fingers")]
    #[fuchsia::test]
    fn matched_contender_verify_event_mismatch(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::MatchedContender> = Box::new(MatchedContender {});

        let got = contender.verify_event(&event);
        assert_matches!(got, VerifyEventResult::Mismatch);
    }

    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
    };"finger hold")]
    #[test_case(TouchpadEvent{
        timestamp: zx::Time::ZERO,
        pressed_buttons: vec![],
        contacts: vec![touch_contact(1, Position{x: 5.0, y: 5.0})],
    };"finger move")]
    #[fuchsia::test]
    fn matched_contender_verify_event_matched_contender(event: TouchpadEvent) {
        let contender: Box<dyn gesture_arena::MatchedContender> = Box::new(MatchedContender {});

        let got = contender.verify_event(&event);
        assert_matches!(got, VerifyEventResult::MatchedContender(_));
    }

    #[fuchsia::test]
    fn matched_contender_process_buffered_events() {
        let contender: Box<dyn gesture_arena::MatchedContender> = Box::new(MatchedContender {});

        let got = contender.process_buffered_events(vec![
            TouchpadEvent {
                timestamp: zx::Time::from_nanos(1),
                pressed_buttons: vec![],
                contacts: vec![touch_contact(1, Position { x: 1.0, y: 1.0 })],
            },
            TouchpadEvent {
                timestamp: zx::Time::from_nanos(2),
                pressed_buttons: vec![],
                contacts: vec![touch_contact(1, Position { x: 5.0, y: 6.0 })],
            },
        ]);

        assert_eq!(
            got.generated_events,
            vec![MouseEvent {
                timestamp: zx::Time::from_nanos(2),
                mouse_data: mouse_binding::MouseEvent::new(
                    mouse_binding::MouseLocation::Relative(mouse_binding::RelativeLocation {
                        counts: Position { x: 0.0, y: 0.0 },
                        millimeters: Position { x: 4.0, y: 5.0 },
                    }),
                    /* wheel_delta_v= */ None,
                    /* wheel_delta_h= */ None,
                    mouse_binding::MousePhase::Move,
                    /* affected_buttons= */ hashset! {},
                    /* pressed_buttons= */ hashset! {},
                ),
            },]
        );
        assert_eq!(got.recognized_gesture, RecognizedGesture::Motion);
    }

    #[fuchsia::test]
    fn winner_process_new_event_end_gesture_none() {
        let winner: Box<dyn gesture_arena::Winner> =
            Box::new(Winner { last_position: Position { x: 1.0, y: 1.0 } });
        let event =
            TouchpadEvent { timestamp: zx::Time::ZERO, pressed_buttons: vec![], contacts: vec![] };
        let got = winner.process_new_event(event);

        assert_matches!(got, ProcessNewEventResult::EndGesture(None));
    }

    #[test_case(
        TouchpadEvent{
            timestamp: zx::Time::ZERO,
            pressed_buttons: vec![1],
            contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})],
        };"button down")]
    #[test_case(
        TouchpadEvent{
            timestamp: zx::Time::ZERO,
            pressed_buttons: vec![],
            contacts: vec![
                touch_contact(1, Position{x: 1.0, y: 1.0}),
                touch_contact(2, Position{x: 5.0, y: 5.0}),
            ],
        };"2 fingers")]
    #[fuchsia::test]
    fn winner_process_new_event_end_gesture_some(event: TouchpadEvent) {
        let winner: Box<dyn gesture_arena::Winner> =
            Box::new(Winner { last_position: Position { x: 1.0, y: 1.0 } });
        let got = winner.process_new_event(event);

        assert_matches!(got, ProcessNewEventResult::EndGesture(Some(_)));
    }

    #[test_case(
        TouchpadEvent{
            timestamp: zx::Time::from_nanos(2),
            pressed_buttons: vec![],
            contacts: vec![touch_contact(1, Position{x: 1.0, y: 1.0})]
        },
        Position {x:0.0, y:0.0}; "finger hold")]
    #[test_case(
        TouchpadEvent{
            timestamp: zx::Time::from_nanos(2),
            pressed_buttons: vec![],
            contacts: vec![touch_contact(1, Position{x: 5.0, y: 6.0})]
        },
        Position {x:4.0, y:5.0};"finger moved")]
    #[fuchsia::test]
    fn winner_process_new_event_continue_gesture(event: TouchpadEvent, want_position: Position) {
        let winner: Box<dyn gesture_arena::Winner> =
            Box::new(Winner { last_position: Position { x: 1.0, y: 1.0 } });
        let got = winner.process_new_event(event);

        // This not able to use `assert_eq` or `assert_matches` because:
        // - assert_matches: floating point is not allow in match.
        // - assert_eq: `ContinueGesture` has Box dyn type.
        match got {
            ProcessNewEventResult::EndGesture(None) => {
                panic!("Got EndGesture(None), want ContinueGesture()")
            }
            ProcessNewEventResult::EndGesture(Some(_)) => {
                panic!("Got EndGesture(Some), want ContinueGesture()")
            }
            ProcessNewEventResult::ContinueGesture(got_mouse_event, _) => {
                pretty_assertions::assert_eq!(
                    got_mouse_event.unwrap(),
                    MouseEvent {
                        timestamp: zx::Time::from_nanos(2),
                        mouse_data: mouse_binding::MouseEvent {
                            location: mouse_binding::MouseLocation::Relative(
                                mouse_binding::RelativeLocation {
                                    counts: Position { x: 0.0, y: 0.0 },
                                    millimeters: want_position,
                                }
                            ),
                            wheel_delta_v: None,
                            wheel_delta_h: None,
                            phase: mouse_binding::MousePhase::Move,
                            affected_buttons: hashset! {},
                            pressed_buttons: hashset! {},
                        },
                    }
                );
            }
        }
    }

    #[fuchsia::test]
    fn winner_process_new_event_continue_multiple_gestures() {
        let mut winner: Box<dyn gesture_arena::Winner> =
            Box::new(Winner { last_position: Position { x: 1.0, y: 1.0 } });
        let event = TouchpadEvent {
            timestamp: zx::Time::from_nanos(2),
            pressed_buttons: vec![],
            contacts: vec![touch_contact(1, Position { x: 5.0, y: 6.0 })],
        };
        let got = winner.process_new_event(event);

        // This not able to use `assert_eq` or `assert_matches` because:
        // - assert_matches: floating point is not allow in match.
        // - assert_eq: `ContinueGesture` has Box dyn type.
        match got {
            ProcessNewEventResult::EndGesture(None) => {
                panic!("Got EndGesture(None), want ContinueGesture()")
            }
            ProcessNewEventResult::EndGesture(Some(_)) => {
                panic!("Got EndGesture(Some), want ContinueGesture()")
            }
            ProcessNewEventResult::ContinueGesture(got_mouse_event, got_winner) => {
                pretty_assertions::assert_eq!(
                    got_mouse_event.unwrap(),
                    MouseEvent {
                        timestamp: zx::Time::from_nanos(2),
                        mouse_data: mouse_binding::MouseEvent {
                            location: mouse_binding::MouseLocation::Relative(
                                mouse_binding::RelativeLocation {
                                    counts: Position { x: 0.0, y: 0.0 },
                                    millimeters: Position { x: 4.0, y: 5.0 },
                                }
                            ),
                            wheel_delta_v: None,
                            wheel_delta_h: None,
                            phase: mouse_binding::MousePhase::Move,
                            affected_buttons: hashset! {},
                            pressed_buttons: hashset! {},
                        },
                    }
                );

                winner = got_winner;
            }
        }

        let event = TouchpadEvent {
            timestamp: zx::Time::from_nanos(3),
            pressed_buttons: vec![],
            contacts: vec![touch_contact(1, Position { x: 7.0, y: 9.0 })],
        };
        let got = winner.process_new_event(event);

        // This not able to use `assert_eq` or `assert_matches` because:
        // - assert_matches: floating point is not allow in match.
        // - assert_eq: `ContinueGesture` has Box dyn type.
        match got {
            ProcessNewEventResult::EndGesture(None) => {
                panic!("Got EndGesture(None), want ContinueGesture()")
            }
            ProcessNewEventResult::EndGesture(Some(_)) => {
                panic!("Got EndGesture(Some), want ContinueGesture()")
            }
            ProcessNewEventResult::ContinueGesture(got_mouse_event, _) => {
                pretty_assertions::assert_eq!(
                    got_mouse_event.unwrap(),
                    MouseEvent {
                        timestamp: zx::Time::from_nanos(3),
                        mouse_data: mouse_binding::MouseEvent {
                            location: mouse_binding::MouseLocation::Relative(
                                mouse_binding::RelativeLocation {
                                    counts: Position { x: 0.0, y: 0.0 },
                                    millimeters: Position { x: 2.0, y: 3.0 },
                                }
                            ),
                            wheel_delta_v: None,
                            wheel_delta_h: None,
                            phase: mouse_binding::MousePhase::Move,
                            affected_buttons: hashset! {},
                            pressed_buttons: hashset! {},
                        },
                    }
                );
            }
        }
    }
}
