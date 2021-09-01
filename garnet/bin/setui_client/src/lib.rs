// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl_fuchsia_settings::{ConfigurationInterfaces, LightState, LightValue, Theme};
use fuchsia_component::client::connect_to_protocol;
use structopt::StructOpt;

pub mod accessibility;
pub mod audio;
pub mod display;
pub mod do_not_disturb;
pub mod factory_reset;
pub mod input;
pub mod intl;
pub mod light;
pub mod night_mode;
pub mod privacy;
pub mod setup;
pub mod utils;
pub mod volume_policy;

/// SettingClient exercises the functionality found in SetUI service. Currently,
/// action parameters are specified at as individual arguments, but the goal is
/// to eventually parse details from a JSON file input.
#[derive(StructOpt, Debug)]
#[structopt(name = "setui_client", about = "set setting values")]
pub enum SettingClient {
    // Operations that use the new interfaces.
    #[structopt(name = "accessibility")]
    Accessibility(AccessibilityOptions),

    #[structopt(name = "audio")]
    Audio {
        #[structopt(flatten)]
        streams: AudioStreams,

        #[structopt(flatten)]
        input: AudioInput,
    },

    #[structopt(name = "display")]
    Display {
        #[structopt(short = "b", long = "brightness")]
        brightness: Option<f32>,

        #[structopt(short = "o", long = "auto_brightness_level")]
        auto_brightness_level: Option<f32>,

        #[structopt(short = "a", long = "auto_brightness")]
        auto_brightness: Option<bool>,

        #[structopt(short = "l", long = "light_sensor")]
        light_sensor: bool,

        #[structopt(
            short = "m",
            long = "low_light_mode",
            parse(try_from_str = "str_to_low_light_mode")
        )]
        low_light_mode: Option<fidl_fuchsia_settings::LowLightMode>,

        #[structopt(short = "t", long = "theme", parse(try_from_str = "str_to_theme"))]
        theme: Option<fidl_fuchsia_settings::Theme>,

        #[structopt(short = "s", long = "screen_enabled")]
        screen_enabled: Option<bool>,
    },

    #[structopt(name = "do_not_disturb")]
    DoNotDisturb {
        #[structopt(short = "u", long = "user_dnd")]
        user_dnd: Option<bool>,

        #[structopt(short = "n", long = "night_mode_dnd")]
        night_mode_dnd: Option<bool>,
    },

    #[structopt(name = "factory_reset")]
    FactoryReset {
        #[structopt(short = "l", long = "is_local_reset_allowed")]
        is_local_reset_allowed: Option<bool>,
    },

    #[structopt(name = "input")]
    Input {
        #[structopt(short = "m", long = "mic_muted")]
        mic_muted: Option<bool>,
    },

    // TODO(fxbug.dev/65686): Move back into input when the clients are migrated over.
    // TODO(fxbug.dev/66186): Support multiple input devices to be set.
    // For simplicity, currently only supports setting one input device at a time.
    #[structopt(name = "input2")]
    Input2 {
        #[structopt(flatten)]
        input_device: InputDeviceOptions,
    },

    #[structopt(name = "intl")]
    Intl {
        #[structopt(short = "z", long, parse(from_str = "str_to_time_zone"))]
        time_zone: Option<fidl_fuchsia_intl::TimeZoneId>,

        #[structopt(short = "u", long, parse(try_from_str = "str_to_temperature_unit"))]
        // Valid options are Celsius and Fahrenheit, or just "c" and "f".
        temperature_unit: Option<fidl_fuchsia_intl::TemperatureUnit>,

        #[structopt(short, long, parse(from_str = "str_to_locale"))]
        /// List of locales, separated by spaces.
        locales: Vec<fidl_fuchsia_intl::LocaleId>,

        #[structopt(short = "h", long, parse(try_from_str = "str_to_hour_cycle"))]
        hour_cycle: Option<fidl_fuchsia_settings::HourCycle>,

        #[structopt(long)]
        /// If set, this flag will set locales as an empty list. Overrides the locales arguments.
        clear_locales: bool,
    },

    #[structopt(name = "light")]
    /// Reads and modifies the hardware light state. To get the value of all light types, omit all
    /// arguments. If setting the value for a light group, name is required, then only one type of
    /// value between simple, brightness, or rgb should be specified.
    Light {
        #[structopt(flatten)]
        light_group: LightGroup,
    },

    #[structopt(name = "night_mode")]
    NightMode {
        #[structopt(short, long)]
        night_mode_enabled: Option<bool>,
    },

    #[structopt(name = "privacy")]
    Privacy {
        #[structopt(short, long)]
        user_data_sharing_consent: Option<bool>,
    },

    #[structopt(name = "setup")]
    Setup {
        #[structopt(short = "i", long = "interfaces", parse(from_str = "str_to_interfaces"))]
        configuration_interfaces: Option<ConfigurationInterfaces>,
    },

    /// Reads and modifies volume policies that affect the behavior of the fuchsia.settings.audio.
    /// To list the policies, run the subcommand without any arguments.
    #[structopt(name = "volume_policy")]
    VolumePolicy {
        /// Adds a policy transform.
        #[structopt(subcommand)]
        add: Option<VolumePolicyCommands>,

        /// Removes a policy transform by its policy ID.
        #[structopt(short, long)]
        remove: Option<u32>,
    },
}

