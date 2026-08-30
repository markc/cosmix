//! Bevy-to-`cosmix-actions` physical keyboard normalisation.
//!
//! Shortcut policy does not live here: this module only translates Bevy's
//! engine vocabulary into the stable, engine-independent key vocabulary.

use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::Resource;
use cosmix_actions::{Key, Modifiers, RawInput, RawInputState};

/// Event-local modifier state folded in [`KeyboardInput`] delivery order.
///
/// Bevy's [`bevy::input::ButtonInput`] resource is the final state after the
/// entire frame's event batch. It cannot describe the modifiers that were held
/// at an earlier event within that batch, so shortcut normalisation must not
/// consult it.
#[derive(Resource, Default)]
pub struct EventKeyState {
    control_left: bool,
    control_right: bool,
    alt_left: bool,
    alt_right: bool,
    shift_left: bool,
    shift_right: bool,
    super_left: bool,
    super_right: bool,
}

impl EventKeyState {
    /// Clear all held modifiers after the window loses keyboard focus.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fold one event and translate non-modifier keys for the shared resolver.
    pub fn normalise(&mut self, input: &KeyboardInput) -> Option<RawInput> {
        if self.update_modifier(input.key_code, input.state) {
            return None;
        }
        Some(RawInput {
            key: key(input.key_code)?,
            modifiers: self.modifiers(),
            state: match input.state {
                ButtonState::Pressed => RawInputState::Pressed,
                ButtonState::Released => RawInputState::Released,
            },
            repeat: input.repeat,
        })
    }

    fn update_modifier(&mut self, key: KeyCode, state: ButtonState) -> bool {
        let pressed = state == ButtonState::Pressed;
        let slot = match key {
            KeyCode::ControlLeft => &mut self.control_left,
            KeyCode::ControlRight => &mut self.control_right,
            KeyCode::AltLeft => &mut self.alt_left,
            KeyCode::AltRight => &mut self.alt_right,
            KeyCode::ShiftLeft => &mut self.shift_left,
            KeyCode::ShiftRight => &mut self.shift_right,
            KeyCode::SuperLeft => &mut self.super_left,
            KeyCode::SuperRight => &mut self.super_right,
            _ => return false,
        };
        *slot = pressed;
        true
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers {
            control: self.control_left || self.control_right,
            alt: self.alt_left || self.alt_right,
            shift: self.shift_left || self.shift_right,
            super_key: self.super_left || self.super_right,
        }
    }
}

