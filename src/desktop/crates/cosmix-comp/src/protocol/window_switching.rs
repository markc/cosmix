//! Shared user/automation activation policy. Stacking never changes cycle order.

use super::*;

impl WaylandState {
    fn window_switch_candidate(&self, record: &SurfaceRecord) -> bool {
        record.mapped
            && !record.minimized
            && record.layout.visible
            && record.role.managed_toplevel()
            && record.role.wl_surface().is_alive()
            && self.surface_is_input_presentable(record)
    }

    /// Requests cannot escape session locking or an exclusive layer. This
    /// intentionally has no focus-stealing timestamp policy yet: local X11
    /// automation is allowed, but cannot activate hidden/unmanaged windows.
    pub(super) fn activate_managed_window(&mut self, surface: &WlSurface) {
        if self.session_lock_active()
            || self.highest_exclusive_layer().is_some()
            || !self
                .surfaces
                .get(&surface.id())
                .is_some_and(|record| self.window_switch_candidate(record))
        {
            return;
        }
        self.arbitrate_keyboard_focus(Some(surface.clone()), false, false);
        self.raise_surface(surface);
        self.retarget_pointer_after_visibility_change();
    }

    pub(super) fn cycle_window(&mut self, reverse: bool) {
        if self.session_lock_active() || self.highest_exclusive_layer().is_some() {
            return;
        }
        let mut candidates: Vec<_> = self
            .surfaces
            .values()
            .filter(|record| self.window_switch_candidate(record))
            .map(|record| (record.id.0, record.role.wl_surface().clone()))
            .collect();
        candidates.sort_unstable_by_key(|(id, _)| *id);
        if candidates.is_empty() {
            return;
        }
        let current = self
            .keyboard
            .current_focus()
            .and_then(|target| target.owned_surface())
            .map(|surface| canonical_root_surface(&self.popup_manager, &surface));
        let current_index = candidates
            .iter()
            .position(|(_, surface)| Some(surface) == current.as_ref());
        let index = match (current_index, reverse) {
            (Some(index), true) => (index + candidates.len() - 1) % candidates.len(),
            (Some(index), false) => (index + 1) % candidates.len(),
            (None, true) => candidates.len() - 1,
            (None, false) => 0,
        };
        self.activate_managed_window(&candidates[index].1);
    }
}