#[derive(StructOpt, Debug, Clone, Copy, Default)]
pub struct AccessibilityOptions {
    #[structopt(short = "a", long)]
    pub audio_description: Option<bool>,

    #[structopt(short = "s", long)]
    pub screen_reader: Option<bool>,

    #[structopt(short = "i", long)]
    pub color_inversion: Option<bool>,

    #[structopt(short = "m", long)]
    pub enable_magnification: Option<bool>,

    #[structopt(short = "c", long, parse(try_from_str = "str_to_color_blindness_type"))]
    pub color_correction: Option<fidl_fuchsia_settings::ColorBlindnessType>,

    #[structopt(subcommand)]
    pub caption_options: Option<CaptionCommands>,
}

#[derive(StructOpt, Debug, Clone, Copy)]
pub enum CaptionCommands {
    #[structopt(name = "captions")]
    CaptionOptions(CaptionOptions),
}

#[derive(StructOpt, Debug, Clone, Copy)]
pub struct CaptionOptions {
    #[structopt(short = "m", long)]
    /// Enable closed captions for media sources of audio.
    pub for_media: Option<bool>,

    #[structopt(short = "t", long)]
    /// Enable closed captions for Text-To-Speech sources of audio.
    pub for_tts: Option<bool>,

    #[structopt(short, long, parse(try_from_str = "str_to_color"))]
    /// Border color used around the closed captions window. Valid options are red, green, or blue,
    /// or just the first letter of each color (r, g, b).
    pub window_color: Option<fidl_fuchsia_ui_types::ColorRgba>,

    #[structopt(short, long, parse(try_from_str = "str_to_color"))]
    /// Border color used around the closed captions window. Valid options are red, green, or blue,
    /// or just the first letter of each color (r, g, b).
    pub background_color: Option<fidl_fuchsia_ui_types::ColorRgba>,

    #[structopt(flatten)]
    pub style: CaptionFontStyle,
}

#[derive(StructOpt, Debug, Clone, Copy)]
pub enum VolumePolicyCommands {
    #[structopt(name = "add")]
    AddPolicy(VolumePolicyOptions),
}

#[derive(StructOpt, Debug, Clone, Copy)]
pub struct VolumePolicyOptions {
    /// Target to apply the policy transform to.
    #[structopt(parse(try_from_str = "str_to_audio_stream"))]
    pub target: fidl_fuchsia_media::AudioRenderUsage,

    #[structopt(long)]
    pub min: Option<f32>,

    #[structopt(long)]
    pub max: Option<f32>,
}

#[derive(StructOpt, Debug, Clone, Copy)]
pub struct CaptionFontStyle {
    #[structopt(short, long, parse(try_from_str = "str_to_font_family"))]
    /// Font family for captions, specified by 47 CFR §79.102(k). Valid options are unknown,
    /// monospaced_serif, proportional_serif, monospaced_sans_serif, proportional_sans_serif,
    /// casual, cursive, and small_capitals,
    pub font_family: Option<fidl_fuchsia_settings::CaptionFontFamily>,

    #[structopt(short = "c", long, parse(try_from_str = "str_to_color"))]
    /// Color of the closed cpation text. Valid options are red, green, or blue, or just the first
    /// letter of each color (r, g, b).
    pub font_color: Option<fidl_fuchsia_ui_types::ColorRgba>,