fn key(code: KeyCode) -> Option<Key> {
    Some(match code {
        KeyCode::KeyA => Key::Character('A'),
        KeyCode::KeyB => Key::Character('B'),
        KeyCode::KeyC => Key::Character('C'),
        KeyCode::KeyD => Key::Character('D'),
        KeyCode::KeyE => Key::Character('E'),
        KeyCode::KeyF => Key::Character('F'),
        KeyCode::KeyG => Key::Character('G'),
        KeyCode::KeyH => Key::Character('H'),
        KeyCode::KeyI => Key::Character('I'),
        KeyCode::KeyJ => Key::Character('J'),
        KeyCode::KeyK => Key::Character('K'),
        KeyCode::KeyL => Key::Character('L'),
        KeyCode::KeyM => Key::Character('M'),
        KeyCode::KeyN => Key::Character('N'),
        KeyCode::KeyO => Key::Character('O'),
        KeyCode::KeyP => Key::Character('P'),
        KeyCode::KeyQ => Key::Character('Q'),
        KeyCode::KeyR => Key::Character('R'),
        KeyCode::KeyS => Key::Character('S'),
        KeyCode::KeyT => Key::Character('T'),
        KeyCode::KeyU => Key::Character('U'),
        KeyCode::KeyV => Key::Character('V'),
        KeyCode::KeyW => Key::Character('W'),
        KeyCode::KeyX => Key::Character('X'),
        KeyCode::KeyY => Key::Character('Y'),
        KeyCode::KeyZ => Key::Character('Z'),
        KeyCode::Digit0 | KeyCode::Numpad0 => Key::Character('0'),
        KeyCode::Digit1 | KeyCode::Numpad1 => Key::Character('1'),
        KeyCode::Digit2 | KeyCode::Numpad2 => Key::Character('2'),
        KeyCode::Digit3 | KeyCode::Numpad3 => Key::Character('3'),
        KeyCode::Digit4 | KeyCode::Numpad4 => Key::Character('4'),
        KeyCode::Digit5 | KeyCode::Numpad5 => Key::Character('5'),
        KeyCode::Digit6 | KeyCode::Numpad6 => Key::Character('6'),
        KeyCode::Digit7 | KeyCode::Numpad7 => Key::Character('7'),
        KeyCode::Digit8 | KeyCode::Numpad8 => Key::Character('8'),
        KeyCode::Digit9 | KeyCode::Numpad9 => Key::Character('9'),
        KeyCode::Space => Key::Space,
        KeyCode::Enter | KeyCode::NumpadEnter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace | KeyCode::NumpadBackspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::F1 => Key::Function(1),
        KeyCode::F2 => Key::Function(2),
        KeyCode::F3 => Key::Function(3),
        KeyCode::F4 => Key::Function(4),
        KeyCode::F5 => Key::Function(5),
        KeyCode::F6 => Key::Function(6),
        KeyCode::F7 => Key::Function(7),
        KeyCode::F8 => Key::Function(8),
        KeyCode::F9 => Key::Function(9),
        KeyCode::F10 => Key::Function(10),
        KeyCode::F11 => Key::Function(11),
        KeyCode::F12 => Key::Function(12),
        KeyCode::F13 => Key::Function(13),
        KeyCode::F14 => Key::Function(14),
        KeyCode::F15 => Key::Function(15),
        KeyCode::F16 => Key::Function(16),
        KeyCode::F17 => Key::Function(17),
        KeyCode::F18 => Key::Function(18),
        KeyCode::F19 => Key::Function(19),
        KeyCode::F20 => Key::Function(20),
        KeyCode::F21 => Key::Function(21),
        KeyCode::F22 => Key::Function(22),
        KeyCode::F23 => Key::Function(23),
        KeyCode::F24 => Key::Function(24),
        KeyCode::Minus | KeyCode::NumpadSubtract => Key::Minus,
        KeyCode::Equal | KeyCode::NumpadEqual | KeyCode::NumpadAdd => Key::Equal,
        KeyCode::Comma | KeyCode::NumpadComma => Key::Comma,
        KeyCode::Period | KeyCode::NumpadDecimal => Key::Period,
        KeyCode::Slash | KeyCode::NumpadDivide => Key::Slash,
        KeyCode::Backslash | KeyCode::IntlBackslash => Key::Backslash,
        KeyCode::Semicolon => Key::Semicolon,
        KeyCode::Quote => Key::Quote,
        KeyCode::BracketLeft => Key::BracketLeft,
        KeyCode::BracketRight => Key::BracketRight,
        KeyCode::Backquote => Key::Backquote,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::keyboard::Key as LogicalKey;

    fn event(key_code: KeyCode, state: ButtonState) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key: match key_code {
                KeyCode::KeyS => LogicalKey::Character("s".into()),
                KeyCode::ControlLeft | KeyCode::ControlRight => LogicalKey::Control,
                _ => LogicalKey::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
            },
            state,
            text: (key_code == KeyCode::KeyS).then(|| "s".into()),
            repeat: false,
            window: bevy::prelude::Entity::PLACEHOLDER,
        }
    }

    #[test]
    fn modifier_state_is_folded_at_each_events_position_in_the_batch() {
        let mut state = EventKeyState::default();
        assert!(state
            .normalise(&event(KeyCode::ControlLeft, ButtonState::Pressed))
            .is_none());
        let chorded = state
            .normalise(&event(KeyCode::KeyS, ButtonState::Pressed))
            .unwrap();
        assert!(state
            .normalise(&event(KeyCode::ControlLeft, ButtonState::Released))
            .is_none());

        assert_eq!(chorded.key, Key::Character('S'));
        assert!(chorded.modifiers.control);

        let plain = state
            .normalise(&event(KeyCode::KeyS, ButtonState::Pressed))
            .unwrap();
        assert!(!plain.modifiers.control);
    }

    #[test]
    fn later_modifier_down_does_not_retroactively_modify_an_earlier_key() {
        let mut state = EventKeyState::default();
        let plain = state
            .normalise(&event(KeyCode::KeyS, ButtonState::Pressed))
            .unwrap();
        assert!(state
            .normalise(&event(KeyCode::ControlLeft, ButtonState::Pressed))
            .is_none());

        assert!(!plain.modifiers.control);
    }

    #[test]
    fn modifier_only_events_are_not_shortcut_strokes() {
        let mut state = EventKeyState::default();
        assert!(state
            .normalise(&event(KeyCode::AltLeft, ButtonState::Pressed))
            .is_none());
    }

    #[test]
    fn focus_loss_reset_clears_held_modifiers() {
        let mut state = EventKeyState::default();
        state.normalise(&event(KeyCode::ControlLeft, ButtonState::Pressed));
        state.reset();

        let raw = state
            .normalise(&event(KeyCode::KeyS, ButtonState::Pressed))
            .unwrap();
        assert!(!raw.modifiers.control);
    }

    #[test]
    fn modifiers_are_folded_at_each_event_position() {
        let mut state = EventKeyState::default();
        assert!(state
            .normalise(&event(KeyCode::ControlLeft, ButtonState::Pressed))
            .is_none());
        let chorded = state
            .normalise(&event(KeyCode::KeyH, ButtonState::Pressed))
            .unwrap();
        assert!(state
            .normalise(&event(KeyCode::ControlLeft, ButtonState::Released))
            .is_none());
        let plain = state
            .normalise(&event(KeyCode::KeyH, ButtonState::Pressed))
            .unwrap();
        assert!(chorded.modifiers.control);
        assert!(!plain.modifiers.control);
    }

    #[test]
    fn reset_clears_held_modifiers() {
        let mut state = EventKeyState::default();
        state.normalise(&event(KeyCode::AltLeft, ButtonState::Pressed));
        state.reset();
        let plain = state
            .normalise(&event(KeyCode::ArrowLeft, ButtonState::Pressed))
            .unwrap();
        assert!(!plain.modifiers.alt);
    }
}
