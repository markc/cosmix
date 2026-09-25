//! Default key bindings (ced E1 plan §4.6, Notepad++ conventions) — **frozen
//! in Stage S** as data. Menus show these as accelerator labels; the root
//! key router resolves them. Before E1f ships, the Ctrl+Alt+* rows are
//! checked against inputd's live global chords (decision 9); a collision
//! moves ced's chord here and in the docs. Ctrl+Alt+P is reserved (Edit
//! panels…).

use crate::actions::ActionId;

/// A parsed chord: modifiers + one key (lower-case letter/digit, or a named
/// key such as `F3`, `Tab`, `PageUp`, `Up`, `Backspace`, `Delete`, `Insert`,
/// `=`, `-`, `/`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

const NAMED: &[&str] = &[
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "Tab", "PageUp", "PageDown", "Up",
    "Down", "Left", "Right", "Home", "End", "Backspace", "Delete", "Insert", "Enter", "Escape",
];

/// Parse `"Ctrl+Alt+S"`-style text (modifiers in any order, key last).
pub fn parse_chord(text: &str) -> Option<Chord> {
    let mut chord = Chord { ctrl: false, alt: false, shift: false, key: String::new() };
    let parts: Vec<&str> = text.split('+').collect();
    // "Ctrl+=" / "Ctrl+-": a trailing empty part means the key was '+' — not used.
    let (key, mods) = parts.split_last()?;
    for m in mods {
        match *m {
            "Ctrl" => chord.ctrl = true,
            "Alt" => chord.alt = true,
            "Shift" => chord.shift = true,
            _ => return None,
        }
    }
    let key = *key;
    let ok = NAMED.contains(&key)
        || (key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric() || "=-/".contains(c)));
    if !ok {
        return None;
    }
    chord.key = if key.chars().count() == 1 { key.to_ascii_lowercase() } else { key.to_string() };
    Some(chord)
}

/// The default bindings. An action may have several chords; a chord maps to
/// exactly one action.
pub const DEFAULT: &[(&str, ActionId)] = &[
    ("Ctrl+N", ActionId::FileNew),
    ("Ctrl+O", ActionId::FileOpen),
    ("Ctrl+S", ActionId::FileSave),
    ("Ctrl+Alt+S", ActionId::FileSaveAs),
    ("Ctrl+Shift+S", ActionId::FileSaveAll),
    ("Ctrl+W", ActionId::FileClose),
    ("Ctrl+Q", ActionId::FileExit),
    ("Ctrl+Z", ActionId::EditUndo),
    ("Ctrl+Y", ActionId::EditRedo),
    ("Ctrl+Shift+Z", ActionId::EditRedo),
    ("Ctrl+Alt+Z", ActionId::EditUndoOther),
    ("Ctrl+X", ActionId::EditCut),
    ("Shift+Delete", ActionId::EditCut),
    ("Ctrl+C", ActionId::EditCopy),
    ("Ctrl+Insert", ActionId::EditCopy),
    ("Ctrl+V", ActionId::EditPaste),
    ("Shift+Insert", ActionId::EditPaste),
    ("Ctrl+A", ActionId::EditSelectAll),
    ("Ctrl+D", ActionId::EditDuplicateLine),
    ("Ctrl+Shift+L", ActionId::EditDeleteLine),
    ("Ctrl+Shift+Up", ActionId::EditMoveLineUp),
    ("Ctrl+Shift+Down", ActionId::EditMoveLineDown),
    ("Ctrl+/", ActionId::EditToggleComment),
    ("Ctrl+Backspace", ActionId::EditDeleteWordLeft),
    ("Ctrl+Delete", ActionId::EditDeleteWordRight),
    ("Insert", ActionId::EditOverwrite),
    ("Ctrl+F", ActionId::SearchFind),
    ("F3", ActionId::SearchFindNext),
    ("Shift+F3", ActionId::SearchFindPrev),
    ("Ctrl+H", ActionId::SearchReplace),
    ("Ctrl+G", ActionId::SearchGotoLine),
    ("Ctrl+=", ActionId::ViewZoomIn),
    ("Ctrl+-", ActionId::ViewZoomOut),
    ("Ctrl+0", ActionId::ViewZoomReset),
    ("Ctrl+Shift+M", ActionId::ViewProblems),
    ("Ctrl+Tab", ActionId::TabsNext),
    ("Ctrl+PageDown", ActionId::TabsNext),
    ("Ctrl+Shift+Tab", ActionId::TabsPrev),
    ("Ctrl+PageUp", ActionId::TabsPrev),
    ("Ctrl+1", ActionId::TabsGoto(1)),
    ("Ctrl+2", ActionId::TabsGoto(2)),
    ("Ctrl+3", ActionId::TabsGoto(3)),
    ("Ctrl+4", ActionId::TabsGoto(4)),
    ("Ctrl+5", ActionId::TabsGoto(5)),
    ("Ctrl+6", ActionId::TabsGoto(6)),
    ("Ctrl+7", ActionId::TabsGoto(7)),
    ("Ctrl+8", ActionId::TabsGoto(8)),
    ("Ctrl+9", ActionId::TabsGoto(9)),
];

/// Tab / Shift+Tab with a multi-line selection indent/outdent; F10 and
/// Alt+<mnemonic> open menus — these are handled by the key router, not the
/// table, because they depend on state.
pub const CONTEXTUAL: &[(&str, &str)] = &[
    ("Tab", "indent the selection (multi-line) or insert a tab"),
    ("Shift+Tab", "outdent the selection"),
    ("F10", "open the menu bar"),
    ("Alt+<letter>", "open that menu (File, Edit, Search, View, Macros, Tabs, Help)"),
];

/// Every chord bound to `action`, in table order (accelerator label = first).
pub fn chords_for(action: ActionId) -> Vec<&'static str> {
    DEFAULT.iter().filter(|(_, a)| *a == action).map(|(c, _)| *c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chord_parses_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for (text, action) in DEFAULT {
            let c = parse_chord(text).unwrap_or_else(|| panic!("{text} does not parse"));
            assert!(seen.insert(c), "{text} bound twice (again to {action:?})");
        }
    }

    #[test]
    fn reserved_chords_are_free() {
        let p = parse_chord("Ctrl+Alt+P").unwrap();
        assert!(DEFAULT.iter().all(|(t, _)| parse_chord(t).unwrap() != p));
    }
}
