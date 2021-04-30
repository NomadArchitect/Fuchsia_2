// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context as _, Error},
    fidl_fuchsia_intl::{LocaleId, TemperatureUnit, TimeZoneId},
    fidl_fuchsia_media::AudioRenderUsage,
    fidl_fuchsia_settings::*,
    fidl_fuchsia_settings_policy::{
        PolicyParameters, Property, Target, Transform, Volume as PolicyVolume,
        VolumePolicyControllerMarker, VolumePolicyControllerRequest,
        VolumePolicyControllerRequestStream,
    },
    fuchsia_async as fasync,
    fuchsia_component::server::ServiceFs,
    futures::prelude::*,
    parking_lot::RwLock,
    setui_client_lib::accessibility,
    setui_client_lib::audio,
    setui_client_lib::device,
    setui_client_lib::display,
    setui_client_lib::do_not_disturb,
    setui_client_lib::factory_reset,
    setui_client_lib::input,
    setui_client_lib::intl,
    setui_client_lib::light,
    setui_client_lib::night_mode,
    setui_client_lib::privacy,
    setui_client_lib::setup,
    setui_client_lib::utils,
    setui_client_lib::volume_policy,
    setui_client_lib::{
        AccessibilityOptions, CaptionCommands, CaptionFontStyle, CaptionOptions,
        VolumePolicyCommands, VolumePolicyOptions,
    },
    std::sync::Arc,
};

/// Validate that the results of the call are successful, and in the case of watch,
/// that the first item can be retrieved, but do not analyze the result.
macro_rules! assert_successful {
    ($expr:expr) => {
        // We only need an extra check on the watch so we can exercise it at least once.
        // The sets already return a result.
        if let ::setui_client_lib::utils::Either::Watch(mut stream) = $expr.await? {
            stream.try_next().await?;
        }
    };
}

/// Validate that the results of the call are a successful set and return the result.
macro_rules! assert_set {
    ($expr:expr) => {
        match $expr.await? {
            ::setui_client_lib::utils::Either::Set(output) => output,
            ::setui_client_lib::utils::Either::Watch(_) => {
                panic!("Did not expect a watch result for a set call")
            }
            ::setui_client_lib::utils::Either::Get(_) => {
                panic!("Did not expect a get result for a set call")
            }
        }
    };
}

/// Validate that the results of the call are a successful watch and return the
/// first result.
macro_rules! assert_watch {
    ($expr:expr) => {
        match $expr.await? {
            ::setui_client_lib::utils::Either::Watch(mut stream) => {
                stream.try_next().await?.expect("Watch should have a result")
            }
            ::setui_client_lib::utils::Either::Set(_) => {
                panic!("Did not expect a set result for a watch call")
            }
            ::setui_client_lib::utils::Either::Get(_) => {
                panic!("Did not expect a get result for a watch call")
            }
        }
    };
}

/// Validate that the results of the call are a successful get and return the result.
macro_rules! assert_get {
    ($expr:expr) => {
        match $expr.await? {
            ::setui_client_lib::utils::Either::Get(output) => output,
            ::setui_client_lib::utils::Either::Watch(_) => {
                panic!("Did not expect a watch result for a get call")
            }
            ::setui_client_lib::utils::Either::Set(_) => {
                panic!("Did not expect a set result for a get call")
            }
        }
    };
}

enum Services {
    Accessibility(AccessibilityRequestStream),
    Audio(AudioRequestStream),
    Device(DeviceRequestStream),
    Display(DisplayRequestStream),
    DoNotDisturb(DoNotDisturbRequestStream),
    FactoryReset(FactoryResetRequestStream),
    Input(InputRequestStream),
    Intl(IntlRequestStream),
    Light(LightRequestStream),
    NightMode(NightModeRequestStream),
    Privacy(PrivacyRequestStream),
    Setup(SetupRequestStream),
    VolumePolicy(VolumePolicyControllerRequestStream),
}

struct ExpectedStreamSettingsStruct {
    stream: Option<AudioRenderUsage>,
    source: Option<fidl_fuchsia_settings::AudioStreamSettingSource>,
    level: Option<f32>,
    volume_muted: Option<bool>,
    input_muted: Option<bool>,
}

