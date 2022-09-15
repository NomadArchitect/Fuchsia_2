// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod test {
    use {
        super::super::utils,
        crate::{gestures::args, input_device, mouse_binding, touch_binding, Position},
        assert_matches::assert_matches,
        maplit::hashset,
        pretty_assertions::assert_eq,
        std::collections::HashSet,
        test_util::{assert_gt, assert_near},
    };

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

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 2);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
    }

    /// TODO(fxbug.dev/109101): This test shows scroll need extra movement to start.
    #[fuchsia::test(allow_stalls = false)]
    async fn place_first_finger_then_second_finger_then_scroll() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger1_pos2_um = finger1_pos1_um.clone();
        let finger2_pos2_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos3_um = Position {
            x: 2_000.0,
            y: finger1_pos2_um.y
                + 1_000.0
                + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos3_um = Position {
            x: 5_000.0,
            y: finger2_pos2_um.y
                + 1_000.0
                + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos4_um = finger1_pos3_um.clone();
        let finger2_pos4_um = finger2_pos3_um.clone();
        let finger1_pos5_um = Position {
            x: 2_000.0,
            y: finger1_pos4_um.y
                + 1_000.0
                + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos5_um = Position {
            x: 5_000.0,
            y: finger1_pos4_um.y
                + 1_000.0
                + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um], hashset! {}),
            // Place 2 finger, keep trying `secondary_tap`.
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            // movement more than threshold, exit `secondary_tap`.
            touchpad_event(vec![finger1_pos3_um, finger2_pos3_um], hashset! {}),
            // start a new matching in gesture_arena
            touchpad_event(vec![finger1_pos4_um, finger2_pos4_um], hashset! {}),
            // movement more than threshold, `scroll` wins.
            touchpad_event(vec![finger1_pos5_um, finger2_pos5_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 5);
        assert_eq!(got[0].as_slice(), []);
        assert_eq!(got[1].as_slice(), []);
        assert_eq!(got[2].as_slice(), []);
        assert_eq!(got[3].as_slice(), []);
        assert_matches!(got[4].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_keep_contact() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let finger2_pos3_um = finger2_pos2_um.clone();
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![finger1_pos3_um, finger2_pos3_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
        assert_matches!(got[2].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_eq!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_lift() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
        assert_eq!(got[2].as_slice(), []);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_lift_1finger_then_click() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let finger1_pos4_um = finger1_pos3_um.clone();
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![finger1_pos3_um], hashset! {1}),
            touchpad_event(vec![finger1_pos4_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
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

    // TODO(fxbug.dev/99510): motion then 2 finger click should generate secondary click.
    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_then_double_finger_click() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let finger2_pos3_um = finger2_pos2_um.clone();
        let finger1_pos4_um = finger1_pos3_um.clone();
        let finger2_pos4_um = finger2_pos3_um.clone();
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![finger1_pos3_um, finger2_pos3_um], hashset! {1}),
            touchpad_event(vec![finger1_pos4_um, finger2_pos4_um], hashset! {}),
            // need finger lift to unbuffer events because the button down is not
            // match to any contender, the button up will start a new matching, then
            // the button up will be buffered to try matching scroll contender.
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 5);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
        assert_eq!(got[2].as_slice(), []);
        assert_eq!(got[3].as_slice(), []);
        assert_eq!(got[4].as_slice(), []);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_lift_1finger_then_move() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let finger1_pos4_um = Position {
            x: finger1_pos3_um.x,
            y: finger1_pos3_um.y
                + 1_000.0
                + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![finger1_pos3_um], hashset! {}),
            touchpad_event(vec![finger1_pos4_um], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
        assert_eq!(got[2].as_slice(), []);
        assert_matches!(got[3].as_slice(), [
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
          assert_near!(location_a.millimeters.x, 0.0, utils::EPSILON);
          assert_gt!(location_a.millimeters.y, 0.0);
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn scroll_lift_1finger_then_lift() {
        let finger1_pos1_um = Position { x: 2_000.0, y: 3_000.0 };
        let finger2_pos1_um = Position { x: 5_000.0, y: 3_000.0 };
        let finger1_pos2_um = Position {
            x: 2_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger2_pos2_um = Position {
            x: 5_000.0,
            y: 4_000.0 + args::SPURIOUS_TO_INTENTIONAL_MOTION_THRESHOLD_MM * 1_000.0,
        };
        let finger1_pos3_um = finger1_pos2_um.clone();
        let inputs = vec![
            touchpad_event(vec![finger1_pos1_um, finger2_pos1_um], hashset! {}),
            touchpad_event(vec![finger1_pos2_um, finger2_pos2_um], hashset! {}),
            touchpad_event(vec![finger1_pos3_um], hashset! {}),
            touchpad_event(vec![], hashset! {}),
        ];
        let got = utils::run_gesture_arena_test(inputs).await;

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_slice(), []);
        assert_matches!(got[1].as_slice(), [
          input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(
              mouse_binding::MouseEvent {
                location,
                wheel_delta_v: Some(mouse_binding::WheelDelta{
                  raw_data: mouse_binding::RawWheelDelta::Millimeters(delta_v),
                  physical_pixel: None,
                }),
                wheel_delta_h: None,
                is_precision_scroll: Some(mouse_binding::PrecisionScroll::Yes),
                ..
              },
            ),
          ..
          },
        ] => {
          assert_gt!(*delta_v, 0.0);
          assert_eq!(location, &utils::NO_MOVEMENT_LOCATION);
        });
        assert_eq!(got[2].as_slice(), []);
        // Does _not_ trigger tap detector.
        assert_eq!(got[3].as_slice(), []);
    }
}