    #[structopt(short, long)]
    /// Size of closed captions text relative to the default captions size. A range of [0.5, 2] is
    /// guaranteed to be supported (as 47 CFR §79.103(c)(4) establishes).
    pub relative_size: Option<f32>,

    #[structopt(short = "e", long, parse(try_from_str = "str_to_edge_style"))]
    /// Edge style for fonts as specified in 47 CFR §79.103(c)(7), valid options are none,
    /// drop_shadow, raised, depressed, and outline.
    pub char_edge_style: Option<fidl_fuchsia_settings::EdgeStyle>,
}

#[derive(StructOpt, Debug, Clone)]
pub struct InputDeviceOptions {
    #[structopt(short = "t", long = "type", parse(try_from_str = "str_to_device_type"))]
    /// The type of input device, e.g. camera or microphone.
    device_type: Option<fidl_fuchsia_settings::DeviceType>,

    #[structopt(short = "n", long = "name")]
    /// The name of the device. Must be unique within a device type.
    device_name: Option<String>,

    #[structopt(short = "s", long = "state", parse(try_from_str = "str_to_device_state"))]
    /// The device state flags, represented by the integer value of the bitwise flags.
    ///
    /// Available = 1
    /// Active = 2
    /// Muted = 4
    /// Disabled = 8
    /// Error = 16
    ///
    /// For combinations of states, add these values together.
    /// Ex: Available && Active -> 1 + 2 -> 3
    device_state: Option<fidl_fuchsia_settings::DeviceState>,
}

#[derive(StructOpt, Debug)]
pub struct AudioStreams {
    #[structopt(short = "t", long = "stream", parse(try_from_str = "str_to_audio_stream"))]
    stream: Option<fidl_fuchsia_media::AudioRenderUsage>,
    #[structopt(short = "s", long = "source", parse(try_from_str = "str_to_audio_source"))]
    source: Option<fidl_fuchsia_settings::AudioStreamSettingSource>,
    #[structopt(flatten)]
    user_volume: UserVolume,
}

#[derive(StructOpt, Debug)]
struct UserVolume {
    #[structopt(short = "l", long = "level")]
    level: Option<f32>,

    #[structopt(short = "v", long = "volume_muted")]
    volume_muted: Option<bool>,
}

#[derive(StructOpt, Debug)]
pub struct AudioInput {
    #[structopt(short = "m", long = "input_muted")]
    input_muted: Option<bool>,
}

#[derive(StructOpt, Debug, Clone)]
pub struct LightGroup {
    #[structopt(short, long)]
    /// Name of a light group to set values for. Required if setting the value of a light group.
    pub name: Option<String>,

    #[structopt(short, long)]
    /// Repeated parameter for a list of simple on/off values to set for a light group.
    pub simple: Vec<bool>,

    #[structopt(short, long)]
    /// Repeated parameter for a list of floating point brightness values from 0.0-1.0 inclusive
    /// to set for a light group, where 0.0 is minimum brightness and 1.0 is maximum.
    pub brightness: Vec<f64>,

    #[structopt(short, long, parse(try_from_str = "str_to_rgb"))]
    /// Repeated parameter for a list of RGB values to set for a light group. Values should be in
    /// the range of 0.0-1.0 inclusive and should be specified as a comma-separated list of the red,
    /// green, and blue components. Ex. 0.1,0.4,0.23
    pub rgb: Vec<fidl_fuchsia_ui_types::ColorRgb>,
}

impl Into<Vec<LightState>> for LightGroup {
    fn into(self) -> Vec<LightState> {
        if self.simple.len() > 0 {
            return self
                .simple
                .clone()
                .into_iter()
                .map(|val| LightState { value: Some(LightValue::On(val)), ..LightState::EMPTY })
                .collect::<Vec<_>>();
        }

        if self.brightness.len() > 0 {
            return self
                .brightness
                .clone()
                .into_iter()
                .map(|val| LightState {
                    value: Some(LightValue::Brightness(val)),
                    ..LightState::EMPTY
                })
                .collect::<Vec<_>>();
        }

        if self.rgb.len() > 0 {
            return self
                .rgb
                .clone()
                .into_iter()
                .map(|val| LightState { value: Some(LightValue::Color(val)), ..LightState::EMPTY })
                .collect::<Vec<_>>();
        }

        return Vec::new();
    }
}

