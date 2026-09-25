//! iced key press -> `cosmix_term_core::terminal::Key`.
//!
//! A pure function, so the mapping is testable without a compositor. It
//! mirrors the Bevy frontend's `keyboard` observer (`apps/bterm/src/main.rs`)
//! deliberately: the two frontends must put the same bytes on the PTY, and the
//! only way to know that is to compare them against the same encoder.

use cosmix_term_core::panes::{Direction, SplitDir};
use cosmix_term_core::terminal::{Key as TerminalKey, ScrollRequest};
use iced::keyboard::key::{Code, Named, Physical};
use iced::keyboard::{Key, Modifiers};
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
    CyclePane { forward: bool },
    FontIncrease,
    FontDecrease,
    FontReset,
    Scroll(ScrollRequest),
    Copy,
    Paste,
}

impl Action {
    /// Whether holding the chord repeats it. Font steps do, as in foot; a
    /// held Ctrl+Shift+T must not open a tab per autorepeat tick. A repeat of
    /// a non-repeating chord is still CONSUMED — it never reaches the PTY as
    /// a control code.
    pub fn repeats(self) -> bool {
        matches!(
            self,
            Self::FontIncrease | Self::FontDecrease | Self::FontReset | Self::Scroll(_)
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
/// The letter chords (Ctrl+Shift+T and friends) read the layout's letter
/// first and fall back to the PHYSICAL key when the layout has no Latin
/// letter there — on a Cyrillic layout Ctrl+Shift+T says "е", and bterm,
/// which matches Bevy's physical `KeyCode`, would still open a tab. Logical
/// first so a Dvorak user's T is the key labelled T.
///
/// Alt and Super chords are never ours, exactly as in [`keys_for`].
pub fn action_for(
    key: &Key,
    modified: &Key,
    physical: Physical,
    modifiers: Modifiers,
) -> Option<Action> {
    if modifiers.alt() || modifiers.logo() {
        return None;
    }
    if !modifiers.control() {
        return if modifiers.shift() {
            if matches!(key, Key::Named(Named::Insert)) {
                Some(Action::Paste)
            } else {
                scroll_request(key).map(Action::Scroll)
            }
        } else {
            None
        };
    }
    let shift = modifiers.shift();
    if matches!(key, Key::Named(Named::Tab)) {
        return Some(Action::CyclePane { forward: !shift });
    }
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
        Key::Character(_) if shift => Some(match chord_letter(key, physical)? {
            'c' => Action::Copy,
            'v' => Action::Paste,
            't' => Action::NewTab,
            'w' => Action::CloseTab,
            'q' => Action::Quit,
            'e' => Action::Split(SplitDir::Vertical),
            'o' => Action::Split(SplitDir::Horizontal),
            'x' => Action::ClosePane,
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

fn scroll_request(key: &Key) -> Option<ScrollRequest> {
    Some(match key {
        Key::Named(Named::PageUp) => ScrollRequest::PageUp,
        Key::Named(Named::PageDown) => ScrollRequest::PageDown,
        Key::Named(Named::Home) => ScrollRequest::Top,
        Key::Named(Named::End) => ScrollRequest::Bottom,
        _ => return None,
    })
}

/// Alternate-screen applications own the scrollback chords too.
pub fn action_on_screen(action: Option<Action>, alternate: bool) -> Option<Action> {
    action.filter(|action| !alternate || !matches!(action, Action::Scroll(_)))
}

/// Layout order is the pane tree's first-child/second-child traversal.
pub fn cycle_pane(ids: &[u64], active: u64, forward: bool) -> Option<u64> {
    let current = ids.iter().position(|id| *id == active)?;
    let next = if forward { (current + 1) % ids.len() } else { (current + ids.len() - 1) % ids.len() };
    Some(ids[next])
}

/// Mouse-area positions are relative to the pane's outer border, in logical
/// pixels. Clamp the border and spare right/bottom pixels to the nearest cell.
pub fn pointer_cell(
    position: iced::Point,
    border: f32,
    cell: (f32, f32),
    grid: (u16, u16),
) -> (u16, u16) {
    let index = |value: f32, size: f32, count: u16| {
        (((value - border).max(0.0) / size) as u16).min(count.saturating_sub(1))
    };
    (
        index(position.x, cell.0, grid.0),
        index(position.y, cell.1, grid.1),
    )
}

/// The lowercase Latin letter a chord key stands for: the layout's own
/// letter when it has one, else the physical key's US position.
///
/// The fallback is only for layouts with NO Latin letter on the key. A Latin
/// layout that puts an accented letter there — Turkish F has "ğ" where US
/// has E and "ö" where US has X — has its own e and x elsewhere, and those
/// are its chords. Falling back there too would bind both keys, and
/// Ctrl+Shift+ö would close a pane (round-2 review finding).
fn chord_letter(key: &Key, physical: Physical) -> Option<char> {
    if let Key::Character(c) = key.as_ref() {
        if let Some(letter) = ascii_letter(c) {
            return Some(letter.to_ascii_lowercase());
        }
        let mut chars = c.chars();
        if let (Some(first), None) = (chars.next(), chars.next())
            && first.is_alphabetic()
            && is_latin(first)
        {
            return None;
        }
    }
    let Physical::Code(code) = physical else {
        return None;
    };
    Some(match code {
        Code::KeyC => 'c',
        Code::KeyV => 'v',
        Code::KeyT => 't',
        Code::KeyW => 'w',
        Code::KeyQ => 'q',
        Code::KeyE => 'e',
        Code::KeyO => 'o',
        Code::KeyX => 'x',
        _ => return None,
    })
}

/// Whether `c` is in a Latin-script block: Basic Latin, the Latin-1
/// Supplement, Latin Extended-A and -B, and Latin Extended Additional.
/// Callers check `is_alphabetic` too, so the symbols in those blocks (such as
/// `×` and `÷`) never count as letters.
fn is_latin(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x00FF | 0x0100..=0x024F | 0x1E00..=0x1EFF
    )
}

/// Logical pixels of smooth (touchpad) scrolling that make one wheel step.
/// A notched wheel reports whole lines and steps once per notch.
pub const PIXELS_PER_STEP: f32 = 40.0;

/// Ctrl+wheel -> font steps: positive grows, negative shrinks. Positive is
/// iced's scroll-up, the direction foot calls `BTN_WHEEL_BACK` (its
/// scrollback-up button), and foot binds `font-increase=Control+BTN_WHEEL_BACK`
/// (`/etc/xdg/foot/foot.ini`), so the two agree.
///
/// Fractions accumulate in `pending` so a touchpad's stream of small deltas
/// adds up to steps instead of rounding every one of them to zero; turning
/// round discards what was accumulated the other way, so a reversal answers
/// at once rather than first paying back the old direction.
pub fn wheel_steps(pending: &mut f32, delta: ScrollDelta) -> i32 {
    scroll_steps(pending, delta, PIXELS_PER_STEP)
}

/// Scrollback touchpad travel uses logical cell height, as bterm does.
pub fn scroll_steps(pending: &mut f32, delta: ScrollDelta, cell_height: f32) -> i32 {
    let amount = match delta {
        ScrollDelta::Lines { y, .. } => y,
        ScrollDelta::Pixels { y, .. } => y / cell_height,
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
    text.map(text_keys).unwrap_or_default()
}

/// Shared by key text and IME commits; format characters (ZWJ/VS) are text.
pub fn text_keys(text: &str) -> Vec<TerminalKey> {
    text.chars()
        .filter(|c| !c.is_control())
        .map(TerminalKey::Char)
        .collect()
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
    fn clipboard_chords_follow_layout_letters_and_physical_fallback() {
        let mods = Modifiers::CTRL | Modifiers::SHIFT;
        for (letter, code, non_latin, action) in [
            ("c", Code::KeyC, "с", Action::Copy),
            ("v", Code::KeyV, "м", Action::Paste),
        ] {
            for text in [letter, non_latin] {
                assert_eq!(action_for(&character(text), &character(text), Physical::Code(code), mods), Some(action));
            }
            assert_eq!(action_for(&character(letter), &character(letter), Physical::Code(Code::KeyX), mods), Some(action));
            for extra in [Modifiers::ALT, Modifiers::LOGO] {
                assert_eq!(action_for(&character(letter), &character(letter), Physical::Code(code), mods | extra), None);
            }
            assert_eq!(action_for(&character("ö"), &character("Ö"), Physical::Code(code), mods), None);
            assert!(!action.repeats());
            assert_eq!(action_on_screen(Some(action), true), Some(action));
        }
        let insert = Key::Named(Named::Insert);
        assert_eq!(action_for(&insert, &insert, Physical::Code(Code::Insert), Modifiers::SHIFT), Some(Action::Paste));
        for modifiers in [Modifiers::empty(), Modifiers::CTRL, mods, Modifiers::SHIFT | Modifiers::ALT] {
            assert_eq!(action_for(&insert, &insert, Physical::Code(Code::Insert), modifiers), None);
        }
        assert_eq!(action_for(&character("c"), &character("c"), Physical::Code(Code::KeyC), Modifiers::CTRL), None);
        assert_eq!(bytes(&character("c"), None, Modifiers::CTRL), [3]);
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

    /// `action_for` with no physical key, so every case below is decided by
    /// the logical keys alone — the layout-independent fallback has its own
    /// test.
    fn act(key: &Key, modified: &Key, modifiers: Modifiers) -> Option<Action> {
        action_for(
            key,
            modified,
            Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            modifiers,
        )
    }

    /// Review finding: on a Cyrillic layout the T key's unmodified character
    /// is "е", and matching the character alone left every tab chord dead.
    /// bterm matches the physical key, so the same press must work here.
    #[test]
    fn letter_chords_fall_back_to_the_physical_key_on_non_latin_layouts() {
        let cyrillic = [
            ("е", "Е", Code::KeyT, Action::NewTab),
            ("ц", "Ц", Code::KeyW, Action::CloseTab),
            ("й", "Й", Code::KeyQ, Action::Quit),
            ("у", "У", Code::KeyE, Action::Split(SplitDir::Vertical)),
            ("щ", "Щ", Code::KeyO, Action::Split(SplitDir::Horizontal)),
            ("ч", "Ч", Code::KeyX, Action::ClosePane),
        ];
        for (plain, shifted, code, action) in cyrillic {
            let key = character(plain);
            assert_eq!(
                act(&key, &character(shifted), ctrl_shift()),
                None,
                "without the physical key there is nothing to go on"
            );
            assert_eq!(
                action_for(&key, &character(shifted), Physical::Code(code), ctrl_shift()),
                Some(action),
                "{code:?}"
            );
        }
        // The layout's own Latin letter wins over the position: on Dvorak the
        // key labelled T sits where QWERTY has K.
        assert_eq!(
            action_for(&character("t"), &character("T"), Physical::Code(Code::KeyK), ctrl_shift()),
            Some(Action::NewTab)
        );
        // Turkish F (round-2 finding): accented Latin letters sit on the US
        // E and X positions, and the layout has its own e and x elsewhere.
        // The accented keys must NOT fire; the layout's letters must.
        assert_eq!(
            action_for(&character("ğ"), &character("Ğ"), Physical::Code(Code::KeyE), ctrl_shift()),
            None,
            "Ctrl+Shift+ğ split a pane"
        );
        assert_eq!(
            action_for(&character("ö"), &character("Ö"), Physical::Code(Code::KeyX), ctrl_shift()),
            None,
            "Ctrl+Shift+ö closed a pane"
        );
        for (letter, code, action) in [
            ("e", Code::KeyQ, Action::Split(SplitDir::Vertical)),
            ("x", Code::KeyB, Action::ClosePane),
        ] {
            assert_eq!(
                action_for(&character(letter), &character(&letter.to_uppercase()), Physical::Code(code), ctrl_shift()),
                Some(action),
                "the layout's own {letter}"
            );
        }
        // Other Latin blocks too: Extended-B (ǝ) and Extended Additional (ẽ).
        for accented in ["ǝ", "ẽ"] {
            assert_eq!(
                action_for(&character(accented), &character(accented), Physical::Code(Code::KeyT), ctrl_shift()),
                None,
                "{accented}"
            );
        }
        // Greek, like Cyrillic, has no Latin letter and still falls back.
        assert_eq!(
            action_for(&character("τ"), &character("Τ"), Physical::Code(Code::KeyT), ctrl_shift()),
            Some(Action::NewTab)
        );
        // A physical fallback must not invent chords: an unmapped position
        // on a non-Latin layout is nothing.
        assert_eq!(
            action_for(&character("к"), &character("К"), Physical::Code(Code::KeyR), ctrl_shift()),
            None
        );
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers::CTRL | Modifiers::SHIFT
    }

    #[test]
    fn shift_navigation_scrolls_plain_navigation_is_shell_input_and_ctrl_cycles() {
        for (key, request, shell) in [
            (Named::PageUp, ScrollRequest::PageUp, b"\x1b[5~".as_slice()),
            (Named::PageDown, ScrollRequest::PageDown, b"\x1b[6~".as_slice()),
            (Named::Home, ScrollRequest::Top, b"\x1b[H".as_slice()),
            (Named::End, ScrollRequest::Bottom, b"\x1b[F".as_slice()),
        ] {
            let key = named(key);
            assert_eq!(act(&key, &key, Modifiers::SHIFT), Some(Action::Scroll(request)));
            assert!(Action::Scroll(request).repeats());
            assert_eq!(act(&key, &key, Modifiers::empty()), None);
            assert_eq!(bytes(&key, None, Modifiers::empty()), shell);
            assert_eq!(act(&key, &key, Modifiers::SHIFT | Modifiers::ALT), None);
        }
        let up = named(Named::PageUp);
        assert_eq!(act(&up, &up, Modifiers::CTRL), Some(Action::Cycle { forward: false }));
    }

    #[test]
    fn pointer_coordinates_exclude_the_border_and_clamp_spare_pixels() {
        let cell = (8.0, 16.0);
        assert_eq!(pointer_cell(iced::Point::new(1.2, 1.2), 1.2, cell, (80, 24)), (0, 0));
        assert_eq!(pointer_cell(iced::Point::new(25.3, 33.3), 1.2, cell, (80, 24)), (3, 2));
        assert_eq!(pointer_cell(iced::Point::ORIGIN, 1.2, cell, (80, 24)), (0, 0));
        assert_eq!(pointer_cell(iced::Point::new(900.0, 500.0), 1.2, cell, (80, 24)), (79, 23));
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
            assert_eq!(act(&key, &character(&letter.to_uppercase()), ctrl_shift()), Some(action));
            // Without Shift it is the shell's Ctrl+letter, not ours.
            assert_eq!(act(&key, &key, Modifiers::CTRL), None, "Ctrl+{letter}");
        }
        for (arrow, direction) in [
            (Named::ArrowLeft, Direction::Left),
            (Named::ArrowRight, Direction::Right),
            (Named::ArrowUp, Direction::Up),
            (Named::ArrowDown, Direction::Down),
        ] {
            let key = named(arrow);
            assert_eq!(act(&key, &key, ctrl_shift()), Some(Action::Focus(direction)));
            assert_eq!(act(&key, &key, Modifiers::CTRL), None);
        }
        let down = named(Named::PageDown);
        let up = named(Named::PageUp);
        assert_eq!(act(&down, &down, Modifiers::CTRL), Some(Action::Cycle { forward: true }));
        assert_eq!(act(&up, &up, Modifiers::CTRL), Some(Action::Cycle { forward: false }));
        // bterm cycles on Ctrl+PageUp/PageDown WITHOUT Shift only.
        assert_eq!(act(&down, &down, ctrl_shift()), None);
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
        assert_eq!(act(&equal, &equal, Modifiers::CTRL), Some(Action::FontIncrease));
        // Control+plus on US: key "=" with Shift, modified "+".
        assert_eq!(act(&equal, &plus, ctrl_shift()), Some(Action::FontIncrease));
        // Control+plus on a layout with a plus key, and Control+KP_Add.
        assert_eq!(act(&plus, &plus, Modifiers::CTRL), Some(Action::FontIncrease));
        // Control+minus and Control+KP_Subtract.
        assert_eq!(act(&minus, &minus, Modifiers::CTRL), Some(Action::FontDecrease));
        // Control+0.
        assert_eq!(act(&zero, &zero, Modifiers::CTRL), Some(Action::FontReset));
        // Control+KP_0 as winit really reports it with NumLock on: the
        // unmodified key is the level-0 keysym KP_Insert, only the modified
        // key is "0" (review finding: the first cut faked "0" in both).
        let insert = named(Named::Insert);
        assert_eq!(act(&insert, &zero, Modifiers::CTRL), Some(Action::FontReset));
        // NumLock off: Insert in both, and nothing fires — as in foot.
        assert_eq!(act(&insert, &insert, Modifiers::CTRL), None);
        // Shift+0 is ")" on US: Ctrl+Shift+0 must not reset.
        assert_eq!(act(&zero, &character(")"), ctrl_shift()), None);
        // Without Ctrl, or with Alt, these are text.
        assert_eq!(act(&equal, &equal, Modifiers::empty()), None);
        assert_eq!(act(&minus, &minus, Modifiers::CTRL | Modifiers::ALT), None);
        assert_eq!(act(&zero, &zero, Modifiers::CTRL | Modifiers::LOGO), None);
        // Font steps repeat when held, as in foot.
        assert!(Action::FontIncrease.repeats());
        assert!(Action::FontDecrease.repeats());
    }

    #[test]
    fn ctrl_tab_cycles_panes_but_bare_tab_stays_with_the_shell() {
        let tab = named(Named::Tab);
        assert_eq!(act(&tab, &tab, Modifiers::CTRL), Some(Action::CyclePane { forward: true }));
        assert_eq!(act(&tab, &tab, ctrl_shift()), Some(Action::CyclePane { forward: false }));
        assert_eq!(act(&tab, &tab, Modifiers::empty()), None);
        assert_eq!(bytes(&tab, Some("\t"), Modifiers::empty()), b"\t");
        for modifier in [Modifiers::ALT, Modifiers::LOGO] {
            assert_eq!(act(&tab, &tab, Modifiers::CTRL | modifier), None);
            assert_eq!(act(&tab, &tab, ctrl_shift() | modifier), None);
        }
        assert!(!Action::CyclePane { forward: true }.repeats());
        let ids = [9, 3, 17]; // Layout order, deliberately not numeric order.
        assert_eq!(cycle_pane(&ids, 9, true), Some(3));
        assert_eq!(cycle_pane(&ids, 17, true), Some(9));
        assert_eq!(cycle_pane(&ids, 9, false), Some(17));
        assert_eq!(cycle_pane(&ids, 17, false), Some(3));
        assert_eq!(cycle_pane(&[9], 9, false), Some(9));
        assert_eq!(cycle_pane(&[], 9, true), None);
    }

    #[test]
    fn alternate_screen_owns_shift_navigation() {
        for (named_key, expected) in [
            (Named::PageUp, &b"\x1b[5~"[..]),
            (Named::PageDown, &b"\x1b[6~"[..]),
            (Named::Home, &b"\x1b[H"[..]),
            (Named::End, &b"\x1b[F"[..]),
        ] {
            let key = named(named_key);
            let action = act(&key, &key, Modifiers::SHIFT);
            assert!(matches!(action_on_screen(action, false), Some(Action::Scroll(_))));
            assert_eq!(action_on_screen(action, true), None);
            assert_eq!(bytes(&key, None, Modifiers::SHIFT), expected);
        }
    }

    #[test]
    fn touchpad_history_uses_cell_height_while_zoom_keeps_forty_pixels() {
        for height in [13.0, 17.0, 24.0] {
            let mut pending = 0.0;
            let half = ScrollDelta::Pixels { x: 0.0, y: height / 2.0 };
            assert_eq!(scroll_steps(&mut pending, half, height), 0);
            assert_eq!(scroll_steps(&mut pending, half, height), 1);
            assert_eq!(scroll_steps(&mut pending, ScrollDelta::Pixels { x: 0.0, y: -height }, height), -1);
        }
        assert_eq!(wheel_steps(&mut 0.0, ScrollDelta::Pixels { x: 0.0, y: 40.0 }), 1);
    }

    /// The chord must beat the shell encoder: without the action table,
    /// Ctrl+Shift+T is Ctrl-T on the PTY. This pins that the two tables
    /// overlap, so the dispatcher's "action first" order is load-bearing.
    #[test]
    fn a_tab_chord_would_otherwise_reach_the_shell_as_a_control_code() {
        let key = character("t");
        assert_eq!(bytes(&key, Some("T"), ctrl_shift()), &[20]);
        assert!(act(&key, &character("T"), ctrl_shift()).is_some());
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
    fn keyboard_text_preserves_complete_unicode_sequences() {
        for text in ["aéb", "👩‍💻", "🇦🇺", "👍🏽", "❤️", "e\u{301}"] {
            assert_eq!(
                bytes(&character(text), Some(text), Modifiers::empty()),
                text.as_bytes()
            );
            assert!(cosmix_term_core::terminal::encode_text(text).is_err());
        }
        assert_eq!(
            bytes(&character("x"), Some("\u{1b}👩‍💻\u{3}"), Modifiers::empty()),
            "👩‍💻".as_bytes()
        );
    }
}
