//! xkb keysym → `iced_core::keyboard` conversion.
//!
//! The host runs xkbcommon (sctk does this) and passes, per key: the keysym
//! with modifiers applied, the level-0 keysym (no modifiers), the raw xkb
//! keycode (evdev + 8) and the UTF-8 text xkb produced, if any.

use iced_core::SmolStr;
use iced_core::keyboard::key::{Code, NativeCode, Physical};
use iced_core::keyboard::{self, Key, Location, Modifiers, key::Named};
use xkbcommon::xkb::Keysym;
use xkbcommon::xkb::keysyms as k;

/// One xkb key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput<'a> {
    /// Keysym with the active modifiers applied (`xkb_state_key_get_one_sym`).
    pub keysym: Keysym,
    /// Keysym at shift level 0, if the host looked it up; `None` reuses
    /// `keysym`.
    pub unmodified_keysym: Option<Keysym>,
    /// Raw xkb keycode (evdev scancode + 8).
    pub keycode: u32,
    pub modifiers: Modifiers,
    /// `xkb_state_key_get_utf8` output.
    pub text: Option<&'a str>,
    pub repeat: bool,
}

pub fn modifiers(shift: bool, ctrl: bool, alt: bool, logo: bool) -> Modifiers {
    let mut result = Modifiers::empty();
    result.set(Modifiers::SHIFT, shift);
    result.set(Modifiers::CTRL, ctrl);
    result.set(Modifiers::ALT, alt);
    result.set(Modifiers::LOGO, logo);
    result
}

pub fn modifiers_changed(modifiers: Modifiers) -> iced_core::Event {
    iced_core::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers))
}

pub fn key_pressed(input: KeyInput<'_>) -> keyboard::Event {
    let text = input
        .text
        .filter(|text| !text.is_empty() && !text.chars().any(is_private_use))
        .map(SmolStr::new);
    keyboard::Event::KeyPressed {
        key: key(input.unmodified_keysym.unwrap_or(input.keysym)),
        modified_key: key(input.keysym),
        physical_key: physical_key(input.keycode),
        location: location_for(input.keysym, input.keycode),
        modifiers: input.modifiers,
        text,
        repeat: input.repeat,
    }
}

pub fn key_released(input: KeyInput<'_>) -> keyboard::Event {
    keyboard::Event::KeyReleased {
        key: key(input.unmodified_keysym.unwrap_or(input.keysym)),
        modified_key: key(input.keysym),
        physical_key: physical_key(input.keycode),
        location: location_for(input.keysym, input.keycode),
        modifiers: input.modifiers,
    }
}

/// Wraps [`key_pressed`] / [`key_released`] as an `iced_core::Event`.
pub fn key_event(input: KeyInput<'_>, pressed: bool) -> iced_core::Event {
    iced_core::Event::Keyboard(if pressed {
        key_pressed(input)
    } else {
        key_released(input)
    })
}

fn is_private_use(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
}

/// The logical key for a keysym. Dead keys and keysyms without a character
/// are `Unidentified`.
pub fn key(keysym: Keysym) -> Key {
    let raw = keysym.raw();
    if let Some(named) = named(raw) {
        return Key::Named(named);
    }
    if (k::KEY_dead_grave..=k::KEY_dead_longsolidusoverlay).contains(&raw) {
        return Key::Unidentified;
    }
    match keysym.key_char() {
        Some(c) if !c.is_control() => Key::Character(SmolStr::new(c.encode_utf8(&mut [0; 4]))),
        _ => Key::Unidentified,
    }
}

pub fn location(keysym: Keysym) -> Location {
    match keysym.raw() {
        k::KEY_Shift_L
        | k::KEY_Control_L
        | k::KEY_Meta_L
        | k::KEY_Alt_L
        | k::KEY_Super_L
        | k::KEY_Hyper_L => Location::Left,
        k::KEY_Shift_R
        | k::KEY_Control_R
        | k::KEY_Meta_R
        | k::KEY_Alt_R
        | k::KEY_Super_R
        | k::KEY_Hyper_R
        // AltGr is the right Alt key on the layouts that have it.
        | k::KEY_ISO_Level3_Shift => Location::Right,
        raw if (k::KEY_KP_Space..=k::KEY_KP_Equal).contains(&raw) => Location::Numpad,
        _ => Location::Standard,
    }
}

