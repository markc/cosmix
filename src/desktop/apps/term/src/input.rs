//! iced key press -> `cosmix_term_core::terminal::Key`.
//!
//! A pure function, so the mapping is testable without a compositor. It
//! mirrors the Bevy frontend's `keyboard` observer (`apps/bterm/src/main.rs`)
//! deliberately: the two frontends must put the same bytes on the PTY, and the
//! only way to know that is to compare them against the same encoder.

use cosmix_term_core::panes::{Direction, SplitDir};
use cosmix_term_core::terminal::Key as TerminalKey;
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::mouse::ScrollDelta;

/// What a chord does to the terminal rather than to the shell in it.
///
/// Tabs and panes use bterm's chords exactly (T3 parity, `docs/cos/term.md`);
/// the font chords are foot's (T4, `man 5 foot.ini` § key-bindings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    CloseTab,
    Quit,
    Split(SplitDir),
    ClosePane,
    Focus(Direction),
    Cycle { forward: bool },
    FontIncrease,
    FontDecrease,
    FontReset,
}

impl Action {
    /// Whether holding the chord repeats it. Font steps do, as in foot; a
    /// held Ctrl+Shift+T must not open a tab per autorepeat tick. A repeat of
    /// a non-repeating chord is still CONSUMED — it never reaches the PTY as
    /// a control code.
    pub fn repeats(self) -> bool {
        matches!(
            self,
            Self::FontIncrease | Self::FontDecrease | Self::FontReset
        )
    }
}

/// The terminal's own chord for a key press, if it is one.
///
/// `key` is the key with no modifiers applied and `modified` is the key with
/// everything but Ctrl applied (iced's `key` and `modified_key`). Both are
/// needed for foot's `Control+plus`: on a US layout plus is Shift+=, so `key`
/// says "=" and `modified` says "+", while on a layout with a dedicated plus
/// key `key` itself says "+". The keypad's plus and minus report as those
/// characters in both, which covers `KP_Add` and `KP_Subtract`.
///
/// The keypad's zero does NOT: `key` is winit's `key_without_modifiers`,
/// which ignores NumLock too, so it reports the level-0 keysym `KP_Insert`
/// (`Named::Insert`). Only `modified` says "0". So minus and zero are matched
/// on either — which is safe, because Shift is refused for both and no
/// layout's unshifted `modified` says "0" or "-" on a key that means
/// something else. With NumLock off both say Insert and nothing fires, as in
/// foot, whose binding is on the `KP_0` keysym.
///
/// Alt and Super chords are never ours, exactly as in [`keys_for`].
pub fn action_for(key: &Key, modified: &Key, modifiers: Modifiers) -> Option<Action> {
    if !modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    let shift = modifiers.shift();
    let is = |candidate: &Key, text: &str| matches!(candidate.as_ref(), Key::Character(c) if c == text);
    if is(modified, "+") || is(modified, "=") || is(key, "+") || is(key, "=") {
        return Some(Action::FontIncrease);
    }
    if !shift && (is(key, "-") || is(modified, "-")) {
        return Some(Action::FontDecrease);
    }
    if !shift && (is(key, "0") || is(modified, "0")) {
        return Some(Action::FontReset);
    }
    match key.as_ref() {
        Key::Character(c) if shift => Some(match c.to_ascii_lowercase().as_str() {
            "t" => Action::NewTab,
            "w" => Action::CloseTab,
            "q" => Action::Quit,
            "e" => Action::Split(SplitDir::Vertical),
            "o" => Action::Split(SplitDir::Horizontal),
            "x" => Action::ClosePane,
            _ => return None,
        }),
        Key::Named(named) if shift => Some(Action::Focus(match named {
            Named::ArrowLeft => Direction::Left,
            Named::ArrowRight => Direction::Right,
            Named::ArrowUp => Direction::Up,
            Named::ArrowDown => Direction::Down,
            _ => return None,
        })),
        Key::Named(Named::PageDown) => Some(Action::Cycle { forward: true }),
        Key::Named(Named::PageUp) => Some(Action::Cycle { forward: false }),
        _ => None,
    }
}