const ENV_NAME: &str = "setui_client_test_environment";
const TEST_BUILD_TAG: &str = "0.20190909.1.0";

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    println!("accessibility service tests");
    println!("  client calls set");
    validate_accessibility_set().await?;

    println!("  client calls watch");
    validate_accessibility_watch().await?;

    println!("audio service tests");
    println!("  client calls audio watch");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: None,
        source: None,
        level: None,
        volume_muted: None,
        input_muted: None,
    })
    .await?;

    println!("  client calls set audio input - stream");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: Some(AudioRenderUsage::Background),
        source: None,
        level: None,
        volume_muted: None,
        input_muted: None,
    })
    .await?;

    println!("  client calls set audio input - source");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: None,
        source: Some(fidl_fuchsia_settings::AudioStreamSettingSource::System),
        level: None,
        volume_muted: None,
        input_muted: None,
    })
    .await?;

    println!("  client calls set audio input - level");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: None,
        source: None,
        level: Some(0.3),
        volume_muted: None,
        input_muted: None,
    })
    .await?;

    println!("  client calls set audio input - volume_muted");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: None,
        source: None,
        level: None,
        volume_muted: Some(true),
        input_muted: None,
    })
    .await?;

    println!("  client calls set audio input - input_muted");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: None,
        source: None,
        level: None,
        volume_muted: None,
        input_muted: Some(false),
    })
    .await?;

    println!("  client calls set audio input - multiple");
    validate_audio(&ExpectedStreamSettingsStruct {
        stream: Some(AudioRenderUsage::Media),
        source: Some(fidl_fuchsia_settings::AudioStreamSettingSource::User),
        level: Some(0.6),
        volume_muted: Some(false),
        input_muted: Some(true),
    })
    .await?;

    println!("device service tests");
    println!("  client calls device watch");
    validate_device().await?;

    println!("display service tests");
    println!("  client calls display watch");
    validate_display(None, None, None, None, None, None).await?;

    println!("  client calls set brightness");
    validate_display(Some(0.5), None, None, None, None, None).await?;

    println!("  client calls set auto brightness");
    validate_display(None, Some(true), None, None, None, None).await?;

    println!("  client calls set auto brightness value");
    validate_display(None, None, Some(0.5), None, None, None).await?;

    println!("  client calls set low light mode");
    validate_display(None, None, None, Some(LowLightMode::Enable), None, None).await?;

    println!("  client calls set theme");
    validate_display(None, None, None, None, Some(ThemeType::Dark), None).await?;

    println!("  client calls set screen enabled");
    validate_display(None, None, None, None, Some(ThemeType::Dark), Some(false)).await?;

    println!("  client can modify multiple settings");
    validate_display(Some(0.3), Some(false), Some(0.8), None, Some(ThemeType::Light), Some(true))
        .await?;

    println!("factory reset tests");
    println!("  client calls set local reset allowed");
    validate_factory_reset(true).await?;

    println!("light tests");
    println!(" client calls light set");
    validate_light_set().await?;
    println!(" client calls watch light groups");
    validate_light_watch().await?;
    println!(" client calls watch individual light group");
    validate_light_watch_individual().await?;

    println!("  client calls watch light sensor");
    validate_light_sensor().await?;

    println!("input service tests");
    println!("  client calls input watch");
    validate_input(None).await?;

    println!("  client calls set input");
    validate_input(Some(false)).await?;

    println!("input2 service tests");
    println!("  client calls input watch2");
    validate_input2_watch().await?;

    println!("  client calls set input with microphone");
    validate_input2_set(DeviceType::Microphone, "microphone", 3, "Available | Active").await?;
    println!("  client calls set input with camera");
    validate_input2_set(DeviceType::Camera, "camera", 3, "Available | Active").await?;

    println!("do not disturb service tests");
    println!("  client calls dnd watch");
    validate_dnd(Some(false), Some(false)).await?;

    println!("  client calls set user initiated do not disturb");
    validate_dnd(Some(true), Some(false)).await?;

    println!("  client calls set night mode initiated do not disturb");
    validate_dnd(Some(false), Some(true)).await?;

    println!("intl service tests");
    println!("  client calls intl set");
    validate_intl_set().await?;
    println!("  client calls intl watch");
    validate_intl_watch().await?;

    println!("night mode service tests");
    println!("  client calls night mode watch");
    validate_night_mode(None).await?;

    println!("  client calls set night_mode_enabled");
    validate_night_mode(Some(true)).await?;

    println!("  set() output");
    validate_night_mode_set_output(true).await?;
    validate_night_mode_set_output(false).await?;

    println!("  watch() output");
    validate_night_mode_watch_output(None).await?;
    validate_night_mode_watch_output(Some(true)).await?;
    validate_night_mode_watch_output(Some(false)).await?;

    println!("privacy service tests");
    println!("  client calls privacy watch");
    validate_privacy(None).await?;

    println!("  client calls set user_data_sharing_consent");
    validate_privacy(Some(true)).await?;

    println!("  set() output");
    validate_privacy_set_output(true).await?;
    validate_privacy_set_output(false).await?;

    println!("  watch() output");
    validate_privacy_watch_output(None).await?;
    validate_privacy_watch_output(Some(true)).await?;
    validate_privacy_watch_output(Some(false)).await?;

    println!("setup service tests");
    println!(" client calls set config interfaces");
    validate_setup().await?;

    println!("volume policy tests");
    println!("  client calls get");
    validate_volume_policy_get().await?;
    println!("  client calls add");
    validate_volume_policy_add().await?;
    println!("  client calls remove");
    validate_volume_policy_remove().await?;

    Ok(())
}

// Creates a service in an environment for a given setting type.
// Usage: create_service!(service_enum_name,
//          request_name => {code block},
//          request2_name => {code_block}
//          ... );
macro_rules! create_service  {
    ($setting_type:path, $( $request:pat => $callback:block ),*) => {{

        let mut fs = ServiceFs::new();
        fs.add_fidl_service($setting_type);
        let env = fs.create_nested_environment(ENV_NAME)?;

        fasync::Task::spawn(fs.for_each_concurrent(None, move |connection| {
            async move {
                #![allow(unreachable_patterns)]
                match connection {
                    $setting_type(stream) => {
                        stream
                            .err_into::<anyhow::Error>()
                            .try_for_each(|req| async move {
                                match req {
                                    $($request => $callback)*
                                    _ => panic!("Incorrect command to service"),
                                }
                                Ok(())
                            })
                            .unwrap_or_else(|e: anyhow::Error| panic!(
                                "error running setui server: {:?}",
                                e
                            )).await;
                    }
                    _ => {
                        panic!("Unexpected service");
                    }
                }
            }
        })).detach();
        env
    }};
}

/// Creates a one-item list of input devices with the given properties.
fn create_input_devices(
    device_type: DeviceType,
    device_name: &str,
    device_state: u64,
) -> Vec<InputDevice> {
    let mut devices = Vec::new();
    let mut source_states = Vec::new();
    source_states.push(SourceState {
        source: Some(DeviceStateSource::Hardware),
        state: Some(DeviceState {
            toggle_flags: ToggleStateFlags::from_bits(1),
            ..DeviceState::EMPTY
        }),
        ..SourceState::EMPTY
    });
    source_states.push(SourceState {
        source: Some(DeviceStateSource::Software),
        state: Some(u64_to_state(device_state)),
        ..SourceState::EMPTY
    });
    let device = InputDevice {
        device_name: Some(device_name.to_string()),
        device_type: Some(device_type),
        source_states: Some(source_states),
        mutable_toggle_state: ToggleStateFlags::from_bits(12),
        state: Some(u64_to_state(device_state)),
        ..InputDevice::EMPTY
    };
    devices.push(device);
    devices
}