fn named(raw: u32) -> Option<Named> {
    use Named as N;
    Some(match raw {
        k::KEY_BackSpace => N::Backspace,
        k::KEY_Tab | k::KEY_ISO_Left_Tab | k::KEY_KP_Tab => N::Tab,
        k::KEY_Clear | k::KEY_KP_Begin => N::Clear,
        k::KEY_Return | k::KEY_KP_Enter | k::KEY_ISO_Enter => N::Enter,
        k::KEY_Pause | k::KEY_Break => N::Pause,
        k::KEY_Scroll_Lock => N::ScrollLock,
        k::KEY_Sys_Req | k::KEY_Print => N::PrintScreen,
        k::KEY_Escape => N::Escape,
        k::KEY_Delete | k::KEY_KP_Delete => N::Delete,
        k::KEY_space | k::KEY_KP_Space => N::Space,

        k::KEY_Home | k::KEY_KP_Home => N::Home,
        k::KEY_Left | k::KEY_KP_Left => N::ArrowLeft,
        k::KEY_Up | k::KEY_KP_Up => N::ArrowUp,
        k::KEY_Right | k::KEY_KP_Right => N::ArrowRight,
        k::KEY_Down | k::KEY_KP_Down => N::ArrowDown,
        k::KEY_Page_Up | k::KEY_KP_Page_Up => N::PageUp,
        k::KEY_Page_Down | k::KEY_KP_Page_Down => N::PageDown,
        k::KEY_End | k::KEY_KP_End => N::End,
        k::KEY_Insert | k::KEY_KP_Insert => N::Insert,

        k::KEY_Select => N::Select,
        k::KEY_Execute => N::Execute,
        k::KEY_Undo => N::Undo,
        k::KEY_Redo => N::Redo,
        k::KEY_Menu => N::ContextMenu,
        k::KEY_Find => N::Find,
        k::KEY_Cancel => N::Cancel,
        k::KEY_Help => N::Help,
        k::KEY_Mode_switch => N::ModeChange,
        k::KEY_Num_Lock => N::NumLock,

        k::KEY_KP_F1 | k::KEY_F1 => N::F1,
        k::KEY_KP_F2 | k::KEY_F2 => N::F2,
        k::KEY_KP_F3 | k::KEY_F3 => N::F3,
        k::KEY_KP_F4 | k::KEY_F4 => N::F4,
        k::KEY_F5 => N::F5,
        k::KEY_F6 => N::F6,
        k::KEY_F7 => N::F7,
        k::KEY_F8 => N::F8,
        k::KEY_F9 => N::F9,
        k::KEY_F10 => N::F10,
        k::KEY_F11 => N::F11,
        k::KEY_F12 => N::F12,
        k::KEY_F13 => N::F13,
        k::KEY_F14 => N::F14,
        k::KEY_F15 => N::F15,
        k::KEY_F16 => N::F16,
        k::KEY_F17 => N::F17,
        k::KEY_F18 => N::F18,
        k::KEY_F19 => N::F19,
        k::KEY_F20 => N::F20,
        k::KEY_F21 => N::F21,
        k::KEY_F22 => N::F22,
        k::KEY_F23 => N::F23,
        k::KEY_F24 => N::F24,
        k::KEY_F25 => N::F25,
        k::KEY_F26 => N::F26,
        k::KEY_F27 => N::F27,
        k::KEY_F28 => N::F28,
        k::KEY_F29 => N::F29,
        k::KEY_F30 => N::F30,
        k::KEY_F31 => N::F31,
        k::KEY_F32 => N::F32,
        k::KEY_F33 => N::F33,
        k::KEY_F34 => N::F34,
        k::KEY_F35 => N::F35,

        k::KEY_Shift_L | k::KEY_Shift_R => N::Shift,
        k::KEY_Control_L | k::KEY_Control_R => N::Control,
        k::KEY_Caps_Lock | k::KEY_Shift_Lock => N::CapsLock,
        k::KEY_Meta_L | k::KEY_Meta_R => N::Meta,
        k::KEY_Alt_L | k::KEY_Alt_R => N::Alt,
        k::KEY_Super_L | k::KEY_Super_R => N::Super,
        k::KEY_Hyper_L | k::KEY_Hyper_R => N::Hyper,
        k::KEY_ISO_Level3_Shift | k::KEY_ISO_Level5_Shift => N::AltGraph,
        k::KEY_ISO_Next_Group => N::GroupNext,
        k::KEY_ISO_Prev_Group => N::GroupPrevious,
        k::KEY_ISO_First_Group => N::GroupFirst,
        k::KEY_ISO_Last_Group => N::GroupLast,

        k::KEY_Multi_key => N::Compose,
        k::KEY_Codeinput => N::CodeInput,
        k::KEY_SingleCandidate => N::SingleCandidate,
        k::KEY_MultipleCandidate => N::AllCandidates,
        k::KEY_PreviousCandidate => N::PreviousCandidate,
        k::KEY_Kanji => N::KanjiMode,
        k::KEY_Muhenkan => N::NonConvert,
        k::KEY_Henkan_Mode => N::Convert,
        k::KEY_Romaji => N::Romaji,
        k::KEY_Hiragana => N::Hiragana,
        k::KEY_Katakana => N::Katakana,
        k::KEY_Hiragana_Katakana => N::HiraganaKatakana,
        k::KEY_Zenkaku => N::Zenkaku,
        k::KEY_Hankaku => N::Hankaku,
        k::KEY_Zenkaku_Hankaku => N::ZenkakuHankaku,
        k::KEY_Kana_Lock | k::KEY_Kana_Shift => N::KanaMode,
        k::KEY_Eisu_Shift | k::KEY_Eisu_toggle => N::Alphanumeric,
        k::KEY_Hangul => N::HangulMode,
        k::KEY_Hangul_Hanja => N::HanjaMode,

        k::KEY_XF86AudioLowerVolume => N::AudioVolumeDown,
        k::KEY_XF86AudioRaiseVolume => N::AudioVolumeUp,
        k::KEY_XF86AudioMute => N::AudioVolumeMute,
        k::KEY_XF86AudioMicMute => N::MicrophoneVolumeMute,
        k::KEY_XF86AudioPlay => N::MediaPlay,
        k::KEY_XF86AudioPause => N::MediaPause,
        k::KEY_XF86AudioStop => N::MediaStop,
        k::KEY_XF86AudioPrev => N::MediaTrackPrevious,
        k::KEY_XF86AudioNext => N::MediaTrackNext,
        k::KEY_XF86AudioRecord => N::MediaRecord,
        k::KEY_XF86AudioRewind => N::MediaRewind,
        k::KEY_XF86AudioForward => N::MediaFastForward,
        k::KEY_XF86MonBrightnessUp => N::BrightnessUp,
        k::KEY_XF86MonBrightnessDown => N::BrightnessDown,
        k::KEY_XF86Copy => N::Copy,
        k::KEY_XF86Cut => N::Cut,
        k::KEY_XF86Paste => N::Paste,
        k::KEY_XF86Back => N::BrowserBack,
        k::KEY_XF86Forward => N::BrowserForward,
        k::KEY_XF86Refresh => N::BrowserRefresh,
        k::KEY_XF86Search => N::BrowserSearch,
        k::KEY_XF86HomePage => N::BrowserHome,
        k::KEY_XF86Favorites => N::BrowserFavorites,
        k::KEY_XF86Stop => N::BrowserStop,
        k::KEY_XF86Mail => N::LaunchMail,
        k::KEY_XF86Calculator => N::LaunchApplication2,
        k::KEY_XF86MyComputer => N::LaunchApplication1,
        k::KEY_XF86PowerOff => N::Power,
        k::KEY_XF86Sleep => N::Standby,
        k::KEY_XF86WakeUp => N::WakeUp,
        k::KEY_XF86Eject => N::Eject,
        k::KEY_XF86ScreenSaver => N::LaunchScreenSaver,
        k::KEY_XF86Close => N::Close,
        k::KEY_XF86Open => N::Open,
        k::KEY_XF86New => N::New,
        k::KEY_XF86Save => N::Save,
        k::KEY_XF86ZoomIn => N::ZoomIn,
        k::KEY_XF86ZoomOut => N::ZoomOut,
        _ => return None,
    })
}

