//! Workspaces (virtual desktops): the model and its core primitives.
//!
//! Every managed toplevel carries a 1-based workspace id (`0` = not mapped
//! yet); every output has a current workspace. A window off its output's
//! current workspace is hidden through the SAME visibility recompute that
//! minimise uses (`visible:false, minimized:false`), so bands, popups and
//! subsurfaces need no code of their own. A switch is the minimise caller
//! sequence over every leaving window plus one recompute/refocus/retarget.
//! An override-redirect X11 window (a menu, tooltip, dropdown, DND icon)
//! is a root of its own in that recompute — it has no `layout.parent` —
//! so it carries the workspace it mapped on too (`carries_workspace`),
//! and follows it through the same funnel: hidden, no frame callbacks,
//! never presented while its workspace is not current. It is still not a
//! managed toplevel: never movable, never suspended, no `_NET_WM_DESKTOP`,
//! no `windows.*` or `surfaces.s<id>.workspace` value.
//! No `ext-workspace-v1`; the Bus props, verbs and bindings live in their
//! own modules and call the `pub(crate)` primitives here.
//!
//! Single-output rule (0.59.0, D3): `current` is keyed per output, but a
//! record is compared against the DEFAULT output's current workspace only,
//! so only the default output can be switched — a request naming any other
//! output is refused (`UnknownOutput`) rather than moving `current` for an
//! output whose windows the visibility term does not read. A real
//! per-record output binding is a later refinement.

use super::*;

/// The most workspaces `set_workspace_count` accepts (the props surface
/// pins its `range` string to this value).
pub(crate) const WORKSPACE_COUNT_MAX: u32 = 16;

/// Per-compositor workspace state.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceState {
    /// Number of workspaces, `1..=WORKSPACE_COUNT_MAX`.
    pub(crate) count: u32,
    /// Current workspace per output key (`o_<slug>`); an absent key reads
    /// as 1.
    pub(crate) current: BTreeMap<String, u32>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            count: 4,
            current: BTreeMap::new(),
        }
    }
}

/// Where a switch or a move is aimed. `Next`/`Prev` have a non-bus
/// constructor (the `workspace-next` / `workspace-prev` chords), so the
/// `--no-default-features` gate (D20) sees every variant constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceTarget {
    /// A 1-based workspace index.
    Index(u32),
    Next,
    Prev,
}

/// Why a workspace primitive changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceRefusal {
    /// An index outside `1..=count`.
    InvalidIndex { count: u32 },
    /// `Next`/`Prev` at an end without `wrap`.
    AtEnd { from: u32, count: u32 },
    /// Not the default output (D3: the only one with a switchable current
    /// workspace in 0.59.0), or no output at all.
    UnknownOutput,
    /// A count outside `1..=WORKSPACE_COUNT_MAX`.
    InvalidCount { max: u32 },
    /// The object is not a mapped managed toplevel.
    NotAWindow,
}

/// What a switch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSwitch {
    pub(crate) output: String,
    pub(crate) from: u32,
    pub(crate) to: u32,
}

/// The `o_<slug>` key an output is published under (also the key of its
/// current workspace). Lives here, unguarded, because the workspace model
/// needs it without the `bus` feature; `port_snapshot` re-exports it.
pub(crate) fn output_key(name: &str) -> String {
    let mut key = String::from("o_");
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            key.push(character.to_ascii_lowercase());
        } else {
            key.push('_');
        }
    }
    key
}

/// Whether a role has a workspace of its own: a managed toplevel, or an
/// override-redirect X11 window. Everything else (bands, layers, popups,
/// subsurfaces, locks) is on every workspace — popups and subsurfaces
/// follow their toplevel through `layout.parent` in the recompute, but an
/// OR record has no parent (xwayland.rs creates it as a root), so without
/// a workspace of its own an open menu would outlive the switch that hid
/// its owner, floating over the next workspace with no window under it.
pub(super) fn carries_workspace(role: &SurfaceRole) -> bool {
    #[cfg(feature = "xwayland")]
    if role.x11().is_some_and(|x11| x11.override_redirect) {
        return true;
    }
    role.managed_toplevel()
}

