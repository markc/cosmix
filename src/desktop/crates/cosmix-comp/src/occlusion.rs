//! Conservative coverage certificates. Unknown always permits client progress.
//!
//! Protocol snapshots contain applied state, never pending transactions. The
//! renderer may certify only a matching layout AND installed content sequence.
//! A bounded shared slot replaces certificates atomically; no timer is involved.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::protocol::{SurfaceId, SurfaceLayout};
use bevy::prelude::*;

const REGION_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
impl Bounds {
    pub(crate) fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    fn valid(self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite() && v.abs() < 1e9)
            && self.w > 0.0
            && self.h > 0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub l: i64,
    pub t: i64,
    pub r: i64,
    pub b: i64,
}
impl Rect {
    fn intersection(self, other: Self) -> Option<Self> {
        let r = Self {
            l: self.l.max(other.l),
            t: self.t.max(other.t),
            r: self.r.min(other.r),
            b: self.b.min(other.b),
        };
        (r.l < r.r && r.t < r.b).then_some(r)
    }
    fn subtract(self, cover: Self, into: &mut Vec<Self>) {
        let Some(i) = self.intersection(cover) else {
            into.push(self);
            return;
        };
        for r in [
            Self { b: i.t, ..self },
            Self { t: i.b, ..self },
            Self {
                t: i.t,
                b: i.b,
                r: i.l,
                ..self
            },
            Self {
                t: i.t,
                b: i.b,
                l: i.r,
                ..self
            },
        ] {
            if r.l < r.r && r.t < r.b {
                into.push(r);
            }
        }
    }
}

