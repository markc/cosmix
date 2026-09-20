//! iced key press -> `cosmix_term_core::terminal::Key`.
//!
//! A pure function, so the mapping is testable without a compositor. It
//! mirrors the Bevy frontend's `keyboard` observer (`apps/bterm/src/main.rs`)
//! deliberately: the two frontends must put the same bytes on the PTY, and the
//! only way to know that is to compare them against the same encoder.

use cosmix_term_core::terminal::Key as TerminalKey;
use iced::keyboard::{Key, Modifiers, key::Named};

/// The keys one press sends, in order. Empty means "not ours" — Alt and Super
/// chords are left unhandled rather than swallowed, so a future accelerator
/// table can take them without changing this.
pub fn keys_for(key: &Key, text: Option<&str>, modifiers: Modifiers) -> Vec<TerminalKey> {
    if modifiers.alt() || modifiers.logo() {
        return Vec::new();
    }
    if modifiers.control() {
        // Ctrl is a chord, never text: winit reports Ctrl+C with a text of
        // "\u{3}" on some seats and None on others, and going through the
        // encoder from the LETTER makes both seats identical.
        return match key.as_ref() {
            Key::Character(character) => ascii_letter(character)
                .map(TerminalKey::Control)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
    }
    if let Key::Named(named) = key
        && let Some(key) = named_key(*named)
    {
        return vec![key];
    }
    // Everything else is what the seat says the key typed, so a non-US layout
    // works without this file knowing anything about layouts.
    text.map(|text| {
        text.chars()
            .filter(|c| c.is_ascii() && !c.is_control())
            .map(TerminalKey::Char)
            .collect()
    })
    .unwrap_or_default()
}

/// A single ASCII letter, or nothing. `Key::Character` can hold a whole
/// grapheme cluster; `Ctrl+é` has no control code and must not be guessed at.
fn ascii_letter(character: &str) -> Option<char> {
    let mut chars = character.chars();
    let c = chars.next()?;
    (c.is_ascii_alphabetic() && chars.next().is_none()).then_some(c)
}

fn named_key(named: Named) -> Option<TerminalKey> {
    Some(match named {
        Named::Enter => TerminalKey::Enter,
        Named::Backspace => TerminalKey::Backspace,
        Named::Tab => TerminalKey::Tab,
        Named::Escape => TerminalKey::Escape,
        Named::ArrowUp => TerminalKey::Up,
        Named::ArrowDown => TerminalKey::Down,
        Named::ArrowLeft => TerminalKey::Left,
        Named::ArrowRight => TerminalKey::Right,
        Named::Home => TerminalKey::Home,
        Named::End => TerminalKey::End,
        Named::Delete => TerminalKey::Delete,
        Named::PageUp => TerminalKey::PageUp,
        Named::PageDown => TerminalKey::PageDown,
        // Space carries its own text; anything else here is not a VT key.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_term_core::terminal::encode;

    /// Asserts on the BYTES, not on the enum: `Key` has no `PartialEq`, and
    /// the bytes are what the shell actually receives.
    fn bytes(key: &Key, text: Option<&str>, modifiers: Modifiers) -> Vec<u8> {
        keys_for(key, text, modifiers)
            .into_iter()
            .flat_map(encode)
            .collect()
    }

    fn character(c: &str) -> Key {
        Key::Character(c.into())
    }

    #[test]
    fn plain_text_reaches_the_pty_as_itself() {
        assert_eq!(
            bytes(&character("a"), Some("a"), Modifiers::empty()),
            b"a"
        );
        assert_eq!(
            bytes(&Key::Named(Named::Space), Some(" "), Modifiers::empty()),
            b" "
        );
        // Shift is not a chord: it is already reflected in the seat's text.
        assert_eq!(bytes(&character("a"), Some("A"), Modifiers::SHIFT), b"A");
    }

    #[test]
    fn named_keys_beat_the_text_the_seat_reports() {
        // winit reports Enter with text "\r" on some seats; either way one
        // Enter must reach the PTY, never two.
        assert_eq!(
            bytes(&Key::Named(Named::Enter), Some("\r"), Modifiers::empty()),
            b"\r"
        );
        assert_eq!(
            bytes(&Key::Named(Named::Tab), Some("\t"), Modifiers::empty()),
            b"\t"
        );
        assert_eq!(
            bytes(&Key::Named(Named::ArrowUp), None, Modifiers::empty()),
            b"\x1b[A"
        );
        assert_eq!(
            bytes(&Key::Named(Named::PageDown), None, Modifiers::empty()),
            b"\x1b[6~"
        );
        assert_eq!(
            bytes(&Key::Named(Named::Backspace), None, Modifiers::empty()),
            &[127]
        );
    }

    #[test]
    fn control_chords_come_from_the_letter_not_the_text() {
        // The seat reported no text at all; Ctrl+C must still interrupt.
        assert_eq!(bytes(&character("c"), None, Modifiers::CTRL), &[3]);
        assert_eq!(bytes(&character("d"), None, Modifiers::CTRL), &[4]);
        // ...and an uppercase letter is the same chord.
        assert_eq!(bytes(&character("C"), Some("C"), Modifiers::CTRL), &[3]);
        // A seat that DID report the control character must not send it
        // twice — the text branch is unreachable under Ctrl.
        assert_eq!(bytes(&character("c"), Some("\u{3}"), Modifiers::CTRL), &[3]);
    }

    #[test]
    fn chords_this_frontend_does_not_own_are_left_alone() {
        // Alt and Super belong to a future accelerator table (T3/T4), and to
        // the compositor. Swallowing them here would make them unreachable.
        assert!(bytes(&character("x"), Some("x"), Modifiers::ALT).is_empty());
        assert!(bytes(&character("x"), Some("x"), Modifiers::LOGO).is_empty());
        assert!(
            bytes(&Key::Named(Named::Enter), None, Modifiers::ALT).is_empty(),
            "a named key under Alt is still not ours"
        );
        // Ctrl with no letter (Ctrl+F5, Ctrl+Shift) produces nothing rather
        // than falling through to the text branch.
        assert!(bytes(&Key::Named(Named::F5), None, Modifiers::CTRL).is_empty());
        assert!(bytes(&character("é"), Some("é"), Modifiers::CTRL).is_empty());
        assert!(bytes(&Key::Unidentified, None, Modifiers::empty()).is_empty());
    }

    #[test]
    fn non_ascii_text_is_dropped_rather_than_mangled() {
        // The core's encoder returns an empty Vec for a non-ASCII char, so
        // passing it through would be a silent no-op with a key press
        // charged against it. Filtering here keeps that visible in one place.
        assert!(bytes(&character("é"), Some("é"), Modifiers::empty()).is_empty());
        assert_eq!(
            keys_for(&character("aéb"), Some("aéb"), Modifiers::empty()).len(),
            2
        );
    }
}
