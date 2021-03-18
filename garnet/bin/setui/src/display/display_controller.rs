// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::{Deserialize, Serialize};

use crate::base::{Merge, SettingInfo, SettingType};
use crate::call;
use crate::config::default_settings::DefaultSetting;
use crate::display::display_configuration::{
    ConfigurationThemeMode, ConfigurationThemeType, DisplayConfiguration,
};
use crate::display::types::{DisplayInfo, LowLightMode, Theme, ThemeBuilder, ThemeMode, ThemeType};
use crate::handler::base::Request;
use crate::handler::device_storage::{DeviceStorageAccess, DeviceStorageCompatible};
use crate::handler::setting_handler::persist::{controller as data_controller, write, ClientProxy};
use crate::handler::setting_handler::{
    controller, ControllerError, IntoHandlerResult, SettingHandlerResult,
};
use crate::service_context::ExternalServiceProxy;
use async_trait::async_trait;
use fidl_fuchsia_ui_brightness::{
    ControlMarker as BrightnessControlMarker, ControlProxy as BrightnessControlProxy,
};
use lazy_static::lazy_static;
use std::sync::Mutex;

lazy_static! {
    /// Default display used if no configuration is available.
    pub static ref DEFAULT_DISPLAY_INFO: DisplayInfo = DisplayInfo::new(
        false,                 /*auto_brightness_enabled*/
        0.5,                   /*brightness_value*/
        true,                  /*screen_enabled*/
        LowLightMode::Disable, /*low_light_mode*/
        None,                  /*theme*/
    );
}

lazy_static! {
    /// Reference to a display configuation.
    pub static ref DISPLAY_CONFIGURATION: Mutex<DefaultSetting<DisplayConfiguration, &'static str>> =
        Mutex::new(DefaultSetting::new(None, "/config/data/display_configuration.json",));
}

/// Returns a default display [`DisplayInfo`] that is derived from
/// [`DEFAULT_DISPLAY_INFO`] with any fields specified in the
/// display configuration set.
pub fn default_display_info() -> DisplayInfo {
    let mut default_display_info = DEFAULT_DISPLAY_INFO.clone();

    if let Some(display_configuration) = DISPLAY_CONFIGURATION.lock().unwrap().get_default_value() {
        default_display_info.theme = Some(Theme {
            theme_type: Some(match display_configuration.theme.theme_type {
                ConfigurationThemeType::Light => ThemeType::Light,
            }),
            theme_mode: if display_configuration
                .theme
                .theme_mode
                .contains(&ConfigurationThemeMode::Auto)
            {
                ThemeMode::AUTO
            } else {
                ThemeMode::empty()
            },
        });
    }

    default_display_info
}

impl DeviceStorageCompatible for DisplayInfo {
    const KEY: &'static str = "display_info";

    fn default_value() -> Self {
        default_display_info()
    }

    fn deserialize_from(value: &String) -> Self {
        Self::extract(&value)
            .unwrap_or_else(|_| Self::from(DisplayInfoV4::deserialize_from(&value)))
    }
}

impl Into<SettingInfo> for DisplayInfo {
    fn into(self) -> SettingInfo {
        SettingInfo::Brightness(self)
    }
}

impl From<DisplayInfoV4> for DisplayInfo {
    fn from(v4: DisplayInfoV4) -> Self {
        DisplayInfo {
            auto_brightness: v4.auto_brightness,
            manual_brightness_value: v4.manual_brightness_value,
            screen_enabled: v4.screen_enabled,
            low_light_mode: v4.low_light_mode,
            theme: Some(Theme::new(
                Some(v4.theme_type),
                if v4.theme_type == ThemeType::Auto { ThemeMode::AUTO } else { ThemeMode::empty() },
            )),
        }
    }
}

#[async_trait]
pub trait BrightnessManager: Sized {
    async fn from_client(client: &ClientProxy) -> Result<Self, ControllerError>;
    async fn update_brightness(
        &self,
        info: DisplayInfo,
        client: &ClientProxy,
    ) -> SettingHandlerResult;
}

