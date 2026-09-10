use crate::panes::{Direction, Geometry, Pane, PaneTree, SplitDir};
use crate::terminal::{Terminal, Wake};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const MAX_TABS: usize = 32;

#[derive(Clone)]
pub struct Cleanup(std::sync::mpsc::SyncSender<Vec<Removed>>);
impl Cleanup {
    pub fn start() -> std::io::Result<(Self, std::thread::JoinHandle<()>)> {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Removed>>(MAX_TABS);
        let worker = std::thread::Builder::new()
            .name("term-cleanup".into())
            .spawn(move || {
                for removed in receiver {
                    drop(removed);
                }
            })?;
        Ok((Self(sender), worker))
    }
    /// Called only after releasing the set lock. Admission bounds the total
    /// queued terminals, so the queue cannot fill with non-empty batches.
    pub fn submit(&self, removed: Vec<Removed>) {
        if !removed.is_empty() {
            let _ = self.0.send(removed);
        }
    }
}

/// Owns teardown after the caller releases the TabSet lock. Pending closes
/// retain their admission slot until bounded shutdown has completed.
pub struct Removed {
    terminals: Vec<Arc<Mutex<Terminal>>>,
    pending: Arc<AtomicUsize>,
}
impl Drop for Removed {
    fn drop(&mut self) {
        for terminal in &self.terminals {
            terminal.lock().unwrap().shutdown();
        }
        self.pending
            .fetch_sub(self.terminals.len(), Ordering::AcqRel);
    }
}

/// Identity of a pane whose shell child exited on its own — the payload of a
/// "task complete" desktop notification. Captured by [`TabSet::reap_exited`]
/// before the pane is closed, since [`Removed`] carries no identity. Only
/// spontaneous exits produce one; a `term.pane.close`/`term.tab.close` reaches
/// `close_pane`/`close` directly and never sets the `quit` flag this reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionNote {
    pub pane_id: u64,
    pub tab_title: String,
    pub child_pid: i32,
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub tree: PaneTree,
    pub active_pane: u64,
    pub revision: u64,
}

pub struct TabSet {
    settings: crate::config::Settings,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    next_pane_id: u64,
    pub revision: u64,
    metadata: HashMap<u64, PaneInfo>,
    wake: Option<Wake>,
    closing: bool,
    pending: Arc<AtomicUsize>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Unknown,
    Remaining(usize),
    Empty,
}

#[derive(Clone)]
pub struct PaneInfo {
    pub id: u64,
    pub active: bool,
    pub cols: usize,
    pub rows: usize,
    pub child_pid: i32,
    pub geometry: Geometry,
}

pub struct TabInfo {
    pub id: u64,
    pub title: String,
    pub active: bool,
    pub cols: usize,
    pub rows: usize,
    pub child_pid: i32,
}

