//! Window-addressed Bus control: `{id, generation}` target fencing, the
//! minimise/restore operations shared by `windows.s<id>.minimized` and the
//! `comp.window.*` verbs, and the presentation stats verbs.

use serde_json::{Value, json};

use super::presentation_stats::PresentationStats;
use super::*;
use crate::port::{ControlReply, SourceTargetError, StatsTarget, WindowOp};

/// `comp_window_control` trace op codes (the record's `detail`).
#[derive(Clone, Copy, Debug)]
enum TracedOp {
    Minimize = 1,
    Restore = 2,
    Stats = 3,
    StatsReset = 4,
}

fn trace_window_control(op: &WindowOp) {
    let (code, target) = match op {
        WindowOp::Minimize { id, generation } => (TracedOp::Minimize, Some((*id, *generation))),
        WindowOp::Restore { target } => (TracedOp::Restore, *target),
        WindowOp::Stats { target, .. } => (TracedOp::Stats, window_of(Some(target))),
        WindowOp::StatsReset { target } => (TracedOp::StatsReset, window_of(target.as_ref())),
    };
    let (id, generation) = target.unwrap_or_default();
    crate::frame_trace::event("comp_window_control", || (id, code as u64, generation));
}

fn window_of(target: Option<&StatsTarget>) -> Option<(u64, u64)> {
    match target {
        Some(StatsTarget::Window { id, generation }) => Some((*id, *generation)),
        _ => None,
    }
}