pub async fn run_command(command: SettingClient) -> Result<(), Error> {
    match command {
        SettingClient::Display {
            brightness,
            auto_brightness_level,
            auto_brightness,
            light_sensor,
            low_light_mode,
            theme,
            screen_enabled,
        } => {
            let display_service = connect_to_protocol::<fidl_fuchsia_settings::DisplayMarker>()
                .context("Failed to connect to display service")?;
            utils::handle_mixed_result(
                "Display",
                display::command(
                    display_service,
                    brightness,
                    auto_brightness,
                    auto_brightness_level,
                    light_sensor,
                    low_light_mode,
                    theme,
                    screen_enabled,
                )
                .await,
            )
            .await?;
        }
        SettingClient::DoNotDisturb { user_dnd, night_mode_dnd } => {
            let dnd_service = connect_to_protocol::<fidl_fuchsia_settings::DoNotDisturbMarker>()
                .context("Failed to connect to do_not_disturb service")?;
            utils::handle_mixed_result(
                "DoNoDisturb",
                do_not_disturb::command(dnd_service, user_dnd, night_mode_dnd).await,
            )
            .await?;
        }
        SettingClient::FactoryReset { is_local_reset_allowed } => {
            let factory_reset_service =
                connect_to_protocol::<fidl_fuchsia_settings::FactoryResetMarker>()
                    .context("Failed to connect to factory_reset service")?;
            utils::handle_mixed_result(
                "FactoryReset",
                factory_reset::command(factory_reset_service, is_local_reset_allowed).await,
            )
            .await?;
        }
        SettingClient::Intl { time_zone, temperature_unit, locales, hour_cycle, clear_locales } => {
            let intl_service = connect_to_protocol::<fidl_fuchsia_settings::IntlMarker>()
                .context("Failed to connect to intl service")?;
            utils::handle_mixed_result(
                "Intl",
                intl::command(
                    intl_service,
                    time_zone,
                    temperature_unit,
                    locales,
                    hour_cycle,
                    clear_locales,
                )
                .await,
            )
            .await?;
        }
        SettingClient::Light { light_group } => {
            let light_mode_service = connect_to_protocol::<fidl_fuchsia_settings::LightMarker>()
                .context("Failed to connect to light service")?;
            utils::handle_mixed_result(
                "Light",
                light::command(light_mode_service, light_group).await,
            )
            .await?;
        }
        SettingClient::NightMode { night_mode_enabled } => {
            let night_mode_service =
                connect_to_protocol::<fidl_fuchsia_settings::NightModeMarker>()
                    .context("Failed to connect to night mode service")?;
            utils::handle_mixed_result(
                "NightMode",
                night_mode::command(night_mode_service, night_mode_enabled).await,
            )
            .await?;
        }
        SettingClient::Accessibility(accessibility_options) => {
            let accessibility_service =
                connect_to_protocol::<fidl_fuchsia_settings::AccessibilityMarker>()
                    .context("Failed to connect to accessibility service")?;

            utils::handle_mixed_result(
                "Accessibility",
                accessibility::command(accessibility_service, accessibility_options).await,
            )
            .await?;
        }
        SettingClient::Privacy { user_data_sharing_consent } => {
            let privacy_service = connect_to_protocol::<fidl_fuchsia_settings::PrivacyMarker>()
                .context("Failed to connect to privacy service")?;
            utils::handle_mixed_result(
                "Privacy",
                privacy::command(privacy_service, user_data_sharing_consent).await,
            )
            .await?;
        }
        SettingClient::Audio { streams, input } => {
            let audio_service = connect_to_protocol::<fidl_fuchsia_settings::AudioMarker>()
                .context("Failed to connect to audio service")?;
            let stream = streams.stream;
            let source = streams.source;
            let level = streams.user_volume.level;
            let volume_muted = streams.user_volume.volume_muted;
            let input_muted = input.input_muted;
            utils::handle_mixed_result(
                "Audio",
                audio::command(audio_service, stream, source, level, volume_muted, input_muted)
                    .await,
            )
            .await?;
        }
        SettingClient::Input { mic_muted } => {
            let input_service = connect_to_protocol::<fidl_fuchsia_settings::InputMarker>()
                .context("Failed to connect to input service")?;
            utils::handle_mixed_result("Input", input::command(input_service, mic_muted).await)
                .await?;
        }
        SettingClient::Input2 { input_device } => {
            let input_service = connect_to_protocol::<fidl_fuchsia_settings::InputMarker>()
                .context("Failed to connect to input2 service")?;
            let device_type = input_device.device_type;
            let device_name = input_device.device_name;
            let device_state = input_device.device_state;
            utils::handle_mixed_result(
                "Input2",
                input::command2(input_service, device_type, device_name, device_state).await,
            )
            .await?;
        }
        SettingClient::Setup { configuration_interfaces } => {
            let setup_service = connect_to_protocol::<fidl_fuchsia_settings::SetupMarker>()
                .context("Failed to connect to setup service")?;
            utils::handle_mixed_result(
                "Setup",
                setup::command(setup_service, configuration_interfaces).await,
            )
            .await?;
        }
        SettingClient::VolumePolicy { add, remove } => {
            let setup_service =
                connect_to_protocol::<fidl_fuchsia_settings_policy::VolumePolicyControllerMarker>()
                    .context("Failed to connect to volume policy service")?;
            utils::handle_mixed_result(
                "Volume policy",
                volume_policy::command(setup_service, add, remove).await,
            )
            .await?;
        }
    }
    Ok(())
}

