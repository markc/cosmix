//! IME composition state for the editor (plan §4.2, codex #17), after the
//! ownership guard in `apps/term/src/ime.rs`: one editor view owns a
//! composition from its first preedit to its commit; a cancel (focus loss, or
//! a remote delta overlapping `EditorModel::composition`) keeps the input
//! method Disabled until the runtime acknowledges `Closed`, so a queued
//! preedit or commit can never land after the cancel.

#[derive(Debug, Default)]
pub struct Composition {
    open: bool,
    resetting: bool,
    /// The preedit text being drawn inline (empty when none).
    pub preedit: String,
    /// The model held a composition range for this preedit at least once, so
    /// its disappearance means a remote delta cancelled it.
    pub anchored: bool,
}

impl Composition {
    /// Whether the input method may be enabled.
    pub fn enabled(&self) -> bool {
        !self.resetting
    }

    pub fn opened(&mut self) {
        if !self.resetting {
            self.open = true;
        }
    }

    /// The runtime closed the input method (also the end of a reset).
    pub fn closed(&mut self) {
        *self = Self::default();
    }

    /// A preedit update; false when it must be ignored (reset in progress).
    pub fn preedit(&mut self, text: &str) -> bool {
        if self.resetting {
            return false;
        }
        self.open = true;
        self.preedit.clear();
        self.preedit.push_str(text);
        if text.is_empty() {
            self.anchored = false;
        }
        true
    }

    /// A commit; false when it must be dropped (reset in progress).
    pub fn commit(&mut self) -> bool {
        if self.resetting {
            return false;
        }
        self.preedit.clear();
        self.anchored = false;
        true
    }

    /// Cancel the composition: the preedit is discarded (the user retypes)
    /// and the input method stays Disabled until `Closed`.
    pub fn cancel(&mut self) {
        self.resetting |= self.open || !self.preedit.is_empty();
        self.open = false;
        self.preedit.clear();
        self.anchored = false;
    }

    pub fn active(&self) -> bool {
        !self.preedit.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_blocks_queued_preedits_and_commits_until_closed() {
        let mut c = Composition::default();
        c.opened();
        assert!(c.preedit("ni"));
        c.anchored = true;
        c.cancel();
        assert!(!c.enabled());
        assert!(!c.active());
        assert!(!c.preedit("nih"), "a queued preedit after the cancel is ignored");
        assert!(!c.commit(), "a queued commit after the cancel is dropped");
        c.opened();
        assert!(!c.enabled(), "a stale Opened does not end the reset");
        c.closed();
        assert!(c.enabled());
        assert!(c.preedit("x"));
        assert!(c.commit());
    }

    #[test]
    fn an_empty_preedit_ends_the_anchor() {
        let mut c = Composition::default();
        assert!(c.preedit("a"));
        c.anchored = true;
        assert!(c.preedit(""));
        assert!(!c.anchored);
        assert!(!c.active());
    }
}