/// Merge `extra`'s fields into the object `body`.
fn merged(mut body: Value, extra: Value) -> Value {
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

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

    fn window_reply(&self, object: &ObjectId, id: u64, changed: bool) -> ControlReply {
        // The record can only vanish between the operation and this read if
        // the operation itself destroyed it; say so rather than `busy`.
        let Some(record) = self.surfaces.get(object) else {
            return ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            };
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

    /// Mapped managed windows currently minimised. Every such window is on
    /// the minimise LIFO, so when `restore {}` finds the LIFO empty this is
    /// 0; it is still reported (as `not_found.minimized_count`) so a lost
    /// LIFO entry would show up to the caller instead of hiding.
    fn minimized_window_count(&self) -> usize {
        self.surfaces
            .values()
            .filter(|record| record.mapped && record.minimized && record.role.managed_toplevel())
            .count()
    }

    /// The entry the LIFO pop will restore: the newest one that is still a
    /// mapped, minimised, managed toplevel (the pop discards older invalid
    /// entries on its way down).
    fn next_lifo_restore(&self) -> Option<(ObjectId, SurfaceId)> {
        self.minimized_toplevels.iter().rev().find_map(|object| {
            let record = self.surfaces.get(object)?;
            (record.mapped && record.minimized && record.role.managed_toplevel())
                .then(|| (object.clone(), record.id))
        })
    }

    /// `comp.window.stats`: the window's (or source's) leaves plus the
    /// newest `samples` of each ring.
    fn service_stats(&mut self, target: &StatsTarget, samples: usize) -> ControlReply {
        match target {
            StatsTarget::Window { id, generation } => {
                if let Err(error) = self.resolve_window_target(*id, Some(*generation)) {
                    return ControlReply::WindowTarget { id: *id, error };
                }
                let stats = &self.presentation.stats;
                let empty = PresentationStats::new(stats.epoch_us);
                let window = stats.window(*id, *generation).unwrap_or(&empty);
                ControlReply::Stats(merged(
                    merged(
                        json!({"id": id, "generation": generation}),
                        window.leaves().to_json(),
                    ),
                    window.samples(samples),
                ))
            }
            StatsTarget::Source { id, registration } => {
                let counters = match self.source_target(id, *registration) {
                    Ok(counters) => counters,
                    Err(reply) => return reply,
                };
                ControlReply::Stats(merged(
                    merged(
                        json!({
                            "source": id,
                            "registration": counters.registration,
                            "output": counters.output,
                            "registered_at_us": counters.registered_at_us,
                            "revision": counters.revision,
                        }),
                        serde_json::to_value(counters.leaves()).unwrap_or(Value::Null),
                    ),
                    counters.samples(samples),
                ))
            }
        }
    }

    fn source_target(
        &self,
        id: &str,
        registration: Option<u64>,
    ) -> Result<&presentation::SourceCounters, ControlReply> {
        let Some(counters) = self.presentation.sources.get(id) else {
            return Err(ControlReply::SourceTarget {
                source: id.to_string(),
                error: SourceTargetError::Unknown,
            });
        };
        if let Some(requested) = registration
            && requested != counters.registration
        {
            return Err(ControlReply::SourceTarget {
                source: id.to_string(),
                error: SourceTargetError::StaleTarget {
                    requested,
                    current: counters.registration,
                },
            });
        }
        Ok(counters)
    }

    /// `comp.window.stats.reset`: one window, one source, or (no target)
    /// every window, output and source. Counting restarts now.
    fn service_stats_reset(&mut self, target: Option<&StatsTarget>) -> ControlReply {
        let now = crate::frame_trace::monotonic_us();
        match target {
            None => {
                self.presentation.stats.reset_all(now);
                self.presentation.sources.reset_all(now);
                ControlReply::Stats(json!({"reset": "all", "since_us": now}))
            }
            Some(StatsTarget::Window { id, generation }) => {
                if let Err(error) = self.resolve_window_target(*id, Some(*generation)) {
                    return ControlReply::WindowTarget { id: *id, error };
                }
                self.presentation.stats.reset_window(*id, *generation, now);
                ControlReply::Stats(json!({
                    "reset": "window",
                    "id": id,
                    "generation": generation,
                    "since_us": now,
                }))
            }
            Some(StatsTarget::Source { id, registration }) => {
                let registration = match self.source_target(id, *registration) {
                    Ok(counters) => counters.registration,
                    Err(reply) => return reply,
                };
                self.presentation.sources.reset(id, now);
                ControlReply::Stats(json!({
                    "reset": "source",
                    "source": id,
                    "registration": registration,
                    "since_us": now,
                }))
            }
        }
    }

    /// The `comp.window.*` verbs. A session lock refuses every one that
    /// names or changes a window: the lock owns what is on screen until it
    /// ends. Source stats and a global reset do not reveal a window.
    pub(crate) fn service_window_op(&mut self, op: &WindowOp) -> ControlReply {
        trace_window_control(op);
        let names_window = match op {
            WindowOp::Stats { target, .. } => window_of(Some(target)).is_some(),
            WindowOp::StatsReset { target } => window_of(target.as_ref()).is_some(),
            WindowOp::Minimize { .. } | WindowOp::Restore { .. } => true,
        };
        if names_window && self.session_lock_active() {
            return ControlReply::Locked;
        }
        let (target, minimized) = match *op {
            WindowOp::Stats {
                ref target,
                samples,
            } => return self.service_stats(target, samples),
            WindowOp::StatsReset { ref target } => {
                return self.service_stats_reset(target.as_ref());
            }
            WindowOp::Minimize { id, generation } => ((id, generation), true),
            WindowOp::Restore {
                target: Some(target),
            } => (target, false),
            WindowOp::Restore { target: None } => {
                // Mark first: the first cause recorded for a surface wins,
                // and the restore itself would record "wayland.focus".
                let expected = self.next_lifo_restore();
                if let Some((_, id)) = &expected {
                    self.mark_surface_dirty(*id, "comp.window");
                }
                let restored = self.restore_most_recently_minimized();
                debug_assert_eq!(
                    restored.as_ref(),
                    expected.as_ref().map(|(object, _)| object),
                    "the LIFO pop restores the predicted entry"
                );
                return match (restored, expected) {
                    (Some(object), Some((_, id))) => self.window_reply(&object, id.0, true),
                    (Some(object), None) => {
                        let id = self.surfaces.get(&object).map_or(0, |record| record.id.0);
                        self.window_reply(&object, id, true)
                    }
                    (None, _) => {
                        let minimized_count = self.minimized_window_count();
                        debug_assert_eq!(
                            minimized_count, 0,
                            "every minimised window is on the LIFO"
                        );
                        ControlReply::NotFound { minimized_count }
                    }
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
            Some((before, after)) => self.window_reply(&object, id, before != after),
            None => ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            },
        }
    }
}
