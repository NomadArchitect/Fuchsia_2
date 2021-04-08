// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context as _, Error},
    fidl_fuchsia_ui_input as ui_input, fidl_fuchsia_ui_input3 as ui_input3,
    fuchsia_syslog::{fx_log_err, fx_log_warn},
    futures::{TryFutureExt, TryStreamExt},
};

use fidl_fuchsia_ui_keyboard_focus as fidl_focus;

use crate::ime_service::ImeService;
use crate::keyboard::{events::KeyEvent, keyboard3};

/// Keyboard service router.
/// Starts providers of input3.Keyboard.
/// Handles keyboard events, routing them to one of the following:
/// - other fuchsia.ui.input.ImeService events to ime_service
pub struct Service {
    ime_service: ImeService,
    keyboard3: keyboard3::KeyboardService,
}

impl Service {
    pub async fn new(ime_service: ImeService) -> Result<Service, Error> {
        let keyboard3 = keyboard3::KeyboardService::new();
        Ok(Service { ime_service, keyboard3 })
    }

    /// Starts a task that processes `fuchsia.ui.keyboard.focus.Controller`.
    /// This method returns immediately without blocking.
    pub fn spawn_focus_controller(&self, mut stream: fidl_focus::ControllerRequestStream) {
        let keyboard3 = self.keyboard3.clone();
        fuchsia_async::Task::spawn(
            async move {
                while let Some(msg) = stream.try_next().await.context(concat!(
                    "keyboard::Service::spawn_focus_controller: ",
                    "error while reading the request stream for ",
                    "fuchsia.ui.keyboard.focus.Controller"
                ))? {
                    match msg {
                        fidl_focus::ControllerRequest::Notify { view_ref, responder, .. } => {
                            let view_ref = keyboard3::ViewRef::new(view_ref);
                            keyboard3.handle_focus_change(view_ref).await;
                            responder.send()?;
                        }
                    }
                }
                Ok(())
            }
            .unwrap_or_else(|e: anyhow::Error| fx_log_err!("couldn't run: {:?}", e)),
        )
        .detach();
    }

    /// Starts a task that processes `fuchsia.ui.input3.KeyEventInjector` requests.
    /// This method returns immediately without blocking.
    pub fn spawn_key_event_injector(&self, mut stream: ui_input3::KeyEventInjectorRequestStream) {
        let mut keyboard3 = self.keyboard3.clone();
        let mut ime_service = self.ime_service.clone();
        fuchsia_async::Task::spawn(
            async move {
                while let Some(msg) = stream.try_next().await.context(concat!(
                    "keyboard::Service::spawn_key_event_injector: ",
                    "error while reading the request stream for ",
                    "fuchsia.ui.input3.KeyEventInjector"
                ))? {
                    match msg {
                        ui_input3::KeyEventInjectorRequest::Inject {
                            key_event, responder, ..
                        } => {
                            let key_event_obj =
                                KeyEvent::new(&key_event, keyboard3.get_keys_pressed().await)?;
                            ime_service.inject_input(key_event_obj.clone()).await.unwrap_or_else(
                                |e| {
                                    fx_log_warn!(
                                        concat!(
                                            "keyboard::Service::spawn_key_event_injector: ",
                                            "error injecting input into IME: {:?}"
                                        ),
                                        e
                                    )
                                },
                            );
                            let was_handled = if keyboard3
                                .handle_key_event(key_event)
                                .await
                                .context("error handling input3 keyboard event")?
                            {
                                ui_input3::KeyEventStatus::Handled
                            } else {
                                ui_input3::KeyEventStatus::NotHandled
                            };
                            responder.send(was_handled).context(concat!(
                                "keyboard::Service::spawn_key_event_injector: ",
                                "error while sending response"
                            ))?;
                        }
                    }
                }
                Ok(())
            }
            .unwrap_or_else(|e: anyhow::Error| fx_log_err!("couldn't run: {:?}", e)),
        )
        .detach();
    }

    pub fn spawn_ime_service(&self, mut stream: ui_input::ImeServiceRequestStream) {
        let mut ime_service = self.ime_service.clone();
        fuchsia_async::Task::spawn(
            async move {
                while let Some(msg) =
                    stream.try_next().await.context("error running keyboard service")?
                {
                    ime_service
                        .handle_ime_service_msg(msg)
                        .await
                        .context("Handle IME service messages")?;
                }
                Ok(())
            }
            .unwrap_or_else(|e: anyhow::Error| fx_log_err!("couldn't run: {:?}", e)),
        )
        .detach();
    }

    pub fn spawn_keyboard3_service(&self, stream: ui_input3::KeyboardRequestStream) {
        let keyboard3 = self.keyboard3.clone();
        fuchsia_async::Task::spawn(
            async move { keyboard3.spawn_service(stream).await }
                .unwrap_or_else(|e: anyhow::Error| fx_log_err!("couldn't run: {:?}", e)),
        )
        .detach();
    }
}