async fn validate_intl_set() -> Result<(), Error> {
    const TEST_TIME_ZONE: &str = "GMT";
    const TEST_TEMPERATURE_UNIT: TemperatureUnit = TemperatureUnit::Celsius;
    const TEST_LOCALE: &str = "blah";
    const TEST_HOUR_CYCLE: fidl_fuchsia_settings::HourCycle = fidl_fuchsia_settings::HourCycle::H12;

    let env = create_service!(Services::Intl,
        IntlRequest::Set { settings, responder } => {
            assert_eq!(Some(TimeZoneId { id: TEST_TIME_ZONE.to_string() }), settings.time_zone_id);
            assert_eq!(Some(TEST_TEMPERATURE_UNIT), settings.temperature_unit);
            assert_eq!(Some(vec![LocaleId { id: TEST_LOCALE.into() }]), settings.locales);
            assert_eq!(Some(TEST_HOUR_CYCLE), settings.hour_cycle);
            responder.send(&mut Ok(()))?;
    });

    let intl_service =
        env.connect_to_protocol::<IntlMarker>().context("Failed to connect to intl service")?;

    assert_set!(intl::command(
        intl_service,
        Some(TimeZoneId { id: TEST_TIME_ZONE.to_string() }),
        Some(TEST_TEMPERATURE_UNIT),
        vec![LocaleId { id: TEST_LOCALE.into() }],
        Some(TEST_HOUR_CYCLE),
        false,
    ));
    Ok(())
}

async fn validate_intl_watch() -> Result<(), Error> {
    const TEST_TIME_ZONE: &str = "GMT";
    const TEST_TEMPERATURE_UNIT: TemperatureUnit = TemperatureUnit::Celsius;
    const TEST_LOCALE: &str = "blah";
    const TEST_HOUR_CYCLE: fidl_fuchsia_settings::HourCycle = fidl_fuchsia_settings::HourCycle::H12;

    let env = create_service!(Services::Intl,
        IntlRequest::Watch { responder } => {
            responder.send(IntlSettings {
                locales: Some(vec![LocaleId { id: TEST_LOCALE.into() }]),
                temperature_unit: Some(TEST_TEMPERATURE_UNIT),
                time_zone_id: Some(TimeZoneId { id: TEST_TIME_ZONE.to_string() }),
                hour_cycle: Some(TEST_HOUR_CYCLE),
                ..IntlSettings::EMPTY
            })?;
        }
    );

    let intl_service =
        env.connect_to_protocol::<IntlMarker>().context("Failed to connect to intl service")?;

    let output = assert_watch!(intl::command(intl_service, None, None, vec![], None, false));
    assert_eq!(
        output,
        format!(
            "{:#?}",
            IntlSettings {
                locales: Some(vec![LocaleId { id: TEST_LOCALE.into() }]),
                temperature_unit: Some(TEST_TEMPERATURE_UNIT),
                time_zone_id: Some(TimeZoneId { id: TEST_TIME_ZONE.to_string() }),
                hour_cycle: Some(TEST_HOUR_CYCLE),
                ..IntlSettings::EMPTY
            }
        )
    );
    Ok(())
}

async fn validate_device() -> Result<(), Error> {
    let env = create_service!(Services::Device,
        DeviceRequest::Watch { responder } => {
            responder.send(DeviceSettings {
                build_tag: Some(TEST_BUILD_TAG.to_string()),
                ..DeviceSettings::EMPTY
            })?;
        }
    );

    let device_service =
        env.connect_to_protocol::<DeviceMarker>().context("Failed to connect to device service")?;

    device::command(device_service).try_next().await?;
    Ok(())
}

