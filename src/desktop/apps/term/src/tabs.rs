use crate::terminal::{Terminal, Wake};
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
    terminal: Arc<Mutex<Terminal>>,
    pending: Arc<AtomicUsize>,
}
impl Drop for Removed {
    fn drop(&mut self) {
        self.terminal.lock().unwrap().shutdown();
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub terminal: Arc<Mutex<Terminal>>,
    cols: usize,
    rows: usize,
    child_pid: i32,
}

pub struct TabSet {
    settings: crate::config::Settings,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
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
        if self.tabs.len() + self.pending.load(Ordering::Acquire) >= MAX_TABS {
            return Err("tab limit (32) reached".into());
        }
        let terminal = std::panic::catch_unwind(start)
            .map_err(|_| "terminal startup panicked".to_string())??;
        if let Some(wake) = &self.wake {
            terminal.set_wake(wake.clone());
        }
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            title: "mix".into(),
            cols: 80,
            rows: 24,
            child_pid: terminal.pid,
            terminal: Arc::new(Mutex::new(terminal)),
        });
        self.active = self.tabs.len() - 1;
        self.notify();
        Ok(id)
    }

    pub fn close(&mut self, id: u64) -> (Outcome, Option<Removed>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return (Outcome::Unknown, None);
        };
        let tab = self.tabs.remove(index);
        self.pending.fetch_add(1, Ordering::AcqRel);
        let removed = Some(Removed {
            terminal: tab.terminal,
            pending: self.pending.clone(),
        });
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
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.terminal.clone())
    }
    pub fn list(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabInfo {
                id: tab.id,
                title: tab.title.clone(),
                active: index == self.active,
                cols: tab.cols,
                rows: tab.rows,
                child_pid: tab.child_pid,
            })
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    pub fn resized(&mut self, id: u64, cols: u16, rows: u16) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.cols = usize::from(cols);
            tab.rows = usize::from(rows);
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
            tab.terminal.lock().unwrap().set_wake(wake.clone());
        }
        self.wake = Some(wake);
    }
    fn notify(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }
    pub fn reap_exited(&mut self) -> Vec<Removed> {
        let ids: Vec<_> = self
            .tabs
            .iter()
            .filter(|tab| {
                tab.terminal
                    .lock()
                    .unwrap()
                    .listener
                    .quit
                    .load(Ordering::Acquire)
            })
            .map(|tab| tab.id)
            .collect();
        ids.into_iter().filter_map(|id| self.close(id).1).collect()
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
