//! Protocol owner of revision invalidation and callback eligibility.
use super::*;
use crate::occlusion::{Bounds, Bridge, CommittedOpacity, Scene, SceneSurface, TreeVisibility};

#[derive(PartialEq)]
struct CoverageInputs {
    generation: u64,
    buffer_size: Option<(u32, u32)>,
    format_opaque: bool,
    size: (f32, f32),
    source: Option<TextureSourceRect>,
    transform: SurfaceTransform,
    opacity: CommittedOpacity,
}

#[derive(Default)]
pub(super) struct OcclusionRuntime {
    pub bridge: Bridge,
    pub decisions: HashMap<SurfaceId, TreeVisibility>,
    pub revision: u64,
    pub decision_revisions: HashMap<SurfaceId, u64>,
    opacity: HashMap<SurfaceId, (u64, u64, CommittedOpacity)>,
    // Updated at every accepted applied transaction, not only at dispatch
    // reconciliation: A -> B -> A in one dispatch must still retire B evidence.
    coverage_epochs: HashMap<SurfaceId, (CoverageInputs, u64)>,
    refused_opacity: HashSet<SurfaceId>,
    scene_indices: HashMap<SurfaceId, usize>,
    pub withheld: HashMap<SurfaceId, HashSet<ObjectId>>,
    /// Per occluded root: when its throttled trickle last fired (or when it
    /// was first seen occluded). Entries leave with the occlusion.
    pub trickle_at: HashMap<SurfaceId, u32>,
    /// Tests drive the trickle clock by hand; it stays frozen unless a test
    /// advances it, so wall-clock time in a slow run cannot fire a trickle.
    #[cfg(test)]
    pub trickle_clock_ms: u32,
    #[cfg(test)]
    pub scene_rebuilds: usize,
}

/// A covered FIFO client (Mesa's default present mode) blocks in present until
/// its frame callback completes and so cannot even answer a configure while
/// withheld. Complete the oldest retained callback of each surface in an
/// occluded tree this often — the
/// ~1 Hz KWin/Mutter give hidden windows — instead of starving it outright.
pub(super) const OCCLUDED_TRICKLE_MS: u32 = 1_000;
impl OcclusionRuntime {
    pub fn is_occluded(&self, id: SurfaceId) -> bool {
        self.decisions.get(&id) == Some(&TreeVisibility::Occluded)
    }
}

impl WaylandState {
    pub(super) fn invalidate_committed_opacity(&mut self, surface: &WlSurface) {
        if let Some(record) = self.surfaces.get(&surface.id()) {
            self.occlusion.opacity.remove(&record.id);
            let replacing = compositor::with_states(surface, |states| {
                matches!(
                    states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer,
                    Some(BufferAssignment::NewBuffer(_))
                )
            });
            if replacing {
                self.occlusion.refused_opacity.insert(record.id);
            }
        }
    }

    pub(super) fn capture_bufferless_opacity(&mut self, surface: &WlSurface) {
        if self
            .surfaces
            .get(&surface.id())
            .is_some_and(|r| self.occlusion.refused_opacity.contains(&r.id))
        {
            return;
        }
        self.capture_committed_opacity(surface);
    }

