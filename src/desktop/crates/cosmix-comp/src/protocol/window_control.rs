//! Window-addressed Bus control: `{id, generation}` target fencing, the
//! minimise/restore operations shared by `windows.s<id>.minimized` and the
//! `comp.window.*` verbs, the presentation stats verbs, and the
//! event-driven `comp.window.wait` / `close {force}` waiters.

use serde_json::{Value, json};
use smithay::reexports::calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_server::backend::DisconnectReason;

use super::port_observation::SetValidationError;
use super::presentation_stats::PresentationStats;
use super::workspaces::{WorkspaceRefusal, WorkspaceTarget};
use super::*;
use crate::port::{
    ControlReply, PlaceSpec, StatsTarget, WaitSpec, WaitUntil, WindowMatch, WindowOp,
    WorkspaceIndex,
};

impl From<WorkspaceIndex> for WorkspaceTarget {
    fn from(index: WorkspaceIndex) -> Self {
        match index {
            WorkspaceIndex::Absolute(index) => Self::Index(index),
            WorkspaceIndex::Next => Self::Next,
            WorkspaceIndex::Prev => Self::Prev,
        }
    }
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

    /// The entry `restore {}` will restore: rule 8's candidate (the current
    /// workspace's most recently minimised window, else the global one) —
    /// the same predicate `restore_most_recently_minimized` uses, so the
    /// prediction below and the restore cannot diverge.
    fn next_lifo_restore(&self) -> Option<(ObjectId, SurfaceId)> {
        self.lifo_restore_candidate()
    }

