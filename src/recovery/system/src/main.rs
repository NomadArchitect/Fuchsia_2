// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use argh::FromArgs;
use carnelian::{
    color::Color,
    drawing::{load_font, path_for_circle, DisplayRotation, FontFace},
    facet::{
        RasterFacet, Scene, SceneBuilder, ShedFacet, TextFacetOptions, TextHorizontalAlignment,
        TextVerticalAlignment,
    },
    input, make_message,
    render::{BlendMode, Context as RenderContext, Fill, FillRule, Raster, Style},
    App, AppAssistant, AppAssistantPtr, AppContext, AssistantCreatorFunc, Coord, LocalBoxFuture,
    Point, Size, ViewAssistant, ViewAssistantContext, ViewAssistantPtr, ViewKey,
};
use euclid::{point2, size2};
use fidl_fuchsia_input_report::ConsumerControlButton;
use fidl_fuchsia_recovery::FactoryResetMarker;
use fuchsia_async::{self as fasync, Task};
use fuchsia_component::client::connect_to_service;
use fuchsia_zircon::{Duration, Event};
use futures::StreamExt;
use std::path::PathBuf;

const FACTORY_RESET_TIMER_IN_SECONDS: u8 = 10;
const LOGO_IMAGE_PATH: &str = "/pkg/data/logo.shed";
const BG_COLOR: Color = Color::white();
const HEADING_COLOR: Color = Color::new();
const BODY_COLOR: Color = Color { r: 0x7e, g: 0x86, b: 0x8d, a: 0xff };
const COUNTDOWN_COLOR: Color = Color { r: 0x42, g: 0x85, b: 0xf4, a: 0xff };

#[cfg(feature = "http_setup_server")]
mod setup;

#[cfg(feature = "http_setup_server")]
mod ota;

#[cfg(feature = "http_setup_server")]
use crate::setup::SetupEvent;

#[cfg(feature = "http_setup_server")]
mod storage;

mod fdr;
use fdr::{FactoryResetState, ResetEvent};

fn display_rotation_from_str(s: &str) -> Result<DisplayRotation, String> {
    match s {
        "0" => Ok(DisplayRotation::Deg0),
        "90" => Ok(DisplayRotation::Deg90),
        "180" => Ok(DisplayRotation::Deg180),
        "270" => Ok(DisplayRotation::Deg270),
        _ => Err(format!("Invalid DisplayRotation {}", s)),
    }
}

fn raster_for_circle(center: Point, radius: Coord, render_context: &mut RenderContext) -> Raster {
    let path = path_for_circle(center, radius, render_context);
    let mut raster_builder = render_context.raster_builder().expect("raster_builder");
    raster_builder.add(&path, None);
    raster_builder.build()
}

/// FDR
#[derive(Debug, FromArgs)]
#[argh(name = "recovery")]
struct Args {
    /// rotate
    #[argh(option, from_str_fn(display_rotation_from_str))]
    rotation: Option<DisplayRotation>,
}

enum RecoveryMessages {
    #[cfg(feature = "http_setup_server")]
    EventReceived,
    #[cfg(feature = "http_setup_server")]
    StartingOta,
    #[cfg(feature = "http_setup_server")]
    OtaFinished {
        result: Result<(), Error>,
    },
    ResetMessage(FactoryResetState),
    CountdownTick(u8),
    ResetFailed,
}

const RECOVERY_MODE_HEADLINE: &'static str = "Recovery mode";
const RECOVERY_MODE_BODY: &'static str = "Press and hold both volume keys to factory reset.";

const COUNTDOWN_MODE_HEADLINE: &'static str = "Factory reset device";
const COUNTDOWN_MODE_BODY: &'static str = "Continue holding the keys to the end of the countdown. \
This will wipe all of your data from this device and reset it to factory settings.";

struct RecoveryAppAssistant {
    app_context: AppContext,
    display_rotation: DisplayRotation,
}

impl RecoveryAppAssistant {
    pub fn new(app_context: &AppContext) -> Self {
        let args: Args = argh::from_env();

        Self {
            app_context: app_context.clone(),
            display_rotation: args.rotation.unwrap_or(DisplayRotation::Deg0),
        }
    }
}

