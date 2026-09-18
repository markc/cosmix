//! Workspaces (virtual desktops): the model and its core primitives.
//!
//! Every managed toplevel carries a 1-based workspace id (`0` = not mapped
//! yet); every output has a current workspace. A window off its output's
//! current workspace is hidden through the SAME visibility recompute that
//! minimise uses (`visible:false, minimized:false`), so bands, popups and
//! subsurfaces need no code of their own. A switch is the minimise caller
//! sequence over every leaving window plus one recompute/refocus/retarget.
//! No `ext-workspace-v1`; the Bus props, verbs and bindings live in their
//! own modules and call the `pub(crate)` primitives here.
//!
//! Single-output rule (0.59.0): `current` is keyed per output, but a record
//! is compared against the DEFAULT output's current workspace only. A real
//! per-record output binding is a later refinement.

// The switch/move/count/ensure primitives have no production caller until
// the verb, prop and binding slices land on top of this one; the tests drive
// them directly. Drop this once the first caller is wired.
#![cfg_attr(not(test), allow(dead_code))]

use super::*;

/// The most workspaces `set_workspace_count` accepts.
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

/// Where a switch or a move is aimed.
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
    /// No such output (or no output at all).
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

/// Rule 2: a managed toplevel joins the current workspace at its mapped
/// false→true edge — the first buffer commit, or an X11 remap from retained
/// content. Called with `was_mapped` read BEFORE the flag flipped. A remap
/// rejoins the current workspace (D4). This is the ONE hook per-window
/// `_NET_WM_DESKTOP` publication attaches to (D19): after a MapRequest a
/// first-map X11 record is still unmapped and reads `workspace == 0`.
pub(crate) fn stamp_workspace_at_map(record: &mut SurfaceRecord, was_mapped: bool, current: u32) {
    if !was_mapped && record.mapped && record.role.managed_toplevel() {
        record.workspace = current;
    }
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
    pub(crate) fn workspace_current(&self) -> u32 {
        self.current_workspace_for(self.default_output_key().as_deref())
    }

    /// Whether `record` is on the current workspace. Only managed toplevels
    /// have a workspace; everything else (bands, layers, popups, locks) is
    /// on every workspace.
    pub(crate) fn on_current_workspace(&self, record: &SurfaceRecord) -> bool {
        !record.role.managed_toplevel() || record.workspace == self.workspace_current()
    }

    /// The output key a request addresses: `None` = the default output;
    /// `Some(k)` matches a published output by key or by name. Without the
    /// `bus` feature there is no output projection, so only the default
    /// output can be named.
    fn resolve_workspace_output(&self, key: Option<&str>) -> Option<String> {
        let Some(requested) = key else {
            return self.default_output_key();
        };
        #[cfg(feature = "bus")]
        if let Some(projection) = port_snapshot::project_outputs(self) {
            return projection
                .rows
                .into_iter()
                .find(|(key, row)| key == requested || row.name == requested)
                .map(|(key, _)| key);
        }
        let output = self.backend.default_output()?;
        let name = output.name();
        let key = output_key(&name);
        (key == requested || name == requested).then_some(key)
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
        // rendering.
        #[cfg(feature = "xwayland")]
        if let Some(role) = self
            .surfaces
            .get(object)
            .and_then(|record| record.role.x11())
        {
            let _ = role.surface.set_suspended(true);
        }
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
        if let Some(role) = self
            .surfaces
            .get(object)
            .filter(|record| !record.minimized)
            .and_then(|record| record.role.x11())
        {
            let _ = role.surface.set_suspended(false);
        }
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
                let hidden = record.minimized || record.workspace != current;
                let _ = role.surface.set_suspended(hidden);
            }
        }
    }

    /// The one visibility settle every workspace change ends in: the same
    /// sequence `minimize_toplevel` runs, plus the X stacking sync.
    fn settle_workspace_visibility(&mut self) {
        self.recompute_effective_visibility();
        self.focus_highest_visible_toplevel();
        self.retarget_pointer_after_visibility_change();
        #[cfg(feature = "xwayland")]
        self.sync_xwm_stacking();
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
        for object in &leaving {
            self.withdraw_window_for_workspace(object, "workspace.switch");
        }
        self.workspaces.current.insert(output.clone(), to);
        for object in &arriving {
            self.present_window_for_workspace(object, "workspace.switch");
        }
        #[cfg(feature = "bus")]
        self.observations
            .full_dirty
            .get_or_insert("workspace.switch");
        self.settle_workspace_visibility();
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
        let count = self.workspaces.count;
        let from = self
            .surfaces
            .get(object)
            .filter(|record| {
                record.mapped && record.role.managed_toplevel() && record.workspace != 0
            })
            .map(|record| record.workspace)
            .ok_or(WorkspaceRefusal::NotAWindow)?;
        let to = resolve_workspace_target(from, count, target, true)?;
        if to == from {
            return Ok((from, to));
        }
        let current = self.workspace_current();
        if let Some(record) = self.surfaces.get_mut(object) {
            record.workspace = to;
        }
        if from == current {
            self.withdraw_window_for_workspace(object, "workspace.move");
        } else if to == current {
            self.present_window_for_workspace(object, "workspace.move");
        } else {
            #[cfg(feature = "bus")]
            if let Some(id) = self.surfaces.get(object).map(|record| record.id) {
                self.mark_surface_dirty(id, "workspace.move");
            }
        }
        // `workspaces.list` window counts change on every move.
        #[cfg(feature = "bus")]
        self.observations.full_dirty.get_or_insert("workspace.move");
        if from == current || to == current {
            self.settle_workspace_visibility();
        }
        Ok((from, to))
    }

    /// Set the workspace count. Shrinking strands: every window above the
    /// new count moves to the last workspace and every output's current is
    /// clamped. Returns `(old, new)`.
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
        self.observations
            .full_dirty
            .get_or_insert("workspace.count");
        if count > old {
            return Ok((old, count));
        }
        #[cfg(feature = "bus")]
        let mut stranded = Vec::new();
        for record in self.surfaces.values_mut() {
            if record.role.managed_toplevel() && record.workspace > count {
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
        self.sync_x11_suspended_for_workspaces();
        self.settle_workspace_visibility();
        Ok((old, count))
    }

    /// Bring a window's workspace on screen before activating it (focus,
    /// restore, xdg-activation, X11 activate/unminimise all go through
    /// this). Returns `true` when it switched. Inert — no side effects at
    /// all — under a session lock or an exclusive layer (D18): the lock or
    /// the layer owns what is on screen, and a client-driven X11 path has no
    /// guard of its own.
    pub(crate) fn ensure_workspace_shown(&mut self, object: &ObjectId) -> bool {
        if self.session_lock_active() || self.highest_exclusive_layer().is_some() {
            return false;
        }
        let Some(workspace) = self
            .surfaces
            .get(object)
            .filter(|record| {
                record.mapped && record.role.managed_toplevel() && record.workspace != 0
            })
            .map(|record| record.workspace)
        else {
            return false;
        };
        if workspace == self.workspace_current() {
            return false;
        }
        self.switch_workspace(None, WorkspaceTarget::Index(workspace), true)
            .is_ok()
    }
}