    /// Every one-pass `comp.window.*` verb. A session lock refuses them all:
    /// the lock owns what is on screen until it ends.
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
                ControlReply::Body(merged(
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
                ControlReply::Body(merged(
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
            return Err(ControlReply::Refused {
                error: "unknown_source",
                detail: json!({"source": id}),
            });
        };
        if let Some(requested) = registration
            && requested != counters.registration
        {
            return Err(ControlReply::Refused {
                error: "stale_target",
                detail: json!({
                    "source": id,
                    "registration": requested,
                    "current": counters.registration,
                }),
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
                ControlReply::Body(json!({"reset": "all", "since_us": now}))
            }
            Some(StatsTarget::Window { id, generation }) => {
                if let Err(error) = self.resolve_window_target(*id, Some(*generation)) {
                    return ControlReply::WindowTarget { id: *id, error };
                }
                self.presentation.stats.reset_window(*id, *generation, now);
                ControlReply::Body(json!({
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
                ControlReply::Body(json!({
                    "reset": "source",
                    "source": id,
                    "registration": registration,
                    "since_us": now,
                }))
            }
        }
    }

    /// Every `comp.window.*` verb that answers in one pass. A session lock
    /// refuses every one that names or changes a window: the lock owns what
    /// is on screen until it ends. Source stats and a global stats reset do
    /// not name a window, so they still answer. A workspace switch names no
    /// window either but changes what is on screen, so it is refused too
    /// (D12) — the arm is explicit so the reads above are not read as the
    /// rule for it.
    pub(crate) fn service_window_op(&mut self, op: &WindowOp) -> ControlReply {
        let names_window = match op {
            WindowOp::Stats { target, .. } => window_of(Some(target)).is_some(),
            WindowOp::StatsReset { target } => window_of(target.as_ref()).is_some(),
            WindowOp::SwitchWorkspace { .. } => true,
            _ => true,
        };
        if names_window && self.session_lock_active() {
            return ControlReply::Locked;
        }
        let (code, id, generation) = match op {
            WindowOp::Minimize { id, generation } => (1, *id, *generation),
            WindowOp::Restore { target } => {
                let (id, generation) = target.unwrap_or_default();
                (2, id, generation)
            }
            WindowOp::Focus { id, generation, .. } => (3, *id, *generation),
            WindowOp::Raise { id, generation } => (4, *id, *generation),
            WindowOp::Close { id, generation } => (5, *id, *generation),
            WindowOp::Place(spec) => (6, spec.id, spec.generation),
            WindowOp::Stats { target, .. } => {
                let (id, generation) = window_of(Some(target)).unwrap_or_default();
                (7, id, generation)
            }
            WindowOp::StatsReset { target } => {
                let (id, generation) = window_of(target.as_ref()).unwrap_or_default();
                (8, id, generation)
            }
            WindowOp::SwitchWorkspace { .. } => (9, 0, 0),
            WindowOp::SendToWorkspace { id, generation, .. } => (10, *id, *generation),
        };
        crate::frame_trace::event("comp_window_control", || (id, code, generation));
        match op {
            WindowOp::Minimize { .. } | WindowOp::Restore { .. } => self.service_minimize_op(op),
            WindowOp::Focus {
                id,
                generation,
                raise,
            } => self.service_window_focus(*id, *generation, *raise),
            WindowOp::Raise { id, generation } => self.service_window_raise(*id, *generation),
            WindowOp::Close { id, generation } => {
                let object = match self.resolve_window_target(*id, Some(*generation)) {
                    Ok(object) => object,
                    Err(error) => return ControlReply::WindowTarget { id: *id, error },
                };
                let surface = self.surfaces[&object].role.wl_surface().clone();
                self.close_managed_toplevel(&surface);
                ControlReply::Body(json!({
                    "id": id,
                    "generation": generation,
                    "closed": "polite",
                }))
            }
            WindowOp::Place(spec) => self.service_window_place(spec),
            WindowOp::Stats { target, samples } => self.service_stats(target, *samples),
            WindowOp::StatsReset { target } => self.service_stats_reset(target.as_ref()),
            WindowOp::SwitchWorkspace {
                output,
                index,
                wrap,
            } => self.service_workspace_switch(output.as_deref(), *index, *wrap),
            WindowOp::SendToWorkspace {
                id,
                generation,
                index,
                follow,
            } => self.service_send_to_workspace(*id, *generation, *index, *follow),
        }
    }

    /// `comp.workspace.switch`: the contract's refusal vocabulary over the
    /// core primitive — `invalid_value` for an index outside `1..=count`
    /// or an unknown output (D13), `at_end` for `next`/`prev` at an end
    /// without `wrap`.
    fn service_workspace_switch(
        &mut self,
        output: Option<&str>,
        index: WorkspaceIndex,
        wrap: bool,
    ) -> ControlReply {
        match self.switch_workspace(output, index.into(), wrap) {
            Ok(switched) => ControlReply::Body(json!({
                "output": switched.output,
                "from": switched.from,
                "to": switched.to,
            })),
            Err(refusal) => {
                // `at_end` names the output it was at by its `o_<slug>`
                // KEY, as the success reply does — a request by output name
                // resolved before the ring did, so the key is what a caller
                // keying replies by output can match. Only an unknown
                // output fails to resolve, and that refusal echoes nothing.
                let output = self.resolve_workspace_output(output);
                workspace_refusal(refusal, output.as_deref(), 0)
            }
        }
    }

    /// `comp.window.send_to_workspace`: the `{id, generation}` fence, then
    /// the move; with `follow`, a switch to the window's new workspace and
    /// its activation (D9). `next`/`prev` are relative to the window's own
    /// workspace and always wrap. The follow is gated exactly as every
    /// switch-first path is (`workspace_switch_allowed_for`: inert under an
    /// exclusive layer, D18, for a minimised window and behind the KMS
    /// input gate); when allowed, the move and the switch are ONE settle
    /// (`move_window_and_follow`) so no bystander on either workspace
    /// takes the keyboard in between, and when not, the move alone runs.
    /// The reply's `followed` is `workspaces.current == index` READ BACK
    /// after the attempt, not a claim about the switch: it is true with no
    /// switch and no activation when the window was already on the current
    /// workspace and the gate held (a minimised window sent to the
    /// workspace it is on answers `followed:true` and stays minimised), and
    /// with no default output `current` reads 1. The move has happened by
    /// then and cannot be refused after the fact.
    fn service_send_to_workspace(
        &mut self,
        id: u64,
        generation: u64,
        index: WorkspaceIndex,
        follow: bool,
    ) -> ControlReply {
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        // No `comp.window` mark, unlike the sibling verbs: every accepted
        // move marks the FULL snapshot dirty (`workspace.move`, D7 — every
        // row's visibility and the `workspaces.*` counts may change), and a
        // full-snapshot diff carries that one cause and discards the
        // per-surface marks, so a mark planted here could never be read.
        // Where it could — a refused move (an index above `count`) or a
        // send to the workspace the window is on, both of which change
        // nothing — it would only blame `comp.window` for the next
        // unrelated edge on this surface. `send_to_workspace_refused_index_
        // attributes_nothing` pins both halves.
        let follow_now = follow && self.workspace_switch_allowed_for(&object).is_some();
        let moved = if follow_now {
            self.move_window_and_follow(&object, index.into())
        } else {
            self.move_window_to_workspace(&object, index.into())
        };
        let (_, to) = match moved {
            Ok(moved) => moved,
            Err(refusal) => return workspace_refusal(refusal, None, id),
        };
        let mut body = json!({
            "id": id,
            "generation": generation,
            "index": to,
        });
        if follow {
            // Read back, not assumed: with no default output there was
            // nothing to switch, and `current` reads 1 regardless.
            let followed = self.workspace_current() == to;
            if followed {
                let surface = self.surfaces[&object].role.wl_surface().clone();
                self.activate_managed_window(&surface);
            }
            body["followed"] = json!(followed);
        }
        ControlReply::Body(body)
    }
}

/// The contract's wire form of a core refusal. `output` is the requested
/// output, echoed on `at_end`; `id` the window a send named.
fn workspace_refusal(refusal: WorkspaceRefusal, output: Option<&str>, id: u64) -> ControlReply {
    match refusal {
        WorkspaceRefusal::InvalidIndex { count } => {
            ControlReply::Validation(SetValidationError::InvalidValue {
                path: "index".into(),
                expected: "unsigned integer",
                range: workspace_index_range(count),
            })
        }
        WorkspaceRefusal::AtEnd { from, count } => ControlReply::refused(
            "at_end",
            json!({"output": output, "from": from, "count": count}),
        ),
        WorkspaceRefusal::UnknownOutput => {
            ControlReply::Validation(SetValidationError::InvalidValue {
                path: "output".into(),
                expected: "outputs.<key> key or output name",
                range: "an existing output",
            })
        }
        WorkspaceRefusal::InvalidCount { max: _ } => {
            ControlReply::Validation(SetValidationError::InvalidValue {
                path: "count".into(),
                expected: "unsigned integer",
                range: "1..=16",
            })
        }
        // Unreachable after `resolve_window_target` (the record is a mapped
        // managed toplevel with a stamped workspace), kept honest anyway.
        WorkspaceRefusal::NotAWindow => ControlReply::WindowTarget {
            id,
            error: WindowTargetError::NotMapped,
        },
    }
}

/// `range` for an index refusal, as `1..=<count>`. `SetValidationError`
/// carries `&'static str`, so the sixteen possible counts are spelled out.
fn workspace_index_range(count: u32) -> &'static str {
    const RANGES: [&str; 16] = [
        "1..=1", "1..=2", "1..=3", "1..=4", "1..=5", "1..=6", "1..=7", "1..=8", "1..=9", "1..=10",
        "1..=11", "1..=12", "1..=13", "1..=14", "1..=15", "1..=16",
    ];
    RANGES
        .get(count.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("1..=workspaces.count")
}

impl WaylandState {
    fn service_minimize_op(&mut self, op: &WindowOp) -> ControlReply {
        let (target, minimized) = match *op {
            WindowOp::Minimize { id, generation } => ((id, generation), true),
            WindowOp::Restore {
                target: Some(target),
            } => (target, false),
            WindowOp::Restore { target: None } => {
                // Mark first: the first cause recorded for a surface wins,
                // and the restore itself would record "wayland.focus".
                // When rule 8's candidate is on another workspace the
                // restore switches, and a switch is a full-snapshot cause
                // (`workspace.switch`) that discards the per-surface marks
                // — as `service_send_to_workspace` documents for its own
                // accepted move — so the edges then read the larger
                // change, not `comp.window`. Deliberate: the mark is only
                // ever read when nothing bigger happened.
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
            WindowOp::Focus { .. }
            | WindowOp::Raise { .. }
            | WindowOp::Close { .. }
            | WindowOp::Place(_)
            | WindowOp::Stats { .. }
            | WindowOp::StatsReset { .. }
            | WindowOp::SwitchWorkspace { .. }
            | WindowOp::SendToWorkspace { .. } => {
                unreachable!("only minimise and restore are routed here")
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

/// Registered `comp.window.wait` and `comp.window.close {force}` waiters.
/// Each is checked once per dispatch cycle, after the cycle's state edges,
/// and holds one calloop timer for its deadline: nothing polls.
#[derive(Default)]
pub(crate) struct WindowWaiters {
    next: u64,
    pub(super) waiters: BTreeMap<u64, WindowWaiter>,
    /// When each window last mapped: its generation, presented count and
    /// CLOCK_MONOTONIC time. `until: presented` needs a newer frame that was
    /// shown at or after that time, so neither an earlier mapping's count
    /// (the counters live as long as the `wl_surface`) nor a late report of a
    /// pre-hide frame satisfies it.
    presented_base: HashMap<SurfaceId, MappingBase>,
    /// Per window root: its generation and the newest frame time at which
    /// the renderer showed any surface of it (new content, or content hidden
    /// at map time and since exposed), from the same frame reports
    /// `comp.window.stats` reads. Needs no `wp_presentation` feedback, which
    /// most clients never request. Sized like `presented_base`: at
    /// `MAX_PRESENTED_BASES` a new entry prunes DEAD surfaces only, so the
    /// table holds max(1024, live window roots) — a pruning threshold, not a
    /// hard cap. Live roots are never evicted: dropping one's evidence would
    /// turn a shown window back into a false `presented` timeout. Past 1024
    /// live roots each first-time insert rescans (O(live roots)).
    shown: HashMap<SurfaceId, (u64, u64)>,
}

const MAX_PRESENTED_BASES: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct MappingBase {
    generation: u64,
    presented: u64,
    mapped_at_us: u64,
}

pub(super) struct WindowWaiter {
    kind: WaiterKind,
    started: Instant,
    timer: Option<RegistrationToken>,
    reply: tokio::sync::oneshot::Sender<ControlReply>,
}

enum WaiterKind {
    Wait(WaitSpec),
    ForceClose { id: u64, generation: u64 },
}

fn geometry_size(record: &SurfaceRecord) -> (i32, i32) {
    record.committed_window_geometry.map_or(
        (
            record.layout.width.round() as i32,
            record.layout.height.round() as i32,
        ),
        |geometry| {
            (
                geometry.width.round() as i32,
                geometry.height.round() as i32,
            )
        },
    )
}

fn names_match(filter: &WindowMatch, record: &SurfaceRecord) -> bool {
    filter
        .app_id
        .as_deref()
        .is_none_or(|app_id| record.app_id.as_deref() == Some(app_id))
        && filter
            .title
            .as_deref()
            .is_none_or(|title| record.title.as_deref() == Some(title))
        && filter.title_contains.as_deref().is_none_or(|needle| {
            record
                .title
                .as_deref()
                .is_some_and(|title| title.contains(needle))
        })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl WaylandState {
    fn service_window_focus(&mut self, id: u64, generation: u64, raise: bool) -> ControlReply {
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let Some(record) = self.surfaces.get(&object) else {
            return ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            };
        };
        // Rule 6 (F1.2): an off-workspace window is brought on screen by
        // switching to its workspace, never by pulling it across, so the
        // ladder below sees it as on-current. Both the raise and the
        // focus-only path inherit; `service_window_op` already refused a
        // session lock before reaching here. The switch happens only for a
        // window the ladder's workspace-independent rungs would let take
        // focus (`ensure_workspace_shown` checks the same terms): a refused
        // verb must not change the desktop. Mark first, as every sibling
        // verb does: the first cause recorded for a surface wins, and the
        // switch itself marks "workspace.switch".
        let may_focus = self.highest_exclusive_layer().is_none()
            && !record.minimized
            && self.surface_is_input_presentable(record);
        let mark = may_focus.then(|| {
            let mark = self.plant_surface_mark(id, "comp.window");
            self.ensure_workspace_shown(&object);
            mark
        });
        // Re-fetched, not indexed: the switch settles the scene in between
        // and the record's liveness across that is not an invariant the
        // settle promises. A record gone here changed nothing the verb can
        // own, so its planted mark goes with it.
        let Some(record) = self.surfaces.get(&object) else {
            if let Some(mark) = mark {
                self.unplant_surface_mark(mark);
            }
            return ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            };
        };
        let surface = record.role.wl_surface().clone();
        // The same candidacy Alt+Tab uses; a refusal says which gate held.
        // The workspace-independent rungs (the `may_focus` terms) come
        // before `not_visible`: they are what kept the switch from running,
        // so an off-workspace window refused by one of them names that gate,
        // not the visibility the switch would have given it.
        let reason = if self.highest_exclusive_layer().is_some() {
            Some("exclusive_layer")
        } else if record.minimized {
            Some("minimized")
        } else if !self.surface_is_input_presentable(record) {
            Some("not_presentable")
        } else if !record.layout.visible {
            Some("not_visible")
        } else {
            None
        };
        if reason.is_none() {
            if raise {
                self.activate_managed_window(&surface);
            } else {
                self.arbitrate_keyboard_focus(Some(surface), false, false);
            }
        } else if let Some(mark) = mark {
            // `not_visible` past the `may_focus` terms: the switch did not
            // run (no default output) or did not make the window visible.
            // Nothing changed, so the refusal attributes nothing — the
            // mark planted above would otherwise blame `comp.window` for
            // the next unrelated edge on this surface.
            self.unplant_surface_mark(mark);
        }
        let focused = self
            .surfaces
            .get(&object)
            .is_some_and(|record| record.focused);
        let mut body = json!({"id": id, "generation": generation, "focused": focused});
        if let Some(reason) = reason.or((!focused).then_some("refused")) {
            body["reason"] = json!(reason);
        }
        ControlReply::Body(body)
    }

    /// Stacking only: never a bring-into-view path. Rule 6 (F1.2) names
    /// focus, restore and the activation requests; a raise of an
    /// off-workspace (or minimised) window restacks it in place and replies
    /// `raised` from the z delta, exactly as it would for a covered window
    /// on the current workspace — the caller that wants it on screen uses
    /// `focus` or `restore`. The manual says the same under
    /// `comp.window.raise`; `raise_on_an_off_workspace_or_minimised_window_
    /// never_switches_or_unminimises` pins it.
    fn service_window_raise(&mut self, id: u64, generation: u64) -> ControlReply {
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let record = &self.surfaces[&object];
        let before = record.layout.z;
        let surface = record.role.wl_surface().clone();
        self.mark_surface_dirty(SurfaceId(id), "comp.window");
        self.raise_surface(&surface);
        // Stacking decides what is under the cursor.
        self.retarget_pointer_after_visibility_change();
        let after = self.surfaces[&object].layout.z;
        ControlReply::Body(json!({
            "id": id,
            "generation": generation,
            "raised": after != before,
        }))
    }

    /// The output row a place addresses: the named one, else the window's
    /// own, else the default output.
    fn place_output(
        &self,
        id: u64,
        requested: Option<&str>,
    ) -> Result<(String, port_snapshot::OutputSnapshot), ControlReply> {
        let projection = port_snapshot::project_outputs(self).ok_or(ControlReply::Busy)?;
        if let Some(requested) = requested {
            return projection
                .rows
                .into_iter()
                .find(|(key, row)| key == requested || row.name == requested)
                .ok_or_else(|| {
                    ControlReply::refused("unknown_output", json!({"output": requested}))
                });
        }
        let current = port_snapshot::project_surface_by_id(self, SurfaceId(id), &projection.keys)
            .and_then(|row| row.output);
        let default = self
            .backend
            .default_output()
            .and_then(|output| port_snapshot::project_output(self, &output))
            .map(|(key, _)| key);
        current
            .into_iter()
            .chain(default)
            .find_map(|key| projection.rows.get(&key).cloned().map(|row| (key, row)))
            .ok_or_else(|| ControlReply::refused("unknown_output", json!({"output": null})))
    }

    fn service_window_place(&mut self, spec: &PlaceSpec) -> ControlReply {
        let object = match self.resolve_window_target(spec.id, Some(spec.generation)) {
            Ok(object) => object,
            Err(error) => {
                return ControlReply::WindowTarget { id: spec.id, error };
            }
        };
        let record = &self.surfaces[&object];
        let maximized = record.committed_maximized || record.requested_maximized;
        let fullscreen = record.committed_fullscreen || record.requested_fullscreen;
        if maximized || fullscreen {
            return ControlReply::refused(
                "invalid_state",
                json!({"id": spec.id, "maximized": maximized, "fullscreen": fullscreen}),
            );
        }
        let surface = record.role.wl_surface().clone();
        let origin = record.window_origin;
        // An absent axis keeps the size the window really has, as the
        // interactive resize does, not the last size comp asked for.
        let current = geometry_size(record);
        let (key, row) = match self.place_output(spec.id, spec.output.as_deref()) {
            Ok(output) => output,
            Err(reply) => return reply,
        };
        // An absent coordinate keeps the window's offset within its output
        // (the old one, when the place changes outputs).
        let (old_x, old_y) = match self.place_output(spec.id, None) {
            Ok((_, old)) => (old.x as f32, old.y as f32),
            Err(_) => (row.x as f32, row.y as f32),
        };
        let target = (
            row.x as f32 + spec.x.map_or(origin.0 - old_x, |x| x as f32),
            row.y as f32 + spec.y.map_or(origin.1 - old_y, |y| y as f32),
        );
        let requested = (spec.width.is_some() || spec.height.is_some()).then(|| {
            self.clamp_window_size(
                &surface,
                (
                    spec.width.unwrap_or(current.0),
                    spec.height.unwrap_or(current.1),
                ),
            )
        });
        // A window placed wholly off every output could never be seen or
        // clicked again by a caller that trusted the reply.
        let size = requested.unwrap_or(current);
        let on_output = port_snapshot::project_outputs(self).is_some_and(|projection| {
            projection.rows.values().any(|output| {
                target.0 < (output.x as f32 + output.width as f32)
                    && target.0 + size.0 as f32 > output.x as f32
                    && target.1 < (output.y as f32 + output.height as f32)
                    && target.1 + size.1 as f32 > output.y as f32
            })
        });
        if !on_output {
            return ControlReply::refused(
                "off_output",
                json!({
                    "id": spec.id,
                    "x": target.0 - row.x as f32,
                    "y": target.1 - row.y as f32,
                    "width": size.0,
                    "height": size.1,
                }),
            );
        }
        // A client move/resize in progress would steer the window straight
        // back.
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| *interactive == surface)
        {
            self.finish_interactive_pointer(true);
        }
        let configure_pending = requested.is_some_and(|size| {
            self.resize_window_to(&surface, target, size, "comp.window")
                .unwrap_or(false)
        });
        if !configure_pending {
            self.move_window_to(&surface, target, "comp.window");
        }
        self.retarget_pointer_after_visibility_change();
        let placed = self
            .surfaces
            .get(&object)
            .map_or(target, |record| record.window_origin);
        ControlReply::Body(json!({
            "id": spec.id,
            "generation": spec.generation,
            "output": key,
            "window_x": placed.0 - row.x as f32,
            "window_y": placed.1 - row.y as f32,
            "requested": requested.map(|(width, height)| json!({"width": width, "height": height})),
            "configure_pending": configure_pending,
        }))
    }

    /// A window is about to map: remember how many frames it had presented
    /// before, so `until: presented` waits for a frame of this mapping.
    pub(crate) fn note_window_mapping(&mut self, id: SurfaceId, generation: u64) {
        let count = self.presentation.ledger.counters(id).presented;
        let objects = &self.surface_objects;
        let bases = &mut self.window_waiters.presented_base;
        if bases.len() >= MAX_PRESENTED_BASES {
            bases.retain(|id, _| objects.contains_key(id));
        }
        bases.insert(
            id,
            MappingBase {
                generation,
                presented: count,
                mapped_at_us: monotonic_micros(),
            },
        );
    }

    /// The renderer showed new content of window root `id` in a frame at
    /// `tv_us` (called from the frame report, alongside the stats fold).
    pub(crate) fn note_window_shown(&mut self, id: SurfaceId, generation: u64, tv_us: u64) {
        let objects = &self.surface_objects;
        let shown = &mut self.window_waiters.shown;
        if shown.len() >= MAX_PRESENTED_BASES && !shown.contains_key(&id) {
            shown.retain(|id, _| objects.contains_key(id));
        }
        let entry = shown.entry(id).or_insert((generation, tv_us));
        if entry.0 != generation {
            *entry = (generation, tv_us);
        }
        // A late report of an older frame never moves the evidence back.
        entry.1 = entry.1.max(tv_us);
    }

    /// A frame of this mapping was shown: renderer-shown content (what
    /// `comp.window.stats` counts) or, additionally, `wp_presentation`
    /// feedback. Either must be at or after the map time.
    ///
    /// Known limit, shared by both signals: the test is the frame's
    /// presentation time against the map time, not when the frame was
    /// extracted. A frame extracted before an unmap but presented after a
    /// remap in the same frame period can count for the new mapping.
    fn presented_since_map(&self, record: &SurfaceRecord) -> bool {
        let counters = self.presentation.ledger.counters(record.id);
        let (base, mapped_at_us) = self
            .window_waiters
            .presented_base
            .get(&record.id)
            .filter(|base| base.generation == record.generation)
            .map_or((0, 0), |base| (base.presented, base.mapped_at_us));
        let shown = self
            .window_waiters
            .shown
            .get(&record.id)
            .is_some_and(|&(generation, tv_us)| {
                generation == record.generation && tv_us >= mapped_at_us
            });
        let fed_back = counters.presented > base
            && counters
                .last_presented_us
                .is_some_and(|presented_us| presented_us >= mapped_at_us);
        shown || fed_back
    }

    /// The target's role ended or was replaced (as opposed to a live window
    /// that is merely unmapped, which a hide-on-close app is).
    fn window_gone(&self, id: u64, generation: u64) -> bool {
        matches!(
            self.resolve_window_target(id, Some(generation)),
            Err(WindowTargetError::UnknownWindow | WindowTargetError::StaleTarget { .. })
        )
    }

    pub(crate) fn start_window_wait(
        &mut self,
        spec: WaitSpec,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
        admitted: Instant,
    ) {
        let timeout = spec.timeout;
        crate::frame_trace::event("comp_window_control", || {
            (spec.window.id.unwrap_or(0), 7, millis(timeout))
        });
        // An id that was never handed out would otherwise satisfy `gone`
        // at once, hiding a typo.
        if let Some(id) = spec.window.id
            && (id == 0 || id >= self.next_surface_id)
        {
            let _ = reply.send(ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            });
            return;
        }
        self.register_waiter(WaiterKind::Wait(spec), timeout, admitted, reply);
    }

    /// `comp.window.close {force}`: the polite close now, the kill only if
    /// the same `{id, generation}` is still alive (mapped or not) at the
    /// deadline.
    pub(crate) fn start_force_close(
        &mut self,
        id: u64,
        generation: u64,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
        admitted: Instant,
    ) {
        crate::frame_trace::event("comp_window_control", || (id, 8, generation));
        if self.session_lock_active() {
            let _ = reply.send(ControlReply::Locked);
            return;
        }
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => {
                let _ = reply.send(ControlReply::WindowTarget { id, error });
                return;
            }
        };
        let record = &self.surfaces[&object];
        let x11 = !matches!(record.role, SurfaceRole::Toplevel(_));
        let surface = record.role.wl_surface().clone();
        self.close_managed_toplevel(&surface);
        if x11 {
            // An X11 window's Wayland client is Xwayland itself, and the XWM
            // offers no per-client kill: refuse now rather than after the
            // wait. The polite close was still sent.
            let _ = reply.send(ControlReply::refused(
                "still_open",
                json!({
                    "id": id,
                    "generation": generation,
                    "reason": "x11_kill_unsupported",
                    "polite_close_sent": true,
                }),
            ));
            return;
        }
        self.register_waiter(
            WaiterKind::ForceClose { id, generation },
            timeout,
            admitted,
            reply,
        );
    }

    fn register_waiter(
        &mut self,
        kind: WaiterKind,
        timeout: Duration,
        admitted: Instant,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
    ) {
        let key = self.window_waiters.next;
        self.window_waiters.next = key.wrapping_add(1);
        // The caller's budget started at admission, not here.
        let remaining = timeout.saturating_sub(admitted.elapsed());
        let timer = self.capture_loop_handle.insert_source(
            Timer::from_duration(remaining),
            move |_, _, state| {
                state.expire_window_waiter(key);
                TimeoutAction::Drop
            },
        );
        let timer = match timer {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "window wait timer unavailable");
                let _ = reply.send(ControlReply::Busy);
                return;
            }
        };
        self.window_waiters.waiters.insert(
            key,
            WindowWaiter {
                kind,
                started: admitted,
                timer: Some(timer),
                reply,
            },
        );
        // A condition that already holds is answered now, not at the next
        // edge.
        self.service_window_waiters();
    }

    /// Resolve every waiter whose condition now holds. Runs at the end of
    /// each observation pass, after the cycle's state has settled.
    pub(crate) fn service_window_waiters(&mut self) {
        if self.window_waiters.waiters.is_empty() {
            return;
        }
        let keys = self
            .window_waiters
            .waiters
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(waiter) = self.window_waiters.waiters.get(&key) else {
                continue;
            };
            if waiter.reply.is_closed() {
                self.finish_window_waiter(key, None);
                continue;
            }
            let waited_ms = millis(waiter.started.elapsed());
            let outcome = match &waiter.kind {
                WaiterKind::Wait(spec) => self
                    .wait_outcome(spec)
                    .map(|window| wait_reply(spec, window, waited_ms)),
                WaiterKind::ForceClose { id, generation } => self
                    .window_gone(*id, *generation)
                    .then(|| close_reply(*id, *generation, "gone", waited_ms)),
            };
            if let Some(reply) = outcome {
                self.finish_window_waiter(key, Some(reply));
            }
        }
    }

    fn finish_window_waiter(&mut self, key: u64, reply: Option<ControlReply>) {
        let Some(waiter) = self.window_waiters.waiters.remove(&key) else {
            return;
        };
        if let Some(timer) = waiter.timer {
            self.capture_loop_handle.remove(timer);
        }
        if let Some(reply) = reply {
            let _ = waiter.reply.send(reply);
        }
    }

    /// The deadline timer fired (and drops itself).
    fn expire_window_waiter(&mut self, key: u64) {
        let Some(waiter) = self.window_waiters.waiters.remove(&key) else {
            return;
        };
        // Nobody is waiting any more: no side effect on their behalf.
        if waiter.reply.is_closed() {
            return;
        }
        let waited_ms = millis(waiter.started.elapsed());
        let reply = match &waiter.kind {
            // An edge in this very dispatch may have beaten the timer.
            WaiterKind::Wait(spec) => match self.wait_outcome(spec) {
                Some(window) => wait_reply(spec, window, waited_ms),
                None => ControlReply::refused(
                    "timeout",
                    json!({"until": spec.until.name(), "waited_ms": waited_ms}),
                ),
            },
            WaiterKind::ForceClose { id, generation } => {
                self.force_close_deadline(*id, *generation, waited_ms)
            }
        };
        let _ = waiter.reply.send(reply);
    }

    fn force_close_deadline(&mut self, id: u64, generation: u64, waited_ms: u64) -> ControlReply {
        if self.window_gone(id, generation) {
            return close_reply(id, generation, "gone", waited_ms);
        }
        // The lock owns the screen; no kill lands while it is up.
        if self.session_lock_active() {
            return ControlReply::Locked;
        }
        let Some(record) = self
            .surface_objects
            .get(&SurfaceId(id))
            .and_then(|object| self.surfaces.get(object))
        else {
            return close_reply(id, generation, "gone", waited_ms);
        };
        let mapped = record.mapped;
        if !matches!(record.role, SurfaceRole::Toplevel(_)) {
            return ControlReply::refused(
                "still_open",
                json!({"id": id, "generation": generation, "reason": "x11_kill_unsupported"}),
            );
        }
        let Some(client) = record.role.wl_surface().client() else {
            return close_reply(id, generation, "gone", waited_ms);
        };
        let pid = client
            .get_credentials(&self.display_handle)
            .ok()
            .map(|credentials| credentials.pid);
        let mut windows = self
            .surfaces
            .values()
            .filter(|record| {
                record.role.managed_toplevel()
                    && record.role.wl_surface().client().as_ref() == Some(&client)
            })
            .map(|record| record.id.0)
            .collect::<Vec<_>>();
        windows.sort_unstable();
        tracing::info!(
            id,
            ?pid,
            mapped,
            ?windows,
            "comp.window.close force: killing the client"
        );
        self.display_handle
            .backend_handle()
            .kill_client(client.id(), DisconnectReason::ConnectionClosed);
        let ControlReply::Body(mut body) = close_reply(id, generation, "killed", waited_ms) else {
            unreachable!("close_reply builds a body");
        };
        body["window"] = json!(if mapped { "mapped" } else { "unmapped" });
        body["scope"] = json!("client");
        body["pid"] = json!(pid);
        body["windows"] = json!(windows);
        ControlReply::Body(body)
    }

    /// The window row a wait resolves to (`null` for `unmapped` / `gone`),
    /// or `None` while the condition does not hold.
    fn wait_outcome(&self, spec: &WaitSpec) -> Option<Value> {
        // Under a session lock the read tree hides every window, so a wait
        // learns nothing from it either: only whether a named id's role
        // ended or unmapped, which `surfaces.*` still shows.
        if self.session_lock_active()
            && !(spec.window.id.is_some()
                && matches!(spec.until, WaitUntil::Gone | WaitUntil::Unmapped))
        {
            return None;
        }
        let current_workspace = self.workspace_current();
        let holds = |record: &SurfaceRecord| match spec.until {
            WaitUntil::Mapped => true,
            WaitUntil::Visible => record.layout.visible && !record.minimized,
            // Evidence of a shown frame is retained, so it is decided now:
            // a window since minimised or moved off the current workspace is
            // not presented, however recently it was shown.
            WaitUntil::Presented => {
                !record.minimized
                    && super::workspaces::on_workspace(record, current_workspace)
                    && self.presented_since_map(record)
            }
            WaitUntil::Size { width, height } => geometry_size(record) == (width, height),
            WaitUntil::Focused => record.focused,
            WaitUntil::Unmapped | WaitUntil::Gone => false,
        };
        let live = |record: &SurfaceRecord| {
            record.mapped && record.role.managed_toplevel() && names_match(&spec.window, record)
        };
        if let Some(id) = spec.window.id {
            let record = self
                .surface_objects
                .get(&SurfaceId(id))
                .and_then(|object| self.surfaces.get(object))
                .filter(|record| {
                    !matches!(record.role, SurfaceRole::Dormant(_))
                        && spec
                            .window
                            .generation
                            .is_none_or(|generation| generation == record.generation)
                });
            return match spec.until {
                WaitUntil::Gone => record.is_none().then_some(Value::Null),
                WaitUntil::Unmapped => record
                    .is_none_or(|record| !record.mapped)
                    .then_some(Value::Null),
                _ => record
                    .filter(|record| live(record) && holds(record))
                    .map(|record| self.window_row(record.id)),
            };
        }
        match spec.until {
            WaitUntil::Gone | WaitUntil::Unmapped => self
                .surfaces
                .values()
                .all(|record| !live(record))
                .then_some(Value::Null),
            _ => self
                .surfaces
                .values()
                .filter(|record| live(record) && holds(record))
                .min_by_key(|record| record.id.0)
                .map(|record| self.window_row(record.id)),
        }
    }

    fn window_row(&self, id: SurfaceId) -> Value {
        port_snapshot::project_outputs(self)
            .and_then(|projection| port_snapshot::project_surface_by_id(self, id, &projection.keys))
            .and_then(|row| serde_json::to_value(port_snapshot::project_window_row(&row)).ok())
            .unwrap_or_else(|| json!({"id": id.0}))
    }
}

fn wait_reply(spec: &WaitSpec, window: Value, waited_ms: u64) -> ControlReply {
    ControlReply::Body(json!({
        "window": window,
        "until": spec.until.name(),
        "waited_ms": waited_ms,
    }))
}

fn close_reply(id: u64, generation: u64, closed: &str, waited_ms: u64) -> ControlReply {
    ControlReply::Body(json!({
        "id": id,
        "generation": generation,
        "closed": closed,
        "waited_ms": waited_ms,
    }))
}