/// Rule 2: a managed toplevel joins the current workspace at its mapped
/// false→true edge — the first buffer commit, or an X11 remap from retained
/// content. Called with `was_mapped` read BEFORE the flag flipped. A remap
/// rejoins the current workspace (D4). This is the ONE hook per-window
/// `_NET_WM_DESKTOP` publication attaches to (D19): after a MapRequest a
/// first-map X11 record is still unmapped and reads `workspace == 0`. An
/// override-redirect record is stamped the same way (it hides with the
/// workspace it mapped on) but gets no `_NET_WM_DESKTOP`: EWMH gives the
/// property to managed windows, and `sync_x11_desktops` skips OR too.
pub(super) fn stamp_workspace_at_map(record: &mut SurfaceRecord, was_mapped: bool, current: u32) {
    if !was_mapped && record.mapped && carries_workspace(&record.role) {
        record.workspace = current;
        // EWMH: the window's `_NET_WM_DESKTOP` is written HERE, at the
        // stamping edge, never at MapRequest (D19). Debug, not warn, on
        // failure: the offline fakes have a dead connection, and a live
        // failure is a dying generation `disconnected` cleans up.
        #[cfg(feature = "xwayland")]
        if let Some(role) = record.role.x11()
            && !role.override_redirect
            && let Err(error) = role.surface.set_desktop(current.saturating_sub(1))
        {
            tracing::debug!(%error, xid = role.surface.window_id(), "failed to publish _NET_WM_DESKTOP at map");
        }
    }
}

/// THE workspace term, shared by every reader: whether `record` is on the
/// workspace `current`. Only a record that `carries_workspace` (a managed
/// toplevel, an override-redirect X11 window) has one; everything else
/// (bands, layers, popups, locks) is on every workspace. Takes the
/// current workspace as a value so hot paths (`handle_frame`,
/// `recompute_effective_visibility`, `frame_presented`) read it once per
/// frame instead of once per surface — `workspace_current` clones an
/// `Output` and builds a key `String`, which is per-frame churn of exactly
/// the kind 0.56.1 removed.
// `pub(super)`, not `pub(crate)`: `SurfaceRecord` is private to `protocol`,
// and a free fn is not visibility-capped the way a method on the private
// `WaylandState` is (rustc private_interfaces under -D warnings). Every
// caller lives inside `protocol`.
pub(super) fn on_workspace(record: &SurfaceRecord, current: u32) -> bool {
    !carries_workspace(&record.role) || record.workspace == current
}

/// THE movable term: what `move_window_to_workspace` (and so every move —
/// the `windows.s<id>.workspace` write, `send_to_workspace`, the
/// Super+Shift+n chord) accepts. A mapped managed toplevel on a real
/// workspace; a record between its MapRequest and first commit still reads
/// `workspace == 0` and is not movable yet.
pub(super) fn workspace_movable(record: &SurfaceRecord) -> bool {
    record.mapped && record.role.managed_toplevel() && record.workspace != 0
}

/// D15's rule for an X11 window's `_NET_WM_STATE_HIDDEN` / suspended flag:
/// hidden when minimised OR off the current workspace. Every site that
/// sets the flag derives it from here, so no path can un-suspend a window
/// that is still off screen.
#[cfg(feature = "xwayland")]
pub(super) fn x11_suspended(record: &SurfaceRecord, current: u32) -> bool {
    record.minimized || !on_workspace(record, current)
}

/// Resolve a target against the workspace `from` on a `count`-wide ring.
fn resolve_workspace_target(
    from: u32,
    count: u32,
    target: WorkspaceTarget,
    wrap: bool,
) -> Result<u32, WorkspaceRefusal> {
    match target {
        WorkspaceTarget::Index(index) if index == 0 || index > count => {
            Err(WorkspaceRefusal::InvalidIndex { count })
        }
        WorkspaceTarget::Index(index) => Ok(index),
        WorkspaceTarget::Next if from >= count => {
            if wrap {
                Ok(1)
            } else {
                Err(WorkspaceRefusal::AtEnd { from, count })
            }
        }
        WorkspaceTarget::Next => Ok(from + 1),
        WorkspaceTarget::Prev if from <= 1 => {
            if wrap {
                Ok(count)
            } else {
                Err(WorkspaceRefusal::AtEnd { from, count })
            }
        }
        WorkspaceTarget::Prev => Ok(from - 1),
    }
}

impl WaylandState {
    /// The default output's key, or `None` when the backend has no output.
    pub(crate) fn default_output_key(&self) -> Option<String> {
        self.backend
            .default_output()
            .map(|output| output_key(&output.name()))
    }

