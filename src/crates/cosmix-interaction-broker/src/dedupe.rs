//! Dedupe / coalesce map (notify.v1 §6).
//!
//! A repeat notify carrying the same `dedupe_key` **replaces** its predecessor
//! (mapping to the freedesktop `replaces_id`) rather than stacking a new toast.
//! The key is scoped **per origin** so two callers' identical keys never collide.

use cosmix_interaction_schema::NotifyHandle;
use std::collections::HashMap;

/// Tracks the live handle currently occupying each `(origin, dedupe_key)` slot.
#[derive(Debug, Clone, Default)]
pub struct DedupeTable {
    by_key: HashMap<(String, String), NotifyHandle>,
}

impl DedupeTable {
    pub fn new() -> Self {
        DedupeTable::default()
    }

    /// The handle a repeat notify with this key would replace, if one is live.
    pub fn lookup(&self, origin: &str, key: &str) -> Option<&NotifyHandle> {
        self.by_key.get(&(origin.to_string(), key.to_string()))
    }

    /// Record `handle` as the live occupant of `(origin, key)`, returning the
    /// prior occupant it displaced, if any.
    pub fn record(
        &mut self,
        origin: &str,
        key: &str,
        handle: NotifyHandle,
    ) -> Option<NotifyHandle> {
        self.by_key
            .insert((origin.to_string(), key.to_string()), handle)
    }

    /// Drop whatever slot maps to `handle` (call when it reaches a terminal
    /// state, so a future same-key notify starts fresh rather than replacing a
    /// dead entry). Returns `true` if a slot was removed.
    pub fn forget_handle(&mut self, handle: &NotifyHandle) -> bool {
        let before = self.by_key.len();
        self.by_key.retain(|_, h| h != handle);
        self.by_key.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> NotifyHandle {
        NotifyHandle(s.to_string())
    }

    #[test]
    fn record_then_lookup() {
        let mut t = DedupeTable::new();
        assert!(t.lookup("musicd", "render").is_none());
        assert_eq!(t.record("musicd", "render", h("h1")), None);
        assert_eq!(t.lookup("musicd", "render"), Some(&h("h1")));
    }

    #[test]
    fn record_displaces_prior() {
        let mut t = DedupeTable::new();
        t.record("musicd", "render", h("h1"));
        assert_eq!(t.record("musicd", "render", h("h2")), Some(h("h1")));
        assert_eq!(t.lookup("musicd", "render"), Some(&h("h2")));
    }

    #[test]
    fn keys_are_scoped_per_origin() {
        let mut t = DedupeTable::new();
        t.record("a", "k", h("ha"));
        t.record("b", "k", h("hb"));
        assert_eq!(t.lookup("a", "k"), Some(&h("ha")));
        assert_eq!(t.lookup("b", "k"), Some(&h("hb")));
    }

    #[test]
    fn forget_frees_the_slot() {
        let mut t = DedupeTable::new();
        t.record("musicd", "render", h("h1"));
        assert!(t.forget_handle(&h("h1")));
        assert!(t.lookup("musicd", "render").is_none());
        assert!(!t.forget_handle(&h("nope")));
    }
}
