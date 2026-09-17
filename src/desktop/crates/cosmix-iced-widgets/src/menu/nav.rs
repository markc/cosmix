//! Pure menu navigation, shared by the in-surface overlay and by hosts that
//! show panels on their own surfaces (xdg_popup).
use iced_core::{Rectangle, keyboard};

use super::{Item, Kind};

/// Which menu is open and which row is selected in each open panel.
///
/// Panel 0 lists the open root's children (bar) or the context items. Panel
/// `n + 1` is the submenu of the row selected in panel `n`, so there is one
/// `path` entry per visible panel. `None` means nothing is selected there
/// (opened by pointer, or the pointer is on a disabled row).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuState {
    /// The open bar entry. A context menu uses `Some(0)`. `None` is closed.
    pub root: Option<usize>,
    /// Selected row per open panel.
    pub path: Vec<Option<usize>>,
    /// Where each panel is anchored, in logical pixels: `anchors[0]` is the
    /// bar title (a 1 x 1 rectangle at the pointer for a context menu) in the
    /// menu widget's window coordinates; `anchors[n]` is the parent row within
    /// panel `n - 1`, relative to that panel's top-left. Filled in by the menu
    /// widget in external-popup mode; the navigator never changes it.
    pub anchors: Vec<Rectangle>,
}

impl MenuState {
    /// True when a menu is open.
    pub fn is_open(&self) -> bool {
        self.root.is_some()
    }
}

/// What a navigation step did.
#[derive(Debug, Clone, PartialEq)]
pub enum NavOutcome<Message> {
    /// Nothing changed.
    None,
    /// The open state or a selection changed (including opening).
    Changed,
    /// An action was chosen; the menu is now closed. Publish the message.
    Activated(Message),
    /// The menu closed without an action.
    Closed,
}

/// Next selectable row after (or before) `selected`, wrapping. `None` starts
/// from the first (or last) row.
pub(crate) fn next<Message>(
    items: &[Item<Message>],
    selected: Option<usize>,
    forward: bool,
) -> Option<usize> {
    let len = items.len();
    (0..len)
        .map(|step| match selected {
            Some(index) if forward => (index + step + 1) % len,
            Some(index) => (index + len - (step + 1) % len) % len,
            None if forward => step,
            None => len - step - 1,
        })
        .find(|index| items[*index].selectable())
}

/// Menu navigation over one item tree. `bar` menus open one root entry at a
/// time and move between roots with Left/Right; context menus show the items
/// themselves as panel 0.
#[derive(Debug)]
pub struct Navigator<'a, Message> {
    pub(super) items: &'a [Item<Message>],
    pub(super) bar: bool,
}

impl<Message> Clone for Navigator<'_, Message> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Message> Copy for Navigator<'_, Message> {}

impl<'a, Message: Clone> Navigator<'a, Message> {
    /// Navigation for a menu bar whose top-level entries are `items`.
    pub fn bar(items: &'a [Item<Message>]) -> Self {
        Self { items, bar: true }
    }

    /// Navigation for a context menu showing `items`.
    pub fn context(items: &'a [Item<Message>]) -> Self {
        Self { items, bar: false }
    }

    /// True for a bar navigator.
    pub fn is_bar(&self) -> bool {
        self.bar
    }

