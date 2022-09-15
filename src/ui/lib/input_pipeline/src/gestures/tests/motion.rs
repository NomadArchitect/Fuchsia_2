// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod tests {
    use {
        super::super::utils,
        crate::{gestures::args, input_device, mouse_binding, touch_binding, Position},
        assert_matches::assert_matches,
        maplit::hashset,
        pretty_assertions::assert_eq,
        std::collections::HashSet,
        test_util::{assert_gt, assert_lt, assert_near},
    };

    const EPSILON: f32 = 1.0 / 100_000.0;

    fn touchpad_event(
        positions: Vec<Position>,
        pressed_buttons: HashSet<mouse_binding::MouseButton>,
    ) -> input_device::InputEvent {
        let injector_contacts: Vec<touch_binding::TouchContact> = positions
            .iter()
            .enumerate()
            .map(|(i, p)| touch_binding::TouchContact {
                id: i as u32,
                position: *p,
                contact_size: None,
                pressure: None,
            })
            .collect();

        utils::make_touchpad_event(touch_binding::TouchpadEvent {
            injector_contacts,
            pressed_buttons,
        })
    }

    const NO_MOVEMENT_LOCATION: mouse_binding::MouseLocation =
        mouse_binding::MouseLocation::Relative(mouse_binding::RelativeLocation {
            counts: Position { x: 0.0, y: 0.0 },
            millimeters: Position { x: 0.0, y: 0.0 },
        });

    #[fuchsia::test(allow_stalls = false)]
    async fn motion_keep_contact() {
        let pos0_um = Position { x: 2_000.0, y: 3_000.0 };
        let pos1_um = Position { x: 2_100.0, y: 3_000.0 };
        let pos2_um = Position {
            x: 2_100.0,
            y: 3_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let pos3_um = pos2_um.clone();
        let inputs = vec![
            touchpad_event(vec![pos0_um], hashset! {}),
            touchpad_event(vec![pos1_um], hashset! {}),
            touchpad_event(vec![pos2_um], hashset! {}),
            touchpad_event(vec![pos3_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_eq!(got[1].as_slice(), []);
        assert_lt!(
            pos1_um.x - pos0_um.x,
            args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0
        );
        assert_matches!(got[2].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location_a),
                ..
              },
            ),
          ..
          },
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location_b),
                ..
              },
            ),
          ..
          },
        ] => {
          // the 2nd event movement < threshold but 3rd event movement > threshold,
          // then the 2nd event got unbuffered and recognized as a mouse move.
          assert_gt!(location_a.millimeters.x, 0.0);
          assert_near!(location_a.millimeters.y, 0.0, EPSILON);
          assert_near!(location_b.millimeters.x, 0.0, EPSILON);
          assert_gt!(location_b.millimeters.y, 0.0);
        });
        assert_matches!(got[3].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: location_a,
                ..
              },
            ),
          ..
          },
        ] => {
          assert_eq!(location_a, &NO_MOVEMENT_LOCATION);
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn motion_then_lift() {
        let pos0_um = Position { x: 2_000.0, y: 3_000.0 };
        let pos1_um = Position {
            x: 2_000.0,
            y: 3_100.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![pos0_um], hashset! {}),
            touchpad_event(vec![pos1_um], hashset! {}),
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location1),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_near!(location1.millimeters.x, 0.0, EPSILON);
          assert_gt!(location1.millimeters.y, 0.0);
        });
        // Does _not_ trigger tap.
        assert_eq!(got[2].as_slice(), []);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn motion_then_click() {
        let pos1 = Position { x: 2_000.0, y: 3_000.0 };
        let pos2 = Position {
            x: 2_000.0,
            y: 3_100.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![pos1], hashset! {}),
            touchpad_event(vec![pos2], hashset! {}),
            touchpad_event(vec![pos2], hashset! {1}),
            touchpad_event(vec![pos2], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location_a),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_near!(location_a.millimeters.x, 0.0, EPSILON);
          assert_gt!(location_a.millimeters.y, 0.0);
        });
        assert_eq!(got[2].as_slice(), []);
        assert_matches!(got[3].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                pressed_buttons: pressed_button_a,
                affected_buttons: affected_button_a,
                ..
              },
            ),
          ..
          },
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                pressed_buttons: pressed_button_b,
                affected_buttons: affected_button_b,
                ..
              },
            ),
          ..
          }
        ] => {
          assert_eq!(pressed_button_a, &hashset! {1});
          assert_eq!(affected_button_a, &hashset! {1});
          assert_eq!(pressed_button_b, &hashset! {});
          assert_eq!(affected_button_b, &hashset! {1});
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn motion_then_place_2nd_finger_then_lift() {
        let finger1_pos0_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger1_pos1_um = Position {
            x: 2_000.0,
            y: 3_100.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos2_um = finger1_pos1_um.clone();
        let finger2_pos2_um = Position { x: 5_000.0, y: 5_000.0 };
        let inputs = vec![
            touchpad_event(vec![finger1_pos0_um], hashset! {}),
            touchpad_event(vec![finger1_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location_a),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_near!(location_a.millimeters.x, 0.0, EPSILON);
          assert_gt!(location_a.millimeters.y, 0.0);
        });
        assert_eq!(got[2].as_slice(), []);
        // Does _not_ trigger secondary-tap detector.
        assert_eq!(got[3].as_slice(), []);
    }

    // TODO(fxbug.dev/99510): motion then 2 finger click should generate secondary click.
    #[fuchsia::test(allow_stalls = false)]
    async fn motion_then_place_2nd_finger_then_click() {
        let finger1_pos0_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger1_pos1_um = Position {
            x: 2_000.0,
            y: 3_100.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos2_um = finger1_pos1_um.clone();
        let finger2_pos2_um = Position { x: 5_000.0, y: 5_000.0 };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let finger2_pos3_um = finger2_pos2_um.clone();
        let inputs = vec![
            touchpad_event(vec![finger1_pos0_um], hashset! {}),
            touchpad_event(vec![finger1_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {1}),
            touchpad_event(vec![finger1_pos3_um, finger2_pos3_um], hashset! {}),
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 5);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location: mouse_binding::MouseLocation::Relative(location_a),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_near!(location_a.millimeters.x, 0.0, EPSILON);
          assert_gt!(location_a.millimeters.y, 0.0);
        });
        assert_eq!(got[2].as_slice(), []);
        assert_eq!(got[3].as_slice(), []);
        assert_eq!(got[4].as_slice(), []);
    }
}