fn str_to_time_zone(src: &&str) -> fidl_fuchsia_intl::TimeZoneId {
    fidl_fuchsia_intl::TimeZoneId { id: src.to_string() }
}

fn str_to_locale(src: &str) -> fidl_fuchsia_intl::LocaleId {
    fidl_fuchsia_intl::LocaleId { id: src.to_string() }
}

fn str_to_device_type(src: &str) -> Result<fidl_fuchsia_settings::DeviceType, &str> {
    let device_type = src.to_lowercase();
    if device_type.contains("microphone") {
        Ok(fidl_fuchsia_settings::DeviceType::Microphone)
    } else if device_type.contains("camera") {
        Ok(fidl_fuchsia_settings::DeviceType::Camera)
    } else {
        Err("Unidentified device type")
    }
}

fn str_to_device_state(src: &str) -> Result<fidl_fuchsia_settings::DeviceState, &str> {
    let bits = src.parse::<u64>().map_err(|_| "Failed to parse device state")?;
    let mut device_state = fidl_fuchsia_settings::DeviceState::EMPTY;
    device_state.toggle_flags = fidl_fuchsia_settings::ToggleStateFlags::from_bits(bits);
    Ok(device_state)
}

fn str_to_low_light_mode(src: &str) -> Result<fidl_fuchsia_settings::LowLightMode, &str> {
    if src.contains("enable") {
        Ok(fidl_fuchsia_settings::LowLightMode::Enable)
    } else if src.contains("disable") {
        Ok(fidl_fuchsia_settings::LowLightMode::Disable)
    } else if src.contains("disableimmediately") {
        Ok(fidl_fuchsia_settings::LowLightMode::DisableImmediately)
    } else {
        Err("Couldn't parse low light mode")
    }
}

fn str_to_theme(src: &str) -> Result<fidl_fuchsia_settings::Theme, &str> {
    match src {
        "default" => Ok(Theme {
            theme_type: Some(fidl_fuchsia_settings::ThemeType::Default),
            ..Theme::EMPTY
        }),
        "dark" => {
            Ok(Theme { theme_type: Some(fidl_fuchsia_settings::ThemeType::Dark), ..Theme::EMPTY })
        }
        "light" => {
            Ok(Theme { theme_type: Some(fidl_fuchsia_settings::ThemeType::Light), ..Theme::EMPTY })
        }
        "auto" => {
            Ok(Theme { theme_type: Some(fidl_fuchsia_settings::ThemeType::Auto), ..Theme::EMPTY })
        }
        _ => Err("Couldn't parse theme."),
    }
}

fn str_to_interfaces(src: &&str) -> ConfigurationInterfaces {
    let mut interfaces = ConfigurationInterfaces::empty();

    for interface in src.split(",") {
        match interface.to_lowercase().as_str() {
            "eth" | "ethernet" => {
                interfaces = interfaces | ConfigurationInterfaces::Ethernet;
            }
            "wireless" | "wifi" => {
                interfaces = interfaces | ConfigurationInterfaces::Wifi;
            }
            _ => {}
        }
    }

    return interfaces;
}