/// The location from the physical key where it tells sides and the keypad
/// apart, otherwise from the keysym.
pub fn location_for(keysym: Keysym, keycode: u32) -> Location {
    use Code as C;
    match evdev_code(keycode.saturating_sub(8)) {
        Some(C::ShiftLeft | C::ControlLeft | C::AltLeft | C::SuperLeft) => Location::Left,
        Some(C::ShiftRight | C::ControlRight | C::AltRight | C::SuperRight) => Location::Right,
        Some(
            C::Numpad0
            | C::Numpad1
            | C::Numpad2
            | C::Numpad3
            | C::Numpad4
            | C::Numpad5
            | C::Numpad6
            | C::Numpad7
            | C::Numpad8
            | C::Numpad9
            | C::NumpadAdd
            | C::NumpadSubtract
            | C::NumpadMultiply
            | C::NumpadDivide
            | C::NumpadDecimal
            | C::NumpadEnter
            | C::NumpadEqual
            | C::NumpadComma,
        ) => Location::Numpad,
        _ => location(keysym),
    }
}

/// Physical key for a raw xkb keycode (Linux evdev scancode + 8).
pub fn physical_key(keycode: u32) -> Physical {
    let scancode = keycode.saturating_sub(8);
    match evdev_code(scancode) {
        Some(code) => Physical::Code(code),
        None => Physical::Unidentified(NativeCode::Xkb(keycode)),
    }
}

