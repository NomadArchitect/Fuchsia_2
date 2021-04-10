// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input;
use fidl_fuchsia_ui_input2::Key;

/// Standard [USB HID] usages.
///
/// [USB HID]: https://www.usb.org/sites/default/files/documents/hut1_12v2.pdf
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Usages {
    HidUsageKeyErrorRollover = 0x01,
    HidUsageKeyPostFail = 0x02,
    HidUsageKeyErrorUndef = 0x03,
    HidUsageKeyA = 0x04,
    HidUsageKeyB = 0x05,
    HidUsageKeyC = 0x06,
    HidUsageKeyD = 0x07,
    HidUsageKeyE = 0x08,
    HidUsageKeyF = 0x09,
    HidUsageKeyG = 0x0A,
    HidUsageKeyH = 0x0B,
    HidUsageKeyI = 0x0C,
    HidUsageKeyJ = 0x0D,
    HidUsageKeyK = 0x0E,
    HidUsageKeyL = 0x0F,
    HidUsageKeyM = 0x10,
    HidUsageKeyN = 0x11,
    HidUsageKeyO = 0x12,
    HidUsageKeyP = 0x13,
    HidUsageKeyQ = 0x14,
    HidUsageKeyR = 0x15,
    HidUsageKeyS = 0x16,
    HidUsageKeyT = 0x17,
    HidUsageKeyU = 0x18,
    HidUsageKeyV = 0x19,
    HidUsageKeyW = 0x1A,
    HidUsageKeyX = 0x1B,
    HidUsageKeyY = 0x1C,
    HidUsageKeyZ = 0x1D,
    HidUsageKey1 = 0x1E,
    HidUsageKey2 = 0x1F,
    HidUsageKey3 = 0x20,
    HidUsageKey4 = 0x21,
    HidUsageKey5 = 0x22,
    HidUsageKey6 = 0x23,
    HidUsageKey7 = 0x24,
    HidUsageKey8 = 0x25,
    HidUsageKey9 = 0x26,
    HidUsageKey0 = 0x27,
    HidUsageKeyEnter = 0x28,
    HidUsageKeyEsc = 0x29,
    HidUsageKeyBackspace = 0x2A,
    HidUsageKeyTab = 0x2B,
    HidUsageKeySpace = 0x2C,
    HidUsageKeyMinus = 0x2D,
    HidUsageKeyEqual = 0x2E,
    HidUsageKeyLeftbrace = 0x2F,
    HidUsageKeyRightbrace = 0x30,
    HidUsageKeyBackslash = 0x31,
    HidUsageKeyNonUsOctothorpe = 0x32,
    HidUsageKeySemicolon = 0x33,
    HidUsageKeyApostrophe = 0x34,
    HidUsageKeyGrave = 0x35,
    HidUsageKeyComma = 0x36,
    HidUsageKeyDot = 0x37,
    HidUsageKeySlash = 0x38,
    HidUsageKeyCapslock = 0x39,
    HidUsageKeyF1 = 0x3A,
    HidUsageKeyF2 = 0x3B,
    HidUsageKeyF3 = 0x3C,
    HidUsageKeyF4 = 0x3D,
    HidUsageKeyF5 = 0x3E,
    HidUsageKeyF6 = 0x3F,
    HidUsageKeyF7 = 0x40,
    HidUsageKeyF8 = 0x41,
    HidUsageKeyF9 = 0x42,
    HidUsageKeyF10 = 0x43,
    HidUsageKeyF11 = 0x44,
    HidUsageKeyF12 = 0x45,
    HidUsageKeyPrintscreen = 0x46,
    HidUsageKeyScrolllock = 0x47,
    HidUsageKeyPause = 0x48,
    HidUsageKeyInsert = 0x49,
    HidUsageKeyHome = 0x4A,
    HidUsageKeyPageup = 0x4B,
    HidUsageKeyDelete = 0x4C,
    HidUsageKeyEnd = 0x4D,
    HidUsageKeyPagedown = 0x4E,
    HidUsageKeyRight = 0x4F,
    HidUsageKeyLeft = 0x50,
    HidUsageKeyDown = 0x51,
    HidUsageKeyUp = 0x52,
    HidUsageKeyNumlock = 0x53,
    HidUsageKeyKpSlash = 0x54,
    HidUsageKeyKpAsterisk = 0x55,
    HidUsageKeyKpMinus = 0x56,
    HidUsageKeyKpPlus = 0x57,
    HidUsageKeyKpEnter = 0x58,
    HidUsageKeyKp1 = 0x59,
    HidUsageKeyKp2 = 0x5A,
    HidUsageKeyKp3 = 0x5B,
    HidUsageKeyKp4 = 0x5C,
    HidUsageKeyKp5 = 0x5D,
    HidUsageKeyKp6 = 0x5E,
    HidUsageKeyKp7 = 0x5F,
    HidUsageKeyKp8 = 0x60,
    HidUsageKeyKp9 = 0x61,
    HidUsageKeyKp0 = 0x62,
    HidUsageKeyKpDot = 0x63,
    HidUsageKeyNonUsBackslash = 0x64,
    HidUsageKeyLeftCtrl = 0xE0,
    HidUsageKeyLeftShift = 0xE1,
    HidUsageKeyLeftAlt = 0xE2,
    HidUsageKeyLeftGui = 0xE3,
    HidUsageKeyRightCtrl = 0xE4,
    HidUsageKeyRightShift = 0xE5,
    HidUsageKeyRightAlt = 0xE6,
    HidUsageKeyRightGui = 0xE7,
    // TODO: The following two values are incorrect and are not actually USB HID codes, but are
    //       currently the values that appear in hid/usages.h. Eventually we will want to migrate to
    //       fuchsia.ui.input.Key.
    HidUsageKeyVolUp = 0xE8,
    HidUsageKeyVolDown = 0xE9,
}