    /// Called only after accepting a new buffer or a valid bufferless applied
    /// transaction. Soft-refused buffers cannot lend newer opacity to retained
    /// old content. The renderer additionally checks the installed sequence.
    pub(super) fn capture_committed_opacity(&mut self, surface: &WlSurface) {
        let Some(record) = self.surfaces.get(&surface.id()) else {
            return;
        };
        let opacity =
            compositor::with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let operations = attributes.current().opaque_region.as_ref().map_or(
                    Some(Vec::new()),
                    |region| {
                        (region.rects.len() <= 256).then(|| {
                            region
                                .rects
                                .iter()
                                .map(|(kind, r)| {
                                    (
                                        matches!(kind, RectangleKind::Add),
                                        Bounds::new(
                                            f64::from(r.loc.x),
                                            f64::from(r.loc.y),
                                            f64::from(r.size.w),
                                            f64::from(r.size.h),
                                        ),
                                    )
                                })
                                .collect()
                        })
                    },
                );
                CommittedOpacity { operations }
            });
        self.occlusion.opacity.insert(
            record.id,
            (record.generation, record.content_seq, opacity.clone()),
        );
        let inputs = CoverageInputs {
            generation: record.generation,
            buffer_size: record.buffer_dimensions,
            format_opaque: record
                .shm_backing
                .as_ref()
                .is_some_and(|b| b.format == wl_shm::Format::Xrgb8888)
                || record
                    .dmabuf_backing
                    .as_ref()
                    .is_some_and(|b| b.descriptor.is_opaque()),
            size: (record.layout.width, record.layout.height),
            source: record.layout.source,
            transform: record.layout.transform,
            opacity,
        };
        if self
            .occlusion
            .coverage_epochs
            .get(&record.id)
            .is_none_or(|(previous, _)| *previous != inputs)
        {
            self.occlusion
                .coverage_epochs
                .insert(record.id, (inputs, record.content_seq));
        }
        self.occlusion.refused_opacity.remove(&record.id);
    }

    /// Reconcile at protocol dispatch boundaries AND before each callback pulse.
    /// Compare indexed coverage inputs without rebuilding/cloning the scene on
    /// idle dispatches. Content sequences are refreshed but do not invalidate
    /// coverage: only accepted opacity, geometry, family, format and output
    /// changes do. The renderer verifies installed occluder content each frame.
    pub(super) fn refresh_occlusion(&mut self) {
        let bridge = self.occlusion.bridge.clone();
        let mut exchange = bridge.0.lock().unwrap_or_else(|e| e.into_inner());
        let outputs = self.backend.occlusion_outputs();
        let locked = self.session_lock_active();
        let mut changed_scene = exchange.scene.outputs != outputs
            || exchange.scene.locked != locked
            || exchange.scene.surfaces.len() != self.surfaces.len();
        for record in self.surfaces.values() {
            let root = canonical_root_surface(&self.popup_manager, record.role.wl_surface());
            let family = self.surfaces.get(&root.id()).map_or(record.id, |r| r.id);
            let opacity = self
                .occlusion
                .opacity
                .get(&record.id)
                .filter(|(generation, seq, _)| {
                    *generation == record.generation && *seq == record.content_seq
                })
                .map(|(_, _, opacity)| opacity);
            let format_opaque = record
                .shm_backing
                .as_ref()
                .is_some_and(|b| b.format == wl_shm::Format::Xrgb8888)
                || record
                    .dmabuf_backing
                    .as_ref()
                    .is_some_and(|b| b.descriptor.is_opaque());
            if let Some(previous) = self
                .occlusion
                .scene_indices
                .get(&record.id)
                .and_then(|i| exchange.scene.surfaces.get_mut(*i))
                .filter(|previous| previous.id == record.id)
            {
                changed_scene |= previous.family != family
                    || previous.generation != record.generation
                    || previous.layout != record.layout
                    || previous.buffer_size != record.buffer_dimensions
                    || previous.format_opaque != format_opaque
                    || previous.coverage_since
                        != self
                            .occlusion
                            .coverage_epochs
                            .get(&record.id)
                            .map_or(record.content_seq, |(_, seq)| *seq)
                    || opacity.unwrap_or(&CommittedOpacity::default()) != &previous.opacity;
                previous.content = record.content_seq;
            } else {
                changed_scene = true;
            }
        }
        if changed_scene {
            let mut scene = Scene {
                outputs,
                locked,
                ..Default::default()
            };
            for record in self.surfaces.values() {
                let root = canonical_root_surface(&self.popup_manager, record.role.wl_surface());
                let family = self.surfaces.get(&root.id()).map_or(record.id, |r| r.id);
                let opacity = self
                    .occlusion
                    .opacity
                    .get(&record.id)
                    .filter(|(generation, seq, _)| {
                        *generation == record.generation && *seq == record.content_seq
                    })
                    .map(|(_, _, opacity)| opacity.clone())
                    .unwrap_or_default();
                scene.surfaces.push(SceneSurface {
                    id: record.id,
                    family,
                    generation: record.generation,
                    layout: record.layout,
                    content: record.content_seq,
                    coverage_since: self
                        .occlusion
                        .coverage_epochs
                        .get(&record.id)
                        .map_or(record.content_seq, |(_, seq)| *seq),
                    buffer_size: record.buffer_dimensions,
                    format_opaque: record
                        .shm_backing
                        .as_ref()
                        .is_some_and(|b| b.format == wl_shm::Format::Xrgb8888)
                        || record
                            .dmabuf_backing
                            .as_ref()
                            .is_some_and(|b| b.descriptor.is_opaque()),
                    opacity,
                });
            }
            scene.surfaces.sort_by_key(|s| s.id.0);
            self.occlusion.scene_indices = scene
                .surfaces
                .iter()
                .enumerate()
                .map(|(i, s)| (s.id, i))
                .collect();
            #[cfg(test)]
            {
                self.occlusion.scene_rebuilds += 1;
            }
            exchange.exhausted |= exchange.revision == u64::MAX;
            exchange.revision = if exchange.exhausted {
                0
            } else {
                exchange.revision + 1
            };
            exchange.scene = scene;
            exchange.coverage = Default::default();
        }
        self.occlusion
            .opacity
            .retain(|id, _| self.surface_objects.contains_key(id));
        self.occlusion
            .refused_opacity
            .retain(|id| self.surface_objects.contains_key(id));
        self.occlusion
            .coverage_epochs
            .retain(|id, _| self.surface_objects.contains_key(id));
        let revision = exchange.revision;
        let decisions = if revision != 0 && exchange.coverage.revision == revision {
            Some(&exchange.coverage.surfaces)
        } else {
            None
        };
        let mut changed = Vec::new();
        for record in self.surfaces.values() {
            let old = self
                .occlusion
                .decisions
                .get(&record.id)
                .copied()
                .unwrap_or_default();
            let new = decisions
                .and_then(|d| d.get(&record.id))
                .copied()
                .unwrap_or_default();
            if old != new {
                self.occlusion
                    .decision_revisions
                    .insert(record.id, revision);
                crate::frame_trace::event("comp_occlusion_transition", || {
                    (
                        record.id.0,
                        u64::from(new == TreeVisibility::Occluded),
                        revision,
                    )
                });
                changed.push(record.id);
            }
            self.occlusion.decisions.insert(record.id, new);
        }
        self.occlusion
            .decisions
            .retain(|id, _| self.surface_objects.contains_key(id));
        self.occlusion
            .withheld
            .retain(|id, _| self.surface_objects.contains_key(id));
        self.occlusion
            .decision_revisions
            .retain(|id, _| self.surface_objects.contains_key(id));
        self.occlusion.revision = revision;
        drop(exchange);
        #[cfg(feature = "bus")]
        for id in changed {
            self.mark_surface_dirty(id, "wayland.occlusion");
        }
        #[cfg(not(feature = "bus"))]
        let _ = changed;
    }

    pub(super) fn count_occlusion_opportunities(&self) {
        let workspace = self.workspace_current();
        let withheld = self
            .surfaces
            .values()
            .filter(|r| {
                self.occlusion.is_occluded(r.id)
                    && r.role.parent_surface().is_none()
                    && self.surface_is_session_presentable(r)
                    && !self.surface_belongs_to_hidden_toplevel(r.role.wl_surface(), workspace)
            })
            .count() as u64;
        let mut exchange = self
            .occlusion
            .bridge
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        exchange.counters.withheld_opportunities += withheld;
    }

    /// Complete excess older callbacks fail-open; never silently drop them.
    /// Then trickle: at most one retained callback per surface of each
    /// occluded tree per [`OCCLUDED_TRICKLE_MS`], paced by this existing frame opportunity (no
    /// timer of its own), so a covered FIFO client keeps making progress.
    pub(super) fn limit_occluded_callbacks(&mut self) {
        if self.session_lock_active()
            || self
                .kms_session_lock_gate
                .client_delivery_blocked(false, false)
        {
            return;
        }
        let workspace = self.workspace_current();
        let frame_time = monotonic_millis();
        #[cfg(not(test))]
        let trickle_now = frame_time;
        #[cfg(test)]
        let trickle_now = self.occlusion.trickle_clock_ms;
        let mut occluded_roots = HashSet::new();
        for record in self.surfaces.values() {
            if self.occlusion.is_occluded(record.id)
                && record.role.parent_surface().is_none()
                && self.surface_is_session_presentable(record)
                && !self.surface_belongs_to_hidden_toplevel(record.role.wl_surface(), workspace)
            {
                occluded_roots.insert(record.id);
                let mut batch = send_frames_surface_tree_limited(
                    record.role.wl_surface(),
                    frame_time,
                    &self.surfaces,
                    64,
                    &HashSet::new(),
                );
                let since = *self
                    .occlusion
                    .trickle_at
                    .entry(record.id)
                    .or_insert(trickle_now);
                if !batch.retained.is_empty()
                    && trickle_now.wrapping_sub(since) >= OCCLUDED_TRICKLE_MS
                {
                    let completed = complete_oldest_frame_callback_per_surface(
                        record.role.wl_surface(),
                        frame_time,
                        &self.surfaces,
                    );
                    for id in &completed {
                        batch.retained.remove(id);
                    }
                    self.occlusion.trickle_at.insert(record.id, trickle_now);
                    crate::frame_trace::event("comp_occluded_callback_trickle", || {
                        (
                            record.id.0,
                            completed.len() as u64,
                            batch.retained.len() as u64,
                        )
                    });
                }
                if !batch.retained.is_empty() {
                    self.occlusion.withheld.insert(record.id, batch.retained);
                } else {
                    self.occlusion.withheld.remove(&record.id);
                }
            }
        }
        // Leaving occlusion resets the pacing: a root covered again later
        // waits a full interval before its first trickle.
        self.occlusion
            .trickle_at
            .retain(|id, _| occluded_roots.contains(id));
    }
}