/// Logical pixels of smooth (touchpad) scrolling that make one font step.
/// A notched wheel reports whole lines and steps once per notch.
pub const PIXELS_PER_STEP: f32 = 40.0;

/// Ctrl+wheel -> font steps: positive grows (wheel forward, away from the
/// user, as in foot's `Control+BTN_WHEEL_FORWARD`), negative shrinks.
///
/// Fractions accumulate in `pending` so a touchpad's stream of small deltas
/// adds up to steps instead of rounding every one of them to zero; turning
/// round discards what was accumulated the other way, so a reversal answers
/// at once rather than first paying back the old direction.
pub fn wheel_steps(pending: &mut f32, delta: ScrollDelta) -> i32 {
    let amount = match delta {
        ScrollDelta::Lines { y, .. } => y,
        ScrollDelta::Pixels { y, .. } => y / PIXELS_PER_STEP,
    };
    if !amount.is_finite() || amount == 0.0 {
        return 0;
    }
    if amount.signum() != pending.signum() {
        *pending = 0.0;
    }
    *pending += amount;
    let steps = pending.trunc();
    *pending -= steps;
    steps as i32
}

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

    fn named(named: Named) -> Key {
        Key::Named(named)
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers::CTRL | Modifiers::SHIFT
    }

    /// bterm's chords, one for one: T3 parity is the same keys doing the same
    /// thing, and the Shift+letter case arrives as the unmodified letter.
    #[test]
    fn tab_and_pane_chords_match_bterm() {
        let cases = [
            ("t", Action::NewTab),
            ("w", Action::CloseTab),
            ("q", Action::Quit),
            ("e", Action::Split(SplitDir::Vertical)),
            ("o", Action::Split(SplitDir::Horizontal)),
            ("x", Action::ClosePane),
        ];
        for (letter, action) in cases {
            let key = character(letter);
            assert_eq!(action_for(&key, &character(&letter.to_uppercase()), ctrl_shift()), Some(action));
            // Without Shift it is the shell's Ctrl+letter, not ours.
            assert_eq!(action_for(&key, &key, Modifiers::CTRL), None, "Ctrl+{letter}");
        }
        for (arrow, direction) in [
            (Named::ArrowLeft, Direction::Left),
            (Named::ArrowRight, Direction::Right),
            (Named::ArrowUp, Direction::Up),
            (Named::ArrowDown, Direction::Down),
        ] {
            let key = named(arrow);
            assert_eq!(action_for(&key, &key, ctrl_shift()), Some(Action::Focus(direction)));
            assert_eq!(action_for(&key, &key, Modifiers::CTRL), None);
        }
        let down = named(Named::PageDown);
        let up = named(Named::PageUp);
        assert_eq!(action_for(&down, &down, Modifiers::CTRL), Some(Action::Cycle { forward: true }));
        assert_eq!(action_for(&up, &up, Modifiers::CTRL), Some(Action::Cycle { forward: false }));
        // bterm cycles on Ctrl+PageUp/PageDown WITHOUT Shift only.
        assert_eq!(action_for(&down, &down, ctrl_shift()), None);
        // Tab chords never repeat: a held Ctrl+Shift+T is one tab.
        assert!(!Action::NewTab.repeats());
        assert!(!Action::ClosePane.repeats());
    }

    /// foot's font-increase / font-decrease / font-reset bindings, including
    /// the keypad and the US-layout "plus is Shift+=" case.
    #[test]
    fn font_chords_match_foot() {
        let plus = character("+");
        let equal = character("=");
        let minus = character("-");
        let zero = character("0");
        // Control+equal.
        assert_eq!(action_for(&equal, &equal, Modifiers::CTRL), Some(Action::FontIncrease));
        // Control+plus on US: key "=" with Shift, modified "+".
        assert_eq!(action_for(&equal, &plus, ctrl_shift()), Some(Action::FontIncrease));
        // Control+plus on a layout with a plus key, and Control+KP_Add.
        assert_eq!(action_for(&plus, &plus, Modifiers::CTRL), Some(Action::FontIncrease));
        // Control+minus and Control+KP_Subtract.
        assert_eq!(action_for(&minus, &minus, Modifiers::CTRL), Some(Action::FontDecrease));
        // Control+0.
        assert_eq!(action_for(&zero, &zero, Modifiers::CTRL), Some(Action::FontReset));
        // Control+KP_0 as winit really reports it with NumLock on: the
        // unmodified key is the level-0 keysym KP_Insert, only the modified
        // key is "0" (review finding: the first cut faked "0" in both).
        let insert = named(Named::Insert);
        assert_eq!(action_for(&insert, &zero, Modifiers::CTRL), Some(Action::FontReset));
        // NumLock off: Insert in both, and nothing fires — as in foot.
        assert_eq!(action_for(&insert, &insert, Modifiers::CTRL), None);
        // Shift+0 is ")" on US: Ctrl+Shift+0 must not reset.
        assert_eq!(action_for(&zero, &character(")"), ctrl_shift()), None);
        // Without Ctrl, or with Alt, these are text.
        assert_eq!(action_for(&equal, &equal, Modifiers::empty()), None);
        assert_eq!(action_for(&minus, &minus, Modifiers::CTRL | Modifiers::ALT), None);
        assert_eq!(action_for(&zero, &zero, Modifiers::CTRL | Modifiers::LOGO), None);
        // Font steps repeat when held, as in foot.
        assert!(Action::FontIncrease.repeats());
        assert!(Action::FontDecrease.repeats());
    }

    /// The chord must beat the shell encoder: without the action table,
    /// Ctrl+Shift+T is Ctrl-T on the PTY. This pins that the two tables
    /// overlap, so the dispatcher's "action first" order is load-bearing.
    #[test]
    fn a_tab_chord_would_otherwise_reach_the_shell_as_a_control_code() {
        let key = character("t");
        assert_eq!(bytes(&key, Some("T"), ctrl_shift()), &[20]);
        assert!(action_for(&key, &character("T"), ctrl_shift()).is_some());
    }

    #[test]
    fn a_notched_wheel_steps_once_per_notch_in_its_direction() {
        let mut pending = 0.0;
        assert_eq!(wheel_steps(&mut pending, ScrollDelta::Lines { x: 0.0, y: 1.0 }), 1);
        assert_eq!(wheel_steps(&mut pending, ScrollDelta::Lines { x: 0.0, y: 3.0 }), 3);
        assert_eq!(wheel_steps(&mut pending, ScrollDelta::Lines { x: 0.0, y: -1.0 }), -1);
        assert_eq!(wheel_steps(&mut pending, ScrollDelta::Lines { x: 1.0, y: 0.0 }), 0);
        assert_eq!(wheel_steps(&mut pending, ScrollDelta::Lines { x: 0.0, y: f32::NAN }), 0);
    }

    #[test]
    fn smooth_scrolling_accumulates_and_a_reversal_starts_fresh() {
        let mut pending = 0.0;
        let small = ScrollDelta::Pixels { x: 0.0, y: PIXELS_PER_STEP / 4.0 };
        assert_eq!(wheel_steps(&mut pending, small), 0);
        assert_eq!(wheel_steps(&mut pending, small), 0);
        assert_eq!(wheel_steps(&mut pending, small), 0);
        assert_eq!(wheel_steps(&mut pending, small), 1, "four quarters are one step");
        // Three quarters forward, then back: the reversal does not have to
        // pay back the forward remainder before it shrinks anything.
        for _ in 0..3 {
            wheel_steps(&mut pending, small);
        }
        let back = ScrollDelta::Pixels { x: 0.0, y: -PIXELS_PER_STEP };
        assert_eq!(wheel_steps(&mut pending, back), -1);
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