/// Converts a [`Key`] to the corresponding USB HID code.
///
/// Note: This is only needed while keyboard input is transitioned away from Scenic.
///
/// # Parameters
/// - `key`: The key to convert to its HID equivalent.
pub fn key_to_hid_usage(key: Key) -> u32 {
    match key {
        Key::A => 0x4,
        Key::B => 0x5,
        Key::C => 0x6,
        Key::D => 0x7,
        Key::E => 0x8,
        Key::F => 0x9,
        Key::G => 0xa,
        Key::H => 0xb,
        Key::I => 0xc,
        Key::J => 0xd,
        Key::K => 0xe,
        Key::L => 0xf,
        Key::M => 0x10,
        Key::N => 0x11,
        Key::O => 0x12,
        Key::P => 0x13,
        Key::Q => 0x14,
        Key::R => 0x15,
        Key::S => 0x16,
        Key::T => 0x17,
        Key::U => 0x18,
        Key::V => 0x19,
        Key::W => 0x1a,
        Key::X => 0x1b,
        Key::Y => 0x1c,
        Key::Z => 0x1d,
        Key::Key1 => 0x1e,
        Key::Key2 => 0x1f,
        Key::Key3 => 0x20,
        Key::Key4 => 0x21,
        Key::Key5 => 0x22,
        Key::Key6 => 0x23,
        Key::Key7 => 0x24,
        Key::Key8 => 0x25,
        Key::Key9 => 0x26,
        Key::Key0 => 0x27,
        Key::Enter => 0x28,
        Key::Escape => 0x29,
        Key::Backspace => 0x2a,
        Key::Tab => 0x2b,
        Key::Space => 0x2c,
        Key::Minus => 0x2d,
        Key::Equals => 0x2e,
        Key::LeftBrace => 0x2f,
        Key::RightBrace => 0x30,
        Key::Backslash => 0x31,
        Key::NonUsHash => 0x32,
        Key::Semicolon => 0x33,
        Key::Apostrophe => 0x34,
        Key::GraveAccent => 0x35,
        Key::Comma => 0x36,
        Key::Dot => 0x37,
        Key::Slash => 0x38,
        Key::CapsLock => 0x39,
        Key::F1 => 0x3a,
        Key::F2 => 0x3b,
        Key::F3 => 0x3c,
        Key::F4 => 0x3d,
        Key::F5 => 0x3e,
        Key::F6 => 0x3f,
        Key::F7 => 0x40,
        Key::F8 => 0x41,
        Key::F9 => 0x42,
        Key::F10 => 0x43,
        Key::F11 => 0x44,
        Key::F12 => 0x45,
        Key::PrintScreen => 0x46,
        Key::ScrollLock => 0x47,
        Key::Pause => 0x48,
        Key::Insert => 0x49,
        Key::Home => 0x4a,
        Key::PageUp => 0x4b,
        Key::Delete => 0x4c,
        Key::End => 0x4d,
        Key::PageDown => 0x4e,
        Key::Right => 0x4f,
        Key::Left => 0x50,
        Key::Down => 0x51,
        Key::Up => 0x52,
        Key::NonUsBackslash => 0x64,
        Key::LeftCtrl => 0xe0,
        Key::LeftShift => 0xe1,
        Key::LeftAlt => 0xe2,
        Key::LeftMeta => 0xe3,
        Key::RightCtrl => 0xe4,
        Key::RightShift => 0xe5,
        Key::RightAlt => 0xe6,
        Key::RightMeta => 0xe7,
        Key::Menu => 0x76,
        Key::NumLock => 0x53,
        Key::KeypadSlash => 0x54,
        Key::KeypadAsterisk => 0x55,
        Key::KeypadMinus => 0x56,
        Key::KeypadPlus => 0x57,
        Key::KeypadEnter => 0x58,
        Key::Keypad1 => 0x59,
        Key::Keypad2 => 0x5a,
        Key::Keypad3 => 0x5b,
        Key::Keypad4 => 0x5c,
        Key::Keypad5 => 0x5d,
        Key::Keypad6 => 0x5e,
        Key::Keypad7 => 0x5f,
        Key::Keypad8 => 0x60,
        Key::Keypad9 => 0x61,
        Key::Keypad0 => 0x62,
        Key::KeypadDot => 0x63,
        Key::KeypadEquals => 0x67,
        Key::MediaMute => 0xe2,
        Key::MediaVolumeIncrement => 0xe9,
        Key::MediaVolumeDecrement => 0xea,
    }
}

