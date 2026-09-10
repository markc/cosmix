use crate::terminal::{Terminal, Wake};
use std::sync::{Arc, Mutex, atomic::Ordering};

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub terminal: Arc<Mutex<Terminal>>,
}

pub struct TabSet {
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    wake: Option<Wake>,
    closing: bool,
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
    pub fn new() -> Result<Self, String> {
        let mut set = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            wake: None,
            closing: false,
        };
        set.open()?;
        Ok(set)
    }

    pub fn open(&mut self) -> Result<u64, String> {
        if self.closing {
            return Err("application closing".into());
        }
        let terminal = Terminal::start()?;
        if let Some(wake) = &self.wake {
            terminal.set_wake(wake.clone());
        }
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            title: "mix".into(),
            terminal: Arc::new(Mutex::new(terminal)),
        });
        self.active = self.tabs.len() - 1;
        self.notify();
        Ok(id)
    }

    pub fn close(&mut self, id: u64) -> Outcome {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Outcome::Unknown;
        };
        let tab = self.tabs.remove(index);
        tab.terminal.lock().unwrap().shutdown();
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            self.closing = true;
            self.notify();
            return Outcome::Empty;
        }
        self.notify();
        Outcome::Remaining(self.tabs.len())
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
            .map(|(index, tab)| {
                let terminal = tab.terminal.lock().unwrap();
                let screen = terminal.screen(false);
                TabInfo {
                    id: tab.id,
                    title: tab.title.clone(),
                    active: index == self.active,
                    cols: screen.cols,
                    rows: screen.rows,
                    child_pid: terminal.pid,
                }
            })
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
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
    pub fn reap_exited(&mut self) {
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
        for id in ids {
            self.close(id);
        }
    }
    pub fn shutdown(&mut self) {
        self.closing = true;
        while let Some(tab) = self.tabs.last() {
            self.close(tab.id);
        }
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
        assert_eq!(tabs.close(first), Outcome::Remaining(1));
        assert_eq!(tabs.active_id(), second);
        assert!(tabs.by_id(first).is_none());
        assert!(tabs.list()[0].active);
        assert_eq!(tabs.close(first), Outcome::Unknown);
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
        assert_eq!(tabs.close(middle), Outcome::Remaining(2));
        assert_eq!(tabs.active_id(), last);
        assert_eq!(tabs.close(last), Outcome::Remaining(1));
        assert_eq!(tabs.active_id(), first);
        assert_eq!(tabs.close(first), Outcome::Empty);
        assert!(tabs.is_empty());
        assert!(tabs.open().is_err());
    }
}
