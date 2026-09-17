//! Window-addressed Bus control: `{id, generation}` target fencing and the
//! minimise/restore operations shared by `windows.s<id>.minimized` and the
//! `comp.window.*` verbs.

use super::*;
use crate::port::{ControlReply, WindowOp};

/// Why a window-addressed request did not resolve to a window. These are
/// correctness refusals (the request is aimed at the wrong thing), never
/// caller checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowTargetError {
    /// No live, role-bearing surface has this id.
    UnknownWindow,
    /// The id is live but now names a different role assignment.
    StaleTarget { requested: u64, current: u64 },
    /// The surface exists but is not a managed toplevel (popup, layer,
    /// override-redirect X11, ...).
    NotManaged,
    /// A managed toplevel with no mapped content.
    NotMapped,
}

impl WaylandState {
    /// The one resolver for every window-addressed request. `generation`
    /// is optional so id-only callers keep working; when given it must
    /// match the surface's current role generation.
    pub(crate) fn resolve_window_target(
        &self,
        id: u64,
        generation: Option<u64>,
    ) -> Result<ObjectId, WindowTargetError> {
        let object = self
            .surface_objects
            .get(&SurfaceId(id))
            .ok_or(WindowTargetError::UnknownWindow)?;
        let record = self
            .surfaces
            .get(object)
            .ok_or(WindowTargetError::UnknownWindow)?;
        if let Some(requested) = generation
            && requested != record.generation
        {
            return Err(WindowTargetError::StaleTarget {
                requested,
                current: record.generation,
            });
        }
        if matches!(record.role, SurfaceRole::Dormant(_)) {
            return Err(WindowTargetError::UnknownWindow);
        }
        if !record.role.managed_toplevel() {
            return Err(WindowTargetError::NotManaged);
        }
        if !record.mapped {
            return Err(WindowTargetError::NotMapped);
        }
        Ok(object.clone())
    }

    /// Minimise (`true`) or restore (`false`) one resolved window. Returns
    /// the value before and after; a no-op leaves both equal.
    pub(crate) fn set_window_minimized(
        &mut self,
        object: &ObjectId,
        minimized: bool,
    ) -> Option<(bool, bool)> {
        let record = self.surfaces.get(object)?;
        let before = record.minimized;
        let surface = record.role.wl_surface().clone();
        if minimized && !before {
            self.minimize_toplevel(&surface);
        } else if !minimized && before {
            self.restore_window(object);
        }
        let after = self.surfaces.get(object)?.minimized;
        Some((before, after))
    }

    fn window_reply(&self, object: &ObjectId, changed: bool) -> ControlReply {
        let Some(record) = self.surfaces.get(object) else {
            return ControlReply::Busy;
        };
        ControlReply::Window {
            id: record.id.0,
            generation: record.generation,
            title: record.title.clone(),
            app_id: record.app_id.clone(),
            minimized: record.minimized,
            changed,
        }
    }

    fn minimized_window_count(&self) -> usize {
        self.surfaces
            .values()
            .filter(|record| record.mapped && record.minimized && record.role.managed_toplevel())
            .count()
    }

    /// `comp.window.minimize` / `comp.window.restore`. A session lock
    /// refuses both: the lock owns what is on screen until it ends.
    pub(crate) fn service_window_op(&mut self, op: &WindowOp) -> ControlReply {
        if self.session_lock_active() {
            return ControlReply::Locked;
        }
        let (target, minimized) = match *op {
            WindowOp::Minimize { id, generation } => ((id, generation), true),
            WindowOp::Restore {
                target: Some(target),
            } => (target, false),
            WindowOp::Restore { target: None } => {
                return match self.restore_most_recently_minimized() {
                    Some(object) => self.window_reply(&object, true),
                    None => ControlReply::NotFound {
                        minimized_count: self.minimized_window_count(),
                    },
                };
            }
        };
        let (id, generation) = target;
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        self.mark_surface_dirty(SurfaceId(id), "comp.window");
        match self.set_window_minimized(&object, minimized) {
            Some((before, after)) => self.window_reply(&object, before != after),
            None => ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            },
        }
    }
}