fn subtract(region: &mut Vec<Rect>, cover: Rect) -> Result<(), ()> {
    let mut next = Vec::new();
    for r in region.iter() {
        r.subtract(cover, &mut next);
        if next.len() > REGION_LIMIT {
            return Err(());
        }
    }
    *region = next;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CommittedOpacity {
    /// None is unknown/excessively complex; an empty region is transparent.
    pub operations: Option<Vec<(bool, Bounds)>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OutputGeometry {
    pub name: String,
    pub source_id: crate::backend::CaptureSourceId,
    pub bounds: Bounds,
    pub scale: f64,
    pub scale_y: f64,
    pub generation: u64,
    pub transform: smithay::utils::Transform,
}
impl OutputGeometry {
    fn project(&self, bounds: Bounds, inward: bool) -> Option<Rect> {
        if !bounds.valid()
            || !self.bounds.valid()
            || !self.scale.is_finite()
            || self.scale <= 0.0
            || !self.scale_y.is_finite()
            || self.scale_y <= 0.0
        {
            return None;
        }
        // Coordinates are already in displayed orientation; rotating them a
        // second time would disagree with the output camera.
        let edges = [
            (bounds.x - self.bounds.x) * self.scale,
            (bounds.y - self.bounds.y) * self.scale_y,
            (bounds.x + bounds.w - self.bounds.x) * self.scale,
            (bounds.y + bounds.h - self.bounds.y) * self.scale_y,
        ];
        let [l, t, r, b] = edges;
        Some(if inward {
            Rect {
                l: l.ceil() as i64,
                t: t.ceil() as i64,
                r: r.floor() as i64,
                b: b.floor() as i64,
            }
        } else {
            Rect {
                l: l.floor() as i64,
                t: t.floor() as i64,
                r: r.ceil() as i64,
                b: b.ceil() as i64,
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SceneSurface {
    pub id: SurfaceId,
    pub family: SurfaceId,
    pub generation: u64,
    pub layout: SurfaceLayout,
    pub content: u64,
    pub opacity: CommittedOpacity,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Scene {
    pub surfaces: Vec<SceneSurface>,
    pub outputs: Vec<OutputGeometry>,
    pub locked: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TreeVisibility {
    #[default]
    Unknown,
    Visible,
    Occluded,
}
impl TreeVisibility {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Visible => "exposed",
            Self::Occluded => "opaque-coverage",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CoverageSnapshot {
    pub revision: u64,
    /// Individual surface contribution; callbacks use the family fold below.
    pub content: HashMap<SurfaceId, TreeVisibility>,
    pub surfaces: HashMap<SurfaceId, TreeVisibility>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "bus", derive(serde::Serialize))]
pub(crate) struct Counters {
    pub withheld_opportunities: u64,
    pub resumes: u64,
    pub recomputes: u64,
    pub conservative_fallbacks: u64,
}

#[cfg(feature = "bus")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Props {
    pub occluded: bool,
    pub occlusion_reason: &'static str,
    pub occlusion_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occlusion_counters: Option<Counters>,
}
#[cfg(feature = "bus")]
impl Default for Props {
    fn default() -> Self {
        Self {
            occluded: false,
            occlusion_reason: "unknown",
            occlusion_revision: 0,
            occlusion_counters: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Exchange {
    pub revision: u64,
    pub scene: Scene,
    pub coverage: CoverageSnapshot,
    pub counters: Counters,
}
#[derive(Clone, Default)]
pub(crate) struct Bridge(pub Arc<Mutex<Exchange>>);
impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OcclusionBridge")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Draw {
    pub id: SurfaceId,
    pub bounds: Bounds,
    pub opaque: bool,
    pub rounded: bool,
    /// Opaque, hard-edged SSD bands, in the same projected logical space.
    pub chrome: Vec<Bounds>,
    pub ready: bool,
}

fn opaque_region(
    surface: &SceneSurface,
    draw: &Draw,
    output: &OutputGeometry,
) -> Result<Vec<Rect>, ()> {
    if !draw.ready || draw.rounded {
        return Ok(Vec::new());
    }
    let bounds = output.project(draw.bounds, true).ok_or(())?;
    let mut region = Vec::new();
    if draw.opaque {
        region.push(bounds);
    } else if let Some(operations) = &surface.opacity.operations {
        // wl_surface opaque regions are in destination surface coordinates.
        // Project through the actual renderer rectangle, including edge snap.
        let sx = draw.bounds.w / f64::from(surface.layout.width);
        let sy = draw.bounds.h / f64::from(surface.layout.height);
        for (add, r) in operations {
            let r = Bounds::new(
                draw.bounds.x + r.x * sx,
                draw.bounds.y + r.y * sy,
                r.w * sx,
                r.h * sy,
            );
            let Some(mut r) = output.project(r, *add) else {
                return Err(());
            };
            // A one-output-pixel guard alone is insufficient for magnified
            // buffers. Use a full surface logical pixel plus a physical pixel;
            // extraction rejects viewports whose sampling footprint is unknown.
            let guard = output.scale.max(output.scale_y).ceil() as i64 + 1;
            if *add {
                r.l += guard;
                r.t += guard;
                r.r -= guard;
                r.b -= guard;
            } else {
                r.l -= guard;
                r.t -= guard;
                r.r += guard;
                r.b += guard;
            }
            if let Some(r) = r.intersection(bounds) {
                subtract(&mut region, r)?;
                if *add {
                    region.push(r);
                }
                if region.len() > REGION_LIMIT {
                    return Err(());
                }
            }
        }
        // A region covering the complete sampled surface has no internal alpha
        // boundary. It is safe to retain its texture-clamped exterior edges.
        if operations.len() == 1 && operations[0].0 {
            let r = operations[0].1;
            if r.x <= 0.0
                && r.y <= 0.0
                && r.x + r.w >= f64::from(surface.layout.width)
                && r.y + r.h >= f64::from(surface.layout.height)
            {
                region = vec![bounds];
            }
        }
    }
    for chrome in &draw.chrome {
        region.push(output.project(*chrome, true).ok_or(())?);
    }
    Ok(region)
}

pub(crate) fn compute(scene: &Scene, draws: &[Draw], revision: u64) -> CoverageSnapshot {
    let mut result = CoverageSnapshot {
        revision,
        ..Default::default()
    };
    let mut ordered = scene.surfaces.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.layout.z));
    for candidate in &ordered {
        let mut intersects = false;
        let mut decision = TreeVisibility::Occluded;
        let Some(draw) = draws.iter().find(|d| d.id == candidate.id) else {
            result
                .surfaces
                .insert(candidate.id, TreeVisibility::Unknown);
            continue;
        };
        if !draw.ready || !candidate.layout.visible || scene.locked || scene.outputs.is_empty() {
            result
                .surfaces
                .insert(candidate.id, TreeVisibility::Unknown);
            continue;
        }
        for output in &scene.outputs {
            if output.generation == 0 {
                decision = TreeVisibility::Unknown;
                break;
            }
            let Some(output_rect) = output.project(output.bounds, false) else {
                decision = TreeVisibility::Unknown;
                break;
            };
            let Some(bounds) = output.project(draw.bounds, false) else {
                decision = TreeVisibility::Unknown;
                break;
            };
            let Some(bounds) = bounds.intersection(output_rect) else {
                continue;
            };
            intersects = true;
            let mut uncovered = vec![bounds];
            let mut failed = false;
            for above in &ordered {
                if above.layout.z <= candidate.layout.z {
                    break;
                }
                // A family's own content must never put its parent to sleep.
                if above.family == candidate.family || !above.layout.visible {
                    continue;
                }
                let Some(above_draw) = draws.iter().find(|d| d.id == above.id) else {
                    continue;
                };
                match opaque_region(above, above_draw, output) {
                    Ok(region) => {
                        for rect in region {
                            if subtract(&mut uncovered, rect).is_err() {
                                failed = true;
                                break;
                            }
                        }
                    }
                    Err(()) => {
                        failed = true;
                    }
                }
                if failed || uncovered.is_empty() {
                    break;
                }
            }
            if failed {
                decision = TreeVisibility::Unknown;
                break;
            }
            if !uncovered.is_empty() {
                decision = TreeVisibility::Visible;
                break;
            }
        }
        if !intersects {
            decision = TreeVisibility::Unknown;
        }
        result.surfaces.insert(candidate.id, decision);
    }
    // Any exposed/unknown member keeps the entire canonical family progressing.
    let individual = result.surfaces.clone();
    result.content = individual.clone();
    for surface in &scene.surfaces {
        let family = scene.surfaces.iter().filter(|s| s.family == surface.family);
        let mut decision = TreeVisibility::Occluded;
        for member in family {
            match individual.get(&member.id).copied().unwrap_or_default() {
                TreeVisibility::Visible => {
                    decision = TreeVisibility::Visible;
                    break;
                }
                TreeVisibility::Unknown => decision = TreeVisibility::Unknown,
                TreeVisibility::Occluded => (),
            }
        }
        if !surface.layout.visible {
            decision = TreeVisibility::Unknown;
        }
        result.surfaces.insert(surface.id, decision);
    }
    result
}

#[derive(Resource, Default)]
pub(crate) struct ExtractedCoverage {
    pub revision: u64,
    pub scene: Scene,
    pub draws: Vec<Draw>,
}

#[derive(Resource, Default)]
pub(crate) struct CoverageCache {
    previous: Option<(u64, Vec<Draw>, Vec<OutputGeometry>)>,
    pub result: CoverageSnapshot,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn extract(
    entities: bevy::render::Extract<Res<crate::compositor_scene::SurfaceEntities>>,
    scale: bevy::render::Extract<Res<crate::compositor_scene::RendererOutputScale120>>,
    materials: bevy::render::Extract<
        Res<Assets<crate::client_surface_material::ClientSurfaceMaterial>>,
    >,
    chrome: bevy::render::Extract<
        Option<Res<Assets<crate::chrome_frame_material::ChromeFrameMaterial>>>,
    >,
    visibility: bevy::render::Extract<Query<&ViewVisibility>>,
    cameras: bevy::render::Extract<
        Query<(
            &Camera,
            &GlobalTransform,
            &Projection,
            &crate::capture::CaptureOutputSource,
        )>,
    >,
    canvas: bevy::render::Extract<Res<crate::compositor_scene::LogicalCanvasSize>>,
    reporter: Option<Res<crate::protocol::FramePresentationReporter>>,
    mut extracted: ResMut<ExtractedCoverage>,
) {
    let Some(reporter) = reporter else {
        return;
    };
    let exchange = reporter
        .occlusion
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    extracted.revision = exchange.revision;
    extracted.scene = exchange.scene.clone();
    drop(exchange);
    // Match actual views, not an assumed origin-zero canvas or nominal scale.
    // Fixed projections may have slightly different X/Y ratios after rounding.
    // Scanout rotation is bijective; do not rotate raster coordinates twice.
    for output in &mut extracted.scene.outputs {
        let mut matching = cameras.iter().filter(|(camera, _, _, source)| {
            camera.is_active && camera.order == 0 && source.source_id == output.source_id
        });
        let view = matching.next();
        if matching.next().is_some() {
            output.generation = 0;
            continue;
        }
        let Some((camera, transform, Projection::Orthographic(projection), source)) = view else {
            output.generation = 0;
            continue;
        };
        let generation = match &source.source_id {
            crate::backend::CaptureSourceId::Nested { .. } => 1,
            crate::backend::CaptureSourceId::Kms { generation, .. } => *generation,
        };
        let Some(size) = camera.physical_viewport_size() else {
            output.generation = 0;
            continue;
        };
        let (view_scale, rotation, translation) = transform.to_scale_rotation_translation();
        if generation != output.generation
            || view_scale != Vec3::ONE
            || rotation != Quat::IDENTITY
            || size.x == 0
            || size.y == 0
        {
            output.generation = 0;
            continue;
        }
        output.bounds = Bounds::new(
            f64::from(translation.x + projection.area.min.x + canvas.0.x / 2.0),
            f64::from(canvas.0.y / 2.0 - translation.y - projection.area.max.y),
            f64::from(projection.area.width()),
            f64::from(projection.area.height()),
        );
        output.scale = f64::from(size.x) / output.bounds.w;
        output.scale_y = f64::from(size.y) / output.bounds.h;
    }
    extracted.draws.clear();
    let surfaces = extracted.scene.surfaces.clone();
    for s in &surfaces {
        let Some(entity) = entities.surfaces.get(&s.id) else {
            continue;
        };
        let Some(material) = materials.get(&entity.material) else {
            continue;
        };
        let r = crate::compositor_scene::renderer_rect(
            entity.layout.x,
            entity.layout.y,
            entity.layout.width,
            entity.layout.height,
            scale.0,
        );
        let mut draw = Draw {
            id: s.id,
            bounds: Bounds::new(
                f64::from(r.x),
                f64::from(r.y),
                f64::from(r.width),
                f64::from(r.height),
            ),
            opaque: material.opaque,
            rounded: material.corner_radius > 0.0,
            chrome: Vec::new(),
            ready: entity.layout == s.layout
                && entity.applied_commit == s.content
                && visibility.get(entity.entity).is_ok_and(|v| v.get()),
        };
        // Partial viewport regions need the exact source-to-destination filter
        // footprint. Until supplied, only format-opaque or complete regions count.
        if s.layout.source.is_some() && !draw.opaque {
            let full = s.opacity.operations.as_ref().is_some_and(|ops| {
                ops.len() == 1
                    && ops[0].0
                    && ops[0].1.x <= 0.0
                    && ops[0].1.y <= 0.0
                    && ops[0].1.x + ops[0].1.w >= f64::from(s.layout.width)
                    && ops[0].1.y + ops[0].1.h >= f64::from(s.layout.height)
            });
            if !full {
                draw.ready = false;
            }
        }
        if let (Some(deco), Some(toplevel), Some(chrome)) =
            (&entity.decoration, s.layout.toplevel, chrome.as_ref())
            && let Some(frame) = chrome.get(&deco.frame_material)
            && frame.square_opaque
        {
            let layout = &deco.chrome_layout;
            let offset = layout.content_offset();
            let x = s.layout.x + toplevel.window_geometry.x - offset.x;
            let y = s.layout.y + toplevel.window_geometry.y - offset.y;
            let outer = crate::compositor_scene::renderer_rect(
                x + layout.window.x,
                y + layout.window.y,
                layout.window.w,
                layout.window.h,
                scale.0,
            );
            let x = f64::from(outer.x);
            let y = f64::from(outer.y);
            let w = f64::from(outer.width);
            let h = f64::from(outer.height);
            let left = f64::from(frame.border_insets.x);
            let right = f64::from(frame.border_insets.z);
            let bottom = f64::from(frame.border_insets.w);
            let top = f64::from(frame.titlebar_bottom);
            for band in [
                Bounds::new(x, y, w, top),
                Bounds::new(x, y, left, h),
                Bounds::new(x + w - right, y, right, h),
                Bounds::new(x, y + h - bottom, w, bottom),
            ] {
                if band.valid() {
                    draw.chrome.push(band);
                }
            }
        }
        extracted.draws.push(draw);
    }
}

pub(crate) fn resolve(
    extracted: &ExtractedCoverage,
    sampled: &HashMap<SurfaceId, u64>,
    bridge: &Bridge,
    cache: &mut CoverageCache,
) {
    let mut draws = extracted.draws.clone();
    for draw in &mut draws {
        draw.ready &= extracted
            .scene
            .surfaces
            .iter()
            .any(|s| s.id == draw.id && sampled.get(&s.id) == Some(&s.content));
    }
    let key = (extracted.revision, draws, extracted.scene.outputs.clone());
    let mut exchange = bridge.0.lock().unwrap_or_else(|e| e.into_inner());
    if cache.previous.as_ref() != Some(&key) {
        cache.result = compute(&extracted.scene, &key.1, extracted.revision);
        cache.previous = Some(key);
        exchange.counters.recomputes += 1;
        exchange.counters.conservative_fallbacks += cache
            .result
            .surfaces
            .values()
            .filter(|v| **v == TreeVisibility::Unknown)
            .count() as u64;
    }
    if exchange.revision == extracted.revision {
        exchange.coverage = cache.result.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SurfaceStackKey, SurfaceTransform};

    fn fixture() -> (Scene, Vec<Draw>) {
        let surfaces = (1..=2)
            .map(|id| SceneSurface {
                id: SurfaceId(id),
                family: SurfaceId(id),
                generation: 1,
                content: 1,
                opacity: CommittedOpacity {
                    operations: Some(Vec::new()),
                },
                layout: SurfaceLayout {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 80.0,
                    z: SurfaceStackKey::normal(id),
                    source: None,
                    parent: None,
                    transform: SurfaceTransform::Normal,
                    visible: true,
                    toplevel: None,
                },
            })
            .collect();
        let scene = Scene {
            surfaces,
            outputs: vec![OutputGeometry {
                name: "test".into(),
                source_id: crate::backend::CaptureSourceId::Nested {
                    output_name: "test".into(),
                },
                bounds: Bounds::new(0.0, 0.0, 100.0, 80.0),
                scale: 2.5,
                scale_y: 2.5,
                generation: 1,
                transform: smithay::utils::Transform::Normal,
            }],
            locked: false,
        };
        let draws = (1..=2)
            .map(|id| Draw {
                id: SurfaceId(id),
                bounds: Bounds::new(0.0, 0.0, 100.0, 80.0),
                opaque: true,
                rounded: false,
                chrome: Vec::new(),
                ready: true,
            })
            .collect();
        (scene, draws)
    }
    fn hidden(scene: &Scene, draws: &[Draw]) -> bool {
        compute(scene, draws, 1).surfaces[&SurfaceId(1)] == TreeVisibility::Occluded
    }

    #[test]
    fn occlusion_fractional_scale_one_pixel_and_translucent() {
        let (scene, mut draws) = fixture();
        assert!(hidden(&scene, &draws));
        draws[1].bounds.w -= 0.4; // one physical pixel at 2.5
        assert!(!hidden(&scene, &draws));
        draws[1].bounds.w = 100.0;
        draws[1].opaque = false;
        assert!(!hidden(&scene, &draws));
        draws[1].opaque = true;
        draws[1].rounded = true;
        assert!(!hidden(&scene, &draws));
        draws[1].rounded = false; // maximised hard edge
        assert!(hidden(&scene, &draws));
    }

    #[test]
    fn occlusion_regions_subtract_holes_and_union_adjacent_occluders() {
        let (mut scene, mut draws) = fixture();
        draws[1].opaque = false;
        scene.surfaces[1].opacity.operations =
            Some(vec![(true, Bounds::new(0.0, 0.0, 100.0, 80.0))]);
        assert!(hidden(&scene, &draws));
        scene.surfaces[1]
            .opacity
            .operations
            .as_mut()
            .unwrap()
            .push((false, Bounds::new(40.0, 30.0, 1.0, 1.0)));
        assert!(!hidden(&scene, &draws));
        draws[1].opaque = true;
        draws[1].bounds.w = 50.0;
        let mut third = scene.surfaces[1].clone();
        third.id = SurfaceId(3);
        third.family = third.id;
        third.layout.z = SurfaceStackKey::normal(3);
        scene.surfaces.push(third);
        let mut third = draws[1].clone();
        third.id = SurfaceId(3);
        third.bounds.x = 50.0;
        draws.push(third);
        assert!(hidden(&scene, &draws));
        draws[2].bounds.x += 0.01;
        assert!(!hidden(&scene, &draws), "never round a subpixel gap away");
    }

    #[test]
    fn occlusion_every_output_and_popup_family_must_be_covered() {
        let (mut scene, mut draws) = fixture();
        scene.outputs.push(OutputGeometry {
            name: "second".into(),
            bounds: Bounds::new(100.0, 0.0, 100.0, 80.0),
            ..scene.outputs[0].clone()
        });
        draws[0].bounds.w = 200.0;
        assert!(!hidden(&scene, &draws));
        draws[1].bounds.w = 200.0;
        assert!(hidden(&scene, &draws));
        let mut popup = scene.surfaces[0].clone();
        popup.id = SurfaceId(3);
        popup.layout.z = SurfaceStackKey::normal(3);
        scene.surfaces.push(popup);
        draws.push(Draw {
            id: SurfaceId(3),
            ..draws[0].clone()
        });
        assert!(!hidden(&scene, &draws), "exposed popup keeps parent alive");
        assert_eq!(
            compute(&scene, &draws, 1).content[&SurfaceId(1)],
            TreeVisibility::Occluded,
            "presentation contribution is independent of callback family eligibility"
        );
        scene.outputs[1].generation = 0;
        assert!(!hidden(&scene, &draws));
    }

    #[test]
    fn occlusion_region_budget_fails_open() {
        let mut region = vec![Rect {
            l: 0,
            t: 0,
            r: 10000,
            b: 10,
        }];
        let mut failed = false;
        for x in 1..600 {
            if subtract(
                &mut region,
                Rect {
                    l: x * 2,
                    t: 0,
                    r: x * 2 + 1,
                    b: 10,
                },
            )
            .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(region.len() <= REGION_LIMIT);
    }

    #[test]
    fn occlusion_stale_revision_and_uninstalled_content_never_withhold() {
        let (scene, draws) = fixture();
        let bridge = Bridge::default();
        bridge.0.lock().unwrap().revision = 2;
        let extracted = ExtractedCoverage {
            revision: 1,
            scene,
            draws,
        };
        let mut cache = CoverageCache::default();
        resolve(
            &extracted,
            &HashMap::from([(SurfaceId(1), 1), (SurfaceId(2), 1)]),
            &bridge,
            &mut cache,
        );
        assert!(bridge.0.lock().unwrap().coverage.surfaces.is_empty());
        bridge.0.lock().unwrap().revision = 1;
        resolve(
            &extracted,
            &HashMap::from([(SurfaceId(1), 1), (SurfaceId(2), 0)]),
            &bridge,
            &mut cache,
        );
        assert_ne!(
            bridge.0.lock().unwrap().coverage.surfaces[&SurfaceId(1)],
            TreeVisibility::Occluded
        );
        resolve(
            &extracted,
            &HashMap::from([(SurfaceId(1), 1), (SurfaceId(2), 1)]),
            &bridge,
            &mut cache,
        );
        assert_eq!(
            bridge.0.lock().unwrap().coverage.surfaces[&SurfaceId(1)],
            TreeVisibility::Occluded
        );
        let count = bridge.0.lock().unwrap().counters.recomputes;
        resolve(
            &extracted,
            &HashMap::from([(SurfaceId(1), 1), (SurfaceId(2), 1)]),
            &bridge,
            &mut cache,
        );
        assert_eq!(
            bridge.0.lock().unwrap().counters.recomputes,
            count,
            "unchanged frame is cached"
        );
    }

    #[test]
    fn occlusion_negative_origin_and_rotated_output_use_displayed_coordinates_once() {
        let (mut scene, mut draws) = fixture();
        scene.outputs[0].bounds.x = -100.0;
        scene.outputs[0].transform = smithay::utils::Transform::_90;
        for d in &mut draws {
            d.bounds.x = -100.0;
        }
        assert!(hidden(&scene, &draws));
    }

    #[test]
    fn occlusion_unknown_candidate_geometry_and_unmapped_child_keep_family_running() {
        let (mut scene, mut draws) = fixture();
        assert!(hidden(&scene, &draws));
        draws[0].ready = false;
        assert!(!hidden(&scene, &draws));
        draws[0].ready = true;
        let mut child = scene.surfaces[0].clone();
        child.id = SurfaceId(3);
        child.layout.visible = false;
        scene.surfaces.push(child);
        assert!(
            !hidden(&scene, &draws),
            "bootstrap cannot wait for a hidden parent callback"
        );
    }

    #[test]
    fn occlusion_camera_projection_is_used_and_missing_camera_fails_open() {
        use crate::compositor_scene::{LogicalCanvasSize, RendererOutputScale120, SurfaceEntities};
        use bevy::{ecs::system::RunSystemOnce, render::MainWorld};
        let (mut scene, _) = fixture();
        scene.outputs[0].name = "camera-test".into();
        scene.outputs[0].source_id = crate::backend::CaptureSourceId::Nested {
            output_name: "camera-test".into(),
        };
        // Deliberately disagree with the actual camera: it owns the proof.
        scene.outputs[0].bounds.x = 500.0;
        let (reporter, _) = crate::protocol::FramePresentationReporter::test_channel();
        {
            let mut exchange = reporter.occlusion.0.lock().unwrap();
            exchange.scene = scene;
            exchange.revision = 1;
        }
        let mut main = MainWorld::default();
        main.init_resource::<SurfaceEntities>();
        main.init_resource::<Assets<crate::client_surface_material::ClientSurfaceMaterial>>();
        main.insert_resource(RendererOutputScale120(300));
        main.insert_resource(LogicalCanvasSize(Vec2::new(100.0, 80.0)));
        main.spawn((
            Camera {
                viewport: Some(bevy::camera::Viewport {
                    physical_size: UVec2::new(251, 200),
                    ..Default::default()
                }),
                ..Default::default()
            },
            GlobalTransform::IDENTITY,
            Projection::Orthographic(OrthographicProjection {
                area: bevy::math::Rect::new(-50.0, -40.0, 50.0, 40.0),
                ..OrthographicProjection::default_2d()
            }),
            crate::capture::CaptureOutputSource {
                source_id: crate::backend::CaptureSourceId::Nested {
                    output_name: "camera-test".into(),
                },
                output_name: "camera-test".into(),
            },
        ));
        let mut render = World::new();
        render.insert_resource(main);
        render.insert_resource(reporter);
        render.init_resource::<ExtractedCoverage>();
        render.run_system_once(extract).unwrap();
        let output = &render.resource::<ExtractedCoverage>().scene.outputs[0];
        assert_eq!(output.bounds, Bounds::new(0.0, 0.0, 100.0, 80.0));
        assert_eq!((output.scale, output.scale_y), (2.51, 2.5));
        assert_eq!(output.generation, 1);
        render.resource_mut::<MainWorld>().clear_entities();
        render.run_system_once(extract).unwrap();
        assert_eq!(
            render.resource::<ExtractedCoverage>().scene.outputs[0].generation,
            0
        );
    }
}