// Can only check one mutate option at once.
async fn validate_display(
    expected_brightness: Option<f32>,
    expected_auto_brightness: Option<bool>,
    expected_auto_brightness_value: Option<f32>,
    expected_low_light_mode: Option<LowLightMode>,
    expected_theme_type: Option<ThemeType>,
    expected_screen_enabled: Option<bool>,
) -> Result<(), Error> {
    let env = create_service!(
        Services::Display, DisplayRequest::Set { settings, responder, } => {
            if let (Some(brightness_value), Some(expected_brightness_value)) =
              (settings.brightness_value, expected_brightness) {
                assert_eq!(brightness_value, expected_brightness_value);
                responder.send(&mut Ok(()))?;
            } else if let (Some(auto_brightness), Some(expected_auto_brightness_value)) =
              (settings.auto_brightness, expected_auto_brightness) {
                assert_eq!(auto_brightness, expected_auto_brightness_value);
                responder.send(&mut Ok(()))?;
            } else if let (Some(auto_brightness_value), Some(expected_auto_brightness_value)) =
              (settings.adjusted_auto_brightness, expected_auto_brightness_value) {
                assert_eq!(auto_brightness_value, expected_auto_brightness_value);
                responder.send(&mut Ok(()))?;
            } else if let (Some(low_light_mode), Some(expected_low_light_mode_value)) =
              (settings.low_light_mode, expected_low_light_mode) {
                assert_eq!(low_light_mode, expected_low_light_mode_value);
                responder.send(&mut Ok(()))?;
            } else if let (Some(Theme{ theme_type: Some(theme_type), ..}), Some(expected_theme_type_value)) =
              (settings.theme, expected_theme_type) {
                assert_eq!(theme_type, expected_theme_type_value);
                responder.send(&mut Ok(()))?;
            } else if let (Some(screen_enabled), Some(expected_screen_enabled_value)) =
              (settings.screen_enabled, expected_screen_enabled) {
              assert_eq!(screen_enabled, expected_screen_enabled_value);
              responder.send(&mut Ok(()))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        DisplayRequest::Watch { responder } => {
            responder.send(DisplaySettings {
                auto_brightness: Some(false),
                adjusted_auto_brightness: Some(0.5),
                brightness_value: Some(0.5),
                low_light_mode: Some(LowLightMode::Disable),
                theme: Some(Theme{theme_type: Some(ThemeType::Default), ..Theme::EMPTY}),
                screen_enabled: Some(true),
                ..DisplaySettings::EMPTY
            })?;
        }
    );

    let display_service = env
        .connect_to_protocol::<DisplayMarker>()
        .context("Failed to connect to display service")?;

    assert_successful!(display::command(
        display_service,
        expected_brightness,
        expected_auto_brightness,
        expected_auto_brightness_value,
        false,
        expected_low_light_mode,
        Some(Theme { theme_type: expected_theme_type, ..Theme::EMPTY }),
        expected_screen_enabled,
    ));

    Ok(())
}

// Validates the set and watch for factory reset.
async fn validate_factory_reset(expected_local_reset_allowed: bool) -> Result<(), Error> {
    let env = create_service!(
        Services::FactoryReset, FactoryResetRequest::Set { settings, responder, } => {
            if let (Some(local_reset_allowed), expected_local_reset_allowed) =
                (settings.is_local_reset_allowed, expected_local_reset_allowed)
            {
                assert_eq!(local_reset_allowed, expected_local_reset_allowed);
                responder.send(&mut Ok(()))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        FactoryResetRequest::Watch { responder } => {
            responder.send(FactoryResetSettings {
                is_local_reset_allowed: Some(true),
                ..FactoryResetSettings::EMPTY
            })?;
        }
    );

    let factory_reset_service = env
        .connect_to_protocol::<FactoryResetMarker>()
        .context("Failed to connect to factory reset service")?;

    assert_successful!(factory_reset::command(
        factory_reset_service,
        Some(expected_local_reset_allowed)
    ));

    Ok(())
}

// Can only check one mutate option at once
async fn validate_light_sensor() -> Result<(), Error> {
    let watch_called = Arc::new(RwLock::new(false));

    let watch_called_clone = watch_called.clone();

    let (display_service, mut stream) =
        fidl::endpoints::create_proxy_and_stream::<DisplayMarker>().unwrap();

    fasync::Task::spawn(async move {
        while let Some(request) = stream.try_next().await.unwrap() {
            match request {
                DisplayRequest::WatchLightSensor2 { delta: _, responder } => {
                    *watch_called_clone.write() = true;
                    responder
                        .send(LightSensorData {
                            illuminance_lux: Some(100.0),
                            color: Some(fidl_fuchsia_ui_types::ColorRgb {
                                red: 25.0,
                                green: 16.0,
                                blue: 59.0,
                            }),
                            ..LightSensorData::EMPTY
                        })
                        .unwrap();
                }
                _ => {}
            }
        }
    })
    .detach();

    assert_watch!(display::command(display_service, None, None, None, true, None, None, None));
    assert_eq!(*watch_called.read(), true);
    Ok(())
}

async fn validate_accessibility_set() -> Result<(), Error> {
    const TEST_COLOR: fidl_fuchsia_ui_types::ColorRgba =
        fidl_fuchsia_ui_types::ColorRgba { red: 238.0, green: 23.0, blue: 128.0, alpha: 255.0 };
    let expected_options: AccessibilityOptions = AccessibilityOptions {
        audio_description: Some(true),
        screen_reader: Some(true),
        color_inversion: Some(false),
        enable_magnification: Some(false),
        color_correction: Some(ColorBlindnessType::Protanomaly),
        caption_options: Some(CaptionCommands::CaptionOptions(CaptionOptions {
            for_media: Some(true),
            for_tts: Some(false),
            window_color: Some(TEST_COLOR),
            background_color: Some(TEST_COLOR),
            style: CaptionFontStyle {
                font_family: Some(CaptionFontFamily::Cursive),
                font_color: Some(TEST_COLOR),
                relative_size: Some(1.0),
                char_edge_style: Some(EdgeStyle::Raised),
            },
        })),
    };

    let env = create_service!(
        Services::Accessibility, AccessibilityRequest::Set { settings, responder } => {
            assert_eq!(expected_options.audio_description, settings.audio_description);
            assert_eq!(expected_options.screen_reader, settings.screen_reader);
            assert_eq!(expected_options.color_inversion, settings.color_inversion);
            assert_eq!(expected_options.enable_magnification, settings.enable_magnification);
            assert_eq!(expected_options.color_correction, settings.color_correction);

            // If no caption options are provided, then captions_settings field in service should
            // also be None. The inverse of this should also be true.
            assert_eq!(expected_options.caption_options.is_some(), settings.captions_settings.is_some());
            match (settings.captions_settings, expected_options.caption_options) {
                (Some(captions_settings), Some(caption_settings_enum)) => {
                    let CaptionCommands::CaptionOptions(input) = caption_settings_enum;

                    assert_eq!(input.for_media, captions_settings.for_media);
                    assert_eq!(input.for_tts, captions_settings.for_tts);
                    assert_eq!(input.window_color, captions_settings.window_color);
                    assert_eq!(input.background_color, captions_settings.background_color);

                    if let Some(font_style) = captions_settings.font_style {
                        let input_style = input.style;

                        assert_eq!(input_style.font_family, font_style.family);
                        assert_eq!(input_style.font_color, font_style.color);
                        assert_eq!(input_style.relative_size, font_style.relative_size);
                        assert_eq!(input_style.char_edge_style, font_style.char_edge_style);
                    }
                }
                _ => {}
            }

            responder.send(&mut Ok(()))?;
        }
    );

    let accessibility_service = env
        .connect_to_protocol::<AccessibilityMarker>()
        .context("Failed to connect to accessibility service")?;

    let output = assert_set!(accessibility::command(accessibility_service, expected_options));
    assert_eq!(output, "Successfully set AccessibilitySettings");
    Ok(())
}

async fn validate_accessibility_watch() -> Result<(), Error> {
    let env = create_service!(
        Services::Accessibility,
        AccessibilityRequest::Watch { responder } => {
            responder.send(AccessibilitySettings::EMPTY)?;
        }
    );

    let accessibility_service = env
        .connect_to_protocol::<AccessibilityMarker>()
        .context("Failed to connect to accessibility service")?;

    let output = assert_watch!(accessibility::command(
        accessibility_service,
        AccessibilityOptions::default()
    ));
    assert_eq!(output, format!("{:#?}", AccessibilitySettings::EMPTY));
    Ok(())
}

async fn validate_audio(expected: &'static ExpectedStreamSettingsStruct) -> Result<(), Error> {
    let env = create_service!(Services::Audio,
        AudioRequest::Set { settings, responder } => {
            if let Some(streams) = settings.streams {
                verify_streams(streams, expected);
                responder.send(&mut (Ok(())))?;
            } else if let Some(input) = settings.input {
                if let (Some(input_muted), Some(expected_input_muted)) =
                    (input.muted, expected.input_muted) {
                    assert_eq!(input_muted, expected_input_muted);
                    responder.send(&mut (Ok(())))?;
                }
            }
        },
        AudioRequest::Watch { responder } => {
            responder.send(AudioSettings {
                streams: Some(vec![AudioStreamSettings {
                    stream: Some(AudioRenderUsage::Media),
                    source: Some(fidl_fuchsia_settings::AudioStreamSettingSource::User),
                    user_volume: Some(Volume {
                        level: Some(0.6),
                        muted: Some(false),
                        ..Volume::EMPTY
                    }),
                    ..AudioStreamSettings::EMPTY
                }]),
                input: Some(AudioInput {
                    muted: Some(true),
                    ..AudioInput::EMPTY
                }),
                ..AudioSettings::EMPTY
            })?;
        }
    );

    let audio_service =
        env.connect_to_protocol::<AudioMarker>().context("Failed to connect to audio service")?;

    assert_successful!(audio::command(
        audio_service,
        expected.stream,
        expected.source,
        expected.level,
        expected.volume_muted,
        expected.input_muted,
    ));
    Ok(())
}

async fn validate_input(expected_mic_muted: Option<bool>) -> Result<(), Error> {
    let env = create_service!(Services::Input,
        InputRequest::Set { settings, responder } => {
            if let Some(Microphone { muted, .. }) = settings.microphone {
                assert_eq!(expected_mic_muted, muted);
                responder.send(&mut (Ok(())))?;
            }
        },
        InputRequest::Watch { responder } => {
            responder.send(InputDeviceSettings {
                microphone: Some(Microphone {
                    muted: expected_mic_muted,
                    ..Microphone::EMPTY
                }),
                ..InputDeviceSettings::EMPTY
            })?;
        }
    );

    let input_service =
        env.connect_to_protocol::<InputMarker>().context("Failed to connect to input service")?;

    let either = input::command(input_service, expected_mic_muted).await?;
    if expected_mic_muted.is_none() {
        if let utils::Either::Watch(mut stream) = either {
            let output = stream.try_next().await?.expect("Watch should have a result");
            assert_eq!(
                output,
                format!(
                    "{:#?}",
                    InputDeviceSettings {
                        microphone: Some(Microphone {
                            muted: expected_mic_muted,
                            ..Microphone::EMPTY
                        }),
                        ..InputDeviceSettings::EMPTY
                    }
                )
            );
        } else {
            panic!("Did not expect set result for a watch command");
        }
    } else if let utils::Either::Set(output) = either {
        assert_eq!(
            output,
            format!("Successfully set mic mute to {}\n", expected_mic_muted.unwrap())
        );
    } else {
        panic!("Did not expect watch result for a set command");
    }

    Ok(())
}

/// Transforms an u64 into an fuchsia_fidl_settings::DeviceState.
fn u64_to_state(num: u64) -> DeviceState {
    DeviceState { toggle_flags: ToggleStateFlags::from_bits(num), ..DeviceState::EMPTY }
}

async fn validate_input2_watch() -> Result<(), Error> {
    let env = create_service!(Services::Input,
        InputRequest::Watch2 { responder } => {
            responder.send(InputSettings {
                devices: Some(
                    create_input_devices(
                        DeviceType::Camera,
                        "camera",
                        1,
                    )
                ),
                ..InputSettings::EMPTY
            })?;
        }
    );

    let input_service =
        env.connect_to_protocol::<InputMarker>().context("Failed to connect to input service")?;

    let output = assert_watch!(input::command2(input_service, None, None, None));
    // Just check that the output contains some key strings that confirms the watch returned.
    // The string representation may not necessarily be in the same order.
    assert!(output.contains("Software"));
    assert!(output.contains("source_states: Some"));
    assert!(output.contains("toggle_flags: Some"));
    assert!(output.contains("camera"));
    assert!(output.contains("Available"));
    Ok(())
}

async fn validate_input2_set(
    device_type: DeviceType,
    device_name: &'static str,
    device_state: u64,
    expected_state_string: &str,
) -> Result<(), Error> {
    let env = create_service!(Services::Input,
        InputRequest::SetStates { input_states, responder } => {
            input_states.iter().for_each(move |state| {
                assert_eq!(Some(device_type), state.device_type);
                assert_eq!(Some(device_name.to_string()), state.name);
                assert_eq!(Some(u64_to_state(device_state)), state.state);
            });
            responder.send(&mut (Ok(())))?;
        }
    );

    let input_service =
        env.connect_to_protocol::<InputMarker>().context("Failed to connect to input service")?;

    let output = assert_set!(input::command2(
        input_service,
        Some(device_type),
        Some(device_name.to_string()),
        Some(u64_to_state(device_state)),
    ));
    // Just check that the output contains some key strings that confirms the set returned.
    // The string representation may not necessarily be in the same order.
    assert!(output.contains(&format!("{:?}", device_type)));
    assert!(output.contains(&format!("{:?}", device_name)));
    assert!(output.contains(expected_state_string));
    Ok(())
}

fn verify_streams(
    streams: Vec<AudioStreamSettings>,
    expected: &'static ExpectedStreamSettingsStruct,
) {
    let extracted_stream_settings = streams.get(0).unwrap();
    if let (Some(stream), Some(expected_stream)) =
        (extracted_stream_settings.stream, expected.stream)
    {
        assert_eq!(stream, expected_stream);
    }
    if let (Some(source), Some(expected_source)) =
        (extracted_stream_settings.source, expected.source)
    {
        assert_eq!(source, expected_source);
    }
    if let Some(user_volume) = extracted_stream_settings.user_volume.as_ref() {
        if let (Some(level), Some(expected_level)) = (user_volume.level, expected.level) {
            assert_eq!(level, expected_level);
        }
        if let (Some(volume_muted), Some(expected_volume_muted)) =
            (user_volume.muted, expected.volume_muted)
        {
            assert_eq!(volume_muted, expected_volume_muted);
        }
    }
}

async fn validate_dnd(
    expected_user_dnd: Option<bool>,
    expected_night_mode_dnd: Option<bool>,
) -> Result<(), Error> {
    let env = create_service!(Services::DoNotDisturb,
        DoNotDisturbRequest::Set { settings, responder } => {
            if let(Some(user_dnd), Some(expected_user_dnd)) =
                (settings.user_initiated_do_not_disturb, expected_user_dnd) {
                assert_eq!(user_dnd, expected_user_dnd);
                responder.send(&mut Ok(()))?;
            } else if let (Some(night_mode_dnd), Some(expected_night_mode_dnd)) =
                (settings.night_mode_initiated_do_not_disturb, expected_night_mode_dnd) {
                assert_eq!(night_mode_dnd, expected_night_mode_dnd);
                responder.send(&mut (Ok(())))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        DoNotDisturbRequest::Watch { responder } => {
            responder.send(DoNotDisturbSettings {
                user_initiated_do_not_disturb: Some(false),
                night_mode_initiated_do_not_disturb: Some(false),
                ..DoNotDisturbSettings::EMPTY
            })?;
        }
    );

    let do_not_disturb_service = env
        .connect_to_protocol::<DoNotDisturbMarker>()
        .context("Failed to connect to do not disturb service")?;

    assert_successful!(do_not_disturb::command(
        do_not_disturb_service,
        expected_user_dnd,
        expected_night_mode_dnd
    ));
    Ok(())
}

async fn validate_light_set() -> Result<(), Error> {
    const TEST_NAME: &str = "test_name";
    const LIGHT_VAL_1: f64 = 0.2;
    const LIGHT_VAL_2: f64 = 0.42;

    let env = create_service!(Services::Light,
        LightRequest::SetLightGroupValues { name, state, responder } => {
            assert_eq!(name, TEST_NAME);
            assert_eq!(state, vec![LightState { value: Some(LightValue::Brightness(LIGHT_VAL_1)), ..LightState::EMPTY },
            LightState { value: Some(LightValue::Brightness(LIGHT_VAL_2)), ..LightState::EMPTY }]);
            responder.send(&mut Ok(()))?;
        }
    );

    let light_service =
        env.connect_to_protocol::<LightMarker>().context("Failed to connect to light service")?;

    assert_set!(light::command(
        light_service,
        setui_client_lib::LightGroup {
            name: Some(TEST_NAME.to_string()),
            simple: vec![],
            brightness: vec![LIGHT_VAL_1, LIGHT_VAL_2],
            rgb: vec![],
        },
    ));
    Ok(())
}

async fn validate_light_watch() -> Result<(), Error> {
    const TEST_NAME: &str = "test_name";
    const ENABLED: bool = false;
    const LIGHT_TYPE: LightType = LightType::Simple;
    const LIGHT_VAL_1: f64 = 0.2;
    const LIGHT_VAL_2: f64 = 0.42;

    let env = create_service!(Services::Light,
        LightRequest::WatchLightGroups { responder } => {
            responder.send(&mut vec![
                LightGroup {
                    name: Some(TEST_NAME.to_string()),
                    enabled: Some(ENABLED),
                    type_: Some(LIGHT_TYPE),
                    lights: Some(vec![
                        LightState { value: Some(LightValue::Brightness(LIGHT_VAL_1)), ..LightState::EMPTY },
                        LightState { value: Some(LightValue::Brightness(LIGHT_VAL_2)), ..LightState::EMPTY }
                    ]),
                    ..LightGroup::EMPTY
                }
            ].into_iter())?;
        }
    );

    let light_service =
        env.connect_to_protocol::<LightMarker>().context("Failed to connect to light service")?;

    let output = assert_watch!(light::command(
        light_service,
        setui_client_lib::LightGroup {
            name: None,
            simple: vec![],
            brightness: vec![],
            rgb: vec![],
        },
    ));
    assert_eq!(
        output,
        format!(
            "{:#?}",
            vec![LightGroup {
                name: Some(TEST_NAME.to_string()),
                enabled: Some(ENABLED),
                type_: Some(LIGHT_TYPE),
                lights: Some(vec![
                    LightState {
                        value: Some(LightValue::Brightness(LIGHT_VAL_1)),
                        ..LightState::EMPTY
                    },
                    LightState {
                        value: Some(LightValue::Brightness(LIGHT_VAL_2)),
                        ..LightState::EMPTY
                    }
                ]),
                ..LightGroup::EMPTY
            }]
        )
    );
    Ok(())
}

async fn validate_light_watch_individual() -> Result<(), Error> {
    const TEST_NAME: &str = "test_name";
    const ENABLED: bool = false;
    const LIGHT_TYPE: LightType = LightType::Simple;
    const LIGHT_VAL_1: f64 = 0.2;
    const LIGHT_VAL_2: f64 = 0.42;

    let env = create_service!(Services::Light,
        LightRequest::WatchLightGroup { name, responder } => {
            responder.send(LightGroup {
                    name: Some(name),
                    enabled: Some(ENABLED),
                    type_: Some(LIGHT_TYPE),
                    lights: Some(vec![
                        LightState { value: Some(LightValue::Brightness(LIGHT_VAL_1)), ..LightState::EMPTY },
                        LightState { value: Some(LightValue::Brightness(LIGHT_VAL_2)), ..LightState::EMPTY }
                    ]),
                    ..LightGroup::EMPTY
                })?;
        }
    );

    let light_service =
        env.connect_to_protocol::<LightMarker>().context("Failed to connect to light service")?;

    let output = assert_watch!(light::command(
        light_service,
        setui_client_lib::LightGroup {
            name: Some(TEST_NAME.to_string()),
            simple: vec![],
            brightness: vec![],
            rgb: vec![],
        },
    ));
    assert_eq!(
        output,
        format!(
            "{:#?}",
            LightGroup {
                name: Some(TEST_NAME.to_string()),
                enabled: Some(ENABLED),
                type_: Some(LIGHT_TYPE),
                lights: Some(vec![
                    LightState {
                        value: Some(LightValue::Brightness(LIGHT_VAL_1)),
                        ..LightState::EMPTY
                    },
                    LightState {
                        value: Some(LightValue::Brightness(LIGHT_VAL_2)),
                        ..LightState::EMPTY
                    }
                ]),
                ..LightGroup::EMPTY
            }
        )
    );
    Ok(())
}

async fn validate_night_mode(expected_night_mode_enabled: Option<bool>) -> Result<(), Error> {
    let env = create_service!(
        Services::NightMode, NightModeRequest::Set { settings, responder, } => {
            if let (Some(night_mode_enabled), Some(expected_night_mode_enabled_value)) =
                (settings.night_mode_enabled, expected_night_mode_enabled) {
                assert_eq!(night_mode_enabled, expected_night_mode_enabled_value);
                responder.send(&mut Ok(()))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        NightModeRequest::Watch { responder } => {
            responder.send(NightModeSettings {
                night_mode_enabled: Some(false),
                ..NightModeSettings::EMPTY
            })?;
        }
    );

    let night_mode_service = env
        .connect_to_protocol::<NightModeMarker>()
        .context("Failed to connect to night mode service")?;

    assert_successful!(night_mode::command(night_mode_service, expected_night_mode_enabled));
    Ok(())
}

async fn validate_night_mode_set_output(expected_night_mode_enabled: bool) -> Result<(), Error> {
    let env = create_service!(
        Services::NightMode, NightModeRequest::Set { settings: _, responder, } => {
            responder.send(&mut Ok(()))?;
        },
        NightModeRequest::Watch { responder } => {
            responder.send(NightModeSettings {
                night_mode_enabled: Some(expected_night_mode_enabled),
                ..NightModeSettings::EMPTY
            })?;
        }
    );

    let night_mode_service = env
        .connect_to_protocol::<NightModeMarker>()
        .context("Failed to connect to night mode service")?;

    let output =
        assert_set!(night_mode::command(night_mode_service, Some(expected_night_mode_enabled)));
    assert_eq!(
        output,
        format!("Successfully set night_mode_enabled to {}", expected_night_mode_enabled)
    );
    Ok(())
}

async fn validate_night_mode_watch_output(
    expected_night_mode_enabled: Option<bool>,
) -> Result<(), Error> {
    let env = create_service!(
        Services::NightMode, NightModeRequest::Set { settings: _, responder, } => {
            responder.send(&mut Ok(()))?;
        },
        NightModeRequest::Watch { responder } => {
            responder.send(NightModeSettings {
                night_mode_enabled: expected_night_mode_enabled,
                ..NightModeSettings::EMPTY
            })?;
        }
    );

    let night_mode_service = env
        .connect_to_protocol::<NightModeMarker>()
        .context("Failed to connect to night_mode service")?;

    let output = assert_watch!(night_mode::command(night_mode_service, None));
    assert_eq!(
        output,
        format!(
            "{:#?}",
            NightModeSettings {
                night_mode_enabled: expected_night_mode_enabled,
                ..NightModeSettings::EMPTY
            }
        )
    );
    Ok(())
}

async fn validate_privacy(expected_user_data_sharing_consent: Option<bool>) -> Result<(), Error> {
    let env = create_service!(
        Services::Privacy, PrivacyRequest::Set { settings, responder, } => {
            if let (Some(user_data_sharing_consent), Some(expected_user_data_sharing_consent_value)) =
                (settings.user_data_sharing_consent, expected_user_data_sharing_consent) {
                assert_eq!(user_data_sharing_consent, expected_user_data_sharing_consent_value);
                responder.send(&mut Ok(()))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        PrivacyRequest::Watch { responder } => {
            responder.send(PrivacySettings {
                user_data_sharing_consent: Some(false),
                ..PrivacySettings::EMPTY
            })?;
        }
    );

    let privacy_service = env
        .connect_to_protocol::<PrivacyMarker>()
        .context("Failed to connect to privacy service")?;

    assert_successful!(privacy::command(privacy_service, expected_user_data_sharing_consent));
    Ok(())
}

async fn validate_privacy_set_output(
    expected_user_data_sharing_consent: bool,
) -> Result<(), Error> {
    let env = create_service!(
        Services::Privacy, PrivacyRequest::Set { settings: _, responder, } => {
            responder.send(&mut Ok(()))?;
        },
        PrivacyRequest::Watch { responder } => {
            responder.send(PrivacySettings {
                user_data_sharing_consent: Some(expected_user_data_sharing_consent),
                ..PrivacySettings::EMPTY
            })?;
        }
    );

    let privacy_service = env
        .connect_to_protocol::<PrivacyMarker>()
        .context("Failed to connect to privacy service")?;

    let output =
        assert_set!(privacy::command(privacy_service, Some(expected_user_data_sharing_consent)));
    assert_eq!(
        output,
        format!(
            "Successfully set user_data_sharing_consent to {}",
            expected_user_data_sharing_consent
        )
    );
    Ok(())
}

async fn validate_privacy_watch_output(
    expected_user_data_sharing_consent: Option<bool>,
) -> Result<(), Error> {
    let env = create_service!(
        Services::Privacy, PrivacyRequest::Set { settings: _, responder, } => {
            responder.send(&mut Ok(()))?;
        },
        PrivacyRequest::Watch { responder } => {
            responder.send(PrivacySettings {
                user_data_sharing_consent: expected_user_data_sharing_consent,
                ..PrivacySettings::EMPTY
            })?;
        }
    );

    let privacy_service = env
        .connect_to_protocol::<PrivacyMarker>()
        .context("Failed to connect to privacy service")?;

    let output = assert_watch!(privacy::command(privacy_service, None));
    assert_eq!(
        output,
        format!(
            "{:#?}",
            PrivacySettings {
                user_data_sharing_consent: expected_user_data_sharing_consent,
                ..PrivacySettings::EMPTY
            }
        )
    );
    Ok(())
}

fn create_setup_setting(interfaces: ConfigurationInterfaces) -> SetupSettings {
    let mut settings = SetupSettings::EMPTY;
    settings.enabled_configuration_interfaces = Some(interfaces);

    settings
}

async fn validate_setup() -> Result<(), Error> {
    let expected_set_interfaces = ConfigurationInterfaces::Ethernet;
    let expected_watch_interfaces =
        ConfigurationInterfaces::Wifi | ConfigurationInterfaces::Ethernet;
    let env = create_service!(
        Services::Setup, SetupRequest::Set { settings, responder, } => {
            if let Some(interfaces) = settings.enabled_configuration_interfaces {
                assert_eq!(interfaces, expected_set_interfaces);
                responder.send(&mut Ok(()))?;
            } else {
                panic!("Unexpected call to set");
            }
        },
        SetupRequest::Watch { responder } => {
            responder.send(create_setup_setting(expected_watch_interfaces))?;
        }
    );

    let setup_service =
        env.connect_to_protocol::<SetupMarker>().context("Failed to connect to setup service")?;

    assert_set!(setup::command(setup_service.clone(), Some(expected_set_interfaces)));
    let output = assert_watch!(setup::command(setup_service.clone(), None));
    assert_eq!(
        output,
        setup::describe_setup_setting(&create_setup_setting(expected_watch_interfaces))
    );
    Ok(())
}

// Verifies that invoking a volume policy command with no arguments fetches the policy properties.
async fn validate_volume_policy_get() -> Result<(), Error> {
    // Create a fake volume policy service that responds to GetProperties with a single property.
    // Any other calls will cause the test to fail.
    let env = create_service!(
        Services::VolumePolicy,
        VolumePolicyControllerRequest::GetProperties { responder } => {
            let mut properties = Vec::new();
            properties.push(Property {
                target: Some(Target::Stream(AudioRenderUsage::Background)),
                active_policies: Some(vec![]),
                available_transforms: Some(vec![Transform::Max]),
                ..Property::EMPTY
            });
            responder.send(&mut properties.into_iter())?;
        }
    );

    let volume_policy_service = env
        .connect_to_protocol::<VolumePolicyControllerMarker>()
        .context("Failed to connect to volume policy service")?;

    let output = assert_get!(volume_policy::command(volume_policy_service, None, None));
    // Spot-check that the output contains the available transform in the data returned from the
    // fake service.
    assert!(output.contains("Max"));
    Ok(())
}

// Verifies that adding a new policy works and prints out the resulting policy ID.
async fn validate_volume_policy_add() -> Result<(), Error> {
    let expected_target = AudioRenderUsage::Background;
    let expected_volume: f32 = 1.0;
    let expected_policy_id = 42;
    let add_options = VolumePolicyCommands::AddPolicy(VolumePolicyOptions {
        target: expected_target,
        min: None,
        max: Some(expected_volume),
    });

    // Create a fake volume policy service that responds to AddPolicy and verifies that the inputs
    // are the same as expected, then return the expected policy ID. Any other calls will cause the
    // test to fail.
    let env = create_service!(
        Services::VolumePolicy,
        VolumePolicyControllerRequest::AddPolicy { target, parameters, responder } => {
            assert_eq!(target, Target::Stream(expected_target));
            assert_eq!(
                parameters,
                PolicyParameters::Max(PolicyVolume {
                    volume: Some(expected_volume),
                    ..PolicyVolume::EMPTY
                })
            );
            responder.send(&mut Ok(expected_policy_id))?;
        }
    );

    let volume_policy_service = env
        .connect_to_protocol::<VolumePolicyControllerMarker>()
        .context("Failed to connect to volume policy service")?;

    // Make the add call.
    let output =
        assert_set!(volume_policy::command(volume_policy_service, Some(add_options), None));
    // Verify that the output contains the policy ID returned from the fake service.
    assert!(output.contains(expected_policy_id.to_string().as_str()));
    Ok(())
}

// Verifies that removing a policy sends the proper call to the volume policy API.
async fn validate_volume_policy_remove() -> Result<(), Error> {
    let expected_policy_id = 42;
    // Create a fake volume policy service that verifies the removed policy ID matches the expected
    // value. Any other calls will cause the
    // test to fail.
    let env = create_service!(
        Services::VolumePolicy, VolumePolicyControllerRequest::RemovePolicy { policy_id, responder } => {
            assert_eq!(policy_id, expected_policy_id);
            responder.send(&mut Ok(()))?;
        }
    );

    let volume_policy_service = env
        .connect_to_protocol::<VolumePolicyControllerMarker>()
        .context("Failed to connect to volume policy service")?;

    // Attempt to remove the given policy ID.
    assert_set!(volume_policy::command(volume_policy_service, None, Some(expected_policy_id)));
    Ok(())
}