impl AppAssistant for RecoveryAppAssistant {
    fn setup(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn create_view_assistant(&mut self, view_key: ViewKey) -> Result<ViewAssistantPtr, Error> {
        Ok(Box::new(RecoveryViewAssistant::new(
            &self.app_context,
            view_key,
            RECOVERY_MODE_HEADLINE,
            RECOVERY_MODE_BODY,
        )?))
    }

    fn get_display_rotation(&self) -> DisplayRotation {
        self.display_rotation
    }
}

struct RenderResources {
    scene: Scene,
}

impl RenderResources {
    fn new(
        render_context: &mut RenderContext,
        target_size: Size,
        heading: &str,
        body: &str,
        countdown_ticks: u8,
        face: &FontFace,
        is_counting_down: bool,
    ) -> Self {
        let min_dimension = target_size.width.min(target_size.height);
        let logo_edge = min_dimension * 0.24;
        let text_size = min_dimension / 10.0;
        let top_margin = 0.255;

        let body_text_size = min_dimension / 18.0;
        let countdown_text_size = min_dimension / 6.0;

        let mut builder = SceneBuilder::new(BG_COLOR);

        let logo_size: Size = size2(logo_edge, logo_edge);
        // Calculate position for centering the logo image
        let logo_position = {
            let x = target_size.width / 2.0;
            let y = top_margin * target_size.height + logo_edge / 2.0;
            point2(x, y)
        };

        if is_counting_down {
            let circle = raster_for_circle(logo_position, logo_edge / 2.0, render_context);
            let circle_facet = RasterFacet::new(
                circle,
                Style {
                    fill_rule: FillRule::NonZero,
                    fill: Fill::Solid(COUNTDOWN_COLOR),
                    blend_mode: BlendMode::Over,
                },
                Point::zero(),
            );

            builder.text(
                face.clone(),
                &format!("{:02}", countdown_ticks),
                countdown_text_size,
                logo_position,
                TextFacetOptions {
                    horizontal_alignment: TextHorizontalAlignment::Center,
                    vertical_alignment: TextVerticalAlignment::Center,
                    color: Color::white(),
                    ..TextFacetOptions::default()
                },
            );
            let _ = builder.facet(Box::new(circle_facet));
        } else {
            let shed_facet =
                ShedFacet::new(PathBuf::from(LOGO_IMAGE_PATH), logo_position, logo_size);
            builder.facet(Box::new(shed_facet));
        }

        let heading_text_location =
            point2(target_size.width / 2.0, logo_position.y + logo_size.height / 2.0 + text_size);
        builder.text(
            face.clone(),
            &heading,
            text_size,
            heading_text_location,
            TextFacetOptions {
                horizontal_alignment: TextHorizontalAlignment::Center,
                color: HEADING_COLOR,
                ..TextFacetOptions::default()
            },
        );

        let margin = 0.23;
        let body_x = target_size.width * margin;
        let wrap_width = target_size.width - 2.0 * body_x;
        builder.text(
            face.clone(),
            &body,
            body_text_size,
            point2(body_x, heading_text_location.y + text_size),
            TextFacetOptions {
                horizontal_alignment: TextHorizontalAlignment::Left,
                color: BODY_COLOR,
                max_width: Some(wrap_width),
                ..TextFacetOptions::default()
            },
        );

        Self { scene: builder.build() }
    }
}

struct RecoveryViewAssistant {
    face: FontFace,
    heading: String,
    body: String,
    reset_state_machine: fdr::FactoryResetStateMachine,
    app_context: AppContext,
    view_key: ViewKey,
    countdown_task: Option<Task<()>>,
    countdown_ticks: u8,
    render_resources: Option<RenderResources>,
}

impl RecoveryViewAssistant {
    fn new(
        app_context: &AppContext,
        view_key: ViewKey,
        heading: &str,
        body: &str,
    ) -> Result<RecoveryViewAssistant, Error> {
        RecoveryViewAssistant::setup(app_context, view_key)?;

        let face = load_font(PathBuf::from("/pkg/data/fonts/Roboto-Regular.ttf"))?;

        Ok(RecoveryViewAssistant {
            face,
            heading: heading.to_string(),
            body: body.to_string(),
            reset_state_machine: fdr::FactoryResetStateMachine::new(),
            app_context: app_context.clone(),
            view_key: 0,
            countdown_task: None,
            countdown_ticks: FACTORY_RESET_TIMER_IN_SECONDS,
            render_resources: None,
        })
    }

    #[cfg(not(feature = "http_setup_server"))]
    fn setup(_: &AppContext, _: ViewKey) -> Result<(), Error> {
        Ok(())
    }