#[async_trait]
impl BrightnessManager for () {
    async fn from_client(_: &ClientProxy) -> Result<Self, ControllerError> {
        Ok(())
    }

    // This does not send the brightness value on anywhere, it simply stores it.
    // External services will pick up the value and set it on the brightness manager.
    async fn update_brightness(
        &self,
        info: DisplayInfo,
        client: &ClientProxy,
    ) -> SettingHandlerResult {
        write(&client, info, false).await.into_handler_result()
    }
}

pub struct ExternalBrightnessControl {
    brightness_service: ExternalServiceProxy<BrightnessControlProxy>,
}

#[async_trait]
impl BrightnessManager for ExternalBrightnessControl {
    async fn from_client(client: &ClientProxy) -> Result<Self, ControllerError> {
        client
            .get_service_context()
            .await
            .lock()
            .await
            .connect::<BrightnessControlMarker>()
            .await
            .map(|brightness_service| Self { brightness_service })
            .map_err(|_| {
                ControllerError::InitFailure("could not connect to brightness service".into())
            })
    }

    async fn update_brightness(
        &self,
        info: DisplayInfo,
        client: &ClientProxy,
    ) -> SettingHandlerResult {
        write(&client, info, false).await?;

        if info.auto_brightness {
            self.brightness_service.call(BrightnessControlProxy::set_auto_brightness)
        } else {
            call!(self.brightness_service => set_manual_brightness(info.manual_brightness_value))
        }
        .map(|_| None)
        .map_err(|_| {
            ControllerError::ExternalFailure(
                SettingType::Display,
                "brightness_service".into(),
                "set_brightness".into(),
            )
        })
    }
}

pub struct DisplayController<T = ()>
where
    T: BrightnessManager,
{
    client: ClientProxy,
    brightness_manager: T,
}

impl<T> DeviceStorageAccess for DisplayController<T>
where
    T: BrightnessManager,
{
    const STORAGE_KEYS: &'static [&'static str] = &[DisplayInfo::KEY];
}

#[async_trait]
impl<T> data_controller::Create for DisplayController<T>
where
    T: BrightnessManager,
{
    /// Creates the controller
    async fn create(client: ClientProxy) -> Result<Self, ControllerError> {
        let brightness_manager = <T as BrightnessManager>::from_client(&client).await?;
        Ok(Self { client, brightness_manager })
    }
}

#[async_trait]
impl<T> controller::Handle for DisplayController<T>
where
    T: BrightnessManager + Send + Sync,
{
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::Restore => {
                // Load and set value.
                Some(
                    self.brightness_manager
                        .update_brightness(self.client.read().await, &self.client)
                        .await,
                )
            }
            Request::SetDisplayInfo(mut set_display_info) => {
                let display_info = self.client.read().await;
                if let Some(manual_brightness_value) = set_display_info.manual_brightness_value {
                    if let (auto_brightness @ Some(true), screen_enabled)
                    | (auto_brightness, screen_enabled @ Some(false)) =
                        (set_display_info.auto_brightness, set_display_info.screen_enabled)
                    {
                        // Invalid argument combination
                        return Some(Err(ControllerError::IncompatibleArguments {
                            setting_type: SettingType::Display,
                            main_arg: "manual_brightness_value".into(),
                            other_args: "auto_brightness, screen_enabled".into(),
                            values: format!(
                                "{}, {:?}, {:?}",
                                manual_brightness_value, auto_brightness, screen_enabled
                            )
                            .into(),
                            reason:
                                "When manual brightness is set, auto brightness must be off or \
                             unset and screen must be enabled or unset"
                                    .into(),
                        }));
                    }
                    set_display_info.auto_brightness = Some(false);
                    set_display_info.screen_enabled = Some(true);
                } else if let Some(screen_enabled) = set_display_info.screen_enabled {
                    // Set auto brightness to the opposite of the screen off state. If the screen is
                    // turned off, auto brightness must be on so that the screen off component can
                    // detect the changes. If the screen is turned on, the default behavior is to
                    // turn it to full manual brightness.
                    if let Some(auto_brightness) = set_display_info.auto_brightness {
                        if screen_enabled == auto_brightness {
                            // Invalid argument combination
                            return Some(Err(ControllerError::IncompatibleArguments {
                                setting_type: SettingType::Display,
                                main_arg: "screen_enabled".into(),
                                other_args: "auto_brightness".into(),
                                values: format!("{}, {}", screen_enabled, auto_brightness).into(),
                                reason: "values cannot be equal".into(),
                            }));
                        }
                    } else {
                        set_display_info.auto_brightness = Some(!screen_enabled);
                    }
                }

                if let Some(theme) = set_display_info.theme {
                    set_display_info.theme = self.build_theme(theme, &display_info);
                }

                Some(
                    self.brightness_manager
                        .update_brightness(display_info.merge(set_display_info), &self.client)
                        .await,
                )
            }
            Request::Get => Some(Ok(Some(SettingInfo::Brightness(self.client.read().await)))),
            _ => None,
        }
    }
}