fn str_to_color(src: &str) -> Result<fidl_fuchsia_ui_types::ColorRgba, &str> {
    Ok(match src.to_lowercase().as_str() {
        "red" | "r" => {
            fidl_fuchsia_ui_types::ColorRgba { red: 255.0, green: 0.0, blue: 0.0, alpha: 255.0 }
        }
        "green" | "g" => {
            fidl_fuchsia_ui_types::ColorRgba { red: 0.0, green: 2.055, blue: 0.0, alpha: 255.0 }
        }
        "blue" | "b" => {
            fidl_fuchsia_ui_types::ColorRgba { red: 0.0, green: 0.0, blue: 255.0, alpha: 255.0 }
        }
        _ => return Err("Couldn't parse color"),
    })
}

/// Converts a comma-separated string of RGB values into a fidl_fuchsia_ui_types::ColorRgb.
fn str_to_rgb(src: &str) -> Result<fidl_fuchsia_ui_types::ColorRgb, &str> {
    let mut part_iter =
        src.split(',').map(|p| p.parse::<f32>().map_err(|_| "failed to parse color value"));

    const WRONG_COUNT: &str = "wrong number of values";
    let color = fidl_fuchsia_ui_types::ColorRgb {
        red: part_iter.next().unwrap_or_else(|| Err(WRONG_COUNT))?,
        green: part_iter.next().unwrap_or_else(|| Err(WRONG_COUNT))?,
        blue: part_iter.next().unwrap_or_else(|| Err(WRONG_COUNT))?,
    };
    part_iter.next().map(|_| Err(WRONG_COUNT)).unwrap_or(Ok(color))
}

fn str_to_font_family(src: &str) -> Result<fidl_fuchsia_settings::CaptionFontFamily, &str> {
    Ok(match src.to_lowercase().as_str() {
        "unknown" => fidl_fuchsia_settings::CaptionFontFamily::Unknown,
        "monospaced_serif" => fidl_fuchsia_settings::CaptionFontFamily::MonospacedSerif,
        "proportional_serif" => fidl_fuchsia_settings::CaptionFontFamily::ProportionalSerif,
        "monospaced_sans_serif" => fidl_fuchsia_settings::CaptionFontFamily::MonospacedSansSerif,
        "proportional_sans_serif" => {
            fidl_fuchsia_settings::CaptionFontFamily::ProportionalSansSerif
        }
        "casual" => fidl_fuchsia_settings::CaptionFontFamily::Casual,
        "cursive" => fidl_fuchsia_settings::CaptionFontFamily::Cursive,
        "small_capitals" => fidl_fuchsia_settings::CaptionFontFamily::SmallCapitals,
        _ => return Err("Couldn't parse font family"),
    })
}

fn str_to_edge_style(src: &str) -> Result<fidl_fuchsia_settings::EdgeStyle, &str> {
    Ok(match src.to_lowercase().as_str() {
        "none" => fidl_fuchsia_settings::EdgeStyle::None,
        "drop_shadow" => fidl_fuchsia_settings::EdgeStyle::DropShadow,
        "raised" => fidl_fuchsia_settings::EdgeStyle::Raised,
        "depressed" => fidl_fuchsia_settings::EdgeStyle::Depressed,
        "outline" => fidl_fuchsia_settings::EdgeStyle::Outline,
        _ => return Err("Couldn't parse edge style"),
    })
}

fn str_to_temperature_unit(src: &str) -> Result<fidl_fuchsia_intl::TemperatureUnit, &str> {
    match src.to_lowercase().as_str() {
        "c" | "celsius" => Ok(fidl_fuchsia_intl::TemperatureUnit::Celsius),
        "f" | "fahrenheit" => Ok(fidl_fuchsia_intl::TemperatureUnit::Fahrenheit),
        _ => Err("Couldn't parse temperature"),
    }
}

