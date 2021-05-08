// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::input_device::{self, InputDeviceBinding},
    anyhow::{format_err, Error},
    async_trait::async_trait,
    fidl_fuchsia_input_report as fidl_input_report,
    fidl_fuchsia_input_report::{InputDeviceProxy, InputReport},
    fuchsia_syslog::fx_log_err,
    futures::channel::mpsc::Sender,
};

/// A [`MediaButtonsEvent`] represents an event where one or more media buttons were pressed.
///
/// # Example
/// The following MediaButtonsEvents represents an event where the volume up button was pressed.
///
/// ```
/// let volume_event = input_device::InputDeviceEvent::MediaButton(MediaButtonsEvent::new(
///     vec![fidl_input_report::ConsumerControlButton::VOLUME_UP],
/// ));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct MediaButtonsEvent {
    pub pressed_buttons: Vec<fidl_input_report::ConsumerControlButton>,
}

impl MediaButtonsEvent {
    /// Creates a new [`MediaButtonsEvent`] with the relevant buttons.
    ///
    /// # Parameters
    /// - `pressed_buttons`: The buttons relevant to this event.
    pub fn new(pressed_buttons: Vec<fidl_input_report::ConsumerControlButton>) -> Self {
        Self { pressed_buttons }
    }
}

/// A [`MediaButtonsBinding`] represents a connection to a consumer control input device with
/// media buttons. The buttons supported by this binding is returned by `supported_buttons()`.
///
/// The [`MediaButtonsBinding`] parses and exposes consumer control descriptor properties
/// for the device it is associated with. It also parses [`InputReport`]s
/// from the device, and sends them to the device binding owner over `event_sender`.
pub struct MediaButtonsBinding {
    /// The channel to stream InputEvents to.
    event_sender: Sender<input_device::InputEvent>,

    /// Holds information about this device.
    device_descriptor: MediaButtonsDeviceDescriptor,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaButtonsDeviceDescriptor {
    /// The list of buttons that this device contains.
    pub buttons: Vec<fidl_input_report::ConsumerControlButton>,
}

#[async_trait]
impl input_device::InputDeviceBinding for MediaButtonsBinding {
    fn input_event_sender(&self) -> Sender<input_device::InputEvent> {
        self.event_sender.clone()
    }

    fn get_device_descriptor(&self) -> input_device::InputDeviceDescriptor {
        input_device::InputDeviceDescriptor::MediaButtons(self.device_descriptor.clone())
    }
}

impl MediaButtonsBinding {
    /// Creates a new [`InputDeviceBinding`] from the `device_proxy`.
    ///
    /// The binding will start listening for input reports immediately and send new InputEvents
    /// to the device binding owner over `input_event_sender`.
    ///
    /// # Parameters
    /// - `device_proxy`: The proxy to bind the new [`InputDeviceBinding`] to.
    /// - `input_event_sender`: The channel to send new InputEvents to.
    ///
    /// # Errors
    /// If there was an error binding to the proxy.
    pub async fn new(
        device_proxy: InputDeviceProxy,
        input_event_sender: Sender<input_device::InputEvent>,
    ) -> Result<Self, Error> {
        let device_binding = Self::bind_device(&device_proxy, input_event_sender).await?;
        input_device::initialize_report_stream(
            device_proxy,
            device_binding.get_device_descriptor(),
            device_binding.input_event_sender(),
            Self::process_reports,
        );

        Ok(device_binding)
    }

    /// Binds the provided input device to a new instance of `Self`.
    ///
    /// # Parameters
    /// - `device`: The device to use to initialize the binding.
    /// - `input_event_sender`: The channel to send new InputEvents to.
    ///
    /// # Errors
    /// If the device descriptor could not be retrieved, or the descriptor could
    /// not be parsed correctly.
    async fn bind_device(
        device: &InputDeviceProxy,
        input_event_sender: Sender<input_device::InputEvent>,
    ) -> Result<Self, Error> {
        let device_descriptor: fidl_input_report::DeviceDescriptor =
            device.get_descriptor().await?;

        let media_buttons_descriptor = device_descriptor.consumer_control.ok_or_else(|| {
            format_err!("DeviceDescriptor does not have a ConsumerControlDescriptor")
        })?;

        let media_buttons_input_descriptor = media_buttons_descriptor.input.ok_or_else(|| {
            format_err!("ConsumerControlDescriptor does not have a ConsumerControlInputDescriptor")
        })?;

        let device_descriptor: MediaButtonsDeviceDescriptor = MediaButtonsDeviceDescriptor {
            buttons: media_buttons_input_descriptor.buttons.unwrap_or_default(),
        };

        Ok(MediaButtonsBinding { event_sender: input_event_sender, device_descriptor })
    }

