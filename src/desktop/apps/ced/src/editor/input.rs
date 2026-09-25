//! Keys the editor widget handles itself (plan §2, §4.6). The root key router
//! has already taken every keymap chord, Alt+letter and Ctrl+wheel; what
//! reaches the widget is navigation, editing keys and text.

use cosmix_edit_client::model::{EditCommand, Motion};
use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

/// The command for a key press, if the editor handles it. `page` is the
/// number of full rows (Page Up/Down distance).
pub fn command(key: &Key, mods: Modifiers, text: Option<&str>, page: usize) -> Option<EditCommand> {
    let extend = mods.shift();
    let word = mods.control();
    let mv = |to| Some(EditCommand::Move { to, extend });
    if let Key::Named(named) = key {
        match named {
            Named::ArrowLeft => return mv(if word { Motion::WordLeft } else { Motion::Left }),
            Named::ArrowRight => return mv(if word { Motion::WordRight } else { Motion::Right }),
            Named::ArrowUp if !word => return mv(Motion::Up),
            Named::ArrowDown if !word => return mv(Motion::Down),
            Named::Home => return mv(if word { Motion::DocStart } else { Motion::Home }),
            Named::End => return mv(if word { Motion::DocEnd } else { Motion::End }),
            Named::PageUp if !word => return mv(Motion::PageUp(page)),
            Named::PageDown if !word => return mv(Motion::PageDown(page)),
            Named::Enter if !word && !mods.alt() => return Some(EditCommand::Newline),
            Named::Backspace if !mods.alt() => {
                return Some(if word { EditCommand::DeleteWordLeft } else { EditCommand::Backspace });
            }
            Named::Delete if !mods.alt() && !mods.shift() => {
                return Some(if word { EditCommand::DeleteWordRight } else { EditCommand::Delete });
            }
            Named::Tab if !word && !mods.alt() => return Some(if extend { EditCommand::Outdent } else { EditCommand::Tab }),
            _ => {}
        }
    }
    // Text: plain or Shift, and AltGr layouts (which report Ctrl+Alt).
    let chord = mods.control() != mods.alt() || mods.logo();
    let text = text?;
    if chord || text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(EditCommand::Insert(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(n: Named, m: Modifiers) -> Option<EditCommand> {
        command(&Key::Named(n), m, None, 30)
    }

    #[test]
    fn navigation_and_editing_keys() {
        let none = Modifiers::empty();
        assert_eq!(named(Named::ArrowLeft, none), Some(EditCommand::Move { to: Motion::Left, extend: false }));
        assert_eq!(named(Named::ArrowRight, Modifiers::CTRL | Modifiers::SHIFT), Some(EditCommand::Move { to: Motion::WordRight, extend: true }));
        assert_eq!(named(Named::Home, Modifiers::CTRL), Some(EditCommand::Move { to: Motion::DocStart, extend: false }));
        assert_eq!(named(Named::PageDown, none), Some(EditCommand::Move { to: Motion::PageDown(30), extend: false }));
        assert_eq!(named(Named::Backspace, Modifiers::CTRL), Some(EditCommand::DeleteWordLeft));
        assert_eq!(named(Named::Tab, Modifiers::SHIFT), Some(EditCommand::Outdent));
        assert_eq!(named(Named::Enter, none), Some(EditCommand::Newline));
        assert_eq!(named(Named::Escape, none), None, "Escape goes to the router's unclaimed hook");
        assert_eq!(named(Named::Delete, Modifiers::SHIFT), None, "Shift+Del is Cut (a chord)");
    }

    #[test]
    fn text_input() {
        let a = Key::Character("a".into());
        assert_eq!(command(&a, Modifiers::empty(), Some("a"), 1), Some(EditCommand::Insert("a".into())));
        assert_eq!(command(&a, Modifiers::SHIFT, Some("A"), 1), Some(EditCommand::Insert("A".into())));
        assert_eq!(command(&a, Modifiers::CTRL, Some("a"), 1), None);
        assert_eq!(command(&a, Modifiers::CTRL | Modifiers::ALT, Some("@"), 1), Some(EditCommand::Insert("@".into())), "AltGr");
        assert_eq!(command(&a, Modifiers::empty(), Some("\u{8}"), 1), None, "control characters are keys, not text");
    }
}
