//! Protocol owner of revision invalidation and callback eligibility.
use super::*;
use crate::occlusion::{Bounds, Bridge, CommittedOpacity, Scene, SceneSurface, TreeVisibility};

#[derive(Default)]
pub(super) struct OcclusionRuntime {
    pub bridge: Bridge,
    pub decisions: HashMap<SurfaceId, TreeVisibility>,
    pub revision: u64,
    pub decision_revisions: HashMap<SurfaceId, u64>,
    opacity: HashMap<SurfaceId, (u64, u64, CommittedOpacity)>,
    refused_opacity: HashSet<SurfaceId>,
}
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
        self.occlusion
            .opacity
            .insert(record.id, (record.generation, record.content_seq, opacity));
        self.occlusion.refused_opacity.remove(&record.id);
    }

    /// Reconcile at protocol dispatch boundaries AND before each callback pulse.
    /// Comparing the entire applied scene makes invalidation independent of
    /// Bus feature flags and of individual mutation sites remembering a hook.
    pub(super) fn refresh_occlusion(&mut self) {
        let mut scene = Scene {
            outputs: self.backend.occlusion_outputs(),
            locked: self.session_lock_active(),
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
                opacity,
            });
        }
        scene.surfaces.sort_by_key(|s| s.id.0);
        self.occlusion
            .opacity
            .retain(|id, _| self.surface_objects.contains_key(id));
        self.occlusion
            .refused_opacity
            .retain(|id| self.surface_objects.contains_key(id));
        let bridge = self.occlusion.bridge.clone();
        let mut exchange = bridge.0.lock().unwrap_or_else(|e| e.into_inner());
        if exchange.scene != scene {
            exchange.exhausted |= exchange.revision == u64::MAX;
            exchange.revision = if exchange.exhausted {
                0
            } else {
                exchange.revision + 1
            };
            exchange.scene = scene;
            exchange.coverage = Default::default();
        }
        let revision = exchange.revision;
        let decisions = if revision != 0 && exchange.coverage.revision == revision {
            exchange.coverage.surfaces.clone()
        } else {
            HashMap::new()
        };
        let mut changed = Vec::new();
        for record in self.surfaces.values() {
            let old = self
                .occlusion
                .decisions
                .get(&record.id)
                .copied()
                .unwrap_or_default();
            let new = decisions.get(&record.id).copied().unwrap_or_default();
            if old != new {
                self.occlusion
                    .decision_revisions
                    .insert(record.id, revision);
                if old == TreeVisibility::Occluded {
                    exchange.counters.resumes += 1;
                }
                crate::frame_trace::event("comp_occlusion_transition", || {
                    (
                        record.id.0,
                        u64::from(new == TreeVisibility::Occluded),
                        revision,
                    )
                });
                changed.push(record.id);
            }
        }
        self.occlusion.decisions = decisions;
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
}