    #[cfg(feature = "http_setup_server")]
    fn setup(app_context: &AppContext, view_key: ViewKey) -> Result<(), Error> {
        let mut receiver = setup::start_server()?;
        let local_app_context = app_context.clone();
        let f = async move {
            while let Some(event) = receiver.next().await {
                println!("recovery: received request");
                match event {
                    SetupEvent::Root => local_app_context
                        .queue_message(view_key, make_message(RecoveryMessages::EventReceived)),
                    SetupEvent::DevhostOta { cfg } => {
                        local_app_context
                            .queue_message(view_key, make_message(RecoveryMessages::StartingOta));
                        let result = ota::run_devhost_ota(cfg).await;
                        local_app_context.queue_message(
                            view_key,
                            make_message(RecoveryMessages::OtaFinished { result }),
                        );
                    }
                }
            }
        };

        fasync::Task::local(f).detach();

        Ok(())
    }

    async fn execute_reset(view_key: ViewKey, app_context: AppContext) {
        let factory_reset_service = connect_to_service::<FactoryResetMarker>();
        let proxy = match factory_reset_service {
            Ok(marker) => marker.clone(),
            Err(error) => {
                app_context.queue_message(view_key, make_message(RecoveryMessages::ResetFailed));
                panic!("Could not connect to factory_reset_service: {}", error);
            }
        };

        println!("recovery: Executing factory reset command");

        let res = proxy.reset().await;
        match res {
            Ok(_) => {}
            Err(error) => {
                app_context.queue_message(view_key, make_message(RecoveryMessages::ResetFailed));
                eprintln!("recovery: Error occurred : {}", error);
            }
        };
    }
}

impl ViewAssistant for RecoveryViewAssistant {
    fn setup(&mut self, context: &ViewAssistantContext) -> Result<(), Error> {
        self.view_key = context.key;
        Ok(())
    }

    fn render(
        &mut self,
        render_context: &mut RenderContext,
        ready_event: Event,
        context: &ViewAssistantContext,
    ) -> Result<(), Error> {
        // Emulate the size that Carnelian passes when the display is rotated
        let target_size = context.size;

        if self.render_resources.is_none() {
            self.render_resources = Some(RenderResources::new(
                render_context,
                target_size,
                &self.heading,
                &self.body,
                self.countdown_ticks,
                &self.face,
                self.reset_state_machine.is_counting_down(),
            ));
        }

        let render_resources = self.render_resources.as_mut().unwrap();
        render_resources.scene.render(render_context, ready_event, context)?;
        context.request_render();
        Ok(())
    }

    fn handle_message(&mut self, message: carnelian::Message) {
        if let Some(message) = message.downcast_ref::<RecoveryMessages>() {
            match message {
                #[cfg(feature = "http_setup_server")]
                RecoveryMessages::EventReceived => {
                    self.body = "Got event".to_string();
                }
                #[cfg(feature = "http_setup_server")]
                RecoveryMessages::StartingOta => {
                    self.body = "Starting OTA update".to_string();
                }
                #[cfg(feature = "http_setup_server")]
                RecoveryMessages::OtaFinished { result } => {
                    if let Err(e) = result {
                        self.body = format!("OTA failed: {:?}", e);
                    } else {
                        self.body = "OTA succeeded".to_string();
                    }
                }
                RecoveryMessages::ResetMessage(state) => {
                    match state {
                        FactoryResetState::Waiting => {
                            self.heading = RECOVERY_MODE_HEADLINE.to_string();
                            self.body = RECOVERY_MODE_BODY.to_string();
                            self.render_resources = None;
                            self.app_context.request_render(self.view_key);
                        }
                        FactoryResetState::StartCountdown => {
                            let view_key = self.view_key;
                            let local_app_context = self.app_context.clone();

                            let mut counter = FACTORY_RESET_TIMER_IN_SECONDS;
                            local_app_context.queue_message(
                                view_key,
                                make_message(RecoveryMessages::CountdownTick(counter)),
                            );

                            // start the countdown timer
                            let f = async move {
                                let mut interval_timer =
                                    fasync::Interval::new(Duration::from_seconds(1));
                                while let Some(()) = interval_timer.next().await {
                                    counter -= 1;
                                    local_app_context.queue_message(
                                        view_key,
                                        make_message(RecoveryMessages::CountdownTick(counter)),
                                    );
                                    if counter == 0 {
                                        break;
                                    }
                                }
                            };
                            self.countdown_task = Some(fasync::Task::local(f));
                        }
                        FactoryResetState::CancelCountdown => {
                            self.countdown_task
                                .take()
                                .and_then(|task| Some(fasync::Task::local(task.cancel())));
                            let state = self
                                .reset_state_machine
                                .handle_event(ResetEvent::CountdownCancelled);
                            assert_eq!(state, fdr::FactoryResetState::Waiting);
                            self.app_context.queue_message(
                                self.view_key,
                                make_message(RecoveryMessages::ResetMessage(state)),
                            );
                        }
                        FactoryResetState::ExecuteReset => {
                            let view_key = self.view_key;
                            let local_app_context = self.app_context.clone();
                            let f = async move {
                                RecoveryViewAssistant::execute_reset(view_key, local_app_context)
                                    .await;
                            };
                            fasync::Task::local(f).detach();
                        }
                    };
                }
                RecoveryMessages::CountdownTick(count) => {
                    self.heading = COUNTDOWN_MODE_HEADLINE.to_string();
                    self.countdown_ticks = *count;
                    if *count == 0 {
                        self.body = "Resetting device...".to_string();
                        let state =
                            self.reset_state_machine.handle_event(ResetEvent::CountdownFinished);
                        assert_eq!(state, FactoryResetState::ExecuteReset);
                        self.app_context.queue_message(
                            self.view_key,
                            make_message(RecoveryMessages::ResetMessage(state)),
                        );
                    } else {
                        self.body = COUNTDOWN_MODE_BODY.to_string();
                    }
                    self.render_resources = None;
                    self.app_context.request_render(self.view_key);
                }
                RecoveryMessages::ResetFailed => {
                    self.heading = "Reset failed".to_string();
                    self.body = "Please restart device to try again".to_string();
                    self.render_resources = None;
                    self.app_context.request_render(self.view_key);
                }
            }
        }
    }

