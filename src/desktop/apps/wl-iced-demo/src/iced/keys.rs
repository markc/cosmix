//! Glue: cosmix-wl-app keyboard events to iced events and menu keys.

use crate::menus::NavKey;
use cosmix_iced_host::core::Event as IcedEvent;
use cosmix_iced_host::keys::{self, KeyInput};
use cosmix_wl_app::{KeyEvent, KeyState, Keysym, Modifiers};

pub fn modifiers(m: Modifiers) -> cosmix_iced_host::core::keyboard::Modifiers {
    keys::modifiers(m.shift, m.ctrl, m.alt, m.logo)
}

pub fn modifiers_event(m: Modifiers) -> IcedEvent {
    keys::modifiers_changed(modifiers(m))
}

pub fn key_event(key: &KeyEvent) -> IcedEvent {
    keys::key_event(
        KeyInput {
            keysym: key.keysym,
            unmodified_keysym: None,
            keycode: key.raw_code + 8,
            modifiers: modifiers(key.modifiers),
            text: key.text.as_deref(),
            repeat: key.state == KeyState::Repeated,
        },
        key.state != KeyState::Released,
    )
}

/// Menu navigation for a key while menus are open.
pub fn nav_key(sym: Keysym) -> Option<NavKey> {
    Some(match sym {
        Keysym::Up => NavKey::Up,
        Keysym::Down => NavKey::Down,
        Keysym::Left => NavKey::Left,
        Keysym::Right => NavKey::Right,
        Keysym::Home => NavKey::Home,
        Keysym::End => NavKey::End,
        Keysym::Return | Keysym::KP_Enter | Keysym::space => NavKey::Enter,
        Keysym::Escape => NavKey::Escape,
        _ => return None,
    })
}

pub fn is_f10(key: &KeyEvent) -> bool {
    key.keysym == Keysym::F10 && !key.modifiers.ctrl && !key.modifiers.alt
}

/// Ctrl+Shift+<letter>, matched on either case.
pub fn ctrl_shift(key: &KeyEvent, lower: Keysym, upper: Keysym) -> bool {
    key.modifiers.ctrl && key.modifiers.shift && (key.keysym == lower || key.keysym == upper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_iced_host::core::keyboard::{self, Key, key::Named};

    fn key(sym: Keysym, text: Option<&str>, state: KeyState) -> KeyEvent {
        KeyEvent {
            surface: None,
            state,
            keysym: sym,
            raw_code: 30,
            text: text.map(Into::into),
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            time: 0,
        }
    }

    #[test]
    fn converts_press_repeat_release() {
        let IcedEvent::Keyboard(keyboard::Event::KeyPressed {
            modified_key,
            text,
            repeat,
            modifiers,
            ..
        }) = key_event(&key(Keysym::A, Some("A"), KeyState::Repeated))
        else {
            panic!("not a press");
        };
        assert_eq!(modified_key, Key::Character("A".into()));
        assert_eq!(text.as_deref(), Some("A"));
        assert!(repeat);
        assert!(modifiers.shift());
        assert!(matches!(
            key_event(&key(Keysym::Escape, None, KeyState::Released)),
            IcedEvent::Keyboard(keyboard::Event::KeyReleased {
                key: Key::Named(Named::Escape),
                ..
            })
        ));
    }

    #[test]
    fn menu_keys() {
        assert_eq!(nav_key(Keysym::Down), Some(NavKey::Down));
        assert_eq!(nav_key(Keysym::KP_Enter), Some(NavKey::Enter));
        assert_eq!(nav_key(Keysym::a), None);
        assert!(is_f10(&key(Keysym::F10, None, KeyState::Pressed)));
        let mut k = key(Keysym::C, None, KeyState::Pressed);
        assert!(!ctrl_shift(&k, Keysym::c, Keysym::C));
        k.modifiers.ctrl = true;
        assert!(ctrl_shift(&k, Keysym::c, Keysym::C));
    }
}