    /// The items shown in panel `level` (empty if that panel is not open).
    pub fn panel(&self, state: &MenuState, level: usize) -> &'a [Item<Message>] {
        let Some(root) = state.root else {
            return &[];
        };
        let mut items = if self.bar {
            self.items.get(root).map(Item::children).unwrap_or(&[])
        } else {
            self.items
        };
        for selected in state.path.iter().take(level) {
            items = selected
                .and_then(|index| items.get(index))
                .map(Item::children)
                .unwrap_or(&[]);
        }
        items
    }

    /// Opens `root` (ignored for context menus). A keyboard open selects the
    /// first enabled row.
    pub fn open(&self, state: &mut MenuState, root: usize, keyboard: bool) -> NavOutcome<Message> {
        let before = state.clone();
        let root = if self.bar { root } else { 0 };
        state.root = Some(root);
        state.path = vec![None];
        if keyboard {
            state.path[0] = next(self.panel(state, 0), None, true);
        }
        outcome(&before, state, None)
    }

    /// Closes the menu.
    pub fn close(&self, state: &mut MenuState) -> NavOutcome<Message> {
        let before = state.clone();
        close(state);
        outcome(&before, state, None)
    }

    /// Handles a key press while open: arrows, Home/End, Enter/Space,
    /// Escape and Tab. Other keys, and any key while closed, do nothing.
    pub fn key(&self, state: &mut MenuState, key: &keyboard::Key) -> NavOutcome<Message> {
        use keyboard::key::Named;
        let keyboard::Key::Named(key) = key else {
            return NavOutcome::None;
        };
        let (Some(root), Some(depth)) = (state.root, state.path.len().checked_sub(1)) else {
            return NavOutcome::None;
        };
        let before = state.clone();
        let mut activated = None;
        match key {
            Named::ArrowDown | Named::ArrowUp | Named::Home | Named::End => {
                let selected = if matches!(key, Named::Home | Named::End) {
                    None
                } else {
                    state.path[depth]
                };
                state.path[depth] = next(
                    self.panel(state, depth),
                    selected,
                    matches!(key, Named::ArrowDown | Named::Home),
                );
            }
            Named::Enter | Named::Space => {
                if self.bar
                    && depth == 0
                    && let Some(Item {
                        kind: Kind::Action(message),
                        enabled: true,
                        ..
                    }) = self.items.get(root)
                {
                    activated = Some(message.clone());
                    close(state);
                } else {
                    activated = self.activate(state);
                }
            }
            Named::ArrowRight => {
                let children = state.path[depth]
                    .and_then(|index| self.panel(state, depth).get(index))
                    .map(Item::children)
                    .unwrap_or(&[]);
                if !children.is_empty() {
                    state.path.push(next(children, None, true));
                } else if self.bar
                    && let Some(root) = next(self.items, Some(root), true)
                {
                    self.open(state, root, true);
                }
            }
            Named::ArrowLeft if depth > 0 => {
                state.path.pop();
            }
            Named::ArrowLeft if self.bar => {
                if let Some(root) = next(self.items, Some(root), false) {
                    self.open(state, root, true);
                }
            }
            Named::Escape if depth > 0 => {
                state.path.pop();
            }
            Named::Escape | Named::Tab => close(state),
            _ => {}
        }
        outcome(&before, state, activated)
    }

    /// The pointer is over `row` of panel `level`, or over a part of it with
    /// no row (`None`). Selects an enabled row and shows its submenu
    /// unselected; a disabled row or gap clears the selection there.
    pub fn hover(
        &self,
        state: &mut MenuState,
        level: usize,
        row: Option<usize>,
    ) -> NavOutcome<Message> {
        if !state.is_open() || level >= state.path.len() {
            return NavOutcome::None;
        }
        let before = state.clone();
        let panel = self.panel(state, level);
        match row.and_then(|index| panel.get(index).map(|item| (index, item))) {
            Some((index, item)) if item.selectable() => {
                if state.path[level] != Some(index) {
                    state.path.truncate(level + 1);
                    state.path[level] = Some(index);
                    if !item.children().is_empty() {
                        state.path.push(None);
                    }
                }
            }
            _ => {
                state.path.truncate(level + 1);
                state.path[level] = None;
            }
        }
        outcome(&before, state, None)
    }

    /// A press on `row` of panel `level`; `None` is a press outside every
    /// panel and closes the menu. An action activates; a submenu opens with
    /// its first enabled row selected; a disabled row does nothing but clear
    /// the selection.
    pub fn click(
        &self,
        state: &mut MenuState,
        level: usize,
        row: Option<usize>,
    ) -> NavOutcome<Message> {
        if !state.is_open() {
            return NavOutcome::None;
        }
        let Some(row) = row else {
            return self.close(state);
        };
        if level >= state.path.len() {
            return NavOutcome::None;
        }
        let before = state.clone();
        let selectable = self
            .panel(state, level)
            .get(row)
            .is_some_and(Item::selectable);
        state.path.truncate(level + 1);
        if selectable {
            state.path[level] = Some(row);
            let activated = self.activate(state);
            return outcome(&before, state, activated);
        }
        state.path[level] = None;
        outcome(&before, state, None)
    }

    /// The pointer moved onto bar entry `index`. While a menu is open this
    /// switches to that entry, as desktop menu bars do.
    pub fn hover_root(&self, state: &mut MenuState, index: usize) -> NavOutcome<Message> {
        if self.bar
            && state.is_open()
            && state.root != Some(index)
            && self.items.get(index).is_some_and(Item::selectable)
        {
            self.open(state, index, false)
        } else {
            NavOutcome::None
        }
    }

    /// A press on bar entry `index`: an action activates, a submenu opens
    /// (or closes if it is already open), a disabled entry closes any menu.
    pub fn click_root(&self, state: &mut MenuState, index: usize) -> NavOutcome<Message> {
        if !self.bar {
            return NavOutcome::None;
        }
        match self.items.get(index) {
            Some(item) if item.selectable() => {
                if let Kind::Action(message) = &item.kind {
                    close(state);
                    NavOutcome::Activated(message.clone())
                } else if state.root != Some(index) {
                    self.open(state, index, false)
                } else {
                    self.close(state)
                }
            }
            _ if state.is_open() => self.close(state),
            _ => NavOutcome::None,
        }
    }

    /// Closes a menu whose state no longer fits the items (after the app
    /// rebuilt them): a vanished or disabled root or selected row.
    pub fn validate(&self, state: &mut MenuState) -> NavOutcome<Message> {
        let Some(root) = state.root else {
            return NavOutcome::None;
        };
        let bad_root = self.bar && self.items.get(root).is_none_or(|item| !item.selectable());
        let bad_row = state.path.iter().enumerate().any(|(level, selected)| {
            selected.is_some_and(|index| {
                self.panel(state, level)
                    .get(index)
                    .is_none_or(|item| !item.selectable())
            })
        });
        if bad_root || bad_row {
            self.close(state)
        } else {
            NavOutcome::None
        }
    }

    // Activates the deepest selection: an action closes and returns its
    // message; a non-empty submenu opens with its first enabled row.
    fn activate(&self, state: &mut MenuState) -> Option<Message> {
        let depth = state.path.len().checked_sub(1)?;
        let index = state.path[depth]?;
        let item = self.panel(state, depth).get(index)?;
        if !item.selectable() {
            return None;
        }
        match &item.kind {
            Kind::Action(message) => {
                close(state);
                Some(message.clone())
            }
            Kind::Submenu(children) if !children.is_empty() => {
                state.path.push(next(children, None, true));
                None
            }
            _ => None,
        }
    }
}

