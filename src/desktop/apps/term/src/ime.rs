//! Composition belongs to a pane, including while its preedit is empty.
#[derive(Default)]
pub(super) struct Composition {
    owner: Option<u64>,
    open: bool,
    resetting: bool,
}

impl Composition {
    pub(super) fn has_owner(&self) -> bool {
        self.owner.is_some()
    }

    pub(super) fn enabled(&self) -> bool {
        !self.resetting
    }

    pub(super) fn opened(&mut self) {
        if !self.resetting {
            self.open = true;
        }
    }

    pub(super) fn closed(&mut self) {
        *self = Self::default();
    }

    pub(super) fn preedit(&mut self, active: Option<u64>) -> bool {
        if self.focus(active) {
            return false;
        }
        if !self.open || self.resetting || active.is_none() {
            return false;
        }
        if self.owner.is_none() {
            self.owner = active;
        }
        self.owner == active
    }

    pub(super) fn commit(&mut self, active: Option<u64>) -> bool {
        if self.focus(active) {
            return false;
        }
        let accepted = self.open
            && !self.resetting
            && active.is_some()
            && self.owner.is_none_or(|owner| Some(owner) == active);
        self.owner = None;
        accepted
    }

    pub(super) fn focus(&mut self, active: Option<u64>) -> bool {
        if self.owner.is_some() && self.owner != active {
            self.cancel();
            true
        } else {
            false
        }
    }

    pub(super) fn cancel(&mut self) {
        // Keep Disabled in the view until the runtime acknowledges Closed.
        // Queued preedits/commits cannot attach to the newly focused pane.
        self.resetting |= self.open;
        self.open = false;
        self.owner = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_change_close_and_return_cannot_redirect_composition() {
        for next in [Some(2), None] {
            // Other pane/tab, or owner closed.
            let mut ime = Composition::default();
            ime.opened();
            assert!(ime.preedit(Some(1)));
            assert!(!ime.focus(Some(1)));
            assert!(ime.focus(next));
            assert!(!ime.enabled());
            assert!(!ime.commit(next));
            assert!(!ime.preedit(next));
            ime.opened(); // A stale Opened does not undo the reset.
            assert!(!ime.commit(Some(1))); // Even after switching back.
            ime.closed();
            assert!(ime.enabled());
            assert!(!ime.commit(Some(2)));
            ime.opened();
            assert!(ime.preedit(Some(2)));
            assert!(ime.commit(Some(2)));
        }
    }

    #[test]
    fn commit_checks_owner_even_before_focus_notification() {
        let mut ime = Composition::default();
        ime.opened();
        assert!(ime.preedit(Some(1)));
        assert!(!ime.commit(Some(2)));
        assert!(!ime.enabled());
        ime.closed();
        ime.opened();
        assert!(ime.commit(Some(2))); // Direct commit without preedit.
        assert!(ime.preedit(Some(2)));
        assert!(ime.preedit(Some(2))); // Including an empty preedit update.
        assert!(ime.commit(Some(2)));
    }
}
