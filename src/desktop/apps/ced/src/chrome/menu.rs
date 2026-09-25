//! The menu bar (ced E1 plan §4.4): File, Edit, Search, View, Macros, Tabs,
//! Help, built from [`crate::actions`] with accelerator labels from
//! [`crate::keymap`]. Toggles carry a check mark; Undo-other names the lane
//! it will undo; Macros lists the discovered macros; Tabs lists the open tabs.
//! Alt+letter opens a menu through `cosmix_iced_widgets::menu::open_operation`
//! on [`BAR_ID`].

use cosmix_edit_client::types::TabId;
use cosmix_iced_widgets::menu::Item;

use crate::actions::{ActionId, Menu};
use crate::app::Msg;
use crate::keymap;
use crate::macros::MacroDef;

/// The widget id of the menu bar.
pub const BAR_ID: &str = "ced-menu-bar";

/// State the menus depend on.
#[derive(Debug, Clone, Default)]
pub struct MenuCtx<'a> {
    pub has_tab: bool,
    /// The active tab's mirror is Live (edits are accepted).
    pub live: bool,
    pub dirty: bool,
    pub any_dirty: bool,
    pub has_path: bool,
    pub has_selection: bool,
    /// Lane of the most recent edit by another origin (Ctrl+Alt+Z target).
    pub other_lane: Option<&'a str>,
    pub find_active: bool,
    pub overwrite: bool,
    pub whitespace: bool,
    pub line_numbers: bool,
    pub remote_carets: bool,
    pub problems: bool,
    pub output: bool,
    pub macros: &'a [MacroDef],
    pub macro_running: bool,
    /// Open tabs in strip order: id and display name.
    pub tabs: Vec<(TabId, String)>,
    pub active: Option<TabId>,
}

/// Whether `action` can run now.
pub fn enabled(action: ActionId, c: &MenuCtx<'_>) -> bool {
    use ActionId::*;
    match action {
        FileNew | FileOpen | FileExit | ViewZoomIn | ViewZoomOut | ViewZoomReset | ViewWhitespace | ViewLineNumbers
        | ViewRemoteCarets | ViewProblems | ViewOutput | ViewReloadSettings | HelpKeys | HelpAbout => true,
        FileSave => c.live && (c.dirty || !c.has_path),
        FileSaveAs => c.live,
        FileSaveAll => c.any_dirty,
        FileReload => c.live && c.has_path,
        FileClose | ViewClearMarkers => c.has_tab,
        EditUndoOther => c.live && c.other_lane.is_some(),
        EditCut | EditCopy => c.has_tab && c.has_selection,
        EditUndo | EditRedo | EditUndoAny | EditPaste | EditSelectAll | EditDuplicateLine | EditDeleteLine
        | EditMoveLineUp | EditMoveLineDown | EditToggleComment | EditIndent | EditOutdent | EditDeleteWordLeft
        | EditDeleteWordRight | EditOverwrite | SearchFind | SearchReplace | SearchGotoLine => c.live || (c.has_tab && is_read_only(action)),
        SearchFindNext | SearchFindPrev => c.has_tab,
        SearchReplaceAll => c.live && c.find_active,
        TabsNext | TabsPrev => c.tabs.len() > 1,
        TabsGoto(n) => usize::from(n) <= c.tabs.len(),
    }
}

/// Actions that only read the buffer (allowed on a detached tab).
fn is_read_only(action: ActionId) -> bool {
    matches!(
        action,
        ActionId::EditCopy | ActionId::EditSelectAll | ActionId::SearchFind | ActionId::SearchGotoLine | ActionId::EditOverwrite
    )
}

/// The label shown for `action` (dynamic for Undo-other and the toggles).
pub fn label(action: ActionId, c: &MenuCtx<'_>) -> String {
    let check = |on: bool, text: &str| format!("{} {text}", if on { "✓" } else { "\u{2007}" });
    match action {
        ActionId::EditUndoOther => match c.other_lane {
            Some(lane) => format!("Undo {lane}'s Last Edit"),
            None => action.label(),
        },
        ActionId::EditOverwrite => check(c.overwrite, "Overwrite Mode"),
        ActionId::ViewWhitespace => check(c.whitespace, "Show Whitespace"),
        ActionId::ViewLineNumbers => check(c.line_numbers, "Line Numbers"),
        ActionId::ViewRemoteCarets => check(c.remote_carets, "Other Origins' Carets"),
        ActionId::ViewProblems => check(c.problems, "Problems"),
        ActionId::ViewOutput => check(c.output, "Output"),
        _ => action.label(),
    }
}

fn entry(action: ActionId, c: &MenuCtx<'_>) -> Item<Msg> {
    let item = Item::action(label(action, c), Msg::Action(action)).enabled(enabled(action, c));
    match keymap::chords_for(action).first() {
        Some(chord) => item.accelerator(*chord),
        None => item,
    }
}

/// Where the separators go inside each menu (after these actions).
fn separator_after(action: ActionId) -> bool {
    use ActionId::*;
    matches!(
        action,
        FileOpen | FileSaveAll | FileReload | FileClose | EditUndoAny | EditPaste | EditSelectAll | EditMoveLineDown
            | EditOutdent | EditDeleteWordRight | SearchFindPrev | SearchReplaceAll | ViewZoomReset | ViewRemoteCarets
            | ViewOutput | ViewClearMarkers | TabsPrev | HelpKeys
    )
}

/// The whole bar.
pub fn bar(c: &MenuCtx<'_>) -> Vec<Item<Msg>> {
    Menu::ALL.iter().map(|menu| Item::submenu(menu.label(), panel(*menu, c))).collect()
}