fn close(state: &mut MenuState) {
    state.root = None;
    state.path.clear();
    state.anchors.clear();
}

fn outcome<Message>(
    before: &MenuState,
    after: &MenuState,
    activated: Option<Message>,
) -> NavOutcome<Message> {
    if let Some(message) = activated {
        NavOutcome::Activated(message)
    } else if before.is_open() && !after.is_open() {
        NavOutcome::Closed
    } else if before.root != after.root || before.path != after.path {
        NavOutcome::Changed
    } else {
        NavOutcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::key::Named;

    fn items() -> Vec<Item<u8>> {
        vec![
            Item::action("disabled", 0).enabled(false),
            Item::separator(),
            Item::action("one", 1),
            Item::submenu("more", vec![Item::separator(), Item::action("two", 2)]),
        ]
    }

    fn key(name: Named) -> keyboard::Key {
        keyboard::Key::Named(name)
    }

    #[test]
    fn navigation_skips_disabled_and_separators_and_wraps() {
        let items = items();
        let nav = Navigator::context(&items);
        let mut state = MenuState::default();
        assert_eq!(nav.open(&mut state, 5, true), NavOutcome::Changed);
        assert_eq!(state.root, Some(0));
        assert_eq!(state.path, [Some(2)]);
        assert_eq!(
            nav.key(&mut state, &key(Named::ArrowUp)),
            NavOutcome::Changed
        );
        assert_eq!(state.path, [Some(3)]);
        nav.key(&mut state, &key(Named::ArrowDown));
        assert_eq!(state.path, [Some(2)]);
        nav.key(&mut state, &key(Named::End));
        assert_eq!(state.path, [Some(3)]);
        nav.key(&mut state, &key(Named::Home));
        assert_eq!(state.path, [Some(2)]);
        // Non-named keys and closed menus are ignored.
        let letter = keyboard::Key::Character("a".into());
        assert_eq!(nav.key(&mut state, &letter), NavOutcome::None);
        let mut closed = MenuState::default();
        assert_eq!(nav.key(&mut closed, &key(Named::Enter)), NavOutcome::None);
    }

    #[test]
    fn bar_action_roots_activate_from_keyboard() {
        let items = vec![Item::action("run", 7)];
        let nav = Navigator::bar(&items);
        let mut state = MenuState::default();
        nav.open(&mut state, 0, true);
        assert_eq!(state.path, [None]);
        assert_eq!(
            nav.key(&mut state, &key(Named::Enter)),
            NavOutcome::Activated(7)
        );
        assert!(!state.is_open());
    }

    #[test]
    fn nested_navigation_and_activation_emit_only_action() {
        let items = items();
        let nav = Navigator::context(&items);
        let mut state = MenuState::default();
        nav.open(&mut state, 0, true);
        nav.key(&mut state, &key(Named::End));
        assert_eq!(
            nav.key(&mut state, &key(Named::ArrowRight)),
            NavOutcome::Changed
        );
        assert_eq!(state.path, [Some(3), Some(1)]);
        assert_eq!(nav.panel(&state, 1).len(), 2);
        nav.key(&mut state, &key(Named::ArrowLeft));
        assert_eq!(state.path, [Some(3)]);
        nav.key(&mut state, &key(Named::Enter));
        assert_eq!(
            nav.key(&mut state, &key(Named::Enter)),
            NavOutcome::Activated(2)
        );
        assert!(!state.is_open());
        assert!(state.path.is_empty());
    }

    #[test]
    fn empty_or_fully_disabled_menus_are_safe() {
        for items in [
            vec![],
            vec![
                Item::action("disabled", 1).enabled(false),
                Item::separator(),
            ],
        ] {
            let nav = Navigator::context(&items);
            let mut state = MenuState::default();
            nav.open(&mut state, 0, true);
            for name in [
                Named::Home,
                Named::End,
                Named::ArrowDown,
                Named::ArrowUp,
                Named::ArrowRight,
                Named::Enter,
            ] {
                assert_eq!(nav.key(&mut state, &key(name)), NavOutcome::None);
            }
            assert_eq!(state.path, [None]);
            assert_eq!(nav.key(&mut state, &key(Named::Escape)), NavOutcome::Closed);
            assert!(!state.is_open());
        }
        // An empty submenu is selectable but opens no panel.
        let items = vec![Item::submenu("empty", vec![]), Item::action("run", 1)];
        let nav = Navigator::context(&items);
        let mut state = MenuState::default();
        nav.open(&mut state, 0, true);
        assert_eq!(state.path, [Some(0)]);
        assert_eq!(nav.key(&mut state, &key(Named::Enter)), NavOutcome::None);
        assert_eq!(
            nav.key(&mut state, &key(Named::ArrowRight)),
            NavOutcome::None
        );
        assert_eq!(nav.hover(&mut state, 0, Some(0)), NavOutcome::None);
        assert_eq!(state.path, [Some(0)]);
    }

    #[test]
    fn bar_arrows_switch_roots_and_escape_unwinds() {
        let menus = vec![
            Item::submenu("first", items()),
            Item::submenu("disabled", items()).enabled(false),
            Item::submenu("last", items()),
        ];
        let nav = Navigator::bar(&menus);
        let mut state = MenuState::default();
        nav.open(&mut state, 0, true);
        nav.key(&mut state, &key(Named::ArrowLeft));
        assert_eq!(state.root, Some(2));
        nav.key(&mut state, &key(Named::End));
        nav.key(&mut state, &key(Named::ArrowRight));
        assert_eq!(state.path.len(), 2);
        nav.key(&mut state, &key(Named::Escape));
        assert_eq!(state.path.len(), 1);
        assert!(state.is_open());
        assert_eq!(nav.key(&mut state, &key(Named::Escape)), NavOutcome::Closed);
        // Tab closes from any depth.
        nav.open(&mut state, 0, true);
        nav.key(&mut state, &key(Named::End));
        nav.key(&mut state, &key(Named::ArrowRight));
        assert_eq!(nav.key(&mut state, &key(Named::Tab)), NavOutcome::Closed);
    }

    #[test]
    fn pointer_hover_and_click_follow_desktop_rules() {
        let menus = vec![
            Item::submenu("file", items()),
            Item::submenu("off", items()).enabled(false),
            Item::action("go", 9),
        ];
        let nav = Navigator::bar(&menus);
        let mut state = MenuState::default();
        // Closed: hovering a root does nothing; clicking opens it unselected.
        assert_eq!(nav.hover_root(&mut state, 0), NavOutcome::None);
        assert_eq!(nav.click_root(&mut state, 0), NavOutcome::Changed);
        assert_eq!(state.path, [None]);
        // Hovering a submenu row shows it unselected; the same row again is a no-op.
        assert_eq!(nav.hover(&mut state, 0, Some(3)), NavOutcome::Changed);
        assert_eq!(state.path, [Some(3), None]);
        assert_eq!(nav.hover(&mut state, 0, Some(3)), NavOutcome::None);
        assert_eq!(nav.hover(&mut state, 1, Some(1)), NavOutcome::Changed);
        assert_eq!(state.path, [Some(3), Some(1)]);
        // A disabled row or a gap clears that level and closes deeper panels.
        assert_eq!(nav.hover(&mut state, 0, Some(0)), NavOutcome::Changed);
        assert_eq!(state.path, [None]);
        assert_eq!(nav.hover(&mut state, 5, Some(0)), NavOutcome::None);
        // Clicking a disabled row keeps the menu open.
        assert_eq!(nav.click(&mut state, 0, Some(1)), NavOutcome::None);
        assert!(state.is_open());
        // Clicking a submenu row opens it with its first enabled row.
        assert_eq!(nav.click(&mut state, 0, Some(3)), NavOutcome::Changed);
        assert_eq!(state.path, [Some(3), Some(1)]);
        assert_eq!(nav.click(&mut state, 1, Some(1)), NavOutcome::Activated(2));
        assert!(!state.is_open());
        // Disabled roots never open; hovering switches between enabled roots.
        nav.click_root(&mut state, 0);
        assert_eq!(nav.hover_root(&mut state, 1), NavOutcome::None);
        assert_eq!(nav.hover_root(&mut state, 0), NavOutcome::None);
        // Clicking the open root again closes; a disabled root closes too.
        assert_eq!(nav.click_root(&mut state, 0), NavOutcome::Closed);
        nav.click_root(&mut state, 0);
        assert_eq!(nav.click_root(&mut state, 1), NavOutcome::Closed);
        // A top-level action activates whether or not a menu is open.
        assert_eq!(nav.click_root(&mut state, 2), NavOutcome::Activated(9));
        nav.click_root(&mut state, 0);
        assert_eq!(nav.click_root(&mut state, 2), NavOutcome::Activated(9));
        assert!(!state.is_open());
        // A press outside every panel closes.
        nav.click_root(&mut state, 0);
        assert_eq!(nav.click(&mut state, 0, None), NavOutcome::Closed);
        assert_eq!(nav.click(&mut state, 0, None), NavOutcome::None);
        // Context navigators ignore root entry calls.
        let context = Navigator::context(&menus);
        assert_eq!(context.click_root(&mut state, 0), NavOutcome::None);
    }

    #[test]
    fn validation_closes_menus_that_no_longer_fit() {
        let items = items();
        let nav = Navigator::context(&items);
        let mut state = MenuState {
            root: Some(0),
            path: vec![Some(3), Some(1)],
            anchors: vec![Rectangle::default(); 2],
        };
        assert_eq!(nav.validate(&mut state), NavOutcome::None);
        state.path[1] = Some(0);
        assert_eq!(nav.validate(&mut state), NavOutcome::Closed);
        assert!(state.anchors.is_empty());
        let shorter = vec![Item::action("only", 1)];
        let mut state = MenuState {
            root: Some(0),
            path: vec![Some(3)],
            anchors: Vec::new(),
        };
        assert_eq!(
            Navigator::context(&shorter).validate(&mut state),
            NavOutcome::Closed
        );
        let bar = vec![Item::submenu("off", items).enabled(false)];
        let mut state = MenuState {
            root: Some(0),
            path: vec![None],
            anchors: Vec::new(),
        };
        assert_eq!(
            Navigator::bar(&bar).validate(&mut state),
            NavOutcome::Closed
        );
    }
}