    /// The current workspace of the output `key`; an absent or unknown key
    /// reads as 1.
    pub(crate) fn current_workspace_for(&self, key: Option<&str>) -> u32 {
        key.and_then(|key| self.workspaces.current.get(key))
            .copied()
            .unwrap_or(1)
    }

    /// The default output's current workspace (the single-output rule).
    /// Per-event callers (the `windows.list` filter, the rows) and per-frame
    /// loops alike read this once and call `on_workspace` with the value;
    /// the `on_current_workspace(record)` convenience the core reserved for
    /// slices 3-4 was never taken up and is gone.
    pub(crate) fn workspace_current(&self) -> u32 {
        self.current_workspace_for(self.default_output_key().as_deref())
    }

    /// D15 applied to one window: derive an X11 window's suspended flag from
    /// what hides it (minimised OR off the current workspace) and set it.
    /// Every `set_suspended` on a managed X11 window goes through here —
    /// minimise, restore, the workspace switch and move halves — so no path
    /// can resume a window that is still off screen. A no-op for every
    /// other role.
    #[cfg(feature = "xwayland")]
    pub(super) fn sync_x11_suspended(&self, object: &ObjectId) {
        let current = self.workspace_current();
        if let Some(record) = self.surfaces.get(object)
            && let Some(role) = record.role.x11()
        {
            let _ = role.surface.set_suspended(x11_suspended(record, current));
        }
    }
}

// The primitives. `switch_workspace`, `move_window_to_workspace` and
// `move_window_and_follow` have non-bus production callers (the workspace
// chords, `handle_binding_action`), and `ensure_workspace_shown` is wired at
// every bring-into-view path (slice 2), so the block is NOT allowed dead: a
// genuinely dead helper trips the lint. The one whose only caller is
// `cfg(bus)` — `set_workspace_count` (the `workspaces.count` prop) — reads
// dead to the `--no-default-features` gate (D20) and carries a per-item
// allow; drop it with its first non-bus caller.
impl WaylandState {
    /// The output key a request addresses: `None` = the default output;
    /// `Some(k)` must be the default output's key or name (D3). Any other
    /// output is refused: `current` for it would be written, but the
    /// visibility term reads the default output's, so windows would be
    /// withdrawn (suspended, feedback discarded, drags ended) while staying
    /// on screen. The refusal goes when records carry an output binding.
    pub(super) fn resolve_workspace_output(&self, key: Option<&str>) -> Option<String> {
        let output = self.backend.default_output()?;
        let name = output.name();
        let key_of_default = output_key(&name);
        match key {
            None => Some(key_of_default),
            Some(requested) => {
                (key_of_default == requested || name == requested).then_some(key_of_default)
            }
        }
    }