    fn handle_consumer_control_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _: &input::Event,
        consumer_control_event: &input::consumer_control::Event,
    ) -> Result<(), Error> {
        match consumer_control_event.button {
            ConsumerControlButton::VolumeUp | ConsumerControlButton::VolumeDown => {
                let state: FactoryResetState =
                    self.reset_state_machine.handle_event(ResetEvent::ButtonPress(
                        consumer_control_event.button,
                        consumer_control_event.phase,
                    ));
                if state != fdr::FactoryResetState::ExecuteReset {
                    context.queue_message(make_message(RecoveryMessages::ResetMessage(state)));
                }
            }
            _ => {}
        }
        Ok(())
    }

    // This is to allow development of this feature on devices without consumer control buttons.
    fn handle_keyboard_event(
        &mut self,
        context: &mut ViewAssistantContext,
        event: &input::Event,
        keyboard_event: &input::keyboard::Event,
    ) -> Result<(), Error> {
        const HID_USAGE_KEY_F11: u32 = 0x44;
        const HID_USAGE_KEY_F12: u32 = 0x45;

        fn keyboard_to_consumer_phase(
            phase: carnelian::input::keyboard::Phase,
        ) -> carnelian::input::consumer_control::Phase {
            match phase {
                carnelian::input::keyboard::Phase::Pressed => {
                    carnelian::input::consumer_control::Phase::Down
                }
                _ => carnelian::input::consumer_control::Phase::Up,
            }
        }

        let synthetic_event = match keyboard_event.hid_usage {
            HID_USAGE_KEY_F11 => Some(input::consumer_control::Event {
                button: ConsumerControlButton::VolumeDown,
                phase: keyboard_to_consumer_phase(keyboard_event.phase),
            }),
            HID_USAGE_KEY_F12 => Some(input::consumer_control::Event {
                button: ConsumerControlButton::VolumeUp,
                phase: keyboard_to_consumer_phase(keyboard_event.phase),
            }),
            _ => None,
        };

        if let Some(synthetic_event) = synthetic_event {
            self.handle_consumer_control_event(context, event, &synthetic_event)?;
        }

        Ok(())
    }
}

fn make_app_assistant_fut(
    app_context: &AppContext,
) -> LocalBoxFuture<'_, Result<AppAssistantPtr, Error>> {
    let f = async move {
        let assistant = Box::new(RecoveryAppAssistant::new(app_context));
        Ok::<AppAssistantPtr, Error>(assistant)
    };
    Box::pin(f)
}

pub fn make_app_assistant() -> AssistantCreatorFunc {
    Box::new(make_app_assistant_fut)
}

fn main() -> Result<(), Error> {
    println!("recovery: started");
    App::run(make_app_assistant())
}

#[cfg(test)]
mod tests {
    use super::make_app_assistant;
    use carnelian::App;

    #[test]
    fn test_ui() -> std::result::Result<(), anyhow::Error> {
        let assistant = make_app_assistant();
        App::test(assistant)
    }
}
