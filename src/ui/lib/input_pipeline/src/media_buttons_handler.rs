// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::input_handler::InputHandler,
    crate::{input_device, media_buttons},
    anyhow::{Context, Error},
    async_trait::async_trait,
    fidl_fuchsia_input_report as fidl_input_report, fidl_fuchsia_ui_input as fidl_ui_input,
    fidl_fuchsia_ui_policy as fidl_ui_policy,
    fuchsia_syslog::fx_log_err,
    futures::lock::Mutex,
    futures::TryStreamExt,
    std::sync::Arc,
};

/// A [`MediaButtonsHandler`] tracks MediaButtonListeners and sends media button events to them.
pub struct MediaButtonsHandler {
    /// The media button listeners.
    listeners: Arc<Mutex<Vec<fidl_ui_policy::MediaButtonsListenerProxy>>>,

    /// The last MediaButtonsEvent sent to all listeners.
    /// This is used to send new listeners the state of the media buttons.
    last_event: Arc<Mutex<Option<fidl_ui_input::MediaButtonsEvent>>>,
}

#[async_trait]
impl InputHandler for MediaButtonsHandler {
    async fn handle_input_event(
        &mut self,
        input_event: input_device::InputEvent,
    ) -> Vec<input_device::InputEvent> {
        match input_event {
            input_device::InputEvent {
                device_event: input_device::InputDeviceEvent::MediaButtons(media_buttons_event),
                device_descriptor: input_device::InputDeviceDescriptor::MediaButtons(_),
                event_time: _,
            } => {
                let media_buttons_event = Self::create_media_buttons_event(media_buttons_event);

                // Send the event if the media buttons are supported.
                if !is_empty_media_buttons_event(&media_buttons_event) {
                    self.send_event_to_listeners(&media_buttons_event).await;
                    *self.last_event.lock().await = Some(media_buttons_event);
                }

                vec![]
            }
            _ => vec![input_event],
        }
    }
}

impl MediaButtonsHandler {
    /// Creates a new [`MediaButtonsHandler`] that sends media button events to listeners.
    pub fn new() -> Self {
        let media_buttons_handler = Self {
            listeners: Arc::new(Mutex::new(Vec::new())),
            last_event: Arc::new(Mutex::new(None)),
        };

        media_buttons_handler
    }