    /// The per-window half of leaving the current workspace: what
    /// `minimize_toplevel` does before it hides the window, minus the
    /// minimise state itself.
    fn withdraw_window_for_workspace(&mut self, object: &ObjectId, cause: &'static str) {
        let Some((surface, id)) = self
            .surfaces
            .get(object)
            .map(|record| (record.role.wl_surface().clone(), record.id))
        else {
            return;
        };
        self.cancel_chrome_pointer_grab_for_surface(&surface, true);
        self.reset_chrome_pointer_tracking(object);
        self.discard_presentation_feedback(id, presentation::DiscardReason::Workspace);
        // A client-started move/resize must not keep steering a hidden
        // window (see `minimize_toplevel`).
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| *interactive == surface)
        {
            self.finish_interactive_pointer(true);
        }
        // X11 windows learn the state through EWMH so the client can stop
        // rendering. Derived, not written as `true`: the caller has already
        // moved the window (or `current`) so the rule reads "off screen".
        #[cfg(feature = "xwayland")]
        self.sync_x11_suspended(object);
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(id, cause);
        #[cfg(not(feature = "bus"))]
        let _ = cause;
    }

    /// The per-window half of arriving on the current workspace: an X11
    /// window that is not minimised may paint again.
    fn present_window_for_workspace(&mut self, object: &ObjectId, cause: &'static str) {
        let Some(id) = self.surfaces.get(object).map(|record| record.id) else {
            return;
        };
        #[cfg(feature = "xwayland")]
        self.sync_x11_suspended(object);
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(id, cause);
        #[cfg(not(feature = "bus"))]
        let _ = (id, cause);
    }

    /// Re-derive every managed X11 window's suspended flag from what hides
    /// it (D15's rule, used where more than one window may change at once).
    #[cfg(feature = "xwayland")]
    fn sync_x11_suspended_for_workspaces(&mut self) {
        let current = self.workspace_current();
        for record in self.surfaces.values() {
            if !record.mapped || !record.role.managed_toplevel() {
                continue;
            }
            if let Some(role) = record.role.x11() {
                let _ = role.surface.set_suspended(x11_suspended(record, current));
            }
        }
    }

    /// The one visibility settle every workspace change ends in: the same
    /// sequence `minimize_toplevel` runs, plus the X stacking sync. The
    /// keyboard goes to `prefer` when a caller names a window it is about
    /// to activate anyway (`move_window_and_follow`), else to the highest
    /// visible toplevel — the same arbitration, the same lock and
    /// exclusive-layer arms (under either, the request is ignored and the
    /// lock or layer keeps the keyboard). Preferring is what keeps a moved
    /// window's arrival from handing the keyboard to a bystander in a
    /// higher band for one round-trip: `raise_surface` raises within the
    /// window's own `StackBand`, so a bottom-band window is never the
    /// highest visible toplevel while a normal one shares the workspace.
    fn settle_workspace_visibility(&mut self, prefer: Option<WlSurface>) {
        self.recompute_effective_visibility();
        self.arbitrate_keyboard_focus(prefer, true, false);
        self.retarget_pointer_after_visibility_change();
        #[cfg(feature = "xwayland")]
        self.sync_xwm_stacking();
    }

    /// THE per-window relabel every move goes through (`move_window_to_
    /// workspace` and `move_window_and_follow`): the record's workspace,
    /// the EWMH per-window `_NET_WM_DESKTOP` (D19, beside the record write —
    /// the suspend sync the caller runs derives from the same record) and
    /// the surface's dirty mark, nothing else — the caller runs the
    /// withdraw/present half and the settle. One site, so the per-move
    /// publication lands on every path (including the release-arm revert
    /// in `move_window_and_follow`, which relabels back to `from`).
    ///
    /// 0.59.1: also relabels `object`'s override-redirect children (a menu,
    /// a tooltip) to the same workspace, one level, recursively through
    /// this same function — never `_NET_WM_DESKTOP` for one of them (guarded
    /// below), since EWMH gives that property to managed windows only
    /// (`stamp_workspace_at_map`). An OR record has no `layout.parent`
    /// linking it to its owner in the visibility recompute (X-2a: it is a
    /// root of its own, stamped with the workspace it mapped on), so
    /// without this a move of the owner strands the child on the old
    /// workspace — it goes invisible while the owner is on screen
    /// elsewhere. This cannot recurse past one level in practice: an OR
    /// record is never `workspace_movable`, so it is never the `object` a
    /// caller of `move_window_to_workspace`/`move_window_and_follow` names;
    /// only a real move seeds the walk, and `or_children_of` only follows
    /// WM_TRANSIENT_FOR one hop from wherever it is seeded.
    fn relabel_workspace(&mut self, object: &ObjectId, to: u32) {
        let Some(record) = self.surfaces.get_mut(object) else {
            return;
        };
        record.workspace = to;
        #[cfg(feature = "xwayland")]
        if let Some(role) = record.role.x11()
            && !role.override_redirect
            && let Err(error) = role.surface.set_desktop(to - 1)
        {
            tracing::debug!(%error, xid = role.surface.window_id(), "failed to publish _NET_WM_DESKTOP on move");
        }
        #[cfg(feature = "bus")]
        {
            let id = record.id;
            self.mark_surface_dirty(id, "workspace.move");
        }
        #[cfg(feature = "xwayland")]
        for child in self.or_children_of(object) {
            self.relabel_workspace(&child, to);
        }
    }

    /// Every override-redirect record whose WM_TRANSIENT_FOR names
    /// `owner`'s X11 window (`X11Surface::is_transient_for`) — the same
    /// identity every OR window that names an owner sets, menu or tooltip
    /// alike. `owner` itself must be an X11 window for any hit to exist
    /// (WM_TRANSIENT_FOR names an X window), so a non-X11 or unassociated
    /// `owner` returns empty rather than guessing. An OR record with no
    /// transient-for (some tooltips set none) is not returned; it stays on
    /// whatever workspace it mapped on, exactly as before this change.
    #[cfg(feature = "xwayland")]
    fn or_children_of(&self, owner: &ObjectId) -> Vec<ObjectId> {
        let Some(owner_xid) = self.xwayland.xids_by_object.get(owner).copied() else {
            return Vec::new();
        };
        self.surfaces
            .iter()
            .filter_map(|(child, record)| {
                let role = record.role.x11()?;
                // `child != owner` blocks a client-set transient-for-self
                // (a malformed OR window naming its own XID) from making
                // `relabel_workspace`'s recursion loop forever.
                (child != owner
                    && role.override_redirect
                    && role.surface.is_transient_for() == Some(owner_xid))
                .then(|| child.clone())
            })
            .collect()
    }

    /// Switch the output `key` (`None` = default) to `target`. Refuses an
    /// index outside `1..=count`, `Next`/`Prev` at an end without `wrap`,
    /// and an unknown output; a switch to the current workspace is `Ok`
    /// with no side effects. Minimise state is untouched: a minimised
    /// window on the arriving workspace stays minimised.
    pub(crate) fn switch_workspace(
        &mut self,
        key: Option<&str>,
        target: WorkspaceTarget,
        wrap: bool,
    ) -> Result<WorkspaceSwitch, WorkspaceRefusal> {
        self.switch_workspace_focusing(key, target, wrap, None)
    }

    /// `switch_workspace` with the settle's keyboard preference (see
    /// `settle_workspace_visibility`); `move_window_and_follow` names the
    /// window it is bringing along so the settle lands on it.
    fn switch_workspace_focusing(
        &mut self,
        key: Option<&str>,
        target: WorkspaceTarget,
        wrap: bool,
        prefer: Option<WlSurface>,
    ) -> Result<WorkspaceSwitch, WorkspaceRefusal> {
        let output = self
            .resolve_workspace_output(key)
            .ok_or(WorkspaceRefusal::UnknownOutput)?;
        let count = self.workspaces.count;
        let from = self.current_workspace_for(Some(&output));
        let to = resolve_workspace_target(from, count, target, wrap)?;
        if to == from {
            return Ok(WorkspaceSwitch { output, from, to });
        }
        let (leaving, arriving): (Vec<ObjectId>, Vec<ObjectId>) = {
            let mut leaving = Vec::new();
            let mut arriving = Vec::new();
            for (object, record) in &self.surfaces {
                if !record.mapped || !record.role.managed_toplevel() {
                    continue;
                }
                if record.workspace == from {
                    leaving.push(object.clone());
                } else if record.workspace == to {
                    arriving.push(object.clone());
                }
            }
            (leaving, arriving)
        };
        self.titlebar_click_candidate = None;
        // `current` moves first: the per-window halves derive the X11
        // suspended flag from it (`sync_x11_suspended`), so a leaving window
        // must already read as off the current workspace.
        self.workspaces.current.insert(output.clone(), to);
        for object in &leaving {
            self.withdraw_window_for_workspace(object, "workspace.switch");
        }
        for object in &arriving {
            self.present_window_for_workspace(object, "workspace.switch");
        }
        #[cfg(feature = "bus")]
        self.mark_workspaces_dirty("workspace.switch");
        // EWMH root `_NET_CURRENT_DESKTOP`: after `current` moved (above)
        // so it reads the new value.
        #[cfg(feature = "xwayland")]
        self.publish_x11_desktops();
        self.settle_workspace_visibility(prefer);
        Ok(WorkspaceSwitch { output, from, to })
    }

    /// Move one mapped managed toplevel to `target` without switching.
    /// `Next`/`Prev` are relative to the window's own workspace and always
    /// wrap. Returns `(from, to)`; never touches the window's generation
    /// (the object is the same window, only placed elsewhere).
    pub(crate) fn move_window_to_workspace(
        &mut self,
        object: &ObjectId,
        target: WorkspaceTarget,
    ) -> Result<(u32, u32), WorkspaceRefusal> {
        self.move_window_to_workspace_focusing(object, target, None)
    }

    /// `move_window_to_workspace` with the settle's keyboard preference
    /// (see `settle_workspace_visibility`); only a move that leaves or
    /// arrives on the current workspace settles at all.
    fn move_window_to_workspace_focusing(
        &mut self,
        object: &ObjectId,
        target: WorkspaceTarget,
        prefer: Option<WlSurface>,
    ) -> Result<(u32, u32), WorkspaceRefusal> {
        let count = self.workspaces.count;
        let from = self
            .surfaces
            .get(object)
            .filter(|record| workspace_movable(record))
            .map(|record| record.workspace)
            .ok_or(WorkspaceRefusal::NotAWindow)?;
        let to = resolve_workspace_target(from, count, target, true)?;
        if to == from {
            return Ok((from, to));
        }
        let current = self.workspace_current();
        self.relabel_workspace(object, to);
        if from == current {
            self.withdraw_window_for_workspace(object, "workspace.move");
        } else if to == current {
            self.present_window_for_workspace(object, "workspace.move");
        }
        // `workspaces.list` window counts change on every move.
        #[cfg(feature = "bus")]
        self.mark_workspaces_dirty("workspace.move");
        if from == current || to == current {
            self.settle_workspace_visibility(prefer);
        }
        Ok((from, to))
    }

    /// Move one window to `target` AND make that workspace the default
    /// output's current one, in ONE settle, with the window on screen, on
    /// top of its band and holding the keyboard throughout — so no
    /// bystander on either workspace takes the keyboard in between (the
    /// Super+Shift+n chord and `send_to_workspace {follow:true}`). Doing it
    /// as a move then a switch (or a switch then a move) hands focus to
    /// whichever window is left highest on the workspace being shown at
    /// the settle — an enter plus an activated configure the caller's
    /// activation immediately reverses. Here the record is relabelled
    /// first, so the switch sees the window ARRIVING rather than leaving
    /// (never withdrawn, never suspended), and the settle is told to
    /// prefer it, so the keyboard lands on it whatever band it is in
    /// (`raise_surface` raises within the band only, so being on top of a
    /// bottom-band window's band is not being the highest visible
    /// toplevel).
    ///
    /// Refuses exactly what `move_window_to_workspace` refuses, before
    /// anything changes; with no default output the switch has nothing to
    /// move and this is the plain move. Returns `(from, to)`; a window
    /// already on the current workspace is only moved when `to` differs.
    ///
    /// The primitive does not gate on D18 — both callers do, with
    /// `workspace_switch_allowed_for`, the one predicate every switch-first
    /// path shares — but its keyboard preference does: a window that gate
    /// would not let a switch bring into focus (minimised, not presentable,
    /// a lock or an exclusive layer on screen) is moved and raised, and the
    /// settle's fallback keeps the keyboard where the lock or layer says.
    /// The caller activates afterwards; for a window already preferred that
    /// is a no-op, so there is exactly one enter.
    pub(crate) fn move_window_and_follow(
        &mut self,
        object: &ObjectId,
        target: WorkspaceTarget,
    ) -> Result<(u32, u32), WorkspaceRefusal> {
        let Some(output) = self.resolve_workspace_output(None) else {
            return self.move_window_to_workspace(object, target);
        };
        let count = self.workspaces.count;
        let from = self
            .surfaces
            .get(object)
            .filter(|record| workspace_movable(record))
            .map(|record| record.workspace)
            .ok_or(WorkspaceRefusal::NotAWindow)?;
        let to = resolve_workspace_target(from, count, target, true)?;
        let current = self.current_workspace_for(Some(&output));
        let surface = self.surfaces[object].role.wl_surface().clone();
        let prefer = self
            .workspace_switch_allowed_for(object)
            .map(|_| surface.clone());
        // Nothing below can refuse: `to` came from the ring and `output`
        // from `resolve_workspace_output`, which round-trips its own key.
        // On top of its band first, so the arrival is also a raise.
        self.raise_surface(&surface);
        if to == current {
            return self.move_window_to_workspace_focusing(object, target, prefer);
        }
        if from != to {
            self.relabel_workspace(object, to);
        }
        // The window is on `to` already, so the switch presents it.
        match self.switch_workspace_focusing(
            Some(&output),
            WorkspaceTarget::Index(to),
            true,
            prefer,
        ) {
            Ok(_) => Ok((from, to)),
            Err(refusal) => {
                // Unreachable by construction (see above), and a debug
                // build says so. The release arm puts the label back so a
                // refusal is not reported alongside a half-done move; it
                // is NOT a full undo — the raise above stays (there is no
                // un-raise), and the `workspace.move` mark the first
                // relabel planted stays on the surface (the second relabel
                // only re-marks it). Both are accepted for a branch no
                // caller can reach, rather than carrying restore state for
                // it; if `switch_workspace_focusing` ever grows a refusal
                // this can hit, this arm needs a real undo.
                if cfg!(debug_assertions) {
                    unreachable!("move_window_and_follow: switch refused {refusal:?}");
                }
                if from != to {
                    self.relabel_workspace(object, from);
                }
                Err(refusal)
            }
        }
    }

    /// Set the workspace count. Shrinking strands: every window above the
    /// new count moves to the last workspace and every output's current is
    /// clamped. Returns `(old, new)`.
    ///
    /// A shrink is a mass re-derivation, not a per-window arrival: both the
    /// window set AND `current` can change in one step, so it re-syncs every
    /// X11 flag from `x11_suspended` and settles once rather than running
    /// `present_window_for_workspace` per stranded window. Any per-arrival
    /// side effect added to `present_window_for_workspace` later must be
    /// added to `sync_x11_suspended_for_workspaces` too (or the shrink path
    /// switched to the per-window halves).
    // The one production caller is the `workspaces.count` props write
    // (`port_observation`, a bus-only module), as the crate's other
    // bus-only entry points say it.
    #[cfg_attr(not(feature = "bus"), allow(dead_code))]
    pub(crate) fn set_workspace_count(
        &mut self,
        count: u32,
    ) -> Result<(u32, u32), WorkspaceRefusal> {
        if count == 0 || count > WORKSPACE_COUNT_MAX {
            return Err(WorkspaceRefusal::InvalidCount {
                max: WORKSPACE_COUNT_MAX,
            });
        }
        let old = self.workspaces.count;
        self.workspaces.count = count;
        if count == old {
            return Ok((old, count));
        }
        #[cfg(feature = "bus")]
        self.mark_workspaces_dirty("workspace.count");
        if count > old {
            // A grow strands nothing: only the root count changes.
            #[cfg(feature = "xwayland")]
            self.publish_x11_desktops();
            return Ok((old, count));
        }
        #[cfg(feature = "bus")]
        let mut stranded = Vec::new();
        // Every record with a workspace, override-redirect included: an OR
        // record left above the count would be hidden on every workspace.
        for record in self.surfaces.values_mut() {
            if carries_workspace(&record.role) && record.workspace > count {
                record.workspace = count;
                #[cfg(feature = "bus")]
                stranded.push(record.id);
            }
        }
        #[cfg(feature = "bus")]
        for id in stranded {
            self.mark_surface_dirty(id, "workspace.count");
        }
        for current in self.workspaces.current.values_mut() {
            if *current > count {
                *current = count;
            }
        }
        #[cfg(feature = "xwayland")]
        {
            self.sync_x11_suspended_for_workspaces();
            // Both the count and (possibly) `current` changed, and stranded
            // windows moved: republish every window, THEN the root pair, so
            // no reader sees a `_NET_WM_DESKTOP` at or above the new
            // `_NET_NUMBER_OF_DESKTOPS` (a window on an old desktop under
            // the old count is valid; the reverse is not).
            self.sync_x11_desktops();
            self.publish_x11_desktops();
        }
        self.settle_workspace_visibility(None);
        Ok((old, count))
    }

    /// The one gate on a bring-into-view switch: `Some(workspace)` when a
    /// switch to `object`'s workspace may run, `None` when it must not.
    /// `None` under a session lock or an exclusive layer (D18: the lock or
    /// the layer owns what is on screen, and a client-driven X11 path has
    /// no guard of its own), and for a window that could not take focus
    /// once shown — a minimised one, or one that is not input-presentable
    /// (the KMS gate while the VT is switched away): a refused activation
    /// must not change the desktop. The terms are `window_switch_candidate`'s
    /// minus `layout.visible`, which is what the switch itself sets.
    ///
    /// Every caller that goes on to focus must either see the switch run or
    /// refuse: `activate_managed_window` re-checks the candidate after,
    /// `service_window_focus` gates on the same terms before, and
    /// `restore_window` / xdg-activation withhold the focus when
    /// `window_off_current_workspace` is still true after the attempt.
    /// `arbitrate_keyboard_focus` has no presentable or workspace term of
    /// its own, so a caller that skipped that would focus an off-screen
    /// window.
    pub(super) fn workspace_switch_allowed_for(&self, object: &ObjectId) -> Option<u32> {
        if self.session_lock_active() || self.highest_exclusive_layer().is_some() {
            return None;
        }
        self.surfaces
            .get(object)
            .filter(|record| {
                record.mapped
                    && !record.minimized
                    && record.role.managed_toplevel()
                    && record.role.wl_surface().is_alive()
                    && record.workspace != 0
                    && self.surface_is_input_presentable(record)
            })
            .map(|record| record.workspace)
    }

    /// Whether `object` is a mapped managed toplevel whose workspace is NOT
    /// the default output's current one — the check a focus-after-switch
    /// caller makes once `ensure_workspace_shown` has had its say. False
    /// for everything that has no workspace of its own (an unstamped or
    /// non-toplevel surface is on every workspace, as `on_workspace`
    /// reads it), so a caller refusing on `true` refuses exactly the
    /// windows the switch would have shown.
    pub(super) fn window_off_current_workspace(&self, object: &ObjectId) -> bool {
        let current = self.workspace_current();
        self.surfaces
            .get(object)
            .is_some_and(|record| workspace_movable(record) && record.workspace != current)
    }

    /// Bring a window's workspace on screen before activating it (focus,
    /// restore, xdg-activation, X11 activate/unminimise all go through
    /// this). Returns `true` when it switched; inert — no side effects at
    /// all — whenever `workspace_switch_allowed_for` says no, or the window
    /// is already on the current workspace.
    ///
    /// The switch's settle prefers the window itself (the same preference
    /// `move_window_and_follow` gives a followed window): every caller goes
    /// on to focus it, and the gate above has already admitted it as a
    /// focus candidate, so settling on the highest bystander first would
    /// hand that bystander one `wl_keyboard.enter` plus an activated
    /// configure the caller's own arbitration reverses a statement later.
    /// With the preference the caller's `arbitrate_keyboard_focus` is a
    /// no-op and there is exactly one enter, on the window brought into
    /// view.
    pub(crate) fn ensure_workspace_shown(&mut self, object: &ObjectId) -> bool {
        let Some(workspace) = self.workspace_switch_allowed_for(object) else {
            return false;
        };
        if workspace == self.workspace_current() {
            return false;
        }
        let prefer = self
            .surfaces
            .get(object)
            .map(|record| record.role.wl_surface().clone());
        self.switch_workspace_focusing(None, WorkspaceTarget::Index(workspace), true, prefer)
            .is_ok()
    }

    /// After a KMS topology change. `workspaces.current` is keyed by the
    /// DEFAULT output (D3) and a hotplug can replace that output: a fresh
    /// `o_<slug>` key reads as workspace 1 with no switch having run, so
    /// from the next statement on every per-frame reader (`handle_frame`,
    /// `frame_presented`) would see one workspace while `layout.visible`
    /// and the X11 suspended flags still described the other — windows on
    /// the old current workspace frozen on screen with no callbacks, the
    /// ones on workspace 1 composited nowhere yet reading `visible:false`
    /// and staying suspended. So the retiring output's current workspace
    /// is CARRIED to the replacing one (the user's desktop does not change
    /// because a monitor was replugged), and if the effective value changed
    /// anyway — the only output went away, or one came back under a key
    /// that still holds an older entry while windows mapped meanwhile were
    /// stamped 1 — the state is re-derived the way a count shrink does it:
    /// every X11 flag from `x11_suspended`, then one settle. The EWMH root
    /// pair is republished either way (a no-op without an XWM), or
    /// `_NET_CURRENT_DESKTOP` keeps the retired output's index until the
    /// next switch. `previous_key` and `previous_current` are read BEFORE
    /// the backend applied the event; the `workspaces.*` snapshot is
    /// already fully dirtied by the apply site (`output.geometry`).
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(super) fn reconcile_workspace_current_after_topology_change(
        &mut self,
        previous_key: Option<&str>,
        previous_current: u32,
    ) {
        if let Some(key) = self.default_output_key()
            && previous_key.is_some_and(|previous| previous != key)
        {
            self.workspaces.current.insert(key, previous_current);
        }
        #[cfg(feature = "xwayland")]
        self.publish_x11_desktops();
        if self.workspace_current() == previous_current {
            return;
        }
        #[cfg(feature = "xwayland")]
        self.sync_x11_suspended_for_workspaces();
        self.settle_workspace_visibility(None);
    }
}
