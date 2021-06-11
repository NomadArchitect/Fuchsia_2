// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::synthesizer::*,
    anyhow::{format_err, Error},
    fidl_fuchsia_ui_input::{self, Touch},
    fuchsia_component::client::new_protocol_connector,
    keymaps::usages,
    std::time::Duration,
};

pub mod legacy_backend;
pub mod synthesizer;

mod modern_backend;

/// Simulates a media button event.
pub async fn media_button_event_command(
    volume_up: bool,
    volume_down: bool,
    mic_mute: bool,
    reset: bool,
    pause: bool,
    camera_disable: bool,
) -> Result<(), Error> {
    media_button_event(
        volume_up,
        volume_down,
        mic_mute,
        reset,
        pause,
        camera_disable,
        get_backend().await?.as_mut(),
    )
    .await
}

/// Simulates a key press of specified `usage`.
///
/// `key_event_duration` is the time spent between key-press and key-release events.
///
/// # Resolves to
/// * `Ok(())` if the events were successfully injected.
/// * `Err(Error)` otherwise.
///
/// # Corner case handling
/// * `key_event_duration` of zero is permitted, and will result in events being generated as
///    quickly as possible.
///
/// # Future directions
/// Per fxbug.dev/63532, this method will be replaced with a method that deals in
/// `fuchsia.input.Key`s, instead of HID Usage IDs.
pub async fn keyboard_event_command(usage: u32, key_event_duration: Duration) -> Result<(), Error> {
    keyboard_event(usage, key_event_duration, get_backend().await?.as_mut()).await
}

/// Simulates `input` being typed on a keyboard, with `key_event_duration` between key events.
///
/// # Requirements
/// * `input` must be non-empty
/// * `input` must only contain characters representable using the current keyboard layout
///    and locale. (At present, it is assumed that the current layout and locale are
///   `US-QWERTY` and `en-US`, respectively.)
///
/// # Resolves to
/// * `Ok(())` if the arguments met the requirements above, and the events were successfully
///   injected.
/// * `Err(Error)` otherwise.
///
/// # Corner case handling
/// * `key_event_duration` of zero is permitted, and will result in events being generated as
///    quickly as possible.
pub async fn text_command(input: String, key_event_duration: Duration) -> Result<(), Error> {
    text(input, key_event_duration, get_backend().await?.as_mut()).await
}

/// Simulates a sequence of key events (presses and releases) on a keyboard.
///
/// Dispatches the supplied `events` into a keyboard device, honoring the timing sequence that is
/// requested in them, to the extent possible using the current scheduling system.
///
/// Since each individual key press is broken down into constituent pieces (presses, releases,
/// pauses), it is possible to dispatch a key event sequence corresponding to multiple keys being
/// pressed and released in an arbitrary sequence.  This sequence is usually understood as a timing
/// diagram like this:
///
/// ```ignore
///           v--- key press   v--- key release
/// A: _______/^^^^^^^^^^^^^^^^\__________
///    |<----->|   <-- duration from start for key press.
///    |<--------------------->|   <-- duration from start for key release.
///
/// B: ____________/^^^^^^^^^^^^^^^^\_____
///                ^--- key press   ^--- key release
///    |<--------->|   <-- duration from start for key press.
///    |<-------------------------->|   <-- duration for key release.
/// ```
///
/// You would from there convert the desired timing diagram into a sequence of [TimedKeyEvent]s
/// that you would pass into this function. Note that all durations are specified as durations
/// from the start of the key event sequence.
///
/// Note that due to the way event timing works, it is in practice impossible to have two key
/// events happen at exactly the same time even if you so specify.  Do not rely on simultaneous
/// asynchronous event processing: it does not work in this code, and it is not how reality works
/// either.  Instead, design your key event processing so that it is robust against the inherent
/// non-determinism in key event delivery.
pub async fn dispatch_key_events(events: &[TimedKeyEvent]) -> Result<(), Error> {
    dispatch_key_events_async(events, get_backend().await?.as_mut()).await
}

/// Simulates `tap_event_count` taps at coordinates `(x, y)` for a touchscreen with horizontal
/// resolution `width` and vertical resolution `height`. `(x, y)` _should_ be specified in absolute
/// coordinations, with `x` normally in the range (0, `width`), `y` normally in the range
/// (0, `height`).
///
/// `duration` is divided equally between touch-down and touch-up event pairs, while the
/// transition between these pairs is immediate.
pub async fn tap_event_command(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    tap_event_count: usize,
    duration: Duration,
) -> Result<(), Error> {
    tap_event(x, y, width, height, tap_event_count, duration, get_backend().await?.as_mut()).await
}