// Scancodes from linux/input-event-codes.h.
fn evdev_code(scancode: u32) -> Option<Code> {
    use Code as C;
    Some(match scancode {
        1 => C::Escape,
        2 => C::Digit1,
        3 => C::Digit2,
        4 => C::Digit3,
        5 => C::Digit4,
        6 => C::Digit5,
        7 => C::Digit6,
        8 => C::Digit7,
        9 => C::Digit8,
        10 => C::Digit9,
        11 => C::Digit0,
        12 => C::Minus,
        13 => C::Equal,
        14 => C::Backspace,
        15 => C::Tab,
        16 => C::KeyQ,
        17 => C::KeyW,
        18 => C::KeyE,
        19 => C::KeyR,
        20 => C::KeyT,
        21 => C::KeyY,
        22 => C::KeyU,
        23 => C::KeyI,
        24 => C::KeyO,
        25 => C::KeyP,
        26 => C::BracketLeft,
        27 => C::BracketRight,
        28 => C::Enter,
        29 => C::ControlLeft,
        30 => C::KeyA,
        31 => C::KeyS,
        32 => C::KeyD,
        33 => C::KeyF,
        34 => C::KeyG,
        35 => C::KeyH,
        36 => C::KeyJ,
        37 => C::KeyK,
        38 => C::KeyL,
        39 => C::Semicolon,
        40 => C::Quote,
        41 => C::Backquote,
        42 => C::ShiftLeft,
        43 => C::Backslash,
        44 => C::KeyZ,
        45 => C::KeyX,
        46 => C::KeyC,
        47 => C::KeyV,
        48 => C::KeyB,
        49 => C::KeyN,
        50 => C::KeyM,
        51 => C::Comma,
        52 => C::Period,
        53 => C::Slash,
        54 => C::ShiftRight,
        55 => C::NumpadMultiply,
        56 => C::AltLeft,
        57 => C::Space,
        58 => C::CapsLock,
        59 => C::F1,
        60 => C::F2,
        61 => C::F3,
        62 => C::F4,
        63 => C::F5,
        64 => C::F6,
        65 => C::F7,
        66 => C::F8,
        67 => C::F9,
        68 => C::F10,
        69 => C::NumLock,
        70 => C::ScrollLock,
        71 => C::Numpad7,
        72 => C::Numpad8,
        73 => C::Numpad9,
        74 => C::NumpadSubtract,
        75 => C::Numpad4,
        76 => C::Numpad5,
        77 => C::Numpad6,
        78 => C::NumpadAdd,
        79 => C::Numpad1,
        80 => C::Numpad2,
        81 => C::Numpad3,
        82 => C::Numpad0,
        83 => C::NumpadDecimal,
        85 => C::Lang5,
        86 => C::IntlBackslash,
        87 => C::F11,
        88 => C::F12,
        89 => C::IntlRo,
        90 => C::Katakana,
        91 => C::Hiragana,
        92 => C::Convert,
        93 => C::KanaMode,
        94 => C::NonConvert,
        95 => C::NumpadComma,
        96 => C::NumpadEnter,
        97 => C::ControlRight,
        98 => C::NumpadDivide,
        99 => C::PrintScreen,
        100 => C::AltRight,
        102 => C::Home,
        103 => C::ArrowUp,
        104 => C::PageUp,
        105 => C::ArrowLeft,
        106 => C::ArrowRight,
        107 => C::End,
        108 => C::ArrowDown,
        109 => C::PageDown,
        110 => C::Insert,
        111 => C::Delete,
        113 => C::AudioVolumeMute,
        114 => C::AudioVolumeDown,
        115 => C::AudioVolumeUp,
        116 => C::Power,
        117 => C::NumpadEqual,
        119 => C::Pause,
        121 => C::NumpadComma,
        122 => C::Lang1,
        123 => C::Lang2,
        124 => C::IntlYen,
        125 => C::SuperLeft,
        126 => C::SuperRight,
        127 => C::ContextMenu,
        128 => C::BrowserStop,
        129 => C::Again,
        130 => C::Props,
        131 => C::Undo,
        133 => C::Copy,
        134 => C::Open,
        135 => C::Paste,
        136 => C::Find,
        137 => C::Cut,
        138 => C::Help,
        142 => C::Sleep,
        143 => C::WakeUp,
        155 => C::LaunchMail,
        156 => C::BrowserFavorites,
        158 => C::BrowserBack,
        159 => C::BrowserForward,
        161 => C::Eject,
        163 => C::MediaTrackNext,
        164 => C::MediaPlayPause,
        165 => C::MediaTrackPrevious,
        166 => C::MediaStop,
        172 => C::BrowserHome,
        173 => C::BrowserRefresh,
        183 => C::F13,
        184 => C::F14,
        185 => C::F15,
        186 => C::F16,
        187 => C::F17,
        188 => C::F18,
        189 => C::F19,
        190 => C::F20,
        191 => C::F21,
        192 => C::F22,
        193 => C::F23,
        194 => C::F24,
        217 => C::BrowserSearch,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // evdev scancodes + 8
    const KC_A: u32 = 30 + 8;
    const KC_1: u32 = 2 + 8;
    const KC_ENTER: u32 = 28 + 8;
    const KC_TAB: u32 = 15 + 8;
    const KC_ESC: u32 = 1 + 8;
    const KC_LEFT: u32 = 105 + 8;
    const KC_F5: u32 = 63 + 8;
    const KC_KP7: u32 = 71 + 8;
    const KC_KPENTER: u32 = 96 + 8;
    const KC_RCTRL: u32 = 97 + 8;
    const KC_APOSTROPHE: u32 = 40 + 8;

    fn input(keysym: u32, keycode: u32, text: Option<&str>) -> KeyInput<'_> {
        KeyInput {
            keysym: Keysym::new(keysym),
            unmodified_keysym: None,
            keycode,
            modifiers: Modifiers::empty(),
            text,
            repeat: false,
        }
    }

    fn pressed(
        event: keyboard::Event,
    ) -> (Key, Key, Physical, Location, Modifiers, Option<SmolStr>) {
        match event {
            keyboard::Event::KeyPressed {
                key,
                modified_key,
                physical_key,
                location,
                modifiers,
                text,
                ..
            } => (key, modified_key, physical_key, location, modifiers, text),
            other => panic!("expected KeyPressed, got {other:?}"),
        }
    }

    fn ch(s: &str) -> Key {
        Key::Character(SmolStr::new(s))
    }

    #[test]
    fn letters_keep_unshifted_key_and_shifted_text() {
        let (key, modified, physical, location, _, text) =
            pressed(key_pressed(input(k::KEY_a, KC_A, Some("a"))));
        assert_eq!(key, ch("a"));
        assert_eq!(modified, ch("a"));
        assert_eq!(physical, Physical::Code(Code::KeyA));
        assert_eq!(location, Location::Standard);
        assert_eq!(text.as_deref(), Some("a"));

        let shifted = KeyInput {
            unmodified_keysym: Some(Keysym::new(k::KEY_a)),
            modifiers: modifiers(true, false, false, false),
            ..input(k::KEY_A, KC_A, Some("A"))
        };
        let (key, modified, physical, _, mods, text) = pressed(key_pressed(shifted));
        assert_eq!(key, ch("a"));
        assert_eq!(modified, ch("A"));
        assert_eq!(physical, Physical::Code(Code::KeyA));
        assert!(mods.shift());
        assert_eq!(text.as_deref(), Some("A"));
    }

    #[test]
    fn digits_and_shifted_digits() {
        let (key, _, physical, _, _, text) = pressed(key_pressed(input(k::KEY_1, KC_1, Some("1"))));
        assert_eq!(key, ch("1"));
        assert_eq!(physical, Physical::Code(Code::Digit1));
        assert_eq!(text.as_deref(), Some("1"));

        let bang = KeyInput {
            unmodified_keysym: Some(Keysym::new(k::KEY_1)),
            ..input(k::KEY_exclam, KC_1, Some("!"))
        };
        let (key, modified, ..) = pressed(key_pressed(bang));
        assert_eq!(key, ch("1"));
        assert_eq!(modified, ch("!"));
    }

    #[test]
    fn named_keys() {
        let cases = [
            (k::KEY_Return, KC_ENTER, Named::Enter, Code::Enter),
            (k::KEY_Tab, KC_TAB, Named::Tab, Code::Tab),
            (k::KEY_ISO_Left_Tab, KC_TAB, Named::Tab, Code::Tab),
            (k::KEY_Escape, KC_ESC, Named::Escape, Code::Escape),
            (k::KEY_Left, KC_LEFT, Named::ArrowLeft, Code::ArrowLeft),
            (k::KEY_Up, 103 + 8, Named::ArrowUp, Code::ArrowUp),
            (k::KEY_Right, 106 + 8, Named::ArrowRight, Code::ArrowRight),
            (k::KEY_Down, 108 + 8, Named::ArrowDown, Code::ArrowDown),
            (k::KEY_F5, KC_F5, Named::F5, Code::F5),
            (k::KEY_F12, 88 + 8, Named::F12, Code::F12),
            (k::KEY_F13, 183 + 8, Named::F13, Code::F13),
            (k::KEY_BackSpace, 14 + 8, Named::Backspace, Code::Backspace),
            (k::KEY_Delete, 111 + 8, Named::Delete, Code::Delete),
            (k::KEY_Home, 102 + 8, Named::Home, Code::Home),
            (k::KEY_Page_Down, 109 + 8, Named::PageDown, Code::PageDown),
            (k::KEY_space, 57 + 8, Named::Space, Code::Space),
        ];
        for (keysym, keycode, named, code) in cases {
            let (key, modified, physical, location, _, _) =
                pressed(key_pressed(input(keysym, keycode, None)));
            assert_eq!(key, Key::Named(named), "keysym {keysym:#x}");
            assert_eq!(modified, Key::Named(named));
            assert_eq!(physical, Physical::Code(code));
            assert_eq!(location, Location::Standard);
        }
    }

    #[test]
    fn return_text_passes_through_for_text_widgets() {
        let (_, _, _, _, _, text) =
            pressed(key_pressed(input(k::KEY_Return, KC_ENTER, Some("\r"))));
        assert_eq!(text.as_deref(), Some("\r"));
    }

    #[test]
    fn ctrl_letter_keeps_the_letter_key() {
        let ctrl_c = KeyInput {
            modifiers: modifiers(false, true, false, false),
            ..input(k::KEY_c, 46 + 8, Some("\u{3}"))
        };
        let (key, modified, physical, _, mods, text) = pressed(key_pressed(ctrl_c));
        assert_eq!(key, ch("c"));
        assert_eq!(modified, ch("c"));
        assert_eq!(physical, Physical::Code(Code::KeyC));
        assert!(mods.control());
        assert!(mods.command());
        assert_eq!(key.to_latin(physical), Some('c'));
        assert_eq!(text.as_deref(), Some("\u{3}"));
    }

    #[test]
    fn modifier_keys_have_sides() {
        let (key, _, physical, location, ..) =
            pressed(key_pressed(input(k::KEY_Control_R, KC_RCTRL, None)));
        assert_eq!(key, Key::Named(Named::Control));
        assert_eq!(physical, Physical::Code(Code::ControlRight));
        assert_eq!(location, Location::Right);
        assert_eq!(location_of(k::KEY_Shift_L), Location::Left);
        assert_eq!(key_of(k::KEY_ISO_Level3_Shift), Key::Named(Named::AltGraph));
    }

    #[test]
    fn keypad() {
        let (key, _, physical, location, _, text) =
            pressed(key_pressed(input(k::KEY_KP_7, KC_KP7, Some("7"))));
        assert_eq!(key, ch("7"));
        assert_eq!(physical, Physical::Code(Code::Numpad7));
        assert_eq!(location, Location::Numpad);
        assert_eq!(text.as_deref(), Some("7"));

        let (key, _, physical, location, ..) =
            pressed(key_pressed(input(k::KEY_KP_Home, KC_KP7, None)));
        assert_eq!(key, Key::Named(Named::Home));
        assert_eq!(physical, Physical::Code(Code::Numpad7));
        assert_eq!(location, Location::Numpad);

        let (key, _, physical, location, ..) =
            pressed(key_pressed(input(k::KEY_KP_Enter, KC_KPENTER, Some("\r"))));
        assert_eq!(key, Key::Named(Named::Enter));
        assert_eq!(physical, Physical::Code(Code::NumpadEnter));
        assert_eq!(location, Location::Numpad);

        assert_eq!(key_of(k::KEY_KP_Add), ch("+"));
        assert_eq!(location_of(k::KEY_KP_Add), Location::Numpad);
        assert_eq!(key_of(k::KEY_KP_Decimal), ch("."));
    }

    #[test]
    fn dead_keys_are_unidentified_without_text() {
        let (key, modified, physical, location, _, text) =
            pressed(key_pressed(input(k::KEY_dead_acute, KC_APOSTROPHE, None)));
        assert_eq!(key, Key::Unidentified);
        assert_eq!(modified, Key::Unidentified);
        assert_eq!(physical, Physical::Code(Code::Quote));
        assert_eq!(location, Location::Standard);
        assert_eq!(text, None);
        assert_eq!(key_of(k::KEY_dead_circumflex), Key::Unidentified);
        // The composed result arrives as a normal keysym with text.
        assert_eq!(key_of(k::KEY_eacute), ch("é"));
    }

    #[test]
    fn unknown_keycodes_and_private_use_text() {
        assert_eq!(
            physical_key(700),
            Physical::Unidentified(NativeCode::Xkb(700))
        );
        assert_eq!(key_of(0), Key::Unidentified);
        let (_, _, _, _, _, text) = pressed(key_pressed(input(0x1008_ff00, 700, Some("\u{E000}"))));
        assert_eq!(text, None);
    }

    #[test]
    fn altgr_and_sided_modifiers_follow_the_physical_key() {
        let altgr = pressed(key_pressed(input(k::KEY_ISO_Level3_Shift, 100 + 8, None)));
        assert_eq!(altgr.0, Key::Named(Named::AltGraph));
        assert_eq!(altgr.2, Physical::Code(Code::AltRight));
        assert_eq!(altgr.3, Location::Right);
        assert_eq!(location_of(k::KEY_ISO_Level3_Shift), Location::Right);
        // A left Alt remapped to AltGr is still on the left.
        assert_eq!(
            pressed(key_pressed(input(k::KEY_ISO_Level3_Shift, 56 + 8, None))).3,
            Location::Left
        );
        // Keypad keys with NumLock off still sit on the keypad.
        assert_eq!(
            pressed(key_pressed(input(k::KEY_Home, 71 + 8, None))).3,
            Location::Numpad
        );
        assert_eq!(physical_key(128 + 8), Physical::Code(Code::BrowserStop));
    }

    #[test]
    fn release_mirrors_press() {
        assert_eq!(
            key_released(input(k::KEY_a, KC_A, Some("a"))),
            keyboard::Event::KeyReleased {
                key: ch("a"),
                modified_key: ch("a"),
                physical_key: Physical::Code(Code::KeyA),
                location: Location::Standard,
                modifiers: Modifiers::empty(),
            }
        );
    }

    fn key_of(raw: u32) -> Key {
        key(Keysym::new(raw))
    }

    fn location_of(raw: u32) -> Location {
        location(Keysym::new(raw))
    }
}