/// One menu's entries.
pub fn panel(menu: Menu, c: &MenuCtx<'_>) -> Vec<Item<Msg>> {
    let mut items = Vec::new();
    for action in ActionId::all().into_iter().filter(|a| a.menu() == menu && !matches!(a, ActionId::TabsGoto(_))) {
        items.push(entry(action, c));
        if separator_after(action) {
            items.push(Item::separator());
        }
    }
    match menu {
        Menu::Macros => {
            if c.macros.is_empty() {
                items.push(Item::action("No macros (config/macros/*.mix)", Msg::Noop).enabled(false));
            }
            for m in c.macros {
                let item = Item::action(m.label.clone(), Msg::RunMacro(m.stem.clone())).enabled(c.live && !c.macro_running);
                items.push(match &m.chord {
                    Some(chord) => item.accelerator(chord.clone()),
                    None => item,
                });
            }
        }
        Menu::Tabs => {
            for (index, (id, name)) in c.tabs.iter().enumerate() {
                let marker = if c.active == Some(*id) { "•" } else { "\u{2007}" };
                let mut item = Item::action(format!("{marker} {name}"), Msg::SelectTab(*id));
                if index < 9 {
                    item = item.accelerator(format!("Ctrl+{}", index + 1));
                }
                items.push(item);
            }
        }
        _ => {}
    }
    while items.last().is_some_and(Item::is_separator) {
        items.pop();
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>() -> MenuCtx<'a> {
        MenuCtx { has_tab: true, live: true, tabs: vec![(1, "a.mix".into()), (2, "b.rs".into())], active: Some(1), ..MenuCtx::default() }
    }

    /// Plan §7.1: every ActionId is reachable — from a menu, or (tabs 1..9)
    /// from the keymap.
    #[test]
    fn every_action_is_reachable() {
        let c = ctx();
        let in_menus: Vec<Msg> = bar(&c)
            .iter()
            .flat_map(|m| m.children().iter().filter_map(|i| i.message().cloned()).collect::<Vec<_>>())
            .collect();
        for action in ActionId::all() {
            let reachable = in_menus.contains(&Msg::Action(action)) || !keymap::chords_for(action).is_empty();
            assert!(reachable, "{action:?} is in no menu and has no chord");
            if !matches!(action, ActionId::TabsGoto(_)) {
                assert!(in_menus.contains(&Msg::Action(action)), "{action:?} missing from its menu");
            }
        }
    }

    #[test]
    fn menus_are_in_bar_order_and_well_formed() {
        let c = ctx();
        let bar = bar(&c);
        let labels: Vec<_> = bar.iter().map(|m| m.label().to_owned()).collect();
        assert_eq!(labels, ["File", "Edit", "Search", "View", "Macros", "Tabs", "Help"]);
        for menu in &bar {
            let items = menu.children();
            assert!(!items.first().unwrap().is_separator() && !items.last().unwrap().is_separator(), "{}", menu.label());
            assert!(items.windows(2).all(|w| !(w[0].is_separator() && w[1].is_separator())), "{}", menu.label());
        }
    }

    #[test]
    fn accelerators_come_from_the_keymap() {
        let c = ctx();
        let file = panel(Menu::File, &c);
        let save_as = file.iter().find(|i| i.message() == Some(&Msg::Action(ActionId::FileSaveAs))).unwrap();
        assert_eq!(save_as.accelerator_label(), "Ctrl+Alt+S");
    }

    #[test]
    fn dynamic_labels_and_enablement() {
        let mut c = ctx();
        assert!(!enabled(ActionId::EditUndoOther, &c));
        assert_eq!(label(ActionId::EditUndoOther, &c), ActionId::EditUndoOther.label());
        c.other_lane = Some("agent:ctl-90");
        assert!(enabled(ActionId::EditUndoOther, &c));
        assert_eq!(label(ActionId::EditUndoOther, &c), "Undo agent:ctl-90's Last Edit");
        c.whitespace = true;
        assert!(label(ActionId::ViewWhitespace, &c).starts_with('✓'));
        c.live = false;
        assert!(!enabled(ActionId::EditPaste, &c), "no edits on a detached tab");
        assert!(enabled(ActionId::EditCopy, &MenuCtx { has_selection: true, ..c.clone() }), "copy still works");
        assert!(enabled(ActionId::TabsGoto(2), &c) && !enabled(ActionId::TabsGoto(3), &c));
        let none = MenuCtx::default();
        assert!(!enabled(ActionId::FileClose, &none) && enabled(ActionId::FileOpen, &none));
    }

    #[test]
    fn macros_and_tabs_are_listed() {
        let defs = [MacroDef { stem: "up".into(), label: "Uppercase".into(), chord: Some("Ctrl+Alt+U".into()), path: "/x/up.mix".into() }];
        let c = MenuCtx { macros: &defs, ..ctx() };
        let macros = panel(Menu::Macros, &c);
        assert!(macros.iter().any(|i| i.message() == Some(&Msg::RunMacro("up".into())) && i.accelerator_label() == "Ctrl+Alt+U"));
        let tabs = panel(Menu::Tabs, &c);
        assert!(tabs.iter().any(|i| i.message() == Some(&Msg::SelectTab(2)) && i.accelerator_label() == "Ctrl+2"));
        let empty = panel(Menu::Macros, &ctx());
        assert!(empty.iter().any(|i| !i.is_enabled() && i.label().starts_with("No macros")));
    }
}
