//! Menu bar model, navigation state machine and panel geometry for menus
//! shown as `xdg_popup`s. Pure: no Wayland, no iced.
//!
//! The state is a root (which bar title is open) plus one level per open
//! panel, each with its selected row. The host turns [`MenuNav::chain`] into
//! a stack of popups: panel 0 hangs off the bar title, panel k off the
//! selected submenu row of panel k-1.

use cosmix_wl_app::Rect;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry<A> {
    Action {
        label: String,
        accelerator: String,
        enabled: bool,
        action: A,
    },
    Submenu {
        label: String,
        children: Vec<Entry<A>>,
    },
    Separator,
}

impl<A> Entry<A> {
    pub fn action(label: &str, action: A) -> Self {
        Entry::Action {
            label: label.into(),
            accelerator: String::new(),
            enabled: true,
            action,
        }
    }

    pub fn submenu(label: &str, children: Vec<Entry<A>>) -> Self {
        Entry::Submenu {
            label: label.into(),
            children,
        }
    }

    pub fn accelerator(mut self, text: &str) -> Self {
        if let Entry::Action { accelerator, .. } = &mut self {
            *accelerator = text.into();
        }
        self
    }

    pub fn disabled(mut self) -> Self {
        if let Entry::Action { enabled, .. } = &mut self {
            *enabled = false;
        }
        self
    }

    pub fn selectable(&self) -> bool {
        match self {
            Entry::Action { enabled, .. } => *enabled,
            // A submenu with nothing to choose opens no panel.
            Entry::Submenu { children, .. } => children.iter().any(Entry::selectable),
            Entry::Separator => false,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Entry::Action { label, .. } | Entry::Submenu { label, .. } => label,
            Entry::Separator => "",
        }
    }
}

/// Logical metrics shared by the bar, the panels and the hit tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub bar_x: i32,
    pub bar_item_width: i32,
    pub bar_height: i32,
    pub panel_width: i32,
    pub row_height: i32,
    pub separator_height: i32,
    pub panel_padding: i32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            bar_x: 4,
            bar_item_width: 56,
            bar_height: 28,
            panel_width: 240,
            row_height: 28,
            separator_height: 9,
            panel_padding: 4,
        }
    }
}

impl Metrics {
    pub fn bar_item(&self, index: usize) -> Rect {
        Rect::new(
            self.bar_x + index as i32 * self.bar_item_width,
            0,
            self.bar_item_width,
            self.bar_height,
        )
    }

    pub fn bar_hit<A>(&self, bar: &[(String, Vec<Entry<A>>)], x: f64, y: f64) -> Option<usize> {
        (0..bar.len()).find(|i| self.bar_item(*i).contains(x, y))
    }

    fn entry_height<A>(&self, entry: &Entry<A>) -> i32 {
        match entry {
            Entry::Separator => self.separator_height,
            _ => self.row_height,
        }
    }

    pub fn panel_size<A>(&self, entries: &[Entry<A>]) -> (u32, u32) {
        let rows: i32 = entries.iter().map(|e| self.entry_height(e)).sum();
        (
            self.panel_width as u32,
            (rows + 2 * self.panel_padding).max(1) as u32,
        )
    }

    pub fn row_rect<A>(&self, entries: &[Entry<A>], index: usize) -> Rect {
        let y: i32 = entries[..index].iter().map(|e| self.entry_height(e)).sum();
        Rect::new(
            0,
            self.panel_padding + y,
            self.panel_width,
            self.entry_height(&entries[index]),
        )
    }

    pub fn row_hit<A>(&self, entries: &[Entry<A>], x: f64, y: f64) -> Option<usize> {
        (0..entries.len()).find(|i| self.row_rect(entries, *i).contains(x, y))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Enter,
    Escape,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome<A> {
    /// Nothing changed.
    None,
    /// Selection or the open panel chain changed.
    Changed,
    /// An action fired; the menus are closed.
    Activated(A),
    /// All menus closed.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Open {
    root: usize,
    selected: Vec<Option<usize>>,
}

/// The open state of a menu bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuNav {
    open: Option<Open>,
}

fn first<A>(entries: &[Entry<A>]) -> Option<usize> {
    entries.iter().position(Entry::selectable)
}

fn last<A>(entries: &[Entry<A>]) -> Option<usize> {
    entries.iter().rposition(Entry::selectable)
}

fn step<A>(entries: &[Entry<A>], from: Option<usize>, forward: bool) -> Option<usize> {
    let n = entries.len();
    if n == 0 {
        return None;
    }
    let start = match (from, forward) {
        (None, true) => n - 1,
        (None, false) => 0,
        (Some(i), _) => i,
    };
    (1..=n)
        .map(|k| {
            if forward {
                (start + k) % n
            } else {
                (start + n - k % n) % n
            }
        })
        .find(|i| entries[*i].selectable())
}

impl MenuNav {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn root(&self) -> Option<usize> {
        self.open.as_ref().map(|o| o.root)
    }