/// Converts a [`Key`] to the corresponding USB HID code.
///
/// Note: This is only needed while keyboard input is transitioned away from Scenic.
///
/// # Parameters
/// - `key`: The key to convert to its HID equivalent.
pub fn input3_key_to_hid_usage(key: fidl_fuchsia_input::Key) -> u32 {
    fidl_fuchsia_input::Key::into_primitive(key) & 0xFFFF
}

/// Converts a USB HID Usage ID to the corresponding input3 [`Key`].
///
/// The Usage ID is interpreted in the context of Usage Page 0x07 ("Keyboard/Keypad"),
/// except that:
/// * 0xe8 is interpreted as Usage Page 0x0c, Usage ID 0xe9 (Volume Increment)
/// * 0xe9 is interpreted as Usage Page 0x0c, Usage ID 0xea (Volume Decrement)
///
/// These exceptions provide backwards compatibility with Root Presenter's input
/// pipeline.
///
/// # Parameters
/// - `usage_id`: The Usage ID to convert to its input3 [`Key`] equivalent.
///
/// # Future directions
/// Per fxbug.dev/63974, this method will be replaced with a method that deals in
/// `fuchsia.input.Key`s, instead of HID Usage IDs.
pub(crate) fn hid_usage_to_input3_key(usage_id: u16) -> Option<fidl_fuchsia_input::Key> {
    if usage_id == Usages::HidUsageKeyVolUp as u16 {
        Some(fidl_fuchsia_input::Key::MediaVolumeIncrement)
    } else if usage_id == Usages::HidUsageKeyVolDown as u16 {
        Some(fidl_fuchsia_input::Key::MediaVolumeDecrement)
    } else {
        fidl_fuchsia_input::Key::from_primitive(u32::from(usage_id) | 0x0007_0000)
    }
}

/// Returns true if the `key` is considered to be a modifier key.
///
/// # Parameters
/// - `key`: The key to check.
pub fn is_modifier(key: Key) -> bool {
    match key {
        Key::LeftAlt
        | Key::RightAlt
        | Key::LeftShift
        | Key::RightShift
        | Key::LeftCtrl
        | Key::RightCtrl
        | Key::LeftMeta
        | Key::RightMeta
        | Key::NumLock
        | Key::CapsLock
        | Key::ScrollLock => true,
        _ => false,
    }
}

/// Returns true if the `key` is considered to be a modifier key.
///
/// # Parameters
/// - `key`: The key to check.
pub fn is_modifier3(key: fidl_fuchsia_input::Key) -> bool {
    match key {
        fidl_fuchsia_input::Key::NumLock
        | fidl_fuchsia_input::Key::CapsLock
        | fidl_fuchsia_input::Key::ScrollLock => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, test_case::test_case};

    #[test]
    fn input3_key_to_hid() {
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::A), 0x4);
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::Key1), 0x1e);
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::Enter), 0x28);
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::F1), 0x3a);
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::PrintScreen), 0x46);
        assert_eq!(input3_key_to_hid_usage(fidl_fuchsia_input::Key::Keypad1), 0x59);
    }

    #[test]
    fn input3_key_is_modifier() {
        assert!(is_modifier3(fidl_fuchsia_input::Key::NumLock));
        assert!(is_modifier3(fidl_fuchsia_input::Key::CapsLock));
        assert!(is_modifier3(fidl_fuchsia_input::Key::ScrollLock));
        assert!(!is_modifier3(fidl_fuchsia_input::Key::LeftShift));
        assert!(!is_modifier3(fidl_fuchsia_input::Key::LeftMeta));
        assert!(!is_modifier3(fidl_fuchsia_input::Key::LeftCtrl));
    }

    #[test_case(Usages::HidUsageKeyVolUp => fidl_fuchsia_input::Key::MediaVolumeIncrement; "volume_up")]
    #[test_case(Usages::HidUsageKeyVolDown => fidl_fuchsia_input::Key::MediaVolumeDecrement; "volume_down")]
    #[test_case(Usages::HidUsageKeyA => fidl_fuchsia_input::Key::A; "letter_a")]
    fn hid_usage_to_input3_key(usage: Usages) -> fidl_fuchsia_input::Key {
        super::hid_usage_to_input3_key(usage as u16).expect("conversion yielded None")
    }
}