impl<T> DisplayController<T>
where
    T: BrightnessManager,
{
    fn build_theme(&self, incoming_theme: Theme, display_info: &DisplayInfo) -> Option<Theme> {
        let mut theme_builder = ThemeBuilder::new();

        let existing_theme_type = display_info.theme.map_or(None, |theme| theme.theme_type);

        let new_theme_type = incoming_theme.theme_type.or(existing_theme_type);

        // Temporarily, if no theme type has ever been set, and the
        // theme mode is Auto, we also set the theme type to Auto
        // to support clients that haven't migrated.
        // TODO(fxb/64775): Remove this assignment.
        let mode_adjusted_new_theme_type = match new_theme_type {
            None | Some(ThemeType::Unknown)
                if incoming_theme.theme_mode.contains(ThemeMode::AUTO) =>
            {
                Some(ThemeType::Auto)
            }
            _ => new_theme_type,
        };

        theme_builder.set_theme_type(mode_adjusted_new_theme_type);

        theme_builder.set_theme_mode(
            incoming_theme.theme_mode
            // Temporarily, if the theme type is auto we also set the
            // theme mode to auto until all clients are sending setUI
            // theme mode Auto.
            // TODO(fxb/64775): Remove this or clause.
            | match incoming_theme.theme_type {
                Some(ThemeType::Auto) => ThemeMode::AUTO,
                _ => ThemeMode::empty(),
            },
        );

        theme_builder.build()
    }
}

/// The following struct should never be modified. It represents an old
/// version of the display settings.
#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoV1 {
    /// The last brightness value that was manually set.
    pub manual_brightness_value: f32,
    pub auto_brightness: bool,
    pub low_light_mode: LowLightMode,
}

impl DisplayInfoV1 {
    pub const fn new(
        auto_brightness: bool,
        manual_brightness_value: f32,
        low_light_mode: LowLightMode,
    ) -> DisplayInfoV1 {
        DisplayInfoV1 { manual_brightness_value, auto_brightness, low_light_mode }
    }
}

impl DeviceStorageCompatible for DisplayInfoV1 {
    const KEY: &'static str = "display_infoV1";

    fn default_value() -> Self {
        DisplayInfoV1::new(
            false,                 /*auto_brightness_enabled*/
            0.5,                   /*brightness_value*/
            LowLightMode::Disable, /*low_light_mode*/
        )
    }
}

/// The following struct should never be modified.  It represents an old
/// version of the display settings.
#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoV2 {
    pub manual_brightness_value: f32,
    pub auto_brightness: bool,
    pub low_light_mode: LowLightMode,
    pub theme_mode: ThemeModeV1,
}

impl DisplayInfoV2 {
    pub const fn new(
        auto_brightness: bool,
        manual_brightness_value: f32,
        low_light_mode: LowLightMode,
        theme_mode: ThemeModeV1,
    ) -> DisplayInfoV2 {
        DisplayInfoV2 { manual_brightness_value, auto_brightness, low_light_mode, theme_mode }
    }
}

impl DeviceStorageCompatible for DisplayInfoV2 {
    const KEY: &'static str = "display_infoV2";

