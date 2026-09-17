//! Glue: the host's synchronous `Clipboard` over cosmix-wl-app's async
//! selections.
//!
//! iced reads the clipboard synchronously inside `update` (Ctrl+V), but a
//! Wayland selection arrives through a pipe. So reads are served from a
//! cache that is filled ahead of time: when another client sets a selection
//! (`SelectionChanged`/`SelectionLost`), the app requests its text and
//! stores the answer here; text the app sets itself goes straight in. A
//! paste in the same instant as a foreign copy can see the previous text.
//! Every change starts a new fetch; an answer to an older fetch is dropped,
//! so a slow or dead read never blocks the next one.
//! Writes are queued and applied by the app after `process`, with the
//! input serial of the event being processed.

use cosmix_iced_host::{Clipboard, ClipboardKind};
use cosmix_wl_app::{ReadStatus, Selection};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Default)]
pub struct Cache {
    pub standard: Option<String>,
    pub primary: Option<String>,
    pub writes: Vec<(Selection, String)>,
    /// Read tokens of refills still out, with the selection each is for.
    fetches: HashMap<u64, Selection>,
    /// The newest refill per selection; only its answer fills the cache.
    latest: HashMap<Selection, u64>,
}

/// What an answer to `request_selection` was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// The newest refill: the cache was updated.
    Filled,
    /// An older refill, overtaken by a newer one: ignore it.
    Stale,
    /// Not a refill (an explicit paste).
    NotAFetch,
}

impl Cache {
    pub fn set(&mut self, selection: Selection, text: Option<String>) {
        match selection {
            Selection::Clipboard => self.standard = text,
            Selection::Primary => self.primary = text,
        }
    }

    /// Record that `token` is a refill of `selection`, replacing any refill
    /// still out for it.
    pub fn start_fetch(&mut self, selection: Selection, token: u64) {
        self.fetches.insert(token, selection);
        self.latest.insert(selection, token);
    }

    /// Consume the answer for `token`.
    pub fn finish_fetch(&mut self, token: u64, text: Option<String>, status: ReadStatus) -> Fetch {
        let Some(selection) = self.fetches.remove(&token) else {
            return Fetch::NotAFetch;
        };
        if self.latest.get(&selection) != Some(&token) {
            return Fetch::Stale;
        }
        self.latest.remove(&selection);
        match status {
            ReadStatus::Complete | ReadStatus::Empty => self.set(selection, text),
            // A failed or superseded read says nothing about the current
            // text; keep what we have.
            _ => {}
        }
        Fetch::Filled
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
        cache.start_fetch(Selection::Clipboard, 1);
        assert_eq!(
            cache.finish_fetch(1, Some("x".into()), ReadStatus::Complete),
            Fetch::Filled
        );
        assert_eq!(cache.standard.as_deref(), Some("x"));
        assert_eq!(
            cache.finish_fetch(2, Some("paste".into()), ReadStatus::Complete),
            Fetch::NotAFetch
        );
        assert_eq!(cache.standard.as_deref(), Some("x"));
    }

    #[test]
    fn foreign_copy_after_own_is_not_frozen() {
        // We own the clipboard; another client copies. `cancelled` comes
        // first (SelectionLost -> fetch 1, which reads our own dead offer),
        // then `selection` (SelectionChanged -> fetch 2).
        let mut cache = Cache::default();
        cache.set(Selection::Clipboard, Some("ours".into()));
        cache.start_fetch(Selection::Clipboard, 1);
        cache.start_fetch(Selection::Clipboard, 2);
        assert_eq!(
            cache.finish_fetch(2, Some("theirs".into()), ReadStatus::Complete),
            Fetch::Filled
        );
        // The dead read ends late and must not overwrite the new text.
        assert_eq!(
            cache.finish_fetch(1, None, ReadStatus::Superseded),
            Fetch::Stale
        );
        assert_eq!(cache.standard.as_deref(), Some("theirs"));
        // A timed-out refill keeps the cache as it was, and the next change
        // is fetched regardless.
        cache.start_fetch(Selection::Clipboard, 3);
        assert_eq!(
            cache.finish_fetch(3, None, ReadStatus::TimedOut),
            Fetch::Filled
        );
        assert_eq!(cache.standard.as_deref(), Some("theirs"));
        cache.start_fetch(Selection::Clipboard, 4);
        assert_eq!(
            cache.finish_fetch(4, None, ReadStatus::Empty),
            Fetch::Filled
        );
        assert_eq!(
            cache.standard, None,
            "a cleared selection empties the cache"
        );
    }
}