/// Simulates `tap_event_count` times to repeat the multi-finger-taps, for touchscreen with
/// horizontal resolution `width` and vertical resolution `height`. Finger positions _should_
/// be specified in absolute coordinations, with `x` values normally in the range (0, `width`),
/// and `y` values normally in the range (0, `height`).
///
/// `duration` is divided equally between multi-touch-down and multi-touch-up
/// pairs, while the transition between these is immediate.
pub async fn multi_finger_tap_event_command(
    fingers: Vec<Touch>,
    width: u32,
    height: u32,
    tap_event_count: usize,
    duration: Duration,
) -> Result<(), Error> {
    multi_finger_tap_event(
        fingers,
        width,
        height,
        tap_event_count,
        duration,
        get_backend().await?.as_mut(),
    )
    .await
}

/// Simulates swipe from coordinates `(x0, y0)` to `(x1, y1)` for a touchscreen with
/// horizontal resolution `width` and vertical resolution `height`, with `move_event_count`
/// touch-move events in between. Positions for move events are linearly interpolated.
///
/// Finger positions _should_ be specified in absolute coordinations, with `x` values normally in the
/// range (0, `width`), and `y` values normally in the range (0, `height`).
///
/// `duration` is the total time from the touch-down event to the touch-up event, inclusive
/// of all move events in between.
pub async fn swipe_command(
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    width: u32,
    height: u32,
    move_event_count: usize,
    duration: Duration,
) -> Result<(), Error> {
    swipe(x0, y0, x1, y1, width, height, move_event_count, duration, get_backend().await?.as_mut())
        .await
}

/// Simulates swipe with fingers starting at `start_fingers`, and moving to `end_fingers`,
/// for a touchscreen for a touchscreen with horizontal resolution `width` and vertical resolution
/// `height`. Finger positions _should_ be specified in absolute coordinations, with `x` values
/// normally in the range (0, `width`), and `y` values normally in the range (0, `height`).
///
/// Linearly interpolates `move_event_count` touch-move events between the start positions
/// and end positions, over `duration` time. (`duration` is the total time from the touch-down
/// event to the touch-up event, inclusive of all move events in between.)
///
/// # Requirements
/// * `start_fingers` and `end_fingers` must have the same length
/// * `start_fingers.len()` and `end_finger.len()` must be representable within a `u32`
///
/// # Resolves to
/// * `Ok(())` if the arguments met the requirements above, and the events were successfully
///   injected.
/// * `Err(Error)` otherwise.
///
/// # Corner case handling
/// * `move_event_count` of zero is permitted, and will result in just the DOWN and UP events
///   being generated.
/// * `duration.as_nanos() < move_event_count` is allowed, and will result in all events having
///   the same timestamp.
/// * `width` and `height` are permitted to be zero; such values are left to the interpretation
///   of the system under test.
/// * finger positions _may_ exceed the expected bounds; such values are left to the interpretation
///   of the sytem under test.
pub async fn multi_finger_swipe_command(
    start_fingers: Vec<(u32, u32)>,
    end_fingers: Vec<(u32, u32)>,
    width: u32,
    height: u32,
    move_event_count: usize,
    duration: Duration,
) -> Result<(), Error> {
    multi_finger_swipe(
        start_fingers,
        end_fingers,
        width,
        height,
        move_event_count,
        duration,
        get_backend().await?.as_mut(),
    )
    .await
}

/// Selects an injection protocol, and returns the corresponding implementation
/// of `synthesizer::InputDeviceRegistry`.
///
/// # Returns
/// * Ok(`modern_backend::InputDeviceRegistry`) if `use_modern_input_injection` is true and
///   `fuchsia.input.injection.InputDeviceRegistry` is available.
/// * Ok(`legacy_backend::InputDeviceRegistry`) if `use_modern_input_injection` is false and
///   `fuchsia.ui.input.InputDeviceRegistry` is available.
/// * Err otherwise. E.g.,
///   * Neither protocol was available.
///   * Access to `/svc` was denied.
async fn get_backend() -> Result<Box<dyn InputDeviceRegistry>, Error> {
    if cfg!(use_modern_input_injection) {
        let modern_registry =
            new_protocol_connector::<fidl_fuchsia_input_injection::InputDeviceRegistryMarker>()?;
        if modern_registry.exists().await? {
            return Ok(Box::new(modern_backend::InputDeviceRegistry::new(
                modern_registry.connect()?,
            )));
        }
    } else {
        let legacy_registry =
            new_protocol_connector::<fidl_fuchsia_ui_input::InputDeviceRegistryMarker>()?;
        if legacy_registry.exists().await? {
            return Ok(Box::new(legacy_backend::InputDeviceRegistry::new()));
        }
    }

    Err(format_err!("no available InputDeviceRegistry"))
}

#[cfg(test)]
mod tests {
    // The functions in this file need to bind to FIDL services in this component's environment to
    // do their work, but a component can't modify its own environment. Hence, we can't validate
    // this module with unit tests.
}