    /// Selected row of each open panel.
    pub fn selected(&self, level: usize) -> Option<usize> {
        self.open.as_ref()?.selected.get(level).copied().flatten()
    }

    pub fn depth(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.selected.len())
    }

    /// The open panels: for each, the submenu rows leading to it from the
    /// root panel. Equal prefixes mean the same popup.
    pub fn chain(&self) -> Vec<(usize, Vec<usize>)> {
        let Some(open) = &self.open else {
            return Vec::new();
        };
        let mut path = Vec::new();
        let mut out = vec![(open.root, path.clone())];
        for sel in &open.selected[..open.selected.len().saturating_sub(1)] {
            match sel {
                Some(i) => path.push(*i),
                None => break,
            }
            out.push((open.root, path.clone()));
        }
        out
    }

    /// Entries of panel `level`.
    pub fn panel<'a, A>(
        &self,
        bar: &'a [(String, Vec<Entry<A>>)],
        level: usize,
    ) -> Option<&'a [Entry<A>]> {
        let open = self.open.as_ref()?;
        let mut entries: &[Entry<A>] = &bar.get(open.root)?.1;
        for l in 0..level {
            match entries.get(open.selected.get(l).copied().flatten()?)? {
                Entry::Submenu { children, .. } => entries = children,
                _ => return None,
            }
        }
        Some(entries)
    }

    pub fn open<A>(&mut self, bar: &[(String, Vec<Entry<A>>)], root: usize, keyboard: bool) {
        let Some((_, entries)) = bar.get(root) else {
            return;
        };
        self.open = Some(Open {
            root,
            selected: vec![if keyboard { first(entries) } else { None }],
        });
    }

    pub fn close(&mut self) -> Outcome<()> {
        if self.open.take().is_some() {
            Outcome::Closed
        } else {
            Outcome::None
        }
    }

    /// Truncate to `levels` open panels (0 closes everything).
    pub fn truncate(&mut self, levels: usize) {
        if levels == 0 {
            self.open = None;
        } else if let Some(open) = &mut self.open {
            open.selected.truncate(levels);
        }
    }

    /// Pointer over bar title `root` while menus are open.
    pub fn hover_root<A>(&mut self, bar: &[(String, Vec<Entry<A>>)], root: usize) -> bool {
        if self.root().is_some_and(|r| r != root) {
            self.open(bar, root, false);
            true
        } else {
            false
        }
    }

    /// Pointer over row `row` of panel `level` (`None` = off the rows).
    /// Hovering a submenu opens it.
    pub fn hover<A>(
        &mut self,
        bar: &[(String, Vec<Entry<A>>)],
        level: usize,
        row: Option<usize>,
    ) -> bool {
        let Some(entries) = self.panel(bar, level) else {
            return false;
        };
        let row = row.filter(|r| entries.get(*r).is_some_and(Entry::selectable));
        let opens = row.is_some_and(|r| matches!(entries[r], Entry::Submenu { .. }));
        let Some(open) = &mut self.open else {
            return false;
        };
        if row.is_none() {
            // Leaving a panel keeps the path to an open child.
            return false;
        }
        let before = open.clone();
        open.selected.truncate(level + 1);
        open.selected[level] = row;
        if opens {
            open.selected.push(None);
        }
        *open != before
    }

    /// Pointer press on row `row` of panel `level`.
    pub fn click<A: Clone>(
        &mut self,
        bar: &[(String, Vec<Entry<A>>)],
        level: usize,
        row: usize,
    ) -> Outcome<A> {
        let changed = self.hover(bar, level, Some(row));
        match self.panel(bar, level).and_then(|e| e.get(row)) {
            Some(Entry::Action {
                enabled: true,
                action,
                ..
            }) => {
                let action = action.clone();
                self.open = None;
                Outcome::Activated(action)
            }
            _ if changed => Outcome::Changed,
            _ => Outcome::None,
        }
    }

    pub fn key<A: Clone>(&mut self, bar: &[(String, Vec<Entry<A>>)], key: NavKey) -> Outcome<A> {
        let Some(open) = self.open.clone() else {
            return Outcome::None;
        };
        let level = open.selected.len() - 1;
        let Some(entries) = self.panel(bar, level) else {
            self.open = None;
            return Outcome::Closed;
        };
        let current = open.selected[level];
        let selected_entry = current.and_then(|i| entries.get(i));
        let set = |nav: &mut Self, value: Option<usize>| {
            if let Some(o) = &mut nav.open
                && o.selected[level] != value
            {
                o.selected[level] = value;
                return Outcome::Changed;
            }
            Outcome::None
        };
        match key {
            NavKey::Down => set(self, step(entries, current, true)),
            NavKey::Up => set(self, step(entries, current, false)),
            NavKey::Home => set(self, first(entries)),
            NavKey::End => set(self, last(entries)),
            NavKey::Right | NavKey::Enter if matches!(selected_entry, Some(e @ Entry::Submenu { .. }) if e.selectable()) =>
            {
                let Some(Entry::Submenu { children, .. }) = selected_entry else {
                    return Outcome::None;
                };
                let child = first(children);
                if let Some(o) = &mut self.open {
                    o.selected.push(child);
                }
                Outcome::Changed
            }
            NavKey::Enter => match selected_entry {
                Some(Entry::Action {
                    enabled: true,
                    action,
                    ..
                }) => {
                    let action = action.clone();
                    self.open = None;
                    Outcome::Activated(action)
                }
                _ => Outcome::None,
            },
            NavKey::Right | NavKey::Left => {
                if key == NavKey::Left && level > 0 {
                    self.truncate(level);
                    return Outcome::Changed;
                }
                let n = bar.len();
                let root = if key == NavKey::Right {
                    (open.root + 1) % n
                } else {
                    (open.root + n - 1) % n
                };
                self.open(bar, root, true);
                Outcome::Changed
            }
            NavKey::Escape => {
                if level > 0 {
                    self.truncate(level);
                    Outcome::Changed
                } else {
                    self.open = None;
                    Outcome::Closed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Bar = Vec<(String, Vec<Entry<u8>>)>;

    fn bar() -> Bar {
        vec![
            (
                "File".into(),
                vec![
                    Entry::action("New", 1).accelerator("Ctrl+N"),
                    Entry::Separator,
                    Entry::action("Quit", 2),
                ],
            ),
            (
                "Edit".into(),
                vec![
                    Entry::action("Undo", 3).disabled(),
                    Entry::action("Copy", 4),
                    Entry::submenu(
                        "More",
                        vec![Entry::action("A", 5), Entry::action("B", 6).disabled()],
                    ),
                ],
            ),
            ("View".into(), vec![Entry::action("Zoom", 7)]),
        ]
    }

    #[test]
    fn keyboard_skips_disabled_and_separators() {
        let bar = bar();
        let mut nav = MenuNav::default();
        nav.open(&bar, 1, true);
        assert_eq!(nav.selected(0), Some(1));
        assert_eq!(nav.key(&bar, NavKey::Down), Outcome::Changed);
        assert_eq!(nav.selected(0), Some(2));
        nav.key(&bar, NavKey::Down);
        assert_eq!(nav.selected(0), Some(1), "wraps past disabled Undo");
        nav.key(&bar, NavKey::Up);
        assert_eq!(nav.selected(0), Some(2));
        nav.open(&bar, 0, true);
        nav.key(&bar, NavKey::Down);
        assert_eq!(nav.selected(0), Some(2), "separator skipped");
        assert_eq!(nav.key(&bar, NavKey::Enter), Outcome::Activated(2));
        assert!(!nav.is_open());
    }

    #[test]
    fn submenu_open_close_and_chain() {
        let bar = bar();
        let mut nav = MenuNav::default();
        nav.open(&bar, 1, true);
        nav.key(&bar, NavKey::End);
        assert_eq!(nav.chain(), vec![(1, vec![])]);
        assert_eq!(nav.key(&bar, NavKey::Right), Outcome::Changed);
        assert_eq!(nav.depth(), 2);
        assert_eq!(nav.selected(1), Some(0));
        assert_eq!(nav.chain(), vec![(1, vec![]), (1, vec![2])]);
        assert_eq!(nav.panel(&bar, 1).map(|p| p.len()), Some(2));
        // B is disabled: Down wraps back to A.
        assert_eq!(nav.key(&bar, NavKey::Down), Outcome::None);
        assert_eq!(nav.key(&bar, NavKey::Escape), Outcome::Changed);
        assert_eq!(nav.depth(), 1);
        nav.key(&bar, NavKey::Enter);
        assert_eq!(nav.depth(), 2);
        assert_eq!(nav.key(&bar, NavKey::Left), Outcome::Changed);
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.key(&bar, NavKey::Escape), Outcome::Closed);
        assert!(nav.chain().is_empty());
    }

    #[test]
    fn left_right_switch_roots_at_top_level() {
        let bar = bar();
        let mut nav = MenuNav::default();
        nav.open(&bar, 0, true);
        nav.key(&bar, NavKey::Left);
        assert_eq!(nav.root(), Some(2));
        nav.key(&bar, NavKey::Right);
        nav.key(&bar, NavKey::Right);
        assert_eq!(nav.root(), Some(1));
        assert_eq!(nav.selected(0), Some(1));
    }

    #[test]
    fn pointer_hover_and_click() {
        let bar = bar();
        let mut nav = MenuNav::default();
        nav.open(&bar, 1, false);
        assert_eq!(nav.selected(0), None);
        assert!(!nav.hover(&bar, 0, Some(0)), "disabled row does not select");
        assert!(nav.hover(&bar, 0, Some(2)));
        assert_eq!(nav.chain().len(), 2, "hovering a submenu opens it");
        assert!(!nav.hover(&bar, 0, None), "leaving keeps the child open");
        assert_eq!(nav.click(&bar, 1, 1), Outcome::None, "disabled B");
        assert_eq!(nav.click(&bar, 1, 0), Outcome::Activated(5));
        assert!(!nav.is_open());
        nav.open(&bar, 0, false);
        assert!(nav.hover_root(&bar, 2));
        assert_eq!(nav.root(), Some(2));
        assert!(!nav.hover_root(&bar, 2));
        assert_eq!(nav.close(), Outcome::Closed);
        assert!(!nav.hover_root(&bar, 1), "closed bar ignores hover");
    }

    #[test]
    fn empty_submenus_open_nothing() {
        let bar: Bar = vec![(
            "Go".into(),
            vec![
                Entry::submenu("Empty", vec![]),
                Entry::submenu(
                    "Dead",
                    vec![Entry::Separator, Entry::action("x", 1).disabled()],
                ),
                Entry::action("Ok", 2),
            ],
        )];
        let mut nav = MenuNav::default();
        nav.open(&bar, 0, false);
        assert!(!nav.hover(&bar, 0, Some(0)));
        assert!(!nav.hover(&bar, 0, Some(1)));
        assert_eq!(nav.depth(), 1, "no level pushed");
        assert_eq!(nav.click(&bar, 0, 1), Outcome::None);
        assert_eq!(nav.depth(), 1);
        nav.open(&bar, 0, true);
        assert_eq!(nav.selected(0), Some(2), "keyboard skips them too");
        assert_eq!(nav.key(&bar, NavKey::Right), Outcome::Changed);
        assert_eq!(
            nav.depth(),
            1,
            "Right switches roots, not into a dead submenu"
        );
    }

    #[test]
    fn geometry() {
        let bar = bar();
        let m = Metrics::default();
        assert_eq!(m.bar_item(1), Rect::new(60, 0, 56, 28));
        assert_eq!(m.bar_hit(&bar, 61.0, 5.0), Some(1));
        assert_eq!(m.bar_hit(&bar, 500.0, 5.0), None);
        let file = &bar[0].1;
        assert_eq!(m.panel_size(file), (240, 28 + 9 + 28 + 8));
        assert_eq!(m.row_rect(file, 2), Rect::new(0, 4 + 28 + 9, 240, 28));
        assert_eq!(m.row_hit(file, 10.0, 4.0 + 28.0 + 9.0 + 1.0), Some(2));
        assert_eq!(m.row_hit(file, 10.0, 1.0), None);
    }
}