    fn default_value() -> Self {
        DisplayInfoV2::new(
            false,                 /*auto_brightness_enabled*/
            0.5,                   /*brightness_value*/
            LowLightMode::Disable, /*low_light_mode*/
            ThemeModeV1::Unknown,  /*theme_mode*/
        )
    }

    fn deserialize_from(value: &String) -> Self {
        Self::extract(&value)
            .unwrap_or_else(|_| Self::from(DisplayInfoV1::deserialize_from(&value)))
    }
}

impl From<DisplayInfoV1> for DisplayInfoV2 {
    fn from(v1: DisplayInfoV1) -> Self {
        DisplayInfoV2 {
            auto_brightness: v1.auto_brightness,
            manual_brightness_value: v1.manual_brightness_value,
            low_light_mode: v1.low_light_mode,
            theme_mode: ThemeModeV1::Unknown,
        }
    }
}

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ThemeModeV1 {
    Unknown,
    Default,
    Light,
    Dark,
    /// Product can choose a theme based on ambient cues.
    Auto,
}

impl From<ThemeModeV1> for ThemeType {
    fn from(theme_mode_v1: ThemeModeV1) -> Self {
        match theme_mode_v1 {
            ThemeModeV1::Unknown => ThemeType::Unknown,
            ThemeModeV1::Default => ThemeType::Default,
            ThemeModeV1::Light => ThemeType::Light,
            ThemeModeV1::Dark => ThemeType::Dark,
            ThemeModeV1::Auto => ThemeType::Auto,
        }
    }
}

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoV3 {
    /// The last brightness value that was manually set.
    pub manual_brightness_value: f32,
    pub auto_brightness: bool,
    pub screen_enabled: bool,
    pub low_light_mode: LowLightMode,
    pub theme_mode: ThemeModeV1,
}

impl DisplayInfoV3 {
    pub const fn new(
        auto_brightness: bool,
        manual_brightness_value: f32,
        screen_enabled: bool,
        low_light_mode: LowLightMode,
        theme_mode: ThemeModeV1,
    ) -> DisplayInfoV3 {
        DisplayInfoV3 {
            manual_brightness_value,
            auto_brightness,
            screen_enabled,
            low_light_mode,
            theme_mode,
        }
    }
}

impl DeviceStorageCompatible for DisplayInfoV3 {
    const KEY: &'static str = "display_info";

    fn default_value() -> Self {
        DisplayInfoV3::new(
            false,                 /*auto_brightness_enabled*/
            0.5,                   /*brightness_value*/
            true,                  /*screen_enabled*/
            LowLightMode::Disable, /*low_light_mode*/
            ThemeModeV1::Unknown,  /*theme_mode*/
        )
    }

    fn deserialize_from(value: &String) -> Self {
        Self::extract(&value)
            .unwrap_or_else(|_| Self::from(DisplayInfoV2::deserialize_from(&value)))
    }
}

impl From<DisplayInfoV2> for DisplayInfoV3 {
    fn from(v2: DisplayInfoV2) -> Self {
        DisplayInfoV3 {
            auto_brightness: v2.auto_brightness,
            manual_brightness_value: v2.manual_brightness_value,
            screen_enabled: true,
            low_light_mode: v2.low_light_mode,
            theme_mode: v2.theme_mode,
        }
    }
}

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoV4 {
    /// The last brightness value that was manually set.
    pub manual_brightness_value: f32,
    pub auto_brightness: bool,
    pub screen_enabled: bool,
    pub low_light_mode: LowLightMode,
    pub theme_type: ThemeType,
}

impl DisplayInfoV4 {
    pub const fn new(
        auto_brightness: bool,
        manual_brightness_value: f32,
        screen_enabled: bool,
        low_light_mode: LowLightMode,
        theme_type: ThemeType,
    ) -> DisplayInfoV4 {
        DisplayInfoV4 {
            manual_brightness_value,
            auto_brightness,
            screen_enabled,
            low_light_mode,
            theme_type,
        }
    }
}