    /// Parses an [`InputReport`] into one or more [`InputEvent`]s. Sends the [`InputEvent`]s
    /// to the device binding owner via [`input_event_sender`].
    ///
    /// # Parameters
    /// `report`: The incoming [`InputReport`].
    /// `previous_report`: The previous [`InputReport`] seen for the same device. This can be
    ///                    used to determine, for example, which keys are no longer present in
    ///                    a keyboard report to generate key released events. If `None`, no
    ///                    previous report was found.
    /// `device_descriptor`: The descriptor for the input device generating the input reports.
    /// `input_event_sender`: The sender for the device binding's input event stream.
    ///
    /// # Returns
    /// An [`InputReport`] which will be passed to the next call to [`process_reports`], as
    /// [`previous_report`]. If `None`, the next call's [`previous_report`] will be `None`.
    fn process_reports(
        report: InputReport,
        previous_report: Option<InputReport>,
        device_descriptor: &input_device::InputDeviceDescriptor,
        input_event_sender: &mut Sender<input_device::InputEvent>,
    ) -> Option<InputReport> {
        // Input devices can have multiple types so ensure `report` is a ConsumerControlInputReport.
        let pressed_buttons: Vec<fidl_input_report::ConsumerControlButton> =
            match report.consumer_control {
                Some(ref consumer_control_report) => consumer_control_report
                    .pressed_buttons
                    .as_ref()
                    .map(|buttons| buttons.iter().cloned().collect())
                    .unwrap_or_default(),
                None => return previous_report,
            };

        let event_time: input_device::EventTime =
            input_device::event_time_or_now(report.event_time);

        send_media_buttons_event(
            pressed_buttons,
            device_descriptor,
            event_time,
            input_event_sender,
        );

        Some(report)
    }

    /// Returns the [`fidl_input_report::ConsumerControlButton`]s that this binding supports.
    pub fn supported_buttons() -> Vec<fidl_input_report::ConsumerControlButton> {
        vec![
            fidl_input_report::ConsumerControlButton::VolumeUp,
            fidl_input_report::ConsumerControlButton::VolumeDown,
            fidl_input_report::ConsumerControlButton::Pause,
            fidl_input_report::ConsumerControlButton::MicMute,
            fidl_input_report::ConsumerControlButton::CameraDisable,
        ]
    }
}

/// Sends an InputEvent over `sender`.
///
/// # Parameters
/// - `pressed_buttons`: The buttons relevant to the event.
/// - `device_descriptor`: The descriptor for the input device generating the input reports.
/// - `event_time`: The time in nanoseconds when the event was first recorded.
/// - `sender`: The stream to send the MouseEvent to.
fn send_media_buttons_event(
    pressed_buttons: Vec<fidl_input_report::ConsumerControlButton>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    event_time: input_device::EventTime,
    sender: &mut Sender<input_device::InputEvent>,
) {
    if let Err(e) = sender.try_send(input_device::InputEvent {
        device_event: input_device::InputDeviceEvent::MediaButtons(MediaButtonsEvent::new(
            pressed_buttons,
        )),
        device_descriptor: device_descriptor.clone(),
        event_time,
    }) {
        fx_log_err!("Failed to send MediaButtonsEvent with error: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::testing_utilities, fuchsia_async as fasync, futures::StreamExt};

    // Tests that an InputReport containing one media button generates an InputEvent containing
    // the same media button.
    #[fasync::run_singlethreaded(test)]
    async fn volume_up_only() {
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();
        let pressed_buttons = vec![fidl_input_report::ConsumerControlButton::VolumeUp];
        let first_report = testing_utilities::create_consumer_control_input_report(
            pressed_buttons.clone(),
            event_time_i64,
        );
        let descriptor = testing_utilities::media_buttons_device_descriptor();

        let input_reports = vec![first_report];
        let expected_events = vec![testing_utilities::create_media_buttons_event(
            pressed_buttons,
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events!(
            input_reports: input_reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: MediaButtonsBinding,
        );
    }

    // Tests that an InputReport containing two media button generates an InputEvent containing
    // both media buttons.
    #[fasync::run_singlethreaded(test)]
    async fn volume_up_and_down() {
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();
        let pressed_buttons = vec![
            fidl_input_report::ConsumerControlButton::VolumeUp,
            fidl_input_report::ConsumerControlButton::VolumeDown,
        ];
        let first_report = testing_utilities::create_consumer_control_input_report(
            pressed_buttons.clone(),
            event_time_i64,
        );
        let descriptor = testing_utilities::media_buttons_device_descriptor();

        let input_reports = vec![first_report];
        let expected_events = vec![testing_utilities::create_media_buttons_event(
            pressed_buttons,
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events!(
            input_reports: input_reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: MediaButtonsBinding,
        );
    }

    // Tests that three InputReports containing one media button generates three InputEvents
    // containing the same media buttons.
    #[fasync::run_singlethreaded(test)]
    async fn sequence_of_buttons() {
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();
        let first_report = testing_utilities::create_consumer_control_input_report(
            vec![fidl_input_report::ConsumerControlButton::VolumeUp],
            event_time_i64,
        );
        let second_report = testing_utilities::create_consumer_control_input_report(
            vec![fidl_input_report::ConsumerControlButton::VolumeDown],
            event_time_i64,
        );
        let third_report = testing_utilities::create_consumer_control_input_report(
            vec![fidl_input_report::ConsumerControlButton::CameraDisable],
            event_time_i64,
        );
        let descriptor = testing_utilities::media_buttons_device_descriptor();

        let input_reports = vec![first_report, second_report, third_report];
        let expected_events = vec![
            testing_utilities::create_media_buttons_event(
                vec![fidl_input_report::ConsumerControlButton::VolumeUp],
                event_time_u64,
                &descriptor,
            ),
            testing_utilities::create_media_buttons_event(
                vec![fidl_input_report::ConsumerControlButton::VolumeDown],
                event_time_u64,
                &descriptor,
            ),
            testing_utilities::create_media_buttons_event(
                vec![fidl_input_report::ConsumerControlButton::CameraDisable],
                event_time_u64,
                &descriptor,
            ),
        ];

        assert_input_report_sequence_generates_events!(
            input_reports: input_reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: MediaButtonsBinding,
        );
    }
}
