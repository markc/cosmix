//! Protocol owner of revision invalidation and callback eligibility.
use super::*;
use crate::occlusion::{Bounds, Bridge, CommittedOpacity, Scene, SceneSurface, TreeVisibility};

#[derive(Default)]
pub(super) struct OcclusionRuntime {
    pub bridge: Bridge,
    pub decisions: HashMap<SurfaceId, TreeVisibility>,
    pub revision: u64,
    pub decision_revisions: HashMap<SurfaceId, u64>,
}
impl OcclusionRuntime {
    pub fn is_occluded(&self, id: SurfaceId) -> bool {
        self.decisions.get(&id) == Some(&TreeVisibility::Occluded)
    }
}

impl WaylandState {
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
            let opacity = compositor::with_states(record.role.wl_surface(), |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let current = attributes.current();
                let operations =
                    current
                        .opaque_region
                        .as_ref()
                        .map_or(Some(Vec::new()), |region| {
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
                        });
                CommittedOpacity { operations }
            });
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
        let bridge = self.occlusion.bridge.clone();
        let mut exchange = bridge.0.lock().unwrap_or_else(|e| e.into_inner());
        if exchange.scene != scene {
            exchange.revision = exchange.revision.checked_add(1).unwrap_or(0);
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
