//! Glue: cosmix-wl-app keyboard events to iced events and menu keys.

use super::app::Action;
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

/// Where a key goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Menus are open and modal: navigate.
    Menu(NavKey),
    /// Menus are open and the key means nothing to them.
    Swallow,
    /// F10: open the first menu with its first row selected.
    OpenBar,
    Shortcut(Action),
    /// To the chrome; if it does not capture the key and `then_grid`, to the
    /// grid. Releases only update the chrome's key state.
    Chrome {
        then_grid: bool,
    },
}

pub fn route(key: &KeyEvent, menus_open: bool) -> Route {
    if key.state == KeyState::Released {
        return Route::Chrome { then_grid: false };
    }
    if menus_open {
        return nav_key(key.keysym).map_or(Route::Swallow, Route::Menu);
    }
    if is_f10(key) {
        return Route::OpenBar;
    }
    let shortcuts = [
        (Keysym::c, Keysym::C, Action::Copy),
        (Keysym::v, Keysym::V, Action::Paste),
        (Keysym::l, Keysym::L, Action::ClearTab),
        (Keysym::q, Keysym::Q, Action::Quit),
    ];
    for (lower, upper, action) in shortcuts {
        if ctrl_shift(key, lower, upper) {
            return Route::Shortcut(action);
        }
    }
    let m = key.modifiers;
    if m.ctrl && !m.shift && !m.alt && key.keysym == Keysym::f {
        return Route::Shortcut(Action::FocusSearch);
    }
    Route::Chrome { then_grid: true }
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
    fn routing_between_bar_popups_chrome_and_grid() {
        let press = |sym| KeyEvent {
            modifiers: Modifiers::default(),
            ..key(sym, None, KeyState::Pressed)
        };
        assert_eq!(route(&press(Keysym::F10), false), Route::OpenBar);
        assert_eq!(route(&press(Keysym::Down), true), Route::Menu(NavKey::Down));
        assert_eq!(
            route(&press(Keysym::Escape), true),
            Route::Menu(NavKey::Escape)
        );
        // Menus are modal: typing and F10 are swallowed while open.
        assert_eq!(route(&press(Keysym::a), true), Route::Swallow);
        assert_eq!(route(&press(Keysym::F10), true), Route::Swallow);
        assert_eq!(
            route(&press(Keysym::a), false),
            Route::Chrome { then_grid: true }
        );
        // Releases never reach the grid or the menus.
        assert_eq!(
            route(&key(Keysym::Down, None, KeyState::Released), true),
            Route::Chrome { then_grid: false }
        );
        let mut ctrl_f = press(Keysym::f);
        ctrl_f.modifiers.ctrl = true;
        assert_eq!(route(&ctrl_f, false), Route::Shortcut(Action::FocusSearch));
        let mut copy = press(Keysym::C);
        copy.modifiers.ctrl = true;
        copy.modifiers.shift = true;
        assert_eq!(route(&copy, false), Route::Shortcut(Action::Copy));
        assert_eq!(route(&copy, true), Route::Swallow);
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