impl From<DisplayInfoV3> for DisplayInfoV4 {
    fn from(v3: DisplayInfoV3) -> Self {
        DisplayInfoV4 {
            auto_brightness: v3.auto_brightness,
            manual_brightness_value: v3.manual_brightness_value,
            screen_enabled: v3.screen_enabled,
            low_light_mode: v3.low_light_mode,
            // In v4, the field formally known as theme_mode was renamed to
            // theme_type.
            theme_type: ThemeType::from(v3.theme_mode),
        }
    }
}

impl DeviceStorageCompatible for DisplayInfoV4 {
    const KEY: &'static str = "display_info";

    fn default_value() -> Self {
        DisplayInfoV4::new(
            false,                 /*auto_brightness_enabled*/
            0.5,                   /*brightness_value*/
            true,                  /*screen_enabled*/
            LowLightMode::Disable, /*low_light_mode*/
            ThemeType::Unknown,    /*theme_type*/
        )
    }

    fn deserialize_from(value: &String) -> Self {
        Self::extract(&value)
            .unwrap_or_else(|_| Self::from(DisplayInfoV3::deserialize_from(&value)))
    }
}

#[test]
fn test_display_migration_v1_to_v2() {
    const BRIGHTNESS_VALUE: f32 = 0.6;
    let mut v1 = DisplayInfoV1::default_value();
    v1.manual_brightness_value = BRIGHTNESS_VALUE;

    let serialized_v1 = v1.serialize_to();

    let v2 = DisplayInfoV2::deserialize_from(&serialized_v1);

    assert_eq!(v2.manual_brightness_value, BRIGHTNESS_VALUE);
    assert_eq!(v2.theme_mode, ThemeModeV1::Unknown);
}

#[test]
fn test_display_migration_v2_to_current() {
    const BRIGHTNESS_VALUE: f32 = 0.6;
    let mut v2 = DisplayInfoV2::default_value();
    v2.manual_brightness_value = BRIGHTNESS_VALUE;

    let serialized_v2 = v2.serialize_to();

    let current = DisplayInfo::deserialize_from(&serialized_v2);

    assert_eq!(current.manual_brightness_value, BRIGHTNESS_VALUE);
    assert_eq!(current.screen_enabled, true);
}

#[test]
fn test_display_migration_v1_to_current() {
    const BRIGHTNESS_VALUE: f32 = 0.6;
    let mut v1 = DisplayInfoV1::default_value();
    v1.manual_brightness_value = BRIGHTNESS_VALUE;

    let serialized_v1 = v1.serialize_to();

    let current = DisplayInfo::deserialize_from(&serialized_v1);

    assert_eq!(current.manual_brightness_value, BRIGHTNESS_VALUE);
    assert_eq!(current.theme.expect("theme not present").theme_type, Some(ThemeType::Unknown));
    assert_eq!(current.screen_enabled, true);
}

#[test]
fn test_display_migration_v3_to_current() {
    let mut v3 = DisplayInfoV3::default_value();
    // In v4 ThemeMode type was renamed to ThemeType, but the field in v3 is
    // still mode.
    v3.theme_mode = ThemeModeV1::Light;
    v3.screen_enabled = false;

    let serialized_v3 = v3.serialize_to();

    let current = DisplayInfo::deserialize_from(&serialized_v3);

    // In v4, the field formally known as theme_mode is theme_type.
    assert_eq!(current.theme.expect("theme not present").theme_type, Some(ThemeType::Light));
    assert_eq!(current.screen_enabled, false);
}

#[test]
fn test_display_migration_v4_to_current() {
    const THEME_TYPE: ThemeType = ThemeType::Auto;
    let mut v4 = DisplayInfoV4::default_value();
    v4.theme_type = THEME_TYPE;

    let serialized_v4 = v4.serialize_to();

    let current = DisplayInfo::deserialize_from(&serialized_v4);

    assert_eq!(current.theme.expect("theme not present").theme_type, Some(THEME_TYPE));
    assert_eq!(
        current.theme.expect("theme not present").theme_mode & ThemeMode::AUTO,
        ThemeMode::AUTO
    );
}