    /// Handles the incoming DeviceListenerRegistryRequestStream.
    ///
    /// This method will end when the request stream is closed. If the stream closes with an
    /// error the error will be returned in the Result.
    ///
    /// # Parameters
    /// - `listeners`: The media button listeners to send events to.
    /// - `last_event`: The last event sent to the listeners.
    /// - `stream`: The stream of DeviceListenerRegistryRequestStream.
    pub async fn handle_device_listener_registry_request_stream(
        mut stream: fidl_ui_policy::DeviceListenerRegistryRequestStream,
        listeners: Arc<Mutex<Vec<fidl_ui_policy::MediaButtonsListenerProxy>>>,
        last_event: Arc<Mutex<Option<fidl_ui_input::MediaButtonsEvent>>>,
    ) -> Result<(), Error> {
        while let Some(request) = stream
            .try_next()
            .await
            .context("Error handling device listener registry request stream")?
        {
            match request {
                fidl_ui_policy::DeviceListenerRegistryRequest::RegisterListener {
                    listener,
                    responder,
                } => {
                    if let Ok(proxy) = listener.into_proxy() {
                        // Add the listener to the registry.
                        let mut listeners_locked = listeners.lock().await;
                        listeners_locked.push(proxy.clone());

                        // Send the listener the last media button event.
                        if let Some(event) = last_event.lock().await.clone() {
                            if !is_empty_media_buttons_event(&event) {
                                let _ = proxy.on_event(event).await;
                            }
                        }
                    }
                    let _ = responder.send();
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Creates a fidl_ui_input::MediaButtonsEvent from a media_buttons::MediaButtonEvent.
    ///
    /// # Parameters
    /// -  `event`: The MediaButtonEvent to create a MediaButtonsEvent from.
    fn create_media_buttons_event(
        event: media_buttons::MediaButtonsEvent,
    ) -> fidl_ui_input::MediaButtonsEvent {
        let mut new_event = fidl_ui_input::MediaButtonsEvent {
            volume: None,
            mic_mute: None,
            pause: None,
            camera_disable: None,
            ..fidl_ui_input::MediaButtonsEvent::EMPTY
        };
        for button in event.pressed_buttons {
            match button {
                fidl_input_report::ConsumerControlButton::VolumeUp => {
                    new_event.volume = Some(new_event.volume.unwrap_or_default() + 1);
                }
                fidl_input_report::ConsumerControlButton::VolumeDown => {
                    new_event.volume = Some(new_event.volume.unwrap_or_default() - 1);
                }
                fidl_input_report::ConsumerControlButton::MicMute => {
                    new_event.mic_mute = Some(!new_event.mic_mute.unwrap_or_default());
                }
                fidl_input_report::ConsumerControlButton::Pause => {
                    new_event.pause = Some(!new_event.pause.unwrap_or_default());
                }
                fidl_input_report::ConsumerControlButton::CameraDisable => {
                    new_event.camera_disable = Some(!new_event.camera_disable.unwrap_or_default());
                }
                _ => {}
            }
        }

        new_event
    }

    /// Sends media button events to media button listeners.
    ///
    /// # Parameters
    /// - `event`: The event to send to the listeners.
    async fn send_event_to_listeners(&self, event: &fidl_ui_input::MediaButtonsEvent) {
        let listeners = self.listeners.lock().await;
        for listener in listeners.iter() {
            if let Err(e) = listener.on_event(event.clone()).await {
                fx_log_err!("Error sending MediaButtonsEvent to listener: {:?}", e);
            }
        }
    }
}

/// Checks if the event contains any media button changes.
///
/// # Parameters
/// `event`: The media button event to check.
fn is_empty_media_buttons_event(event: &fidl_ui_input::MediaButtonsEvent) -> bool {
    let empty_event = fidl_ui_input::MediaButtonsEvent {
        volume: None,
        mic_mute: None,
        pause: None,
        camera_disable: None,
        ..fidl_ui_input::MediaButtonsEvent::EMPTY
    };

    empty_event == *event
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::testing_utilities, fidl::endpoints::create_proxy_and_stream,
        fidl_fuchsia_input_report as fidl_input_report, fuchsia_async as fasync,
        fuchsia_zircon as zx, futures::StreamExt,
    };

    fn spawn_device_listener_registry_server(
        listeners: Arc<Mutex<Vec<fidl_ui_policy::MediaButtonsListenerProxy>>>,
        last_event: Arc<Mutex<Option<fidl_ui_input::MediaButtonsEvent>>>,
    ) -> fidl_ui_policy::DeviceListenerRegistryProxy {
        let (device_listener_proxy, device_listener_stream) =
            create_proxy_and_stream::<fidl_ui_policy::DeviceListenerRegistryMarker>()
                .expect("Failed to create DeviceListenerRegistry proxy and stream.");

        fasync::Task::spawn(async move {
            let _ = MediaButtonsHandler::handle_device_listener_registry_request_stream(
                device_listener_stream,
                listeners,
                last_event,
            )
            .await;
        })
        .detach();

        device_listener_proxy
    }

    fn create_ui_input_media_buttons_event(
        volume: Option<i8>,
        mic_mute: Option<bool>,
        pause: Option<bool>,
        camera_disable: Option<bool>,
    ) -> fidl_ui_input::MediaButtonsEvent {
        fidl_ui_input::MediaButtonsEvent {
            volume,
            mic_mute,
            pause,
            camera_disable,
            ..fidl_ui_input::MediaButtonsEvent::EMPTY
        }
    }

    /// Tests that a media button listener can be registered and is sent the latest event upon
    /// registration.
    #[fasync::run_singlethreaded(test)]
    async fn register_media_buttons_listener() {
        // Set up DeviceListenerRegistry.
        let listeners: Arc<Mutex<Vec<fidl_ui_policy::MediaButtonsListenerProxy>>> =
            Arc::new(Mutex::new(vec![]));
        let last_event: Arc<Mutex<Option<fidl_ui_input::MediaButtonsEvent>>> = Arc::new(
            Mutex::new(Some(create_ui_input_media_buttons_event(Some(1), None, None, None))),
        );
        let device_listener_proxy =
            spawn_device_listener_registry_server(listeners.clone(), last_event.clone());

        // Register a listener.
        let (listener, mut listener_stream) =
            fidl::endpoints::create_request_stream::<fidl_ui_policy::MediaButtonsListenerMarker>()
                .unwrap();
        fasync::Task::spawn(async move {
            let _ = device_listener_proxy.register_listener(listener).await;
        })
        .detach();

        let expected_event = create_ui_input_media_buttons_event(Some(1), None, None, None);

        // Assert listener was registered and received last event.
        if let Some(request) = listener_stream.next().await {
            match request {
                Ok(fidl_ui_policy::MediaButtonsListenerRequest::OnEvent { event, responder }) => {
                    assert_eq!(event, expected_event);
                    let _ = responder.send();
                }
                _ => assert!(false),
            }
        }

        let unlocked_listeners = listeners.lock().await;
        assert_eq!(unlocked_listeners.len(), 1);
    }

    /// Tests that all supported buttons are sent.
    #[fasync::run_singlethreaded(test)]
    async fn listener_receives_all_buttons() {
        let mut media_buttons_handler = MediaButtonsHandler::new();
        let device_listener_proxy = spawn_device_listener_registry_server(
            media_buttons_handler.listeners.clone(),
            media_buttons_handler.last_event.clone(),
        );

        // Register a listener.
        let (listener, listener_stream) =
            fidl::endpoints::create_request_stream::<fidl_ui_policy::MediaButtonsListenerMarker>()
                .unwrap();
        let _ = device_listener_proxy.register_listener(listener).await;

        // Setup events and expectations.
        let descriptor = testing_utilities::media_buttons_device_descriptor();
        let event_time = zx::Time::get_monotonic().into_nanos() as input_device::EventTime;
        let input_events = vec![testing_utilities::create_media_buttons_event(
            vec![
                fidl_input_report::ConsumerControlButton::VolumeUp,
                fidl_input_report::ConsumerControlButton::VolumeDown,
                fidl_input_report::ConsumerControlButton::Pause,
                fidl_input_report::ConsumerControlButton::MicMute,
                fidl_input_report::ConsumerControlButton::CameraDisable,
            ],
            event_time,
            &descriptor,
        )];
        let expected_events =
            vec![create_ui_input_media_buttons_event(Some(0), Some(true), Some(true), Some(true))];

        // Assert registered listener receives event.
        assert_input_event_sequence_generates_media_buttons_events!(
            input_handler: media_buttons_handler,
            input_events: input_events,
            expected_events: expected_events,
            media_buttons_listener_request_stream: vec![listener_stream],
        );
    }

    /// Tests that multiple listeners are supported.
    #[fasync::run_singlethreaded(test)]
    async fn multiple_listeners_receive_event() {
        let mut media_buttons_handler = MediaButtonsHandler::new();
        let device_listener_proxy = spawn_device_listener_registry_server(
            media_buttons_handler.listeners.clone(),
            media_buttons_handler.last_event.clone(),
        );

        // Register two listeners.
        let (first_listener, first_listener_stream) =
            fidl::endpoints::create_request_stream::<fidl_ui_policy::MediaButtonsListenerMarker>()
                .unwrap();
        let (second_listener, second_listener_stream) =
            fidl::endpoints::create_request_stream::<fidl_ui_policy::MediaButtonsListenerMarker>()
                .unwrap();
        let _ = device_listener_proxy.register_listener(first_listener).await;
        let _ = device_listener_proxy.register_listener(second_listener).await;

        // Setup events and expectations.
        let descriptor = testing_utilities::media_buttons_device_descriptor();
        let event_time = zx::Time::get_monotonic().into_nanos() as input_device::EventTime;
        let input_events = vec![testing_utilities::create_media_buttons_event(
            vec![fidl_input_report::ConsumerControlButton::VolumeUp],
            event_time,
            &descriptor,
        )];
        let expected_events = vec![create_ui_input_media_buttons_event(Some(1), None, None, None)];

        // Assert registered listeners receives event.
        assert_input_event_sequence_generates_media_buttons_events!(
            input_handler: media_buttons_handler,
            input_events: input_events,
            expected_events: expected_events,
            media_buttons_listener_request_stream:
                vec![first_listener_stream, second_listener_stream],
        );
    }
}