fn str_to_hour_cycle(src: &str) -> Result<fidl_fuchsia_settings::HourCycle, &str> {
    match src.to_lowercase().as_str() {
        "unknown" => Ok(fidl_fuchsia_settings::HourCycle::Unknown),
        "h11" => Ok(fidl_fuchsia_settings::HourCycle::H11),
        "h12" => Ok(fidl_fuchsia_settings::HourCycle::H12),
        "h23" => Ok(fidl_fuchsia_settings::HourCycle::H23),
        "h24" => Ok(fidl_fuchsia_settings::HourCycle::H24),
        _ => Err("Couldn't parse hour cycle"),
    }
}

fn str_to_color_blindness_type(
    src: &str,
) -> Result<fidl_fuchsia_settings::ColorBlindnessType, &str> {
    match src.to_lowercase().as_str() {
        "none" | "n" => Ok(fidl_fuchsia_settings::ColorBlindnessType::None),
        "protanomaly" | "p" => Ok(fidl_fuchsia_settings::ColorBlindnessType::Protanomaly),
        "deuteranomaly" | "d" => Ok(fidl_fuchsia_settings::ColorBlindnessType::Deuteranomaly),
        "tritanomaly" | "t" => Ok(fidl_fuchsia_settings::ColorBlindnessType::Tritanomaly),
        _ => Err("Couldn't parse color blindness type"),
    }
}

fn str_to_audio_stream(src: &str) -> Result<fidl_fuchsia_media::AudioRenderUsage, &str> {
    match src.to_lowercase().as_str() {
        "background" | "b" => Ok(fidl_fuchsia_media::AudioRenderUsage::Background),
        "media" | "m" => Ok(fidl_fuchsia_media::AudioRenderUsage::Media),
        "interruption" | "i" => Ok(fidl_fuchsia_media::AudioRenderUsage::Interruption),
        "system_agent" | "systemagent" | "system agent" | "s" => {
            Ok(fidl_fuchsia_media::AudioRenderUsage::SystemAgent)
        }
        "communication" | "c" => Ok(fidl_fuchsia_media::AudioRenderUsage::Communication),
        _ => Err("Couldn't parse audio stream type"),
    }
}

fn str_to_audio_source(src: &str) -> Result<fidl_fuchsia_settings::AudioStreamSettingSource, &str> {
    match src.to_lowercase().as_str() {
        "user" | "u" => Ok(fidl_fuchsia_settings::AudioStreamSettingSource::User),
        "system" | "s" => Ok(fidl_fuchsia_settings::AudioStreamSettingSource::System),
        _ => Err("Couldn't parse audio source type"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit test for str_to_audio_stream.
    #[test]
    fn test_str_to_audio_stream() {
        println!("Running test_str_to_audio_stream");
        let test_cases = vec![
            "Background",
            "MEDIA",
            "interruption",
            "SYSTEM_AGENT",
            "SystemAgent",
            "system agent",
            "Communication",
            "unexpected_stream_type",
        ];
        let expected = vec![
            Ok(fidl_fuchsia_media::AudioRenderUsage::Background),
            Ok(fidl_fuchsia_media::AudioRenderUsage::Media),
            Ok(fidl_fuchsia_media::AudioRenderUsage::Interruption),
            Ok(fidl_fuchsia_media::AudioRenderUsage::SystemAgent),
            Ok(fidl_fuchsia_media::AudioRenderUsage::SystemAgent),
            Ok(fidl_fuchsia_media::AudioRenderUsage::SystemAgent),
            Ok(fidl_fuchsia_media::AudioRenderUsage::Communication),
            Err("Couldn't parse audio stream type"),
        ];
        let mut results = vec![];
        for test_case in test_cases {
            results.push(str_to_audio_stream(test_case));
        }
        for (expected, result) in expected.iter().zip(results.iter()) {
            assert_eq!(expected, result);
        }
    }

    /// Unit test for str_to_audio_source.
    #[test]
    fn test_str_to_audio_source() {
        println!("Running test_str_to_audio_source");
        let test_cases = vec!["USER", "system", "unexpected_source_type"];
        let expected = vec![
            Ok(fidl_fuchsia_settings::AudioStreamSettingSource::User),
            Ok(fidl_fuchsia_settings::AudioStreamSettingSource::System),
            Err("Couldn't parse audio source type"),
        ];
        let mut results = vec![];
        for test_case in test_cases {
            results.push(str_to_audio_source(test_case));
        }
        for (expected, result) in expected.iter().zip(results.iter()) {
            assert_eq!(expected, result);
        }
    }
}
