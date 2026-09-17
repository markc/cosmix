//! Glue: the host's synchronous `Clipboard` over cosmix-wl-app's async
//! selections.
//!
//! iced reads the clipboard synchronously inside `update` (Ctrl+V), but a
//! Wayland selection arrives through a pipe. So reads are served from a
//! cache that is filled ahead of time: when another client sets a selection
//! (`SelectionChanged`/`SelectionLost`), the app requests its text and
//! stores the answer here; text the app sets itself goes straight in. A
//! paste in the same instant as a foreign copy can see the previous text.
//! Writes are queued and applied by the app after `process`, with the
//! input serial of the event being processed.

use cosmix_iced_host::{Clipboard, ClipboardKind};
use cosmix_wl_app::Selection;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Default)]
pub struct Cache {
    pub standard: Option<String>,
    pub primary: Option<String>,
    pub writes: Vec<(Selection, String)>,
    /// Selections whose text was requested to refill the cache.
    pub pending: Vec<Selection>,
}

impl Cache {
    pub fn set(&mut self, selection: Selection, text: Option<String>) {
        match selection {
            Selection::Clipboard => self.standard = text,
            Selection::Primary => self.primary = text,
        }
    }

    /// Record that `selection` is being fetched. False if already pending.
    pub fn start_fetch(&mut self, selection: Selection) -> bool {
        if self.pending.contains(&selection) {
            return false;
        }
        self.pending.push(selection);
        true
    }

    /// Consume a fetch answer. False when nobody asked for a refill (the
    /// answer belongs to an explicit paste).
    pub fn finish_fetch(&mut self, selection: Selection, text: Option<String>) -> bool {
        let Some(i) = self.pending.iter().position(|s| *s == selection) else {
            return false;
        };
        self.pending.remove(i);
        self.set(selection, text);
        true
    }
}

#[derive(Debug, Clone, Default)]
pub struct Shared(pub Rc<RefCell<Cache>>);

fn selection(kind: ClipboardKind) -> Selection {
    match kind {
        ClipboardKind::Standard => Selection::Clipboard,
        ClipboardKind::Primary => Selection::Primary,
    }
}

impl Clipboard for Shared {
    fn read(&self, kind: ClipboardKind) -> Option<String> {
        let cache = self.0.borrow();
        match kind {
            ClipboardKind::Standard => cache.standard.clone(),
            ClipboardKind::Primary => cache.primary.clone(),
        }
    }

    fn write(&mut self, kind: ClipboardKind, contents: String) {
        let mut cache = self.0.borrow_mut();
        let sel = selection(kind);
        cache.set(sel, Some(contents.clone()));
        cache.writes.push((sel, contents));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_cached_and_queued() {
        let shared = Shared::default();
        let mut c = shared.clone();
        c.write(ClipboardKind::Primary, "sel".into());
        assert_eq!(c.read(ClipboardKind::Primary).as_deref(), Some("sel"));
        assert_eq!(c.read(ClipboardKind::Standard), None);
        let writes = std::mem::take(&mut shared.0.borrow_mut().writes);
        assert_eq!(writes, vec![(Selection::Primary, "sel".to_string())]);
    }

    #[test]
    fn fetches_fill_cache_and_pastes_pass_through() {
        let mut cache = Cache::default();
        assert!(cache.start_fetch(Selection::Clipboard));
        assert!(!cache.start_fetch(Selection::Clipboard));
        assert!(cache.finish_fetch(Selection::Clipboard, Some("x".into())));
        assert_eq!(cache.standard.as_deref(), Some("x"));
        assert!(!cache.finish_fetch(Selection::Clipboard, Some("paste".into())));
        assert_eq!(cache.standard.as_deref(), Some("x"));
    }
}
