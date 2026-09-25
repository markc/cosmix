//! Every menu / keymap / `ced.action` action (ced E1 plan §4.4, §4.6, §4.8) —
//! **complete and frozen in Stage S**. The string id is the `ced.action` /
//! `ced.actions` wire name and never changes once shipped.
//!
//! Macros are not here: they are discovered at run time and appear as
//! `macro.<stem>` actions (plan §4.9).

/// Top-level menus, in bar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Menu {
    File,
    Edit,
    Search,
    View,
    Macros,
    Tabs,
    Help,
}

impl Menu {
    pub const ALL: [Menu; 7] = [Menu::File, Menu::Edit, Menu::Search, Menu::View, Menu::Macros, Menu::Tabs, Menu::Help];

    pub fn label(self) -> &'static str {
        match self {
            Menu::File => "File",
            Menu::Edit => "Edit",
            Menu::Search => "Search",
            Menu::View => "View",
            Menu::Macros => "Macros",
            Menu::Tabs => "Tabs",
            Menu::Help => "Help",
        }
    }

    /// The Alt+letter that opens it.
    pub fn mnemonic(self) -> char {
        self.label().chars().next().unwrap_or(' ').to_ascii_lowercase()
    }
}

macro_rules! actions {
    ($( $variant:ident => $id:literal, $label:literal, $menu:ident; )*) => {
        /// One action. `TabsGoto(n)` is `tabs.goto.<n>` (n = 1..=9).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ActionId {
            $( $variant, )*
            TabsGoto(u8),
        }

        impl ActionId {
            /// Every fixed action (plus `tabs.goto.1..9`), in menu order.
            pub fn all() -> Vec<ActionId> {
                let mut v = vec![$( ActionId::$variant, )*];
                v.extend((1..=9).map(ActionId::TabsGoto));
                v
            }

            /// Stable wire id (`ced.action {id}`).
            pub fn id(self) -> String {
                match self {
                    $( ActionId::$variant => $id.to_string(), )*
                    ActionId::TabsGoto(n) => format!("tabs.goto.{n}"),
                }
            }

            pub fn label(self) -> String {
                match self {
                    $( ActionId::$variant => $label.to_string(), )*
                    ActionId::TabsGoto(n) => format!("Tab {n}"),
                }
            }

            pub fn menu(self) -> Menu {
                match self {
                    $( ActionId::$variant => Menu::$menu, )*
                    ActionId::TabsGoto(_) => Menu::Tabs,
                }
            }

            pub fn from_id(id: &str) -> Option<ActionId> {
                match id {
                    $( $id => Some(ActionId::$variant), )*
                    _ => {
                        let n: u8 = id.strip_prefix("tabs.goto.")?.parse().ok()?;
                        (1..=9).contains(&n).then_some(ActionId::TabsGoto(n))
                    }
                }
            }
        }
    };
}

actions! {
    FileNew => "file.new", "New", File;
    FileOpen => "file.open", "Open…", File;
    FileSave => "file.save", "Save", File;
    FileSaveAs => "file.save_as", "Save As…", File;
    FileSaveAll => "file.save_all", "Save All", File;
    FileReload => "file.reload", "Reload from Disk", File;
    FileClose => "file.close", "Close Tab", File;
    FileExit => "file.exit", "Exit", File;
    EditUndo => "edit.undo", "Undo", Edit;
    EditRedo => "edit.redo", "Redo", Edit;
    EditUndoOther => "edit.undo_other", "Undo Last Edit by Another Origin", Edit;
    EditUndoAny => "edit.undo_any", "Undo Anyone's Last", Edit;
    EditCut => "edit.cut", "Cut", Edit;
    EditCopy => "edit.copy", "Copy", Edit;
    EditPaste => "edit.paste", "Paste", Edit;
    EditSelectAll => "edit.select_all", "Select All", Edit;
    EditDuplicateLine => "edit.duplicate_line", "Duplicate Line", Edit;
    EditDeleteLine => "edit.delete_line", "Delete Line", Edit;
    EditMoveLineUp => "edit.move_line_up", "Move Line Up", Edit;
    EditMoveLineDown => "edit.move_line_down", "Move Line Down", Edit;
    EditToggleComment => "edit.toggle_comment", "Toggle Comment", Edit;
    EditIndent => "edit.indent", "Indent", Edit;
    EditOutdent => "edit.outdent", "Outdent", Edit;
    EditDeleteWordLeft => "edit.delete_word_left", "Delete Word Left", Edit;
    EditDeleteWordRight => "edit.delete_word_right", "Delete Word Right", Edit;
    EditOverwrite => "edit.overwrite", "Overwrite Mode", Edit;
    SearchFind => "search.find", "Find…", Search;
    SearchFindNext => "search.find_next", "Find Next", Search;
    SearchFindPrev => "search.find_prev", "Find Previous", Search;
    SearchReplace => "search.replace", "Replace…", Search;
    SearchReplaceAll => "search.replace_all", "Replace All", Search;
    SearchGotoLine => "search.goto_line", "Go to Line…", Search;
    ViewZoomIn => "view.zoom_in", "Zoom In", View;
    ViewZoomOut => "view.zoom_out", "Zoom Out", View;
    ViewZoomReset => "view.zoom_reset", "Reset Zoom", View;
    ViewWhitespace => "view.whitespace", "Show Whitespace", View;
    ViewLineNumbers => "view.line_numbers", "Line Numbers", View;
    ViewRemoteCarets => "view.remote_carets", "Other Origins' Carets", View;
    ViewProblems => "view.problems", "Problems", View;
    ViewOutput => "view.output", "Output", View;
    ViewClearMarkers => "view.clear_markers", "Clear Change Markers", View;
    ViewReloadSettings => "view.reload_settings", "Reload Settings", View;
    TabsNext => "tabs.next", "Next Tab", Tabs;
    TabsPrev => "tabs.prev", "Previous Tab", Tabs;
    HelpKeys => "help.keys", "Keyboard Shortcuts", Help;
    HelpAbout => "help.about", "About CosMix Editor", Help;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_are_unique() {
        let all = ActionId::all();
        let mut seen = std::collections::HashSet::new();
        for a in &all {
            assert!(seen.insert(a.id()), "duplicate id {}", a.id());
            assert_eq!(ActionId::from_id(&a.id()), Some(*a));
            assert!(a.id().contains('.'));
        }
        assert_eq!(ActionId::from_id("tabs.goto.0"), None);
        assert_eq!(ActionId::from_id("tabs.goto.10"), None);
        assert_eq!(ActionId::from_id("nope"), None);
    }

    #[test]
    fn menu_mnemonics_are_distinct() {
        let m: std::collections::HashSet<char> = Menu::ALL.iter().map(|m| m.mnemonic()).collect();
        assert_eq!(m.len(), Menu::ALL.len());
    }
}