impl TabSet {
    #[cfg(test)]
    pub fn new() -> Result<Self, String> {
        Self::with_settings(crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        })
    }

    pub fn with_settings(settings: crate::config::Settings) -> Result<Self, String> {
        let mut set = Self {
            settings,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            next_pane_id: 1,
            revision: 0,
            metadata: HashMap::new(),
            wake: None,
            closing: false,
            pending: Arc::new(AtomicUsize::new(0)),
        };
        set.open()?;
        Ok(set)
    }

    pub fn open(&mut self) -> Result<u64, String> {
        let settings = self.settings;
        self.open_with(move || Terminal::start(settings))
    }

    fn open_with(
        &mut self,
        start: impl FnOnce() -> Result<Terminal, String> + std::panic::UnwindSafe,
    ) -> Result<u64, String> {
        if self.closing {
            return Err("application closing".into());
        }
        // VERIFY: cap-spans-panes — pending shutdowns retain their terminal slots.
        if self.metadata.len() + self.pending.load(Ordering::Acquire) >= MAX_TABS {
            return Err("tab limit (32) reached".into());
        }
        let pane = self.start_pane(start)?;
        let active_pane = pane.id;
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            title: "mix".into(),
            tree: PaneTree::Leaf(pane),
            active_pane,
            revision: self.revision + 1,
        });
        self.revision += 1;
        self.active = self.tabs.len() - 1;
        self.notify();
        Ok(id)
    }

    fn start_pane(
        &mut self,
        start: impl FnOnce() -> Result<Terminal, String> + std::panic::UnwindSafe,
    ) -> Result<Pane, String> {
        if self.closing {
            return Err("application closing".into());
        }
        if self.metadata.len() + self.pending.load(Ordering::Acquire) >= MAX_TABS {
            return Err("tab limit (32) reached".into());
        }
        let terminal = std::panic::catch_unwind(start)
            .map_err(|_| "terminal startup panicked".to_string())??;
        if let Some(wake) = &self.wake {
            terminal.set_wake(wake.clone());
        }
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.metadata.insert(
            id,
            PaneInfo {
                id,
                active: false,
                cols: 80,
                rows: 24,
                child_pid: terminal.pid,
                geometry: Geometry::default(),
            },
        );
        Ok(Pane {
            id,
            terminal: Arc::new(Mutex::new(terminal)),
        })
    }
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    pub fn active_pane_terminal(&self) -> Arc<Mutex<Terminal>> {
        let tab = self.active_tab();
        tab.tree
            .pane_by_id(tab.active_pane)
            .unwrap()
            .terminal
            .clone()
    }
    pub fn pane_by_id(&self, id: u64) -> Option<Arc<Mutex<Terminal>>> {
        self.tabs
            .iter()
            .find_map(|tab| tab.tree.pane_by_id(id))
            .map(|pane| pane.terminal.clone())
    }
    pub fn leaves(&self) -> Vec<PaneInfo> {
        if self.is_empty() {
            return Vec::new();
        }
        let tab = self.active_tab();
        tab.tree
            .leaves(Geometry {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            })
            .into_iter()
            .map(|(pane, _)| {
                let mut info = self.metadata[&pane.id].clone();
                info.active = pane.id == tab.active_pane;
                info
            })
            .collect()
    }
    pub fn geometry(&mut self, id: u64, geometry: Geometry) {
        if let Some(info) = self.metadata.get_mut(&id) {
            info.geometry = geometry;
        }
    }
    pub fn split_active(&mut self, dir: SplitDir) -> Result<u64, String> {
        let settings = self.settings;
        let pane = self.start_pane(move || Terminal::start(settings))?;
        let id = pane.id;
        let tab = &mut self.tabs[self.active];
        tab.tree.split(tab.active_pane, dir, pane);
        tab.active_pane = id;
        self.revision += 1;
        tab.revision = self.revision;
        self.invalidate_geometry(self.active);
        self.notify();
        Ok(id)
    }
    pub fn focus(&mut self, id: u64) -> bool {
        if self.is_empty() || self.active_tab().tree.pane_by_id(id).is_none() {
            return false;
        }
        self.tabs[self.active].active_pane = id;
        self.notify();
        true
    }
    fn invalidate_geometry(&mut self, index: usize) {
        for (pane, _) in self.tabs[index].tree.leaves(Geometry::default()) {
            self.metadata.get_mut(&pane.id).unwrap().geometry = Geometry::default();
        }
    }
    pub fn focus_dir(&mut self, dir: Direction) -> bool {
        if self.is_empty() {
            return false;
        }
        let mut leaves: Vec<_> = self
            .leaves()
            .iter()
            .map(|pane| (pane.id, pane.geometry))
            .collect();
        if leaves
            .iter()
            .any(|(_, geometry)| geometry.w == 0.0 || geometry.h == 0.0)
        {
            leaves = self
                .active_tab()
                .tree
                .leaves(Geometry {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                })
                .into_iter()
                .map(|(pane, geometry)| (pane.id, geometry))
                .collect();
        }
        crate::panes::neighbour(self.active_tab().active_pane, dir, &leaves)
            .is_some_and(|id| self.focus(id))
    }
    pub fn close_active(&mut self) -> (Outcome, Option<Removed>) {
        if self.is_empty() {
            return (Outcome::Unknown, None);
        }
        self.close_pane(self.active_tab().active_pane)
    }
    fn close_pane(&mut self, id: u64) -> (Outcome, Option<Removed>) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.tree.pane_by_id(id).is_some())
        else {
            return (Outcome::Unknown, None);
        };
        let tab = &mut self.tabs[index];
        let terminal = tab.tree.pane_by_id(id).unwrap().terminal.clone();
        let sibling = tab.tree.sibling_focus(id);
        let Some(tree) = tab.tree.clone().without(id) else {
            return self.close(self.tabs[index].id);
        };
        tab.tree = tree;
        if tab.active_pane == id {
            tab.active_pane = sibling.expect("non-final pane has a sibling");
        }
        let count = tab.tree.leaves(Geometry::default()).len();
        self.metadata.remove(&id);
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.revision += 1;
        tab.revision = self.revision;
        self.invalidate_geometry(index);
        self.notify();
        (
            Outcome::Remaining(count),
            Some(Removed {
                terminals: vec![terminal],
                pending: self.pending.clone(),
            }),
        )
    }

    pub fn close(&mut self, id: u64) -> (Outcome, Option<Removed>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return (Outcome::Unknown, None);
        };
        let tab = self.tabs.remove(index);
        let ids: Vec<_> = tab
            .tree
            .leaves(Geometry::default())
            .iter()
            .map(|(pane, _)| pane.id)
            .collect();
        let terminals = ids
            .iter()
            .map(|id| tab.tree.pane_by_id(*id).unwrap().terminal.clone())
            .collect();
        for id in &ids {
            self.metadata.remove(id);
        }
        self.pending.fetch_add(ids.len(), Ordering::AcqRel);
        let removed = Some(Removed {
            terminals,
            pending: self.pending.clone(),
        });
        self.revision += 1;
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            self.closing = true;
            self.notify();
            return (Outcome::Empty, removed);
        }
        self.notify();
        (Outcome::Remaining(self.tabs.len()), removed)
    }

    pub fn select(&mut self, id: u64) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        self.active = index;
        self.notify();
        true
    }

    // Active accessors require a non-empty set; callers check is_empty under
    // the same set lock, so Bus close cannot invalidate their selection.
    pub fn active_terminal(&self) -> Arc<Mutex<Terminal>> {
        self.by_id(self.active_id()).expect("active tab")
    }
    pub fn active_id(&self) -> u64 {
        self.tabs[self.active].id
    }
    pub fn by_id(&self, id: u64) -> Option<Arc<Mutex<Terminal>>> {
        self.tabs.iter().find(|tab| tab.id == id).map(|tab| {
            tab.tree
                .pane_by_id(tab.active_pane)
                .unwrap()
                .terminal
                .clone()
        })
    }
    pub fn list(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabInfo {
                id: tab.id,
                title: tab.title.clone(),
                active: index == self.active,
                cols: self.metadata[&tab.active_pane].cols,
                rows: self.metadata[&tab.active_pane].rows,
                child_pid: self.metadata[&tab.active_pane].child_pid,
            })
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    pub fn resized(&mut self, id: u64, cols: u16, rows: u16) {
        if let Some(info) = self.metadata.get_mut(&id) {
            info.cols = usize::from(cols);
            info.rows = usize::from(rows);
        }
    }
    pub fn cycle(&mut self, forward: bool) {
        if self.is_empty() {
            return;
        }
        let offset = if forward { 1 } else { self.tabs.len() - 1 };
        self.active = (self.active + offset) % self.tabs.len();
        self.notify();
    }
    pub fn set_wake(&mut self, wake: Wake) {
        for tab in &self.tabs {
            for (pane, _) in tab.tree.leaves(Geometry::default()) {
                pane.terminal.lock().unwrap().set_wake(wake.clone());
            }
        }
        self.wake = Some(wake);
    }
    fn notify(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }
    pub fn reap_exited(&mut self) -> (Vec<Removed>, Vec<CompletionNote>) {
        // One pass captures the identity of every self-exited pane before any
        // close mutates the tree; a second closes them. Notes are gathered
        // here, not derived from `Removed` (which is identity-free), and only
        // for panes whose child set `quit` — i.e. spontaneous shell exits.
        let mut ids = Vec::new();
        let mut notes = Vec::new();
        for tab in &self.tabs {
            for (pane, _) in tab.tree.leaves(Geometry::default()) {
                let terminal = pane.terminal.lock().unwrap();
                if terminal.listener.quit.load(Ordering::Acquire) {
                    ids.push(pane.id);
                    notes.push(CompletionNote {
                        pane_id: pane.id,
                        tab_title: tab.title.clone(),
                        child_pid: terminal.pid,
                    });
                }
            }
        }
        let removed = ids
            .into_iter()
            .filter_map(|id| self.close_pane(id).1)
            .collect();
        (removed, notes)
    }

    pub fn shutdown(&mut self) -> Vec<Removed> {
        self.closing = true;
        let mut removed = Vec::new();
        while let Some(tab) = self.tabs.last() {
            removed.extend(self.close(tab.id).1);
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_close_and_last_pane_outcomes() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let first_tab = tabs.active_id();
        let first_pane = tabs.active_tab().active_pane;
        let original = tabs.active_pane_terminal();
        let second = tabs.split_active(SplitDir::Vertical).unwrap();
        assert_eq!(tabs.leaves().len(), 2);
        assert_eq!(tabs.active_tab().active_pane, second);
        assert!(Arc::ptr_eq(
            &tabs.pane_by_id(first_pane).unwrap(),
            &original
        ));
        assert!(!Arc::ptr_eq(&tabs.active_pane_terminal(), &original));
        let third = tabs.split_active(SplitDir::Horizontal).unwrap();
        assert_eq!(
            tabs.leaves().iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![first_pane, second, third]
        );
        assert_eq!(tabs.close_active().0, Outcome::Remaining(2));
        assert_eq!(tabs.active_tab().active_pane, second);
        assert!(tabs.pane_by_id(third).is_none());
        assert!(matches!(tabs.active_tab().tree, PaneTree::Split { .. }));
        tabs.focus(second);
        assert_eq!(tabs.close_active().0, Outcome::Remaining(1));
        assert!(matches!(tabs.active_tab().tree, PaneTree::Leaf(_)));
        assert_eq!(tabs.active_tab().active_pane, first_pane);
        tabs.open().unwrap();
        assert_eq!(tabs.close_active().0, Outcome::Remaining(1));
        assert_eq!(tabs.active_id(), first_tab);
        assert_eq!(tabs.close_active().0, Outcome::Empty);
    }
    #[test]
    fn cap_spans_tabs_panes_and_pending_cleanup() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        for _ in 0..15 {
            tabs.split_active(SplitDir::Vertical).unwrap();
        }
        for _ in 0..16 {
            tabs.open().unwrap();
        }
        assert!(tabs.open().is_err());
        assert!(tabs.split_active(SplitDir::Horizontal).is_err());
        let (_, removed) = tabs.close_active();
        assert!(tabs.split_active(SplitDir::Vertical).is_err());
        drop(removed);
        assert!(tabs.split_active(SplitDir::Vertical).is_ok());
        let ids: std::collections::HashSet<_> = tabs
            .tabs
            .iter()
            .flat_map(|tab| tab.tree.leaves(Geometry::default()))
            .map(|(pane, _)| pane.id)
            .collect();
        assert_eq!(ids.len(), 32);
        drop(tabs.shutdown());
    }
    #[test]
    fn directional_focus_uses_geometry_and_rejects_other_tabs() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let left = tabs.active_tab().active_pane;
        let top = tabs.split_active(SplitDir::Vertical).unwrap();
        let bottom = tabs.split_active(SplitDir::Horizontal).unwrap();
        tabs.geometry(
            left,
            Geometry {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 600.0,
            },
        );
        tabs.geometry(
            top,
            Geometry {
                x: 403.0,
                y: 0.0,
                w: 400.0,
                h: 298.0,
            },
        );
        tabs.geometry(
            bottom,
            Geometry {
                x: 403.0,
                y: 301.0,
                w: 400.0,
                h: 299.0,
            },
        );
        assert!(tabs.focus_dir(Direction::Up));
        assert_eq!(tabs.active_tab().active_pane, top);
        assert!(tabs.focus_dir(Direction::Down));
        assert_eq!(tabs.active_tab().active_pane, bottom);
        assert!(tabs.focus_dir(Direction::Left));
        assert_eq!(tabs.active_tab().active_pane, left);
        assert!(!tabs.focus_dir(Direction::Left));
        assert!(tabs.focus_dir(Direction::Right));
        assert_eq!(tabs.active_tab().active_pane, bottom);
        let first = tabs.active_id();
        tabs.open().unwrap();
        let other = tabs.active_tab().active_pane;
        assert!(!tabs.focus(left));
        tabs.select(first);
        assert!(!tabs.focus(other));
        assert!(!tabs.focus(u64::MAX));
        drop(tabs.shutdown());
    }
    #[test]
    fn close_then_focus_discards_old_rectangles_without_refresh() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let left = tabs.active_tab().active_pane;
        let top = tabs.split_active(SplitDir::Vertical).unwrap();
        let bottom = tabs.split_active(SplitDir::Horizontal).unwrap();
        for (id, x, y, w, h) in [
            (left, 0.0, 0.0, 400.0, 600.0),
            (top, 403.0, 0.0, 400.0, 298.0),
            (bottom, 403.0, 301.0, 400.0, 299.0),
        ] {
            tabs.geometry(id, Geometry { x, y, w, h });
        }
        drop(tabs.close_active().1);
        assert_eq!(tabs.active_tab().active_pane, top);
        assert!(tabs.leaves().iter().all(|pane| pane.geometry.w == 0.0));
        // The old top rectangle would make Down jump sideways to the left.
        assert!(!tabs.focus_dir(Direction::Down));
        assert_eq!(tabs.active_tab().active_pane, top);
        assert!(tabs.focus_dir(Direction::Left));
        assert_eq!(tabs.active_tab().active_pane, left);
        assert!(!tabs.focus_dir(Direction::Up));
        assert!(tabs.focus_dir(Direction::Right));
        assert_eq!(tabs.active_tab().active_pane, top);
        // A split also invalidates previously measured surviving leaves.
        tabs.geometry(
            left,
            Geometry {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 600.0,
            },
        );
        tabs.split_active(SplitDir::Horizontal).unwrap();
        assert!(tabs.leaves().iter().all(|pane| pane.geometry.w == 0.0));
        drop(tabs.shutdown());
    }
    #[test]
    fn exited_pane_preserves_live_sibling() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let original = tabs.active_tab().active_pane;
        let pane = tabs.split_active(SplitDir::Vertical).unwrap();
        tabs.pane_by_id(pane)
            .unwrap()
            .lock()
            .unwrap()
            .listener
            .quit
            .store(true, Ordering::Release);
        let (removed, notes) = tabs.reap_exited();
        assert_eq!(removed.len(), 1);
        // The self-exited pane yields exactly one identity-bearing note.
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pane_id, pane);
        assert_eq!(tabs.active_tab().active_pane, original);
        assert!(matches!(tabs.active_tab().tree, PaneTree::Leaf(_)));
        drop(removed);
        drop(tabs.shutdown());
    }
    fn fixture() -> Option<TabSet> {
        if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
            eprintln!("SKIP tab PTY test: /opt/cosmix/bin/mix unavailable");
            return None;
        }
        Some(TabSet::new().expect("real Mix PTY"))
    }
    #[test]
    fn cap_includes_pending_close_and_recovers() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        for _ in 1..MAX_TABS {
            tabs.open().unwrap();
        }
        assert_eq!(tabs.open(), Err("tab limit (32) reached".into()));
        let id = tabs.active_id();
        let (_, removed) = tabs.close(id);
        assert_eq!(tabs.open(), Err("tab limit (32) reached".into()));
        drop(removed);
        assert!(tabs.open().is_ok());
        drop(tabs.shutdown());
    }
    #[test]
    fn startup_panic_does_not_poison_set() {
        let Some(tabs) = fixture() else {
            return;
        };
        let set = Mutex::new(tabs);
        {
            let mut tabs = set.lock().unwrap();
            assert_eq!(
                tabs.open_with(|| panic!("injected spawn failure")),
                Err("terminal startup panicked".into())
            );
        }
        assert_eq!(set.lock().unwrap().list().len(), 1);
    }
    #[test]
    fn metadata_does_not_lock_terminal_and_tracks_resize() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let id = tabs.active_id();
        let terminal = tabs.active_terminal();
        let guard = terminal.lock().unwrap();
        tabs.resized(id, 100, 30);
        let info = tabs.list();
        assert_eq!((info[0].cols, info[0].rows), (100, 30));
        assert_eq!(info[0].child_pid, guard.pid);
    }
    #[test]
    fn close_releases_set_before_teardown() {
        let Some(tabs) = fixture() else {
            return;
        };
        let set = Mutex::new(tabs);
        let terminal = set.lock().unwrap().active_terminal();
        let terminal_guard = terminal.lock().unwrap();
        let removed = {
            let mut tabs = set.lock().unwrap();
            let id = tabs.active_id();
            let (outcome, removed) = tabs.close(id);
            assert_eq!(outcome, Outcome::Empty);
            removed
        };
        assert!(set.try_lock().unwrap().is_empty());
        drop(terminal_guard);
        drop(removed);
    }
    #[test]
    fn cleanup_queue_returns_without_waiting_for_terminal_lock() {
        let Some(tabs) = fixture() else {
            return;
        };
        let set = Mutex::new(tabs);
        let (cleanup, worker) = Cleanup::start().unwrap();
        let terminal = set.lock().unwrap().active_terminal();
        let held = terminal.lock().unwrap();
        let removed = set.lock().unwrap().shutdown();
        cleanup.submit(removed);
        assert!(set.try_lock().unwrap().is_empty());
        assert_eq!(set.lock().unwrap().pending.load(Ordering::Acquire), 1);
        drop(held);
        drop(cleanup);
        worker.join().unwrap();
        assert_eq!(set.lock().unwrap().pending.load(Ordering::Acquire), 0);
    }
    #[test]
    fn open_list_and_select() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let first = tabs.active_id();
        let second = tabs.open().unwrap();
        let list = tabs.list();
        assert_eq!(list.len(), 2);
        assert!(!list[0].active && list[1].active);
        assert_eq!(list[1].id, second);
        assert_eq!(list[1].title, "mix");
        assert!(tabs.select(first));
        assert_eq!(tabs.active_id(), first);
        assert!(!tabs.select(999));
        assert_eq!(tabs.active_id(), first);
    }
    #[test]
    fn close_non_active_preserves_selection() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let first = tabs.active_id();
        let second = tabs.open().unwrap();
        assert_eq!(tabs.close(first).0, Outcome::Remaining(1));
        assert_eq!(tabs.active_id(), second);
        assert!(tabs.by_id(first).is_none());
        assert!(tabs.list()[0].active);
        assert_eq!(tabs.close(first).0, Outcome::Unknown);
    }
    #[test]
    fn close_active_selects_neighbour_and_last_is_empty() {
        let Some(mut tabs) = fixture() else {
            return;
        };
        let first = tabs.active_id();
        let middle = tabs.open().unwrap();
        let last = tabs.open().unwrap();
        tabs.select(middle);
        assert_eq!(tabs.close(middle).0, Outcome::Remaining(2));
        assert_eq!(tabs.active_id(), last);
        assert_eq!(tabs.close(last).0, Outcome::Remaining(1));
        assert_eq!(tabs.active_id(), first);
        assert_eq!(tabs.close(first).0, Outcome::Empty);
        assert!(tabs.is_empty());
        assert!(tabs.open().is_err());
    }
}
