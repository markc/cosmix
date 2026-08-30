use std::{collections::HashMap, mem, sync::Arc, time::Instant};

use bevy::{
    camera::visibility::{InheritedVisibility, NoFrustumCulling},
    prelude::*,
    sprite::Anchor,
    sprite_render::{ColorMaterial, MeshMaterial2d},
    text::{
        ComputedTextBlock, FontCx, FontSource, FontWeight, LayoutCx, LetterSpacing, LineHeight,
        RemSize, TextPipeline,
    },
};
use cosmix_deco::{
    ButtonShape, ButtonState, CaptionButton, ChromeLayout, DecoFontFamily, DecoFontWeight,
    DecoTheme, Focus, GlyphPolicy, Rect, Srgba, TitleAlign, Vec2 as DecoVec2, rect, vec2,
};
use fontique::{FamilyId, GenericFamily};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
use crate::protocol::SurfaceStackKey;
use crate::{
    chrome_frame_material::{ChromeFrameMaterial, ChromeFrameMaterialPlugin},
    compositor_scene::{
        CLIENT_CONTENT_Z_MAX, CLIENT_CONTENT_Z_MIN, CompositorSceneSet, LogicalCanvasSize,
        RendererOutputScale120, SurfaceEntities, client_surface_clip, relayout_surface_entity,
        renderer_rect, set_surface_client_clip, surface_rotation, sync_surface_parent,
    },
    decoration::DecorationStartup,
    protocol::{
        MAX_GLOBAL_SURFACES, SceneDecorationMode, SceneSurfaceKind, SurfaceId, SurfaceLayout,
    },
    shadow_material::{ShadowMaterial, ShadowMaterialPlugin},
};

const MIN_CLIENT_Z_GAP: f32 =
    (CLIENT_CONTENT_Z_MAX - CLIENT_CONTENT_Z_MIN) / (MAX_GLOBAL_SURFACES as f32 + 1.0);
const DECO_Z_EPSILON: f32 = MIN_CLIENT_Z_GAP / 8.0;
const GLYPH_THICKNESS: f32 = 1.0;
const EMBEDDED_CHROME_FONT_FAMILY: &str = "DejaVu Sans";
// DejaVu Sans Book 2.37 is the terminal Latin/punctuation/symbol fallback.
// It does not cover CJK or emoji; those title-script gaps remain unresolved.
const EMBEDDED_CHROME_FONT_DATA: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

pub(crate) struct DecorationPlugin;

struct ChromeTypographyPlugin;

#[derive(Resource)]
struct ChromeFontCxInitMeasurement {
    elapsed_ms: f64,
}

#[derive(Resource)]
struct DecorationSceneTheme(DecoTheme);

#[derive(Resource, Default)]
struct ChromeFontSelection {
    /// The last whole family which successfully resolved through the theme
    /// ladder. This is deliberately separate from the per-glyph chain below.
    last_known_good: Option<String>,
    resolved_family: Option<String>,
    discovered_ui_sans_families: Option<Vec<String>>,
    /// What the last emitted face report said. The system runs every frame, so
    /// the report is latched on this rather than logged per frame.
    reported_face: Option<ChromeFaceReport>,
    /// Number of face reports emitted. Observable because the `info!` beside it
    /// is not: a test can prove the latch, but not the formatting.
    report_generation: u32,
}

/// Which rung of the family ladder answered. Distinct from the family name
/// because two rungs can name the same family — a rescue that happens to land
/// on the themed family is still a rescue, and an operator reading the log is
/// entitled to know the theme request itself failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChromeFaceRung {
    Theme,
    LastKnownGood,
    SystemUiRescue,
    Embedded,
}

#[derive(Clone, Debug, PartialEq)]
struct ChromeFaceReport {
    family: String,
    rung: ChromeFaceRung,
    /// Bits, not the `f32`: this is an identity latch, and NaN must compare
    /// equal to itself here or a NaN size would re-log every frame.
    size_px_bits: u32,
    weight: u16,
}

#[derive(Resource, Default)]
pub(crate) struct DecorationDirtySurfaceIds(pub(crate) std::collections::HashSet<SurfaceId>);

#[derive(Resource)]
struct DecorationRenderAssets {
    quad: Handle<Mesh>,
    circle: Handle<Mesh>,
    materials: HashMap<SrgbaKey, Handle<ColorMaterial>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SrgbaKey([u32; 4]);

impl From<Srgba> for SrgbaKey {
    fn from(color: Srgba) -> Self {
        Self([
            color.r.to_bits(),
            color.g.to_bits(),
            color.b.to_bits(),
            color.a.to_bits(),
        ])
    }
}

#[derive(Component)]
#[allow(dead_code)]
pub(crate) struct DecoRoot(pub(crate) SurfaceId);

#[derive(Component)]
pub(crate) struct DecoTitlebar;

#[derive(Component)]
pub(crate) struct DecoChromeFrame;

#[derive(Component)]
pub(crate) struct DecoShadow;

#[derive(Component)]
pub(crate) struct DecoTitle;

#[derive(Clone, Component, Debug, PartialEq)]
struct DecoTitleProjection {
    source: Arc<str>,
    slot_width: f32,
    font_size: f32,
    /// Kept alongside the size because weight changes advance widths: a title
    /// measured Light and re-rendered Bold would elide at the wrong character.
    font_weight: u16,
    scale120: u32,
}

#[derive(Component, Default)]
struct DecoTitleElisionCache {
    key: Option<TitleElisionKey>,
    rendered: String,
}

#[derive(Clone, Debug, PartialEq)]
struct TitleElisionKey {
    source: Arc<str>,
    slot_width_bits: u32,
    font_size_bits: u32,
    font_weight: u16,
    scale120: u32,
}

#[derive(Component)]
#[allow(dead_code)]
pub(crate) struct DecoButtonBody(pub(crate) CaptionButton);

#[derive(Component)]
#[allow(dead_code)]
pub(crate) struct DecoGlyph {
    pub(crate) button: CaptionButton,
    pub(crate) segment: usize,
}

pub(crate) struct DecorationEntities {
    pub(crate) root: Entity,
    pub(crate) client_transform: Transform,
    pub(crate) chrome_layout: ChromeLayout,
    shadow: Entity,
    shadow_material: Handle<ShadowMaterial>,
    frame: Entity,
    frame_material: Handle<ChromeFrameMaterial>,
    title: Entity,
    buttons: Vec<(CaptionButton, Entity)>,
    glyphs: Vec<(CaptionButton, usize, Entity)>,
}

impl Plugin for ChromeTypographyPlugin {
    fn build(&self, app: &mut App) {
        let theme = app.world().resource::<DecorationStartup>().theme.clone();
        app.init_resource::<Assets<Font>>();
        // Own FontCx even in minimal/headless plugin graphs. Production callers
        // initialise it before DefaultPlugins so the eager discovery cost below
        // measures the actual construction instead of finding Bevy's resource.
        init_chrome_font_cx(app);
        if let Some(measurement) = app
            .world_mut()
            .remove_resource::<ChromeFontCxInitMeasurement>()
        {
            info!(
                elapsed_ms = measurement.elapsed_ms,
                "initialised chrome FontCx with eager system font discovery"
            );
        }
        app.init_resource::<ChromeFontSelection>()
            .insert_resource(DecorationSceneTheme(theme));
        {
            let mut fonts = app.world_mut().resource_mut::<Assets<Font>>();
            // Replace Bevy's tiny default subset even when TextPlugin already
            // populated the slot. DejaVu is also registered into FontCx below
            // as the terminal per-glyph fallback.
            fonts
                .insert(
                    AssetId::default(),
                    Font::from_bytes(EMBEDDED_CHROME_FONT_DATA.to_vec()),
                )
                .expect("default Bevy font slot is available");
        }
        app.add_systems(
            PostUpdate,
            configure_chrome_typography.after(bevy::text::load_font_assets_into_font_collection),
        );
    }
}

impl Plugin for DecorationPlugin {
    fn build(&self, app: &mut App) {
        let startup = app.world().resource::<DecorationStartup>();
        let enabled = startup.enabled;
        // Logged before the early return, and in the plugin rather than either
        // backend's `main`, so one line answers "did the flags take effect?"
        // for nested and KMS alike — including the `--no-ssd` case, where
        // nothing downstream of here ever runs.
        info!(
            ssd = enabled,
            chrome = ?startup.theme.style,
            scheme = ?startup.theme.scheme,
            mode = ?startup.theme.mode,
            "resolved decoration startup"
        );
        if !enabled {
            return;
        }
        app.add_plugins(ChromeTypographyPlugin);
        app.add_plugins((ChromeFrameMaterialPlugin, ShadowMaterialPlugin));
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<ColorMaterial>>()
            .init_resource::<TextPipeline>()
            .init_resource::<LayoutCx>()
            .init_resource::<RemSize>()
            .init_resource::<DecorationDirtySurfaceIds>();
        let (quad, circle) = {
            let mut meshes = app.world_mut().resource_mut::<Assets<Mesh>>();
            (
                meshes.add(Rectangle::new(1.0, 1.0)),
                meshes.add(Circle::new(0.5)),
            )
        };
        app.insert_resource(DecorationRenderAssets {
            quad,
            circle,
            materials: HashMap::new(),
        })
        .add_systems(First, sync_static_decorations.after(CompositorSceneSet))
        .add_systems(
            PostUpdate,
            elide_decoration_titles
                .after(configure_chrome_typography)
                .before(bevy::sprite::update_text2d_layout),
        )
        .add_systems(Last, log_resolved_surface_placement);
    }
}

pub(crate) fn init_chrome_font_cx(app: &mut App) {
    if app.world().contains_resource::<FontCx>() {
        return;
    }

    let started = Instant::now();
    let font_cx = FontCx::default();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    // Phase 4 replaces Bevy's eager construction path, publishes the completed
    // collection and forces title relayout if representative cold starts exceed
    // 100 ms, or fontconfig paths stop being strictly local and controlled.
    app.insert_resource(font_cx)
        .insert_resource(ChromeFontCxInitMeasurement { elapsed_ms });
}

fn generic_family_names(font_cx: &mut FontCx, generic: GenericFamily) -> Vec<String> {
    let ids = font_cx
        .collection
        .generic_families(generic)
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| font_cx.collection.family_name(id).map(str::to_owned))
        .collect()
}

fn named_family(font_cx: &mut FontCx, name: &str) -> Option<(FamilyId, String)> {
    let id = font_cx.collection.family_id(name)?;
    let canonical = font_cx.collection.family_name(id)?.to_owned();
    Some((id, canonical))
}

fn requested_chrome_family(
    font_cx: &mut FontCx,
    requested: &DecoFontFamily,
    discovered_ui_sans_families: &[String],
) -> Option<(FamilyId, String)> {
    match requested {
        DecoFontFamily::SystemUi => {
            let system_ui = font_cx
                .collection
                .generic_families(GenericFamily::SystemUi)
                .next();
            let id = match system_ui {
                Some(id) => id,
                None => discovered_ui_sans_families
                    .iter()
                    .find_map(|name| font_cx.collection.family_id(name))?,
            };
            let name = font_cx.collection.family_name(id)?.to_owned();
            Some((id, name))
        }
        DecoFontFamily::Named(name) => named_family(font_cx, name),
    }
}

fn configure_chrome_typography(
    theme: Res<DecorationSceneTheme>,
    mut selection: ResMut<ChromeFontSelection>,
    mut font_cx: ResMut<FontCx>,
    mut titles: Query<
        (
            &mut TextFont,
            &mut DecoTitleProjection,
            &mut DecoTitleElisionCache,
        ),
        With<DecoTitle>,
    >,
) {
    let Some((embedded_id, _)) = named_family(&mut font_cx, EMBEDDED_CHROME_FONT_FAMILY) else {
        // The font asset loader has not registered the embedded face yet.
        return;
    };

    if selection.discovered_ui_sans_families.is_none() {
        selection.discovered_ui_sans_families = Some(generic_family_names(
            &mut font_cx,
            GenericFamily::UiSansSerif,
        ));
    }

    // Whole-family resolution is requested -> last-known-good -> embedded.
    // It chooses the primary family; it is not the per-glyph fallback chain.
    let requested = requested_chrome_family(
        &mut font_cx,
        &theme.0.metrics.title_font_family,
        selection
            .discovered_ui_sans_families
            .as_deref()
            .unwrap_or_default(),
    );
    // A themed family this host does not have must not fall straight past the
    // platform UI family to our vendored last resort — that is the common case
    // for a default naming a family only some hosts ship. Neither rescue is a
    // successful theme resolution, so neither becomes `last_known_good`.
    let (resolved, rung) = if let Some(resolved) = requested {
        // Only a current theme request is a new known-good result. Keep an
        // unavailable prior family's name across transient collection rebuilds.
        selection.last_known_good = Some(resolved.1.clone());
        (resolved, ChromeFaceRung::Theme)
    } else if let Some(good) = selection
        .last_known_good
        .as_deref()
        .and_then(|name| named_family(&mut font_cx, name))
    {
        (good, ChromeFaceRung::LastKnownGood)
    } else if let Some(system) = requested_chrome_family(
        &mut font_cx,
        &DecoFontFamily::SystemUi,
        selection
            .discovered_ui_sans_families
            .as_deref()
            .unwrap_or_default(),
    ) {
        (system, ChromeFaceRung::SystemUiRescue)
    } else {
        (
            (embedded_id, EMBEDDED_CHROME_FONT_FAMILY.to_owned()),
            ChromeFaceRung::Embedded,
        )
    };

    // The chrome face is the one part of the theme with no compile-time answer:
    // it depends on what this host has installed. Report it once per change so
    // an operator (or a smoke gate) can read the outcome instead of eyeballing
    // glyph shapes. Latched — this system runs every frame.
    let report = ChromeFaceReport {
        family: resolved.1.clone(),
        rung,
        size_px_bits: theme.0.metrics.title_size_px.to_bits(),
        weight: theme.0.metrics.title_font_weight.resolved().0,
    };
    if selection.reported_face.as_ref() != Some(&report) {
        info!(
            requested_family = ?theme.0.metrics.title_font_family,
            resolved_family = ?report.family,
            rung = ?report.rung,
            title_size_px = theme.0.metrics.title_size_px,
            title_font_weight = report.weight,
            "resolved chrome title face"
        );
        selection.reported_face = Some(report);
        selection.report_generation += 1;
    }

    selection.resolved_family = Some(resolved.1);

    // Per-glyph coverage is a separate ordered UiSansSerif chain. Reassert it
    // only after `load_font_assets_into_font_collection`: that system clears
    // registered generic mappings when it rebuilds the collection. The
    // embedded DejaVu family is explicitly kept terminal.
    let mut chain = Vec::new();
    if resolved.0 != embedded_id {
        chain.push(resolved.0);
    }
    for name in selection
        .discovered_ui_sans_families
        .as_deref()
        .unwrap_or_default()
    {
        if let Some((id, _)) = named_family(&mut font_cx, name)
            && id != embedded_id
            && !chain.contains(&id)
        {
            chain.push(id);
        }
    }
    chain.push(embedded_id);
    let chain_changed = !font_cx
        .collection
        .generic_families(GenericFamily::UiSansSerif)
        .eq(chain.iter().copied());
    font_cx
        .collection
        .set_generic_families(GenericFamily::UiSansSerif, chain.into_iter());
    if chain_changed {
        // Fontique mapping changes do not participate in Bevy component change
        // detection. Invalidate both shaping and our independent elision cache.
        for (mut font, mut projection, mut elision_cache) in &mut titles {
            font.set_changed();
            projection.set_changed();
            elision_cache.key = None;
        }
    }
}

/// The chrome title face at a given size. `weight` is the theme token; the
/// family is resolved per-glyph through the `UiSansSerif` chain, and Parley
/// matches the nearest available face — a family with no light face renders
/// heavier than requested rather than failing.
fn chrome_title_font(font_size: f32, weight: DecoFontWeight) -> TextFont {
    TextFont::from(FontSource::UiSansSerif)
        .with_font_size(font_size)
        .with_font_weight(FontWeight(weight.resolved().0))
}

pub(crate) fn sync_static_decorations(world: &mut World) {
    let dirty = {
        let mut ids = world.resource_mut::<DecorationDirtySurfaceIds>();
        mem::take(&mut ids.0)
    };
    let theme = world.resource::<DecorationSceneTheme>().0.clone();

    for id in dirty {
        let state = world
            .resource::<SurfaceEntities>()
            .surfaces
            .get(&id)
            .map(|surface| (surface.layout, surface.renderer_z, surface.title.clone()));
        let Some((layout, renderer_z, title)) = state else {
            continue;
        };
        let server_side = layout
            .toplevel
            .is_some_and(|toplevel| toplevel.decoration == SceneDecorationMode::ServerSide);
        if !server_side {
            remove_static_decoration(world, id);
            sync_rounded_clips_for_branch(world, id, &theme);
            continue;
        }

        let existing = world
            .resource_mut::<SurfaceEntities>()
            .surfaces
            .get_mut(&id)
            .and_then(|surface| surface.decoration.take());
        let mut decoration = existing.unwrap_or_else(|| spawn_static_decoration(world, id, &theme));
        update_static_decoration(world, &theme, layout, renderer_z, title, &mut decoration);
        world
            .resource_mut::<SurfaceEntities>()
            .surfaces
            .get_mut(&id)
            .expect("dirty surface remains tracked")
            .decoration = Some(decoration);
        sync_surface_parent(world, id);
        sync_rounded_clips_for_branch(world, id, &theme);
    }
}

/// Resolved placement forensics for the intermittent two-window fault reported
/// 2026-08-11: a second window arriving with a blank titlebar and no content,
/// both restored by maximising either window.
///
/// The 20:22 capture that day settled what this is *not*. Depth was correct and
/// self-consistent throughout -- content agreeing with its own decoration root,
/// title a fixed epsilon above that root, the two windows cleanly separated at
/// their ranks -- and *both* windows reported a live title string and visible
/// content while one rendered empty on screen. A surface whose content and
/// title are present, visible and correctly ranked, yet absent from its own
/// frame, is being drawn somewhere else. The broken axis is position, not
/// order. Two `foot` windows are pixel-identical, which is why one window
/// drawn over another reads as a blank window beside a normal one, and why the
/// original report described content "showing through" between windows.
///
/// So this reports both sides of the projection: the protocol-side layout and
/// window geometry that came in, and the resolved `GlobalTransform`
/// translation that went out, for each surface's content, decoration root and
/// title. That separates "wrong layout arrived" from "correct layout projected
/// wrongly" without needing a further capture. Runs in `Last`, after transform
/// and visibility propagation. Rows stay in ascending resolved z, so draw order
/// is still readable at a glance.
///
/// Emits only when that picture changes, so an idle compositor is silent, and
/// short-circuits entirely when debug is disabled.
fn log_resolved_surface_placement(world: &mut World, mut previous: Local<String>) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let surfaces = world
        .resource::<SurfaceEntities>()
        .surfaces
        .iter()
        .map(|(id, surface)| {
            (
                *id,
                surface.entity,
                surface.layout,
                surface
                    .decoration
                    .as_ref()
                    .map(|decoration| (decoration.root, decoration.title)),
            )
        })
        .collect::<Vec<_>>();

    let translation = |world: &World, entity: Entity| {
        world
            .get::<GlobalTransform>(entity)
            .map(|transform| transform.translation())
    };
    let shown = |world: &World, entity: Entity| {
        world
            .get::<InheritedVisibility>(entity)
            .is_some_and(|visibility| visibility.get())
    };
    let describe = |point: Option<Vec3>| {
        point.map_or_else(
            || "none".to_owned(),
            |point| format!("{:.0},{:.0},{:.2}", point.x, point.y, point.z),
        )
    };

    let mut rows = surfaces
        .into_iter()
        .map(|(id, client, layout, decoration)| {
            let content = translation(world, client);
            let chrome = match decoration {
                Some((root, title)) => format!(
                    " root={} title_at={} title_shown={} title={:?}",
                    describe(translation(world, root)),
                    describe(translation(world, title)),
                    shown(world, title),
                    world
                        .get::<Text2d>(title)
                        .map_or_else(String::new, |text| text.0.clone()),
                ),
                None => " undecorated".to_owned(),
            };
            let geometry = layout.toplevel.map_or_else(
                || "none".to_owned(),
                |toplevel| {
                    format!(
                        "{},{} {}x{}",
                        toplevel.window_geometry.x,
                        toplevel.window_geometry.y,
                        toplevel.window_geometry.width,
                        toplevel.window_geometry.height,
                    )
                },
            );
            (
                content.map_or(f32::NEG_INFINITY, |point| point.z),
                format!(
                    "{}(layout={},{} {}x{} geom={geometry} content={} shown={}{chrome})",
                    id.0,
                    layout.x,
                    layout.y,
                    layout.width,
                    layout.height,
                    describe(content),
                    shown(world, client),
                ),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|(left, _), (right, _)| left.total_cmp(right));

    let resolved = rows
        .into_iter()
        .map(|(_, row)| row)
        .collect::<Vec<_>>()
        .join(" < ");
    if resolved == *previous {
        return;
    }
    previous.clone_from(&resolved);
    tracing::debug!(placement = %resolved, "resolved surface placement");
}

pub(crate) fn remove_static_decoration(world: &mut World, id: SurfaceId) {
    let removed = world
        .resource_mut::<SurfaceEntities>()
        .surfaces
        .get_mut(&id)
        .and_then(|surface| {
            surface
                .decoration
                .take()
                .map(|decoration| (surface.entity, decoration))
        });
    let Some((client, decoration)) = removed else {
        return;
    };
    if let Ok(mut client) = world.get_entity_mut(client) {
        client.remove::<ChildOf>();
    }
    if let Ok(root) = world.get_entity_mut(decoration.root) {
        root.despawn();
    }
    world
        .resource_mut::<Assets<ChromeFrameMaterial>>()
        .remove(decoration.frame_material.id());
    world
        .resource_mut::<Assets<ShadowMaterial>>()
        .remove(decoration.shadow_material.id());
    relayout_surface_entity(world, id);
    sync_surface_parent(world, id);
}

fn spawn_static_decoration(
    world: &mut World,
    id: SurfaceId,
    theme: &DecoTheme,
) -> DecorationEntities {
    let root = world
        .spawn((
            Name::new(format!("Decoration root {}", id.0)),
            DecoRoot(id),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();
    let frame_material =
        world
            .resource_mut::<Assets<ChromeFrameMaterial>>()
            .add(ChromeFrameMaterial {
                size: Vec2::ONE,
                corner_radius: 0.0,
                titlebar_bottom: 0.0,
                divider_thickness: 0.0,
                border_insets: Vec4::ZERO,
                titlebar_color: Color::NONE,
                divider_color: Color::NONE,
                border_color: Color::NONE,
            });
    let shadow_material = world
        .resource_mut::<Assets<ShadowMaterial>>()
        .add(ShadowMaterial {
            size: Vec2::ONE,
            window_origin: Vec2::ZERO,
            window_size: Vec2::ONE,
            corner_radius: 0.0,
            softness: 0.0,
            offset_y: 0.0,
            color: Color::NONE,
        });
    let shadow = world
        .spawn((
            Name::new("Decoration shadow"),
            DecoShadow,
            Mesh2d(world.resource::<DecorationRenderAssets>().quad.clone()),
            MeshMaterial2d(shadow_material.clone()),
            NoFrustumCulling,
            Transform::from_xyz(0.0, 0.0, -2.0 * DECO_Z_EPSILON),
            Visibility::Inherited,
            ChildOf(root),
        ))
        .id();
    let frame = world
        .spawn((
            Name::new("Decoration chrome frame"),
            DecoChromeFrame,
            DecoTitlebar,
            Mesh2d(world.resource::<DecorationRenderAssets>().quad.clone()),
            MeshMaterial2d(frame_material.clone()),
            NoFrustumCulling,
            Transform::from_xyz(0.0, 0.0, DECO_Z_EPSILON),
            Visibility::Inherited,
            ChildOf(root),
        ))
        .id();
    let title = world
        .spawn((
            Name::new("Decoration title"),
            DecoTitle,
            Text2d::new(""),
            chrome_title_font(theme.metrics.title_size_px, theme.metrics.title_font_weight),
            TextLayout::no_wrap(),
            TextColor(Color::WHITE),
            Anchor::CENTER,
            Transform::from_xyz(0.0, 0.0, 3.0 * DECO_Z_EPSILON),
            Visibility::Hidden,
            DecoTitleProjection {
                source: Arc::from(""),
                slot_width: 0.0,
                font_size: theme.metrics.title_size_px,
                font_weight: theme.metrics.title_font_weight.resolved().0,
                scale120: crate::backend::kms::OutputScale120::ONE.get(),
            },
            DecoTitleElisionCache::default(),
            ChildOf(root),
        ))
        .id();
    let buttons = theme
        .buttons
        .order
        .into_iter()
        .map(|button| {
            let entity = match theme.buttons.shape {
                ButtonShape::Circle { .. } => spawn_circle(world, root, button),
                ButtonShape::FullHeightRect { .. } => {
                    spawn_quad(world, root, "Decoration button", (DecoButtonBody(button),))
                }
            };
            (button, entity)
        })
        .collect::<Vec<_>>();
    let glyphs = theme
        .buttons
        .order
        .into_iter()
        .flat_map(|button| {
            let count = glyph_segment_count(button);
            (0..count).map(move |segment| (button, segment))
        })
        .map(|(button, segment)| {
            let entity = spawn_quad(
                world,
                root,
                "Decoration glyph",
                (DecoGlyph { button, segment },),
            );
            (button, segment, entity)
        })
        .collect();

    DecorationEntities {
        root,
        client_transform: Transform::default(),
        chrome_layout: ChromeLayout::compute(theme, vec2(1.0, 1.0)),
        shadow,
        shadow_material,
        frame,
        frame_material,
        title,
        buttons,
        glyphs,
    }
}

fn spawn_quad<B: Bundle>(world: &mut World, root: Entity, name: &'static str, marker: B) -> Entity {
    let mesh = world.resource::<DecorationRenderAssets>().quad.clone();
    let material = material_for(world, Srgba::TRANSPARENT);
    world
        .spawn((
            Name::new(name),
            marker,
            Mesh2d(mesh),
            MeshMaterial2d(material),
            Transform::default(),
            Visibility::Inherited,
            ChildOf(root),
        ))
        .id()
}

fn spawn_circle(world: &mut World, root: Entity, button: CaptionButton) -> Entity {
    let mesh = world.resource::<DecorationRenderAssets>().circle.clone();
    let material = material_for(world, Srgba::TRANSPARENT);
    world
        .spawn((
            Name::new("Decoration button"),
            DecoButtonBody(button),
            Mesh2d(mesh),
            MeshMaterial2d(material),
            Transform::default(),
            Visibility::Inherited,
            ChildOf(root),
        ))
        .id()
}

fn update_static_decoration(
    world: &mut World,
    theme: &DecoTheme,
    layout: SurfaceLayout,
    renderer_z: f32,
    title: Option<Arc<str>>,
    entities: &mut DecorationEntities,
) {
    let toplevel = layout
        .toplevel
        .expect("server-side decoration owns a toplevel snapshot");
    let chrome = ChromeLayout::compute(
        theme,
        vec2(
            toplevel.window_geometry.width,
            toplevel.window_geometry.height,
        ),
    );
    let content_offset = chrome.content_offset();
    let geometry_global = vec2(
        layout.x + toplevel.window_geometry.x,
        layout.y + toplevel.window_geometry.y,
    );
    let outer_origin = vec2(
        geometry_global.x - content_offset.x,
        geometry_global.y - content_offset.y,
    );
    let canvas = world.resource::<LogicalCanvasSize>().0;
    let scale120 = world.resource::<RendererOutputScale120>().0;
    let root_origin = renderer_rect(outer_origin.x, outer_origin.y, 0.0, 0.0, scale120);
    let root_transform = Transform::from_xyz(
        root_origin.x - canvas.x / 2.0,
        canvas.y / 2.0 - root_origin.y,
        renderer_z,
    );
    set_transform_if_changed(world, entities.root, root_transform);
    set_visibility(world, entities.root, layout.visible);

    let client_rect = rect(
        content_offset.x - toplevel.window_geometry.x,
        content_offset.y - toplevel.window_geometry.y,
        layout.width,
        layout.height,
    );
    let client = projected_child_rect(client_rect, outer_origin, root_origin, scale120);
    entities.client_transform = Transform::from_xyz(client.center.x, client.center.y, 0.0)
        .with_rotation(surface_rotation(layout.transform));

    let focus = if toplevel.focused {
        Focus::Focused
    } else {
        Focus::Unfocused
    };
    let effective_radius = effective_corner_radius(theme, toplevel.committed_maximized);
    update_shadow(
        world,
        entities,
        &chrome,
        outer_origin,
        root_origin,
        scale120,
        effective_radius,
        &theme.metrics.shadow,
        theme.shadow_alpha(focus),
        toplevel.committed_maximized,
    );
    update_chrome_frame(
        world,
        entities,
        &chrome,
        outer_origin,
        root_origin,
        scale120,
        effective_radius,
        theme.titlebar_fill(focus),
        theme.colors.titlebar_divider,
        theme.border(focus),
    );
    let projected_title =
        projected_child_rect(chrome.title_slot, outer_origin, root_origin, scale120);
    let source = title.unwrap_or_else(|| Arc::from(""));
    let projection = DecoTitleProjection {
        source: source.clone(),
        slot_width: projected_title.size.x,
        font_size: theme.metrics.title_size_px,
        font_weight: theme.metrics.title_font_weight.resolved().0,
        scale120,
    };
    if world.get::<DecoTitleProjection>(entities.title) != Some(&projection) {
        world.entity_mut(entities.title).insert(projection);
    }
    // Bevy 0.19 hard-codes TextureView render targets to scale factor 1.0 and
    // exposes no supported camera override. Rasterise at the compositor output
    // scale, then cancel that enlargement in the entity transform: layout stays
    // logical while KMS atlas glyphs have one texel per physical output pixel.
    let title_raster_scale = scale120 as f32 / 120.0;
    let title_font = chrome_title_font(
        theme.metrics.title_size_px * title_raster_scale,
        theme.metrics.title_font_weight,
    );
    if world.get::<TextFont>(entities.title) != Some(&title_font) {
        world.entity_mut(entities.title).insert(title_font);
    }
    let title_color = TextColor(bevy_color(theme.title_text(focus)));
    if world.get::<TextColor>(entities.title) != Some(&title_color) {
        world.entity_mut(entities.title).insert(title_color);
    }
    let (anchor, title_x) = match theme.metrics.title_align {
        TitleAlign::Center => (Anchor::CENTER, projected_title.center.x),
        // Desktop decoration titles deliberately use an LTR anchoring policy
        // for now; client-provided bidi text does not reverse the chrome edge.
        TitleAlign::Leading => (
            Anchor::CENTER_LEFT,
            projected_title.center.x - projected_title.size.x / 2.0,
        ),
    };
    if world.get::<Anchor>(entities.title) != Some(&anchor) {
        world.entity_mut(entities.title).insert(anchor);
    }
    set_transform_if_changed(
        world,
        entities.title,
        Transform::from_xyz(title_x, projected_title.center.y, 3.0 * DECO_Z_EPSILON).with_scale(
            Vec3::new(title_raster_scale.recip(), title_raster_scale.recip(), 1.0),
        ),
    );
    set_visibility(
        world,
        entities.title,
        !source.is_empty() && projected_title.size.x > 0.0,
    );
    for ((button, rect), (stored_button, entity)) in
        chrome.buttons.into_iter().zip(&entities.buttons)
    {
        debug_assert_eq!(button, *stored_button);
        let projected = projected_child_rect(rect, outer_origin, root_origin, scale120);
        let state = chrome_button_state(toplevel.chrome_pointer, button);
        let fill = theme.buttons.colors(button).fill(state, focus);
        match theme.buttons.shape {
            ButtonShape::Circle { .. } => update_circle(world, *entity, projected, fill),
            ButtonShape::FullHeightRect { .. } => {
                update_quad(world, *entity, projected, fill, 2.0 * DECO_Z_EPSILON, 0.0)
            }
        }
    }
    let glyphs_visible =
        theme.buttons.glyphs == GlyphPolicy::Always || toplevel.chrome_pointer.cluster_hovered;
    for (button, segment, entity) in &entities.glyphs {
        let button_rect = chrome
            .buttons
            .iter()
            .find_map(|(candidate, rect)| (*candidate == *button).then_some(*rect))
            .expect("stored glyph button belongs to the theme cluster");
        let specs = glyph_segments(
            *button,
            button_rect,
            theme.buttons.glyph_extent_ratio,
            toplevel.committed_maximized,
        );
        let Some(spec) = specs.get(*segment).copied() else {
            set_visibility(world, *entity, false);
            continue;
        };
        let colors = theme.buttons.colors(*button);
        let color = if chrome_button_state(toplevel.chrome_pointer, *button) == ButtonState::Idle {
            colors.glyph
        } else {
            colors.glyph_hover
        };
        update_quad(
            world,
            *entity,
            projected_child_rect(spec.rect, outer_origin, root_origin, scale120),
            color,
            3.0 * DECO_Z_EPSILON,
            spec.rotation,
        );
        set_visibility(world, *entity, glyphs_visible);
    }
    entities.chrome_layout = chrome;
}

fn chrome_button_state(
    pointer: crate::protocol::ChromePointerSceneState,
    button: CaptionButton,
) -> ButtonState {
    if pointer.pressed_button == Some(button) && pointer.hovered_button == Some(button) {
        ButtonState::Pressed
    } else if pointer.hovered_button == Some(button) {
        ButtonState::Hover
    } else {
        ButtonState::Idle
    }
}

fn effective_corner_radius(theme: &DecoTheme, committed_maximized: bool) -> f32 {
    if committed_maximized {
        0.0
    } else {
        theme.metrics.corner_radius.max(0.0)
    }
}

#[allow(clippy::too_many_arguments)]
fn update_shadow(
    world: &mut World,
    entities: &DecorationEntities,
    chrome: &ChromeLayout,
    outer_origin: DecoVec2,
    root_origin: crate::compositor_scene::RendererRect,
    scale120: u32,
    corner_radius: f32,
    shadow_spec: &cosmix_deco::ShadowSpec,
    alpha: f32,
    committed_maximized: bool,
) {
    let shadow = projected_child_rect(chrome.shadow, outer_origin, root_origin, scale120);
    set_transform_if_changed(
        world,
        entities.shadow,
        Transform::from_xyz(shadow.center.x, shadow.center.y, -2.0 * DECO_Z_EPSILON),
    );
    set_visibility(world, entities.shadow, !committed_maximized);

    let shadow_bounds = renderer_rect(
        outer_origin.x + chrome.shadow.x,
        outer_origin.y + chrome.shadow.y,
        chrome.shadow.w,
        chrome.shadow.h,
        scale120,
    );
    let window = renderer_rect(
        outer_origin.x + chrome.window.x,
        outer_origin.y + chrome.window.y,
        chrome.window.w,
        chrome.window.h,
        scale120,
    );
    let shifted_window = renderer_rect(
        outer_origin.x + chrome.window.x,
        outer_origin.y + chrome.window.y + shadow_spec.offset_y,
        chrome.window.w,
        chrome.window.h,
        scale120,
    );
    let physical_to_logical = 120.0 / scale120 as f32;
    let projected_metric =
        |logical: f32| (logical.max(0.0) * scale120 as f32 / 120.0).round() * physical_to_logical;
    let desired = ShadowMaterial {
        size: shadow.size,
        window_origin: Vec2::new(window.x - shadow_bounds.x, window.y - shadow_bounds.y),
        window_size: Vec2::new(shifted_window.width, shifted_window.height),
        corner_radius: projected_metric(corner_radius)
            .min(shifted_window.width / 2.0)
            .min(shifted_window.height / 2.0),
        softness: projected_metric(shadow_spec.softness),
        offset_y: shifted_window.y - window.y,
        color: bevy_color(shadow_spec.color.with_alpha(alpha.clamp(0.0, 1.0))),
    };
    let mut materials = world.resource_mut::<Assets<ShadowMaterial>>();
    if materials.get(&entities.shadow_material) != Some(&desired) {
        *materials
            .get_mut(&entities.shadow_material)
            .expect("decoration shadow owns its material asset") = desired;
    }
}

#[allow(clippy::too_many_arguments)]
fn update_chrome_frame(
    world: &mut World,
    entities: &DecorationEntities,
    chrome: &ChromeLayout,
    outer_origin: DecoVec2,
    root_origin: crate::compositor_scene::RendererRect,
    scale120: u32,
    corner_radius: f32,
    titlebar_color: Srgba,
    divider_color: Srgba,
    border_color: Srgba,
) {
    let frame = projected_child_rect(chrome.window, outer_origin, root_origin, scale120);
    set_transform_if_changed(
        world,
        entities.frame,
        Transform::from_xyz(frame.center.x, frame.center.y, DECO_Z_EPSILON),
    );
    let window = renderer_rect(
        outer_origin.x + chrome.window.x,
        outer_origin.y + chrome.window.y,
        chrome.window.w,
        chrome.window.h,
        scale120,
    );
    let titlebar = renderer_rect(
        outer_origin.x + chrome.titlebar.x,
        outer_origin.y + chrome.titlebar.y,
        chrome.titlebar.w,
        chrome.titlebar.h,
        scale120,
    );
    let content = renderer_rect(
        outer_origin.x + chrome.content.x,
        outer_origin.y + chrome.content.y,
        chrome.content.w,
        chrome.content.h,
        scale120,
    );
    let physical_to_logical = 120.0 / scale120 as f32;
    let projected_metric =
        |logical: f32| (logical.max(0.0) * scale120 as f32 / 120.0).round() * physical_to_logical;
    let desired = ChromeFrameMaterial {
        size: frame.size,
        corner_radius: projected_metric(corner_radius)
            .min(frame.size.x / 2.0)
            .min(frame.size.y / 2.0),
        titlebar_bottom: titlebar.y + titlebar.height - window.y,
        divider_thickness: if divider_color.a > 0.0 {
            physical_to_logical
        } else {
            0.0
        },
        border_insets: Vec4::new(
            titlebar.x - window.x,
            titlebar.y - window.y,
            window.x + window.width - titlebar.x - titlebar.width,
            window.y + window.height - content.y - content.height,
        ),
        titlebar_color: bevy_color(titlebar_color),
        divider_color: bevy_color(divider_color),
        border_color: bevy_color(border_color),
    };
    let mut materials = world.resource_mut::<Assets<ChromeFrameMaterial>>();
    if materials.get(&entities.frame_material) != Some(&desired) {
        *materials
            .get_mut(&entities.frame_material)
            .expect("decoration frame owns its material asset") = desired;
    }
}

fn sync_rounded_clips_for_branch(world: &mut World, id: SurfaceId, theme: &DecoTheme) {
    let mut affected = vec![id];
    affected.extend(crate::compositor_scene::descendant_surface_ids(
        &world.resource::<SurfaceEntities>().children,
        id,
    ));
    for affected_id in affected {
        let clip = rounded_clip_for_surface(world, affected_id, theme);
        set_surface_client_clip(world, affected_id, clip);
    }
}

fn rounded_clip_for_surface(
    world: &World,
    id: SurfaceId,
    theme: &DecoTheme,
) -> Option<crate::compositor_scene::ClientSurfaceClip> {
    let surfaces = &world.resource::<SurfaceEntities>().surfaces;
    let surface = surfaces.get(&id)?;
    let mut current = id;
    let root = loop {
        let candidate = surfaces.get(&current)?;
        match candidate.kind {
            SceneSurfaceKind::Popup => return None,
            SceneSurfaceKind::Toplevel => break candidate,
            SceneSurfaceKind::Subsurface => current = candidate.layout.parent?,
        }
    };
    let toplevel = root.layout.toplevel?;
    if toplevel.decoration != SceneDecorationMode::ServerSide {
        return None;
    }
    let chrome = ChromeLayout::compute(
        theme,
        vec2(
            toplevel.window_geometry.width,
            toplevel.window_geometry.height,
        ),
    );
    let content_offset = chrome.content_offset();
    let window_geometry_origin = vec2(
        root.layout.x + toplevel.window_geometry.x,
        root.layout.y + toplevel.window_geometry.y,
    );
    let outer_origin = vec2(
        window_geometry_origin.x - content_offset.x,
        window_geometry_origin.y - content_offset.y,
    );
    Some(client_surface_clip(
        surface.layout,
        outer_origin.x,
        outer_origin.y,
        chrome.window.w,
        chrome.window.h,
        effective_corner_radius(theme, toplevel.committed_maximized),
        world.resource::<RendererOutputScale120>().0,
    ))
}

#[derive(Clone, Copy)]
struct GlyphSegment {
    rect: Rect,
    rotation: f32,
}

fn glyph_segment_count(button: CaptionButton) -> usize {
    match button {
        CaptionButton::Close => 2,
        CaptionButton::Minimize => 1,
        CaptionButton::Maximize => 6,
    }
}

/// `extent_ratio` comes from the theme's `ButtonCluster`, not from a constant
/// here: how big a glyph should be depends on how much of the button is drawn
/// around it, which is a property of the chrome style.
fn glyph_segments(
    button: CaptionButton,
    button_rect: Rect,
    extent_ratio: f32,
    committed_maximized: bool,
) -> Vec<GlyphSegment> {
    let extent = button_rect.w.min(button_rect.h) * extent_ratio;
    let left = button_rect.x + (button_rect.w - extent) / 2.0;
    let top = button_rect.y + (button_rect.h - extent) / 2.0;
    let centre_y = button_rect.y + button_rect.h / 2.0 - GLYPH_THICKNESS / 2.0;
    match button {
        CaptionButton::Close => vec![
            GlyphSegment {
                rect: rect(left, centre_y, extent, GLYPH_THICKNESS),
                rotation: std::f32::consts::FRAC_PI_4,
            },
            GlyphSegment {
                rect: rect(left, centre_y, extent, GLYPH_THICKNESS),
                rotation: -std::f32::consts::FRAC_PI_4,
            },
        ],
        CaptionButton::Minimize => vec![GlyphSegment {
            rect: rect(left, centre_y, extent, GLYPH_THICKNESS),
            rotation: 0.0,
        }],
        CaptionButton::Maximize if committed_maximized => {
            let shift = extent * 0.22;
            let size = extent - shift;
            let rear_left = left + shift;
            let rear_top = top;
            let front_left = left;
            let front_top = top + shift;
            let mut segments = vec![
                GlyphSegment {
                    rect: rect(rear_left, rear_top, size, GLYPH_THICKNESS),
                    rotation: 0.0,
                },
                GlyphSegment {
                    rect: rect(
                        rear_left + size - GLYPH_THICKNESS,
                        rear_top,
                        GLYPH_THICKNESS,
                        size,
                    ),
                    rotation: 0.0,
                },
            ];
            segments.extend(square_glyph_segments(front_left, front_top, size));
            segments
        }
        CaptionButton::Maximize => square_glyph_segments(left, top, extent),
    }
}

fn square_glyph_segments(left: f32, top: f32, extent: f32) -> Vec<GlyphSegment> {
    vec![
        GlyphSegment {
            rect: rect(left, top, extent, GLYPH_THICKNESS),
            rotation: 0.0,
        },
        GlyphSegment {
            rect: rect(
                left + extent - GLYPH_THICKNESS,
                top,
                GLYPH_THICKNESS,
                extent,
            ),
            rotation: 0.0,
        },
        GlyphSegment {
            rect: rect(
                left,
                top + extent - GLYPH_THICKNESS,
                extent,
                GLYPH_THICKNESS,
            ),
            rotation: 0.0,
        },
        GlyphSegment {
            rect: rect(left, top, GLYPH_THICKNESS, extent),
            rotation: 0.0,
        },
    ]
}

#[derive(Clone, Copy)]
struct ProjectedChildRect {
    center: Vec2,
    size: Vec2,
}

fn projected_child_rect(
    rect: Rect,
    outer_origin: DecoVec2,
    root_origin: crate::compositor_scene::RendererRect,
    scale120: u32,
) -> ProjectedChildRect {
    let projected = renderer_rect(
        outer_origin.x + rect.x,
        outer_origin.y + rect.y,
        rect.w,
        rect.h,
        scale120,
    );
    ProjectedChildRect {
        center: Vec2::new(
            projected.x - root_origin.x + projected.width / 2.0,
            -(projected.y - root_origin.y + projected.height / 2.0),
        ),
        size: Vec2::new(projected.width, projected.height),
    }
}

fn update_quad(
    world: &mut World,
    entity: Entity,
    projected: ProjectedChildRect,
    color: Srgba,
    z: f32,
    rotation: f32,
) {
    let material = material_for(world, color);
    if world
        .get::<MeshMaterial2d<ColorMaterial>>(entity)
        .map(|current| &current.0)
        != Some(&material)
    {
        world.entity_mut(entity).insert(MeshMaterial2d(material));
    }
    set_transform_if_changed(
        world,
        entity,
        Transform::from_xyz(projected.center.x, projected.center.y, z)
            .with_rotation(Quat::from_rotation_z(rotation))
            .with_scale(Vec3::new(projected.size.x, projected.size.y, 1.0)),
    );
}

fn update_circle(world: &mut World, entity: Entity, projected: ProjectedChildRect, color: Srgba) {
    let material = material_for(world, color);
    if world
        .get::<MeshMaterial2d<ColorMaterial>>(entity)
        .map(|current| &current.0)
        != Some(&material)
    {
        world.entity_mut(entity).insert(MeshMaterial2d(material));
    }
    set_transform_if_changed(
        world,
        entity,
        Transform::from_xyz(projected.center.x, projected.center.y, 2.0 * DECO_Z_EPSILON)
            .with_scale(Vec3::new(projected.size.x, projected.size.y, 1.0)),
    );
}

fn material_for(world: &mut World, color: Srgba) -> Handle<ColorMaterial> {
    let key = SrgbaKey::from(color);
    if let Some(material) = world
        .resource::<DecorationRenderAssets>()
        .materials
        .get(&key)
        .cloned()
    {
        return material;
    }
    let material = world
        .resource_mut::<Assets<ColorMaterial>>()
        .add(ColorMaterial::from_color(bevy_color(color)));
    world
        .resource_mut::<DecorationRenderAssets>()
        .materials
        .insert(key, material.clone());
    material
}

fn bevy_color(color: Srgba) -> Color {
    Color::srgba(color.r, color.g, color.b, color.a)
}

fn set_transform_if_changed(world: &mut World, entity: Entity, transform: Transform) {
    if world.get::<Transform>(entity) != Some(&transform) {
        world.entity_mut(entity).insert(transform);
    }
}

fn set_visibility(world: &mut World, entity: Entity, visible: bool) {
    let visibility = if visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    if world.get::<Visibility>(entity) != Some(&visibility) {
        world.entity_mut(entity).insert(visibility);
    }
}

type DecoTitleElisionQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static DecoTitleProjection,
        &'static TextFont,
        &'static TextLayout,
        &'static LineHeight,
        &'static LetterSpacing,
        &'static mut Text2d,
        &'static mut DecoTitleElisionCache,
    ),
    Changed<DecoTitleProjection>,
>;

/// Elision measures candidate strings; it must never measure into the title's
/// own `ComputedTextBlock`.
///
/// `TextPipeline::create_text_measure` is a mutating call: it clears
/// `needs_rerender`, `uses_rem_sizes` and `uses_viewport_sizes`, replaces
/// `entities` with the spans it was handed, and leaves `layout` holding the last
/// candidate shaped `UNBOUNDED`. Handing it the live component would therefore
/// clear a rerender request that `detect_text_needs_rerender` had just raised —
/// the two systems are both in `PostUpdate` with no ordering between them — and
/// `update_text2d_layout` only re-lays-out when `Text2d`/`TextLayout`/`TextBounds`
/// changed or `needs_rerender` is set. A theme weight change that leaves the
/// elided string byte-identical satisfies none of those once the flag is gone, so
/// the title would keep rendering at its previous weight until something else
/// dirtied it. Measuring into a scratch block keeps the component untouched; the
/// scratch is a `Local` so the shaping buffers are still reused between calls.
#[allow(clippy::too_many_arguments)] // The shaping contexts are separate Bevy resources.
fn elide_decoration_titles(
    mut titles: DecoTitleElisionQuery<'_, '_>,
    mut measure_block: Local<ComputedTextBlock>,
    fonts: Res<Assets<Font>>,
    mut text_pipeline: ResMut<TextPipeline>,
    mut font_cx: ResMut<FontCx>,
    mut layout_cx: ResMut<LayoutCx>,
    rem_size: Res<RemSize>,
    canvas: Res<LogicalCanvasSize>,
) {
    for (entity, projection, font, layout, line_height, letter_spacing, mut text, mut cache) in
        &mut titles
    {
        let key = TitleElisionKey {
            source: projection.source.clone(),
            slot_width_bits: projection.slot_width.to_bits(),
            font_size_bits: projection.font_size.to_bits(),
            font_weight: projection.font_weight,
            scale120: projection.scale120,
        };
        if cache.key.as_ref() == Some(&key) {
            if text.0 != cache.rendered {
                text.0.clone_from(&cache.rendered);
            }
            continue;
        }

        let source = single_line_title(&projection.source);
        let logical_font = font.clone().with_font_size(projection.font_size);
        let max_width = projection.slot_width.max(0.0);
        let rendered = elide_title_end_with_measure(&source, max_width, |candidate| {
            text_pipeline
                .create_text_measure(
                    entity,
                    &fonts,
                    std::iter::once((
                        entity,
                        0,
                        candidate,
                        &logical_font,
                        Color::WHITE,
                        *line_height,
                        *letter_spacing,
                    )),
                    // renderer_rect() and slot_width are logical. Measure at
                    // 1.0 as well: the KMS title's output-scale font inflation
                    // is cancelled by its Transform, while a nested Window's
                    // Bevy scale factor is independent of compositor scale120.
                    1.0,
                    layout,
                    &mut measure_block,
                    &mut font_cx,
                    &mut layout_cx,
                    canvas.0,
                    rem_size.0,
                )
                .map(|measure| measure.max.x)
        })
        .unwrap_or_default();

        if text.0 != rendered {
            text.0.clone_from(&rendered);
        }
        cache.key = Some(key);
        cache.rendered = rendered;
    }
}

fn single_line_title(source: &str) -> String {
    let mut title = String::with_capacity(source.len());
    let mut pending_space = false;

    for character in source.chars() {
        if matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        if character.is_control() || character.is_whitespace() {
            pending_space = !title.is_empty();
            continue;
        }
        if pending_space {
            title.push(' ');
            pending_space = false;
        }
        title.push(character);
    }

    title
}

fn elide_title_end_with_measure<E>(
    text: &str,
    max_width: f32,
    mut measure: impl FnMut(&str) -> Result<f32, E>,
) -> Result<String, E> {
    const ELLIPSIS: &str = "…";

    if text.is_empty() || !max_width.is_finite() || max_width <= 0.0 {
        return Ok(String::new());
    }
    if measure(text)? <= max_width {
        return Ok(text.to_owned());
    }
    if measure(ELLIPSIS)? > max_width {
        return Ok(String::new());
    }

    let boundaries = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let graphemes = boundaries.len().saturating_sub(1);
    let mut low = 0usize;
    let mut high = graphemes.saturating_sub(1);
    let mut longest = ELLIPSIS.to_owned();
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = format!("{}{}", &text[..boundaries[middle]], ELLIPSIS);
        if measure(&candidate)? <= max_width {
            longest = candidate;
            low = middle + 1;
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    Ok(longest)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::any::TypeId;
    use std::sync::{Arc, mpsc::SyncSender};

    use bevy::{
        ecs::{
            schedule::NodeId,
            system::{IntoSystem, System},
        },
        text::DEFAULT_FONT_DATA,
    };
    use cosmix_deco::{ChromeStyle, Mode, Scheme, presets};
    use fontique::{Attributes, Collection, CollectionOptions, QueryStatus};

    use crate::{
        backend::kms::OutputScale120,
        client_surface_material::ClientSurfaceMaterial,
        compositor_scene::{
            CompositorScenePlugin, SceneCursorMode, drain_protocol_events,
            set_compositor_logical_output_geometry,
        },
        protocol::{
            ChromePointerSceneState, ClientSceneFeed, ProtocolEvent, SceneWindowGeometry, ShmFrame,
            SurfaceFrame, SurfaceSceneSnapshot, SurfaceTransform, ToplevelSceneState,
        },
    };

    fn scene_app(style: ChromeStyle) -> (App, SyncSender<Vec<ProtocolEvent>>) {
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .insert_resource(DecorationStartup::resolve(true, style))
            .add_plugins(CompositorScenePlugin::new(
                960,
                640,
                SceneCursorMode::HostCursor,
            ))
            .add_systems(
                PostUpdate,
                bevy::text::load_font_assets_into_font_collection,
            );
        (app, sender)
    }

    fn typography_app(font_cx: Option<FontCx>) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        if let Some(font_cx) = font_cx {
            app.insert_resource(font_cx);
        }
        app.insert_resource(DecorationStartup::resolve(true, ChromeStyle::Mac))
            .add_plugins(ChromeTypographyPlugin)
            // Register the loader after the typography plugin to exercise the
            // same independently-added systems as production. The graph test
            // below verifies their explicit dependency rather than inferring
            // it from Bevy's incidental execution order.
            .add_systems(
                PostUpdate,
                bevy::text::load_font_assets_into_font_collection,
            );
        app
    }

    fn typography_app_without_system_fonts() -> App {
        let mut font_cx = FontCx::default();
        font_cx.context.collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        typography_app(Some(font_cx))
    }

    fn font_cx_with_ui_sans_only() -> FontCx {
        let mut font_cx = FontCx::default();
        font_cx.context.collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        let font = Font::from_bytes(DEFAULT_FONT_DATA.to_vec());
        let family = font_cx
            .collection
            .register_fonts(font.data, None)
            .first()
            .expect("Bevy's test font contains a family")
            .0;
        font_cx
            .collection
            .set_generic_families(GenericFamily::UiSansSerif, [family].into_iter());
        font_cx
    }

    fn ui_sans_family_names(app: &mut App) -> Vec<String> {
        let mut font_cx = app.world_mut().resource_mut::<FontCx>();
        generic_family_names(&mut font_cx, GenericFamily::UiSansSerif)
    }

    fn resolved_ui_sans_glyph_id(app: &mut App, codepoint: char) -> u32 {
        let mut font_cx = app.world_mut().resource_mut::<FontCx>();
        let context = &mut font_cx.context;
        let mut query = context.collection.query(&mut context.source_cache);
        query.set_families([GenericFamily::UiSansSerif]);
        query.set_attributes(Attributes::default());
        let mut glyph_id = 0;
        query.matches_with(|font| {
            glyph_id = font
                .charmap()
                .and_then(|charmap| charmap.map(codepoint))
                .unwrap_or(0);
            if glyph_id == 0 {
                QueryStatus::Continue
            } else {
                QueryStatus::Stop
            }
        });
        glyph_id
    }

    fn assert_chrome_codepoints_have_glyphs(app: &mut App) {
        for codepoint in ['\u{00b7}', '\u{2026}', '\u{2192}'] {
            let glyph_id = resolved_ui_sans_glyph_id(app, codepoint);
            assert_ne!(
                glyph_id,
                0,
                "chrome UiSansSerif resolved U+{:04X} ({codepoint}) to GID 0 (.notdef/tofu)",
                u32::from(codepoint)
            );
        }
    }

    #[test]
    fn resolved_chrome_font_has_non_tofu_title_symbols() {
        let mut app = typography_app(None);
        app.update();

        assert!(
            app.world()
                .resource::<ChromeFontSelection>()
                .resolved_family
                .is_some(),
            "the real chrome family ladder must resolve before querying glyphs"
        );
        assert_chrome_codepoints_have_glyphs(&mut app);
    }

    #[test]
    fn embedded_dejavu_is_load_bearing_without_system_discovery() {
        let mut app = typography_app_without_system_fonts();
        app.update();

        assert_eq!(
            ui_sans_family_names(&mut app),
            [EMBEDDED_CHROME_FONT_FAMILY],
            "a minimal container must resolve only the embedded terminal fallback"
        );
        assert_chrome_codepoints_have_glyphs(&mut app);
    }

    #[test]
    fn system_ui_uses_ui_sans_when_that_is_the_only_populated_generic() {
        let mut app = typography_app(Some(font_cx_with_ui_sans_only()));
        app.update();

        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .resolved_family
                .as_deref(),
            Some("Fira Mono")
        );
        assert_eq!(
            ui_sans_family_names(&mut app),
            ["Fira Mono", EMBEDDED_CHROME_FONT_FAMILY]
        );
    }

    #[test]
    fn missing_named_family_degrades_to_the_platform_ui_family() {
        let mut app = typography_app(Some(font_cx_with_ui_sans_only()));
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Definitely Missing Chrome Font".to_owned());
        app.update();

        let selection = app.world().resource::<ChromeFontSelection>();
        assert_eq!(
            selection.resolved_family.as_deref(),
            Some("Fira Mono"),
            "a themed family this host lacks must degrade to the platform UI \
             family, not skip it for the embedded last resort"
        );
        assert_eq!(
            selection.last_known_good, None,
            "a platform rescue is not a successful theme resolution"
        );
    }

    #[test]
    fn chrome_title_font_requests_the_theme_weight() {
        let theme = presets::resolve(ChromeStyle::Mac, Scheme::Ocean, Mode::Light);
        assert_eq!(
            theme.metrics.title_font_weight,
            DecoFontWeight::LIGHT,
            "the shipped default is a light face"
        );
        assert_eq!(
            chrome_title_font(theme.metrics.title_size_px, theme.metrics.title_font_weight).weight,
            FontWeight(300)
        );
        assert_eq!(
            chrome_title_font(13.0, DecoFontWeight(0)).weight,
            FontWeight(400),
            "an unset theme weight must reach the renderer as regular, never 0"
        );
    }

    /// The face report is the only machine-readable record of which family a
    /// given host actually landed on, and the Phase 3 close smoke reads it. This
    /// proves the *latch* — that it reports on change and not per frame. It
    /// deliberately does not assert the `info!` formatting: capturing a
    /// subscriber here would test tracing, not the decision.
    #[test]
    fn the_chrome_face_report_is_latched_on_change_not_emitted_per_frame() {
        let mut app = typography_app(Some(font_cx_with_ui_sans_only()));
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .report_generation,
            1,
            "the first resolution must report"
        );

        app.update();
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .report_generation,
            1,
            "an unchanged face must not re-report every frame"
        );

        // Weight alone, with the family untouched: the report covers the whole
        // requested face, not just which family answered.
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_weight = DecoFontWeight::BLACK;
        app.update();
        let selection = app.world().resource::<ChromeFontSelection>();
        assert_eq!(
            selection.report_generation, 2,
            "a changed title weight must re-report the face"
        );
        assert_eq!(
            selection.reported_face.as_ref().map(|face| face.weight),
            Some(DecoFontWeight::BLACK.0)
        );
        assert_eq!(
            selection.reported_face.as_ref().map(|face| face.rung),
            Some(ChromeFaceRung::SystemUiRescue),
            "this fixture registers no system fonts, so the preset family is \
             absent and the rung stays the rescue across the weight change"
        );
    }

    /// A rescue that lands on a family is not the same event as the theme
    /// resolving to it, and the log has to say so — otherwise an operator
    /// reading `resolved_family` alone cannot tell a working host from one
    /// silently running the wrong face.
    #[test]
    fn the_face_report_names_the_rung_that_answered() {
        let mut app = typography_app(Some(font_cx_with_ui_sans_only()));
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Definitely Missing Chrome Font".to_owned());
        app.update();

        let selection = app.world().resource::<ChromeFontSelection>();
        assert_eq!(
            selection.reported_face.as_ref().map(|face| face.rung),
            Some(ChromeFaceRung::SystemUiRescue),
            "a themed family this host lacks is a rescue, not a theme resolution"
        );
        assert_eq!(
            selection
                .reported_face
                .as_ref()
                .map(|face| face.family.as_str()),
            Some("Fira Mono")
        );
    }

    #[test]
    fn missing_named_family_keeps_the_last_known_good_family() {
        let mut app = typography_app_without_system_fonts();
        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .add(Font::from_bytes(DEFAULT_FONT_DATA.to_vec()));
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Fira Mono".to_owned());
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .resolved_family
                .as_deref(),
            Some("Fira Mono")
        );

        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Definitely Missing Chrome Font".to_owned());
        app.update();

        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .resolved_family
                .as_deref(),
            Some("Fira Mono"),
            "requested -> LastKnownGood -> embedded must retain the good family"
        );
        assert_eq!(
            ui_sans_family_names(&mut app).first().map(String::as_str),
            Some("Fira Mono")
        );
    }

    #[test]
    fn embedded_fallback_does_not_launder_last_known_good() {
        let mut app = typography_app_without_system_fonts();
        let transient = app
            .world_mut()
            .resource_mut::<Assets<Font>>()
            .add(Font::from_bytes(DEFAULT_FONT_DATA.to_vec()));
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Fira Mono".to_owned());
        app.update();

        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Missing Replacement".to_owned());
        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .remove(transient.id());
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .last_known_good
                .as_deref(),
            Some("Fira Mono"),
            "the terminal embedded fallback is not a successful theme resolution"
        );

        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .add(Font::from_bytes(DEFAULT_FONT_DATA.to_vec()));
        app.update();
        assert_eq!(
            app.world()
                .resource::<ChromeFontSelection>()
                .resolved_family
                .as_deref(),
            Some("Fira Mono"),
            "the retained family must recover after transient unavailability"
        );
    }

    #[test]
    fn changed_ui_sans_chain_invalidates_existing_title_layout_and_elision() {
        let mut app = typography_app_without_system_fonts();
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Fira Mono".to_owned());
        app.update();

        let title = app
            .world_mut()
            .spawn((
                DecoTitle,
                chrome_title_font(13.0, DecoFontWeight::LIGHT),
                DecoTitleProjection {
                    source: Arc::from("Measured title"),
                    slot_width: 200.0,
                    font_size: 13.0,
                    font_weight: DecoFontWeight::LIGHT.0,
                    scale120: 120,
                },
                DecoTitleElisionCache {
                    key: Some(TitleElisionKey {
                        source: Arc::from("Measured title"),
                        slot_width_bits: 200.0_f32.to_bits(),
                        font_size_bits: 13.0_f32.to_bits(),
                        font_weight: DecoFontWeight::LIGHT.0,
                        scale120: 120,
                    }),
                    rendered: "stale measurement".to_owned(),
                },
            ))
            .id();
        app.world_mut().clear_trackers();
        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .add(Font::from_bytes(DEFAULT_FONT_DATA.to_vec()));
        app.world_mut().run_schedule(PostUpdate);

        assert!(
            app.world()
                .entity(title)
                .get_ref::<TextFont>()
                .expect("title has TextFont")
                .is_changed(),
            "a new primary family must invalidate existing shaped glyphs"
        );
        assert!(
            app.world()
                .entity(title)
                .get_ref::<DecoTitleProjection>()
                .expect("title has an elision projection")
                .is_changed(),
            "a new primary family must invalidate cached title measurement"
        );
        assert!(
            app.world()
                .get::<DecoTitleElisionCache>(title)
                .expect("title has an elision cache")
                .key
                .is_none(),
            "a changed projection must not hit an unchanged elision cache key"
        );
    }

    #[test]
    fn chrome_typography_has_explicit_post_loader_dependency() {
        let app = typography_app_without_system_fonts();
        let schedule = app
            .get_schedule(PostUpdate)
            .expect("PostUpdate schedule was registered");
        let graph = schedule.graph();

        let typography_set = IntoSystemSet::into_system_set(configure_chrome_typography).intern();
        let typography_set_key = graph
            .system_sets
            .get_key(typography_set)
            .expect("configure_chrome_typography system type set was registered");
        let typography_systems = graph
            .hierarchy()
            .neighbors(NodeId::Set(typography_set_key))
            .filter_map(|node| node.as_system())
            .collect::<Vec<_>>();
        assert_eq!(
            typography_systems.len(),
            1,
            "configure_chrome_typography must have exactly one PostUpdate instance"
        );

        let loader_set =
            IntoSystemSet::into_system_set(bevy::text::load_font_assets_into_font_collection)
                .intern();
        let loader_set_key = graph
            .system_sets
            .get_key(loader_set)
            .expect("font collection loader system type set was registered");
        assert!(
            graph.dependency().contains_edge(
                NodeId::Set(loader_set_key),
                NodeId::System(typography_systems[0]),
            ),
            "ChromeTypographyPlugin must explicitly order configure_chrome_typography after the font collection loader because the loader clears generic mappings"
        );
    }

    #[test]
    fn ui_sans_chain_is_reasserted_after_font_collection_rebuild() {
        let mut app = typography_app_without_system_fonts();
        let transient = app
            .world_mut()
            .resource_mut::<Assets<Font>>()
            .add(Font::from_bytes(DEFAULT_FONT_DATA.to_vec()));
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_family = DecoFontFamily::Named("Fira Mono".to_owned());
        app.update();
        {
            let mut font_cx = app.world_mut().resource_mut::<FontCx>();
            let fira = named_family(&mut font_cx, "Fira Mono")
                .expect("transient family was loaded")
                .0;
            let dejavu = named_family(&mut font_cx, EMBEDDED_CHROME_FONT_FAMILY)
                .expect("embedded family was loaded")
                .0;
            // Establish the desired chain before the call which clears it.
            // This makes the fixture independent of insertion order and means
            // only the post-loader reassertion can satisfy the final check.
            font_cx
                .collection
                .set_generic_families(GenericFamily::UiSansSerif, [fira, dejavu].into_iter());
        }
        assert_eq!(
            ui_sans_family_names(&mut app),
            ["Fira Mono", EMBEDDED_CHROME_FONT_FAMILY]
        );

        // Removing a loaded asset makes Bevy clear and rebuild the collection,
        // which deletes generic mappings registered before the loader runs.
        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .remove(transient.id());
        app.update();

        assert_eq!(
            ui_sans_family_names(&mut app),
            [EMBEDDED_CHROME_FONT_FAMILY],
            "post-loader reassertion must restore the terminal fallback after clear()"
        );
    }

    fn frame(value: u8) -> SurfaceFrame {
        SurfaceFrame::Shm(ShmFrame {
            width: 4,
            height: 4,
            opaque: true,
            rgba: Arc::new(vec![value; 64]),
        })
    }

    fn toplevel_layout(
        x: f32,
        y: f32,
        z: f32,
        decoration: SceneDecorationMode,
        focused: bool,
    ) -> SurfaceLayout {
        SurfaceLayout {
            x,
            y,
            width: 320.0,
            height: 200.0,
            z: SurfaceStackKey::normal(z as u64),
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: Some(ToplevelSceneState {
                decoration,
                focused,
                committed_maximized: false,
                chrome_pointer: ChromePointerSceneState::default(),
                window_geometry: SceneWindowGeometry {
                    x: 0.0,
                    y: 0.0,
                    width: 320.0,
                    height: 200.0,
                },
            }),
        }
    }

    fn popup_layout(parent: SurfaceId, z: f32) -> SurfaceLayout {
        SurfaceLayout {
            x: 140.0,
            y: 130.0,
            width: 80.0,
            height: 50.0,
            z: SurfaceStackKey::normal(z as u64),
            source: None,
            parent: Some(parent),
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        }
    }

    fn publish(app: &mut App, sender: &SyncSender<Vec<ProtocolEvent>>, events: Vec<ProtocolEvent>) {
        sender.send(events).expect("scene feed accepts the batch");
        app.update();
    }

    fn upsert(id: SurfaceId, layout: SurfaceLayout, value: u8) -> ProtocolEvent {
        ProtocolEvent::SurfaceUpserted {
            id,
            scene: scene(layout),
            frame: frame(value),
        }
    }

    fn upsert_kind(
        id: SurfaceId,
        layout: SurfaceLayout,
        kind: SceneSurfaceKind,
        value: u8,
    ) -> ProtocolEvent {
        ProtocolEvent::SurfaceUpserted {
            id,
            scene: SurfaceSceneSnapshot {
                layout,
                kind,
                title: None,
            },
            frame: frame(value),
        }
    }

    fn scene(layout: SurfaceLayout) -> SurfaceSceneSnapshot {
        SurfaceSceneSnapshot {
            layout,
            kind: if layout.toplevel.is_some() {
                SceneSurfaceKind::Toplevel
            } else {
                SceneSurfaceKind::Subsurface
            },
            title: None,
        }
    }

    fn titled_scene(layout: SurfaceLayout, title: impl Into<Arc<str>>) -> SurfaceSceneSnapshot {
        SurfaceSceneSnapshot {
            layout,
            kind: if layout.toplevel.is_some() {
                SceneSurfaceKind::Toplevel
            } else {
                SceneSurfaceKind::Subsurface
            },
            title: Some(title.into()),
        }
    }

    fn titled_upsert(
        id: SurfaceId,
        layout: SurfaceLayout,
        value: u8,
        title: impl Into<Arc<str>>,
    ) -> ProtocolEvent {
        ProtocolEvent::SurfaceUpserted {
            id,
            scene: titled_scene(layout, title),
            frame: frame(value),
        }
    }

    fn root_and_client(world: &World, id: SurfaceId) -> (Entity, Entity) {
        let surface = &world.resource::<SurfaceEntities>().surfaces[&id];
        (
            surface
                .decoration
                .as_ref()
                .expect("surface owns static decoration")
                .root,
            surface.entity,
        )
    }

    fn local_quad_edges(world: &World, entity: Entity) -> (f32, f32, f32, f32) {
        let transform = world.get::<Transform>(entity).expect("quad transform");
        let size = transform.scale.truncate();
        (
            transform.translation.x - size.x / 2.0,
            -transform.translation.y - size.y / 2.0,
            transform.translation.x + size.x / 2.0,
            -transform.translation.y + size.y / 2.0,
        )
    }

    fn local_frame_edges(world: &World, decoration: &DecorationEntities) -> (f32, f32, f32, f32) {
        let transform = world
            .get::<Transform>(decoration.frame)
            .expect("frame transform");
        let size = world
            .resource::<Assets<ChromeFrameMaterial>>()
            .get(&decoration.frame_material)
            .expect("frame material")
            .size;
        (
            transform.translation.x - size.x / 2.0,
            -transform.translation.y - size.y / 2.0,
            transform.translation.x + size.x / 2.0,
            -transform.translation.y + size.y / 2.0,
        )
    }

    fn assert_hierarchy_for_style(
        style: ChromeStyle,
        glyph_count: usize,
        circle_count: usize,
        sprite_count: usize,
    ) {
        let (mut app, sender) = scene_app(style);
        let id = SurfaceId(1);
        publish(
            &mut app,
            &sender,
            vec![upsert(
                id,
                toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
            )],
        );
        let world = app.world_mut();
        let (root, client) = root_and_client(world, id);
        assert_eq!(world.query::<&DecoRoot>().iter(world).count(), 1);
        assert_eq!(world.query::<&DecoTitlebar>().iter(world).count(), 1);
        assert_eq!(world.query::<&DecoTitle>().iter(world).count(), 1);
        assert_eq!(world.query::<&DecoChromeFrame>().iter(world).count(), 1);
        assert_eq!(world.query::<&DecoShadow>().iter(world).count(), 1);
        assert_eq!(world.query::<&DecoButtonBody>().iter(world).count(), 3);
        assert_eq!(world.query::<&DecoGlyph>().iter(world).count(), glyph_count);
        assert_eq!(
            world.query::<&Mesh2d>().iter(world).count(),
            circle_count + sprite_count + 2
        );
        assert_eq!(world.query::<&SpriteMesh>().iter(world).count(), 0);
        assert_eq!(
            world
                .query::<&MeshMaterial2d<ClientSurfaceMaterial>>()
                .iter(world)
                .count(),
            1
        );
        assert_eq!(
            world.get::<ChildOf>(client).map(ChildOf::parent),
            Some(root)
        );
        let decoration = world.resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("surface owns decoration");
        for shader_sized_quad in [client, decoration.frame, decoration.shadow] {
            assert!(
                world.get::<NoFrustumCulling>(shader_sized_quad).is_some(),
                "shader-sized quads must not inherit the shared 1x1 mesh bounds"
            );
        }
        let children = world
            .get::<Children>(root)
            .expect("root owns its hierarchy");
        assert_eq!(
            children.len(),
            1 + 1 + 1 + 1 + 3 + glyph_count,
            "client plus every chrome primitive is a direct child"
        );
    }

    #[test]
    fn mac_static_hierarchy_has_titlebar_three_circles_and_stable_glyph_segments() {
        assert_hierarchy_for_style(ChromeStyle::Mac, 9, 3, 10);
    }

    #[test]
    fn disabled_startup_installs_no_decoration_work_or_dirty_bookkeeping() {
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .insert_resource(DecorationStartup::resolve(false, ChromeStyle::Mac))
            .add_plugins(CompositorScenePlugin::new(
                960,
                640,
                SceneCursorMode::HostCursor,
            ));
        publish(
            &mut app,
            &sender,
            vec![upsert(
                SurfaceId(1),
                toplevel_layout(20.0, 20.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
            )],
        );
        assert!(!app.world().contains_resource::<DecorationDirtySurfaceIds>());
        assert!(!app.world().contains_resource::<DecorationRenderAssets>());
        assert_eq!(
            app.world_mut()
                .query::<&DecoRoot>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[test]
    fn win11_static_hierarchy_has_borders_quad_buttons_and_nine_glyph_segments() {
        assert_hierarchy_for_style(ChromeStyle::Win11, 9, 0, 13);
    }

    #[test]
    fn cosmix_static_hierarchy_has_borders_three_circles_and_nine_glyph_segments() {
        assert_hierarchy_for_style(ChromeStyle::Cosmix, 9, 3, 10);
    }

    #[test]
    fn unbound_and_client_side_toplevels_never_create_a_decoration_root() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(
                    SurfaceId(1),
                    toplevel_layout(10.0, 20.0, 1.0, SceneDecorationMode::Unbound, false),
                    1,
                ),
                upsert(
                    SurfaceId(2),
                    toplevel_layout(30.0, 40.0, 2.0, SceneDecorationMode::ClientSide, true),
                    2,
                ),
            ],
        );
        let world = app.world_mut();
        assert_eq!(world.query::<&DecoRoot>().iter(world).count(), 0);
        assert!(
            world
                .resource::<SurfaceEntities>()
                .surfaces
                .values()
                .all(|surface| surface.decoration.is_none())
        );
    }

    #[test]
    fn decoration_mode_round_trip_preserves_client_and_popup_entities() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let parent = SurfaceId(1);
        let popup = SurfaceId(2);
        let server = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(parent, server, 1),
                upsert_kind(popup, popup_layout(parent, 2.0), SceneSurfaceKind::Popup, 2),
            ],
        );
        let (first_root, client) = root_and_client(app.world(), parent);
        let popup_entity = app.world().resource::<SurfaceEntities>().surfaces[&popup].entity;

        let client_side = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                decoration: SceneDecorationMode::ClientSide,
                ..server.toplevel.expect("toplevel state")
            }),
            ..server
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id: parent,
                scene: scene(client_side),
            }],
        );
        assert!(app.world().get_entity(first_root).is_err());
        assert!(app.world().get_entity(client).is_ok());
        assert!(app.world().get_entity(popup_entity).is_ok());
        assert!(app.world().get::<ChildOf>(client).is_none());
        assert_eq!(
            app.world_mut()
                .query::<&DecoTitlebar>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world_mut()
                .query::<&DecoButtonBody>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world_mut()
                .query::<&DecoGlyph>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world()
                .get::<ChildOf>(popup_entity)
                .map(ChildOf::parent),
            Some(client)
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id: parent,
                scene: scene(server),
            }],
        );
        let (second_root, same_client) = root_and_client(app.world(), parent);
        assert_ne!(first_root, second_root);
        assert_eq!(client, same_client);
        assert_eq!(
            app.world().get::<ChildOf>(client).map(ChildOf::parent),
            Some(second_root)
        );
        assert_eq!(
            app.world()
                .get::<ChildOf>(popup_entity)
                .map(ChildOf::parent),
            Some(client)
        );
        assert_eq!(
            app.world_mut()
                .query::<&DecoRoot>()
                .iter(app.world())
                .count(),
            1
        );
    }

    #[test]
    fn unmapping_a_decorated_toplevel_despawns_its_complete_chrome_tree() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        publish(
            &mut app,
            &sender,
            vec![upsert(
                id,
                toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
            )],
        );
        let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let client = surface.entity;
        let decoration = surface.decoration.as_ref().expect("decoration");
        let chrome = std::iter::once(decoration.root)
            .chain(std::iter::once(decoration.shadow))
            .chain(std::iter::once(decoration.frame))
            .chain(decoration.buttons.iter().map(|(_, entity)| *entity))
            .chain(decoration.glyphs.iter().map(|(_, _, entity)| *entity))
            .collect::<Vec<_>>();

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUnmapped { id }],
        );

        assert!(
            !app.world()
                .resource::<SurfaceEntities>()
                .surfaces
                .contains_key(&id)
        );
        assert!(app.world().get_entity(client).is_err());
        assert!(
            chrome
                .into_iter()
                .all(|entity| app.world().get_entity(entity).is_err())
        );
        assert_eq!(
            app.world_mut()
                .query::<&DecoRoot>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[test]
    fn undersized_client_geometry_keeps_chrome_minimum_without_growing_content() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let mut layout = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, false);
        layout.width = 10.0;
        layout.height = 5.0;
        layout.toplevel.as_mut().expect("toplevel").window_geometry = SceneWindowGeometry {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 5.0,
        };
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);
        let theme = presets::resolve(ChromeStyle::Mac, Scheme::Ocean, Mode::Light);
        let minimum = ChromeLayout::min_content_size(&theme);
        let chrome = &app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration")
            .chrome_layout;
        assert_eq!((chrome.content.w, chrome.content.h), (10.0, 5.0));
        assert_eq!(chrome.window.w, minimum.x);
        assert_eq!(chrome.window.h, theme.metrics.titlebar_height + 5.0);
    }

    #[test]
    fn nonzero_window_geometry_offset_is_applied_once_at_the_root_boundary() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let mut layout = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        layout.toplevel.as_mut().expect("toplevel").window_geometry = SceneWindowGeometry {
            x: 8.0,
            y: 11.0,
            width: 300.0,
            height: 180.0,
        };
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);
        let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration");
        let offset = decoration.chrome_layout.content_offset();
        let root = app
            .world()
            .get::<Transform>(decoration.root)
            .expect("root transform");
        let expected_outer = vec2(100.0 + 8.0 - offset.x, 80.0 + 11.0 - offset.y);
        assert_eq!(root.translation.x, expected_outer.x - 480.0);
        assert_eq!(root.translation.y, 320.0 - expected_outer.y);
        assert_eq!(
            decoration.client_transform.translation.x,
            offset.x - 8.0 + layout.width / 2.0
        );
        assert_eq!(
            decoration.client_transform.translation.y,
            -(offset.y - 11.0 + layout.height / 2.0)
        );
    }

    #[test]
    fn subsurface_tree_window_geometry_sizes_chrome_beyond_the_root_buffer() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let mut layout = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        layout.width = 320.0;
        layout.height = 32.0;
        layout.toplevel = Some(ToplevelSceneState {
            window_geometry: SceneWindowGeometry {
                x: 0.0,
                y: 0.0,
                width: 450.0,
                height: 32.0,
            },
            ..layout.toplevel.expect("toplevel")
        });
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);

        let chrome = &app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration")
            .chrome_layout;
        assert_eq!((chrome.content.w, chrome.content.h), (450.0, 32.0));
        assert_eq!(
            chrome.window.w,
            450.0 + 2.0 * presets::win11(Mode::Light).metrics.border_thickness
        );
    }

    /// The glyph ratio moved out of a `const` here and into the theme
    /// (`ButtonCluster::glyph_extent_ratio`, deco 0.4.0). Nothing else in the
    /// suite reads glyph *size*, so without this the call site could go on
    /// passing a hardcoded 0.36 and every test would stay green — the plumbing
    /// would be a silent no-op.
    #[test]
    fn the_drawn_glyph_takes_its_extent_from_the_theme_not_a_constant() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let layout = toplevel_layout(400.0, 300.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);

        let theme = presets::win11(Mode::Light);
        let cell = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration")
            .chrome_layout
            .buttons
            .iter()
            .find_map(|(kind, rect)| (*kind == CaptionButton::Minimize).then_some(*rect))
            .expect("win11 draws a minimize cell");
        let expected = cell.w.min(cell.h) * theme.buttons.glyph_extent_ratio;

        // Minimize is one unrotated bar `extent` wide, so its quad scale IS the
        // extent — no unpicking a rotation or a nine-segment composite.
        let mut query = app.world_mut().query::<(&DecoGlyph, &Transform)>();
        let world = app.world();
        let (_, transform) = query
            .iter(world)
            .find(|(glyph, _)| glyph.button == CaptionButton::Minimize)
            .expect("the minimize glyph is drawn");
        assert_eq!(
            transform.scale.x, expected,
            "the drawn glyph must follow the theme's ratio"
        );
        assert_ne!(
            expected,
            cell.w.min(cell.h) * 0.36,
            "win11's ratio must differ from the retired constant, or this test \
             cannot tell the two apart"
        );
    }

    fn static_color(world: &mut World, button: CaptionButton) -> Color {
        let mut query = world.query::<(&DecoButtonBody, &MeshMaterial2d<ColorMaterial>)>();
        let (_, material) = query
            .iter(world)
            .find(|(body, _)| body.0 == button)
            .expect("button exists");
        let handle = material.0.clone();
        world
            .resource::<Assets<ColorMaterial>>()
            .get(&handle)
            .expect("decoration material remains live")
            .color
    }

    fn chrome_frame_material(world: &World, id: SurfaceId) -> ChromeFrameMaterial {
        let handle = &world.resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("surface owns decoration")
            .frame_material;
        world
            .resource::<Assets<ChromeFrameMaterial>>()
            .get(handle)
            .expect("chrome frame material remains live")
            .clone()
    }

    fn shadow_material(world: &World, id: SurfaceId) -> ShadowMaterial {
        let handle = &world.resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("surface owns decoration")
            .shadow_material;
        world
            .resource::<Assets<ShadowMaterial>>()
            .get(handle)
            .expect("shadow material remains live")
            .clone()
    }

    fn client_material(world: &World, id: SurfaceId) -> ClientSurfaceMaterial {
        let handle = &world.resource::<SurfaceEntities>().surfaces[&id].material;
        world
            .resource::<Assets<ClientSurfaceMaterial>>()
            .get(handle)
            .expect("client material remains live")
            .clone()
    }

    fn assert_vec2_close(actual: Vec2, expected: Vec2) {
        assert!(
            actual.distance(expected) < 0.001,
            "actual {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn rounded_clip_covers_toplevel_and_subsurfaces_but_not_popups() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let root = SurfaceId(1);
        let subsurface = SurfaceId(2);
        let nested_subsurface = SurfaceId(3);
        let popup = SurfaceId(4);
        let popup_child = SurfaceId(5);
        let child_layout = |parent, x, y, z: f32| SurfaceLayout {
            x,
            y,
            width: 80.0,
            height: 50.0,
            z: SurfaceStackKey::normal(z as u64),
            source: None,
            parent: Some(parent),
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        publish(
            &mut app,
            &sender,
            vec![
                upsert(
                    root,
                    toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                    1,
                ),
                upsert_kind(
                    subsurface,
                    child_layout(root, 112.0, 96.0, 2.0),
                    SceneSurfaceKind::Subsurface,
                    2,
                ),
                upsert_kind(
                    nested_subsurface,
                    child_layout(subsurface, 124.0, 108.0, 3.0),
                    SceneSurfaceKind::Subsurface,
                    3,
                ),
                upsert_kind(
                    popup,
                    child_layout(root, 150.0, 115.0, 4.0),
                    SceneSurfaceKind::Popup,
                    4,
                ),
                upsert_kind(
                    popup_child,
                    child_layout(popup, 158.0, 123.0, 5.0),
                    SceneSurfaceKind::Subsurface,
                    5,
                ),
            ],
        );

        for id in [root, subsurface, nested_subsurface] {
            assert!(client_material(app.world(), id).corner_radius > 0.0);
        }
        for id in [popup, popup_child] {
            let material = client_material(app.world(), id);
            assert_eq!(material.corner_radius, 0.0);
            assert_eq!(material.clip_size, Vec2::ZERO);
        }
        let surfaces = &app.world().resource::<SurfaceEntities>().surfaces;
        assert_eq!(surfaces[&root].kind, SceneSurfaceKind::Toplevel);
        assert_eq!(surfaces[&subsurface].kind, SceneSurfaceKind::Subsurface);
        assert_eq!(surfaces[&popup].kind, SceneSurfaceKind::Popup);
        assert_eq!(surfaces[&popup_child].kind, SceneSurfaceKind::Subsurface);
    }

    #[test]
    fn clip_transform_handles_all_surface_transforms() {
        let cases = [
            (SurfaceTransform::Normal, Vec2::new(20.0, 20.0)),
            (SurfaceTransform::Rotate90, Vec2::new(20.0, 60.0)),
            (SurfaceTransform::Rotate180, Vec2::new(100.0, 60.0)),
            (SurfaceTransform::Rotate270, Vec2::new(100.0, 20.0)),
            (SurfaceTransform::Flipped, Vec2::new(20.0, 20.0)),
            (SurfaceTransform::Flipped90, Vec2::new(20.0, 60.0)),
            (SurfaceTransform::Flipped180, Vec2::new(100.0, 60.0)),
            (SurfaceTransform::Flipped270, Vec2::new(100.0, 20.0)),
        ];
        for (transform, expected_origin) in cases {
            let layout = SurfaceLayout {
                x: 30.0,
                y: 40.0,
                width: 80.0,
                height: 40.0,
                z: SurfaceStackKey::normal(1),
                source: None,
                parent: None,
                transform,
                visible: true,
                toplevel: None,
            };
            let clip = client_surface_clip(layout, 10.0, 20.0, 200.0, 150.0, 12.0, 120);
            let projected_origin = (clip.clip_from_uv * Vec3::new(0.0, 0.0, 1.0)).truncate();
            assert_vec2_close(projected_origin, expected_origin);
            let corners = [Vec2::ZERO, Vec2::X, Vec2::Y, Vec2::ONE]
                .map(|uv| (clip.clip_from_uv * uv.extend(1.0)).truncate());
            let min = corners
                .iter()
                .copied()
                .reduce(Vec2::min)
                .expect("four corners");
            let max = corners
                .iter()
                .copied()
                .reduce(Vec2::max)
                .expect("four corners");
            assert_vec2_close(min, Vec2::new(20.0, 20.0));
            assert_vec2_close(max, Vec2::new(100.0, 60.0));
        }
    }

    #[test]
    fn nonzero_window_geometry_offsets_the_clip_once() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let mut layout = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        layout.toplevel.as_mut().expect("toplevel").window_geometry = SceneWindowGeometry {
            x: 12.0,
            y: 7.0,
            width: 300.0,
            height: 180.0,
        };
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);
        let material = client_material(app.world(), id);
        let origin = (material.clip_from_uv * Vec3::new(0.0, 0.0, 1.0)).truncate();
        assert_vec2_close(origin, Vec2::new(-12.0, 21.0));
        assert_eq!(material.clip_size, Vec2::new(300.0, 208.0));
    }

    #[test]
    fn fractional_scale_clip_uses_projected_physical_edges() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        set_compositor_logical_output_geometry(
            app.world_mut(),
            960,
            640,
            OutputScale120::new(300).expect("2.5 scale"),
        );
        let id = SurfaceId(1);
        let mut layout = toplevel_layout(100.2, 80.2, 1.0, SceneDecorationMode::ServerSide, true);
        layout.toplevel.as_mut().expect("toplevel").window_geometry = SceneWindowGeometry {
            x: 3.1,
            y: 2.3,
            width: 319.4,
            height: 199.4,
        };
        publish(&mut app, &sender, vec![upsert(id, layout, 1)]);
        let material = client_material(app.world(), id);
        let outer_x = layout.x + 3.1;
        let outer_y = layout.y + 2.3 - 28.0;
        let window = renderer_rect(outer_x, outer_y, 319.4, 227.4, 300);
        let surface = renderer_rect(layout.x, layout.y, layout.width, layout.height, 300);
        assert_eq!(material.clip_size, Vec2::new(window.width, window.height));
        assert_vec2_close(
            (material.clip_from_uv * Vec3::new(0.0, 0.0, 1.0)).truncate(),
            Vec2::new(surface.x - window.x, surface.y - window.y),
        );
        for edge in [
            window.x,
            window.y,
            window.x + window.width,
            window.y + window.height,
        ] {
            let physical = edge * 2.5;
            assert!((physical - physical.round()).abs() < 0.001);
        }
    }

    #[test]
    fn chrome_frame_draws_titlebar_divider_and_border_inside_one_rounded_silhouette() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let id = SurfaceId(1);
        publish(
            &mut app,
            &sender,
            vec![upsert(
                id,
                toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
            )],
        );
        let world = app.world_mut();
        assert_eq!(world.query::<&DecoChromeFrame>().iter(world).count(), 1);
        let decoration = world.resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration");
        assert!(world.get::<DecoTitlebar>(decoration.frame).is_some());
        assert!(
            world
                .get::<MeshMaterial2d<ColorMaterial>>(decoration.frame)
                .is_none()
        );
        let material = chrome_frame_material(world, id);
        let theme = presets::resolve(ChromeStyle::Cosmix, Scheme::Ocean, Mode::Light);
        assert_eq!(
            material.titlebar_color,
            bevy_color(theme.titlebar_fill(Focus::Focused))
        );
        assert_eq!(
            material.divider_color,
            bevy_color(theme.colors.titlebar_divider)
        );
        assert_eq!(
            material.border_color,
            bevy_color(theme.border(Focus::Focused))
        );
        assert!(material.corner_radius > 0.0);
        assert!(material.divider_thickness > 0.0);
        assert!(material.border_insets.cmpgt(Vec4::ZERO).any());
        assert_eq!(
            world
                .get::<Transform>(decoration.frame)
                .expect("frame transform")
                .translation
                .z,
            DECO_Z_EPSILON
        );
    }

    #[test]
    fn shadow_uses_layout_bounds_softness_offset_and_focus_alpha() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let focused = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, focused, 1)]);

        let theme = presets::resolve(ChromeStyle::Mac, Scheme::Ocean, Mode::Light);
        let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let decoration = surface.decoration.as_ref().expect("decoration");
        let shadow_entity = decoration.shadow;
        let chrome = decoration.chrome_layout.clone();
        let material = shadow_material(app.world(), id);
        let transform = app
            .world()
            .get::<Transform>(shadow_entity)
            .expect("shadow transform");
        let edges = (
            transform.translation.x - material.size.x / 2.0,
            -transform.translation.y - material.size.y / 2.0,
            transform.translation.x + material.size.x / 2.0,
            -transform.translation.y + material.size.y / 2.0,
        );

        assert_eq!(
            edges,
            (
                chrome.shadow.x,
                chrome.shadow.y,
                chrome.shadow.x + chrome.shadow.w,
                chrome.shadow.y + chrome.shadow.h,
            )
        );
        assert_eq!(material.softness, theme.metrics.shadow.softness);
        assert_eq!(material.offset_y, theme.metrics.shadow.offset_y);
        assert_eq!(
            material.window_size,
            Vec2::new(chrome.window.w, chrome.window.h)
        );
        assert_eq!(
            material.window_origin + Vec2::Y * material.offset_y,
            Vec2::splat(theme.metrics.shadow.softness)
        );
        assert_eq!(
            material.color,
            bevy_color(
                theme
                    .metrics
                    .shadow
                    .color
                    .with_alpha(theme.shadow_alpha(Focus::Focused))
            )
        );
        assert_eq!(transform.translation.z, -2.0 * DECO_Z_EPSILON);

        let unfocused = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..focused.toplevel.expect("toplevel")
            }),
            ..focused
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(unfocused),
            }],
        );
        assert_eq!(
            shadow_material(app.world(), id).color,
            bevy_color(
                theme
                    .metrics
                    .shadow
                    .color
                    .with_alpha(theme.shadow_alpha(Focus::Unfocused))
            )
        );
    }

    #[test]
    fn maximised_window_hides_shadow() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let normal = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, normal, 1)]);
        let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration");
        let shadow = decoration.shadow;
        let handle = decoration.shadow_material.clone();
        assert_eq!(
            app.world().get::<Visibility>(shadow),
            Some(&Visibility::Inherited)
        );

        let maximised = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                committed_maximized: true,
                ..normal.toplevel.expect("toplevel")
            }),
            ..normal
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(maximised),
            }],
        );
        let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration remains retained");
        assert_eq!(decoration.shadow, shadow);
        assert_eq!(decoration.shadow_material, handle);
        assert_eq!(
            app.world().get::<Visibility>(shadow),
            Some(&Visibility::Hidden)
        );
    }

    #[test]
    fn upper_shadow_stays_above_lower_window_and_below_its_own_content() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let lower = SurfaceId(1);
        let upper = SurfaceId(2);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(
                    lower,
                    toplevel_layout(20.0, 20.0, 1.0, SceneDecorationMode::ServerSide, false),
                    1,
                ),
                upsert(
                    upper,
                    toplevel_layout(80.0, 70.0, 2.0, SceneDecorationMode::ServerSide, true),
                    2,
                ),
            ],
        );
        let surfaces = &app.world().resource::<SurfaceEntities>().surfaces;
        let lower_top = surfaces[&lower].renderer_z + 3.0 * DECO_Z_EPSILON;
        let upper_client = surfaces[&upper].renderer_z;
        let upper_shadow = upper_client - 2.0 * DECO_Z_EPSILON;
        assert!(lower_top < upper_shadow);
        assert!(upper_shadow < upper_client);
    }

    #[test]
    fn focus_and_resize_reuse_shadow_and_frame_material_handles() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let id = SurfaceId(1);
        let initial = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, initial, 1)]);
        let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration");
        let shadow_entity = decoration.shadow;
        let shadow_handle = decoration.shadow_material.clone();
        let frame_entity = decoration.frame;
        let frame_handle = decoration.frame_material.clone();
        let shadow_before = shadow_material(app.world(), id);
        let frame_before = chrome_frame_material(app.world(), id);

        let mut resized = SurfaceLayout {
            width: 440.0,
            height: 260.0,
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..initial.toplevel.expect("toplevel")
            }),
            ..initial
        };
        resized.toplevel.as_mut().expect("toplevel").window_geometry = SceneWindowGeometry {
            x: 0.0,
            y: 0.0,
            width: 440.0,
            height: 260.0,
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(resized),
            }],
        );

        let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
            .decoration
            .as_ref()
            .expect("decoration remains retained");
        assert_eq!(decoration.shadow, shadow_entity);
        assert_eq!(decoration.shadow_material, shadow_handle);
        assert_eq!(decoration.frame, frame_entity);
        assert_eq!(decoration.frame_material, frame_handle);
        assert_ne!(shadow_material(app.world(), id), shadow_before);
        assert_ne!(chrome_frame_material(app.world(), id), frame_before);
    }

    #[test]
    fn only_dirty_decorations_mutate_phase3_assets() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let dirty = SurfaceId(1);
        let untouched = SurfaceId(2);
        let dirty_layout = toplevel_layout(20.0, 20.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(dirty, dirty_layout, 1),
                upsert(
                    untouched,
                    toplevel_layout(400.0, 80.0, 2.0, SceneDecorationMode::ServerSide, false),
                    2,
                ),
            ],
        );
        let dirty_client_before = client_material(app.world(), dirty);
        let dirty_frame_before = chrome_frame_material(app.world(), dirty);
        let dirty_shadow_before = shadow_material(app.world(), dirty);
        let untouched_client_before = client_material(app.world(), untouched);
        let untouched_frame_before = chrome_frame_material(app.world(), untouched);
        let untouched_shadow_before = shadow_material(app.world(), untouched);

        let changed_focus = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..dirty_layout.toplevel.expect("toplevel")
            }),
            ..dirty_layout
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id: dirty,
                scene: scene(changed_focus),
            }],
        );

        assert_eq!(client_material(app.world(), dirty), dirty_client_before);
        assert_ne!(
            chrome_frame_material(app.world(), dirty),
            dirty_frame_before
        );
        assert_ne!(shadow_material(app.world(), dirty), dirty_shadow_before);
        assert_eq!(
            client_material(app.world(), untouched),
            untouched_client_before
        );
        assert_eq!(
            chrome_frame_material(app.world(), untouched),
            untouched_frame_before
        );
        assert_eq!(
            shadow_material(app.world(), untouched),
            untouched_shadow_before
        );
    }

    #[test]
    fn committed_maximise_collapses_radius_to_zero() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let normal = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, normal, 1)]);
        let normal_radius = client_material(app.world(), id).corner_radius;
        assert!(normal_radius > 0.0);
        assert_eq!(
            chrome_frame_material(app.world(), id).corner_radius,
            normal_radius
        );

        let maximised = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                committed_maximized: true,
                ..normal.toplevel.expect("toplevel")
            }),
            ..normal
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(maximised),
            }],
        );
        assert_eq!(client_material(app.world(), id).corner_radius, 0.0);
        assert_eq!(chrome_frame_material(app.world(), id).corner_radius, 0.0);

        let unfocused = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..normal.toplevel.expect("toplevel")
            }),
            ..normal
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(unfocused),
            }],
        );
        assert_eq!(
            client_material(app.world(), id).corner_radius,
            normal_radius
        );
        assert_eq!(
            chrome_frame_material(app.world(), id).corner_radius,
            normal_radius
        );
    }

    #[test]
    fn rounded_material_handles_remain_stable_across_resize_and_focus() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let id = SurfaceId(1);
        let initial = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, initial, 1)]);
        let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let client_handle = surface.material.clone();
        let decoration = surface.decoration.as_ref().expect("decoration");
        let frame_entity = decoration.frame;
        let frame_handle = decoration.frame_material.clone();

        let resized = SurfaceLayout {
            width: 480.0,
            height: 260.0,
            toplevel: Some(ToplevelSceneState {
                window_geometry: SceneWindowGeometry {
                    width: 480.0,
                    height: 260.0,
                    ..initial.toplevel.expect("toplevel").window_geometry
                },
                ..initial.toplevel.expect("toplevel")
            }),
            ..initial
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(resized),
            }],
        );
        let unfocused = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..resized.toplevel.expect("toplevel")
            }),
            ..resized
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(unfocused),
            }],
        );
        let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(surface.material, client_handle);
        let decoration = surface.decoration.as_ref().expect("decoration remains");
        assert_eq!(decoration.frame, frame_entity);
        assert_eq!(decoration.frame_material, frame_handle);
    }

    fn glyph_colours(world: &mut World, button: CaptionButton) -> Vec<Color> {
        let handles = world
            .query::<(&DecoGlyph, &MeshMaterial2d<ColorMaterial>)>()
            .iter(world)
            .filter(|(glyph, _)| glyph.button == button)
            .map(|(_, material)| material.0.clone())
            .collect::<Vec<_>>();
        let materials = world.resource::<Assets<ColorMaterial>>();
        handles
            .iter()
            .map(|handle| {
                materials
                    .get(handle)
                    .expect("glyph material remains live")
                    .color
            })
            .collect()
    }

    #[test]
    fn focus_change_resolves_titlebar_border_and_idle_button_colours() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let id = SurfaceId(1);
        let focused = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, focused, 1)]);
        let theme = presets::resolve(ChromeStyle::Cosmix, Scheme::Ocean, Mode::Light);
        assert_eq!(
            chrome_frame_material(app.world(), id).titlebar_color,
            bevy_color(theme.titlebar_fill(Focus::Focused))
        );
        assert_eq!(
            static_color(app.world_mut(), CaptionButton::Close),
            bevy_color(theme.buttons.close.fill(ButtonState::Idle, Focus::Focused))
        );

        let unfocused = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: false,
                ..focused.toplevel.expect("toplevel")
            }),
            ..focused
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(unfocused),
            }],
        );
        assert_eq!(
            chrome_frame_material(app.world(), id).titlebar_color,
            bevy_color(theme.titlebar_fill(Focus::Unfocused))
        );
        assert_eq!(
            static_color(app.world_mut(), CaptionButton::Close),
            bevy_color(
                theme
                    .buttons
                    .close
                    .fill(ButtonState::Idle, Focus::Unfocused)
            )
        );
        assert_eq!(
            chrome_frame_material(app.world(), id).border_color,
            bevy_color(theme.border(Focus::Unfocused))
        );
    }

    #[test]
    fn title_text_uses_slot_size_alignment_and_focus_colour() {
        for (style, expected_anchor) in [
            (ChromeStyle::Mac, Anchor::CENTER),
            (ChromeStyle::Cosmix, Anchor::CENTER_LEFT),
        ] {
            let (mut app, sender) = scene_app(style);
            let id = SurfaceId(1);
            let focused = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
            publish(
                &mut app,
                &sender,
                vec![titled_upsert(id, focused, 1, "Short title")],
            );
            let theme = presets::resolve(style, Scheme::Ocean, Mode::Light);
            let title = app
                .world_mut()
                .query_filtered::<Entity, With<DecoTitle>>()
                .single(app.world())
                .expect("one title child");
            let decoration = app.world().resource::<SurfaceEntities>().surfaces[&id]
                .decoration
                .as_ref()
                .expect("surface owns decoration");
            let title_slot = decoration.chrome_layout.title_slot;
            let expected_x = match theme.metrics.title_align {
                TitleAlign::Center => title_slot.x + title_slot.w / 2.0,
                TitleAlign::Leading => title_slot.x,
            };

            assert_eq!(app.world().get::<Text2d>(title).unwrap().0, "Short title");
            assert_eq!(
                app.world().get::<TextFont>(title),
                Some(&chrome_title_font(
                    theme.metrics.title_size_px,
                    theme.metrics.title_font_weight
                ))
            );
            assert_eq!(
                app.world().get::<TextLayout>(title).unwrap().linebreak,
                bevy::text::LineBreak::NoWrap
            );
            assert_eq!(app.world().get::<Anchor>(title), Some(&expected_anchor));
            assert_eq!(
                app.world().get::<TextColor>(title),
                Some(&TextColor(bevy_color(theme.title_text(Focus::Focused))))
            );
            let transform = app.world().get::<Transform>(title).unwrap();
            assert_eq!(transform.translation.x, expected_x);
            assert_eq!(transform.translation.z, 3.0 * DECO_Z_EPSILON);

            let unfocused = SurfaceLayout {
                toplevel: Some(ToplevelSceneState {
                    focused: false,
                    ..focused.toplevel.expect("toplevel")
                }),
                ..focused
            };
            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::SurfaceRelayout {
                    id,
                    scene: titled_scene(unfocused, "Short title"),
                }],
            );
            assert_eq!(
                app.world().get::<TextColor>(title),
                Some(&TextColor(bevy_color(theme.title_text(Focus::Unfocused))))
            );
        }
    }

    #[test]
    fn a_changed_title_weight_reaches_the_elision_cache_key() {
        let source = "A deliberately long compositor title that must be elided consistently";
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let id = SurfaceId(1);
        let scene = || {
            vec![titled_upsert(
                id,
                toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
                source,
            )]
        };
        publish(&mut app, &sender, scene());
        let title = app
            .world_mut()
            .query_filtered::<Entity, With<DecoTitle>>()
            .single(app.world())
            .expect("one title child");
        assert_eq!(
            app.world()
                .get::<DecoTitleElisionCache>(title)
                .and_then(|cache| cache.key.as_ref())
                .map(|key| key.font_weight),
            Some(DecoFontWeight::LIGHT.0),
            "the measured title must record the weight it was measured at"
        );

        // Weight advances glyph widths, so a re-themed weight has to re-measure
        // even when text, slot and size are untouched.
        app.world_mut()
            .resource_mut::<DecorationSceneTheme>()
            .0
            .metrics
            .title_font_weight = DecoFontWeight::BLACK;
        publish(&mut app, &sender, scene());

        assert_eq!(
            app.world()
                .get::<DecoTitleProjection>(title)
                .unwrap()
                .font_weight,
            DecoFontWeight::BLACK.0
        );
        assert_eq!(
            app.world()
                .get::<DecoTitleElisionCache>(title)
                .and_then(|cache| cache.key.as_ref())
                .map(|key| key.font_weight),
            Some(DecoFontWeight::BLACK.0),
            "an unchanged-text relayout at a new weight must not hit the stale \
             measurement cache"
        );
    }

    #[test]
    fn measuring_a_title_leaves_its_rerender_request_standing() {
        let source = "A deliberately long compositor title that must be elided consistently";
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        publish(
            &mut app,
            &sender,
            vec![titled_upsert(
                SurfaceId(1),
                toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                1,
                source,
            )],
        );

        let title = app
            .world_mut()
            .query_filtered::<Entity, With<DecoTitle>>()
            .single(app.world())
            .expect("one title child");

        // Without this the assertion below is vacuous: a title that never
        // reaches the measure loop cannot clobber anything.
        let rendered = &app.world().get::<Text2d>(title).expect("a title string").0;
        assert!(
            rendered.ends_with('…') && rendered != source,
            "this title must actually be elided for the measurement path to have run, but it \
             rendered as {rendered:?}"
        );

        // The assertion below isolates the elision measurement only while this
        // harness runs neither `detect_text_needs_rerender` — which would
        // re-raise a cleared flag and let a reverted fix pass — nor
        // `update_text2d_layout`, which lowers it legitimately. Assert that
        // rather than trust a comment, and compare by `System::system_type`
        // rather than by name: bevy's `debug` feature is off in this build, so
        // every `System::name()` is the literal "<Enable the debug feature to
        // see the name>" and a name-based guard would pass vacuously forever.
        let present: Vec<TypeId> = app
            .get_schedule(bevy::app::PostUpdate)
            .expect("the harness has a PostUpdate schedule")
            .systems()
            .expect("PostUpdate has run, so its executor is initialised")
            .map(|(_, system)| system.system_type())
            .collect();
        for (system_type, path) in [
            (
                IntoSystem::into_system(bevy::text::detect_text_needs_rerender).system_type(),
                "bevy::text::detect_text_needs_rerender",
            ),
            (
                IntoSystem::into_system(bevy::sprite::update_text2d_layout).system_type(),
                "bevy::sprite::update_text2d_layout",
            ),
        ] {
            assert!(
                !present.contains(&system_type),
                "`scene_app` now runs `{path}`, which invalidates this test: that system moves \
                 `needs_rerender` on its own, so the assertion below would no longer isolate the \
                 elision measurement. Drive the flag explicitly instead of relying on the harness."
            );
        }

        // Nothing left in this harness may lower the flag an inserted
        // `ComputedTextBlock` starts with. Only a measure call into the live
        // component can, and that is exactly what would strand the title at a
        // stale weight in production.
        assert!(
            app.world()
                .get::<ComputedTextBlock>(title)
                .expect("a title keeps its ComputedTextBlock")
                .needs_rerender(false, false),
            "elision measured into the title's own ComputedTextBlock and cleared its rerender \
             request, so `update_text2d_layout` would skip a title whose elided string happens to \
             be unchanged and keep rendering the previous layout"
        );
    }

    #[test]
    fn title_raster_scale_is_cancelled_in_transform_and_elision_stays_logical() {
        let source = "A deliberately long compositor title that must be elided consistently";
        let mut rendered = Vec::new();

        for scale120 in [120_u32, 300_u32] {
            let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
            set_compositor_logical_output_geometry(
                app.world_mut(),
                960,
                640,
                OutputScale120::new(scale120).expect("valid output scale"),
            );
            let id = SurfaceId(1);
            publish(
                &mut app,
                &sender,
                vec![titled_upsert(
                    id,
                    toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true),
                    1,
                    source,
                )],
            );
            let title = app
                .world_mut()
                .query_filtered::<Entity, With<DecoTitle>>()
                .single(app.world())
                .expect("one title child");
            let theme = presets::resolve(ChromeStyle::Cosmix, Scheme::Ocean, Mode::Light);
            let raster_scale = scale120 as f32 / 120.0;

            assert_eq!(
                app.world().get::<TextFont>(title),
                Some(&chrome_title_font(
                    theme.metrics.title_size_px * raster_scale,
                    theme.metrics.title_font_weight
                ))
            );
            assert_eq!(
                app.world().get::<Transform>(title).unwrap().scale,
                Vec3::new(raster_scale.recip(), raster_scale.recip(), 1.0)
            );
            rendered.push(app.world().get::<Text2d>(title).unwrap().0.clone());
        }

        assert_eq!(rendered[0], rendered[1]);
        assert_ne!(rendered[0], source);
        assert!(rendered[0].ends_with('…'));
    }

    #[test]
    fn single_line_title_sanitises_client_controlled_text() {
        for (source, expected) in [
            ("plain title", "plain title"),
            ("a\r\nb", "a b"),
            ("before\u{1b}after", "before after"),
            ("before\0after", "before after"),
            ("left\u{202e}right", "leftright"),
            (
                " \t multiple\n\r separators \u{2003} ",
                "multiple separators",
            ),
        ] {
            assert_eq!(single_line_title(source), expected, "source: {source:?}");
        }
    }

    #[test]
    fn title_elision_is_grapheme_safe_and_never_exceeds_slot() {
        let source = "A👩‍👩‍👧‍👦BCDEF";
        let measure = |candidate: &str| {
            Ok::<_, std::convert::Infallible>(candidate.graphemes(true).count() as f32 * 10.0)
        };

        let rendered = elide_title_end_with_measure(source, 35.0, measure).unwrap();
        assert!(rendered.ends_with('…'));
        assert!(rendered.graphemes(true).count() as f32 * 10.0 <= 35.0);
        let prefix = rendered.strip_suffix('…').expect("elided title suffix");
        assert!(source.starts_with(prefix));
        assert!(
            source
                .grapheme_indices(true)
                .map(|(index, _)| index)
                .chain(std::iter::once(source.len()))
                .any(|boundary| boundary == prefix.len())
        );
        assert_eq!(
            elide_title_end_with_measure(source, 5.0, measure).unwrap(),
            ""
        );
        assert_eq!(
            elide_title_end_with_measure(source, 100.0, measure).unwrap(),
            source
        );
    }

    #[test]
    fn committed_maximise_swaps_to_restore_segments() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let normal = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, normal, 1)]);
        let mut glyphs = app
            .world_mut()
            .query::<(Entity, &DecoGlyph)>()
            .iter(app.world())
            .filter_map(|(entity, glyph)| {
                (glyph.button == CaptionButton::Maximize).then_some((glyph.segment, entity))
            })
            .collect::<Vec<_>>();
        glyphs.sort_by_key(|(segment, _)| *segment);
        assert_eq!(glyphs.len(), 6);
        assert!(glyphs[..4].iter().all(|(_, entity)| {
            app.world().get::<Visibility>(*entity) == Some(&Visibility::Inherited)
        }));
        assert!(glyphs[4..].iter().all(|(_, entity)| {
            app.world().get::<Visibility>(*entity) == Some(&Visibility::Hidden)
        }));
        let normal_transforms = glyphs
            .iter()
            .map(|(_, entity)| *app.world().get::<Transform>(*entity).unwrap())
            .collect::<Vec<_>>();

        let maximised = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                committed_maximized: true,
                ..normal.toplevel.expect("toplevel")
            }),
            ..normal
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(maximised),
            }],
        );
        assert!(glyphs.iter().all(|(_, entity)| {
            app.world().get::<Visibility>(*entity) == Some(&Visibility::Inherited)
        }));
        assert_ne!(
            glyphs
                .iter()
                .map(|(_, entity)| *app.world().get::<Transform>(*entity).unwrap())
                .collect::<Vec<_>>(),
            normal_transforms
        );
    }

    #[test]
    fn title_and_glyph_updates_reuse_existing_entities() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let initial = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(
            &mut app,
            &sender,
            vec![titled_upsert(id, initial, 1, "First")],
        );
        let title = app
            .world_mut()
            .query_filtered::<Entity, With<DecoTitle>>()
            .single(app.world())
            .expect("one title child");
        let mut glyphs = app
            .world_mut()
            .query_filtered::<Entity, With<DecoGlyph>>()
            .iter(app.world())
            .collect::<Vec<_>>();
        glyphs.sort();

        let updated = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                committed_maximized: true,
                chrome_pointer: ChromePointerSceneState {
                    hovered_button: Some(CaptionButton::Maximize),
                    cluster_hovered: true,
                    pressed_button: None,
                },
                ..initial.toplevel.expect("toplevel")
            }),
            ..initial
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: titled_scene(updated, "Second"),
            }],
        );

        let updated_title = app
            .world_mut()
            .query_filtered::<Entity, With<DecoTitle>>()
            .single(app.world())
            .expect("one title child");
        let mut updated_glyphs = app
            .world_mut()
            .query_filtered::<Entity, With<DecoGlyph>>()
            .iter(app.world())
            .collect::<Vec<_>>();
        updated_glyphs.sort();
        assert_eq!(updated_title, title);
        assert_eq!(updated_glyphs, glyphs);
        assert_eq!(app.world().get::<Text2d>(title).unwrap().0, "Second");
    }

    #[test]
    fn mac_cluster_hover_reveals_all_three_glyphs_and_leave_hides_them() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let idle = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, false);
        publish(&mut app, &sender, vec![upsert(id, idle, 1)]);
        let theme = presets::resolve(ChromeStyle::Mac, Scheme::Ocean, Mode::Light);
        let glyph_entities = app
            .world_mut()
            .query_filtered::<Entity, With<DecoGlyph>>()
            .iter(app.world())
            .collect::<Vec<_>>();
        assert_eq!(glyph_entities.len(), 9);
        assert!(
            glyph_entities.iter().all(|entity| {
                app.world().get::<Visibility>(*entity) == Some(&Visibility::Hidden)
            })
        );
        assert_eq!(
            static_color(app.world_mut(), CaptionButton::Close),
            bevy_color(
                theme
                    .buttons
                    .close
                    .fill(ButtonState::Idle, Focus::Unfocused)
            )
        );

        let hovered = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                chrome_pointer: ChromePointerSceneState {
                    hovered_button: Some(CaptionButton::Close),
                    cluster_hovered: true,
                    pressed_button: None,
                },
                ..idle.toplevel.expect("toplevel")
            }),
            ..idle
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(hovered),
            }],
        );
        assert_eq!(
            static_color(app.world_mut(), CaptionButton::Close),
            bevy_color(
                theme
                    .buttons
                    .close
                    .fill(ButtonState::Hover, Focus::Unfocused)
            )
        );
        for button in [
            CaptionButton::Close,
            CaptionButton::Minimize,
            CaptionButton::Maximize,
        ] {
            assert!(glyph_entities.iter().any(|entity| {
                app.world()
                    .get::<DecoGlyph>(*entity)
                    .is_some_and(|glyph| glyph.button == button)
                    && app.world().get::<Visibility>(*entity) == Some(&Visibility::Inherited)
            }));
        }
        let gap_hovered = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                chrome_pointer: ChromePointerSceneState {
                    hovered_button: None,
                    cluster_hovered: true,
                    pressed_button: None,
                },
                ..idle.toplevel.expect("toplevel")
            }),
            ..idle
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(gap_hovered),
            }],
        );
        for button in [
            CaptionButton::Close,
            CaptionButton::Minimize,
            CaptionButton::Maximize,
        ] {
            assert!(glyph_entities.iter().any(|entity| {
                app.world()
                    .get::<DecoGlyph>(*entity)
                    .is_some_and(|glyph| glyph.button == button)
                    && app.world().get::<Visibility>(*entity) == Some(&Visibility::Inherited)
            }));
        }
        assert_eq!(
            static_color(app.world_mut(), CaptionButton::Close),
            bevy_color(
                theme
                    .buttons
                    .close
                    .fill(ButtonState::Idle, Focus::Unfocused)
            )
        );
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(idle),
            }],
        );
        assert!(
            glyph_entities.iter().all(|entity| {
                app.world().get::<Visibility>(*entity) == Some(&Visibility::Hidden)
            })
        );
    }

    #[test]
    fn win11_close_hover_pressed_and_glyph_colours_follow_pointer_state() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let idle = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, idle, 1)]);
        let theme = presets::resolve(ChromeStyle::Win11, Scheme::Ocean, Mode::Light);
        assert!(
            glyph_colours(app.world_mut(), CaptionButton::Close)
                .into_iter()
                .all(|color| color == bevy_color(theme.buttons.close.glyph))
        );

        for (pointer, expected_state) in [
            (
                ChromePointerSceneState {
                    hovered_button: Some(CaptionButton::Close),
                    cluster_hovered: true,
                    pressed_button: None,
                },
                ButtonState::Hover,
            ),
            (
                ChromePointerSceneState {
                    hovered_button: Some(CaptionButton::Close),
                    cluster_hovered: true,
                    pressed_button: Some(CaptionButton::Close),
                },
                ButtonState::Pressed,
            ),
        ] {
            let layout = SurfaceLayout {
                toplevel: Some(ToplevelSceneState {
                    chrome_pointer: pointer,
                    ..idle.toplevel.expect("toplevel")
                }),
                ..idle
            };
            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::SurfaceRelayout {
                    id,
                    scene: scene(layout),
                }],
            );
            assert_eq!(
                static_color(app.world_mut(), CaptionButton::Close),
                bevy_color(theme.buttons.close.fill(expected_state, Focus::Focused))
            );
            assert!(
                glyph_colours(app.world_mut(), CaptionButton::Close)
                    .into_iter()
                    .all(|color| color == bevy_color(theme.buttons.close.glyph_hover))
            );
        }
    }

    #[test]
    fn focusing_one_equal_sized_window_does_not_recolour_the_other() {
        let (mut app, sender) = scene_app(ChromeStyle::Cosmix);
        let a = SurfaceId(1);
        let b = SurfaceId(2);
        let a_layout = toplevel_layout(40.0, 40.0, 1.0, SceneDecorationMode::ServerSide, false);
        let b_layout = toplevel_layout(420.0, 40.0, 2.0, SceneDecorationMode::ServerSide, false);
        publish(
            &mut app,
            &sender,
            vec![upsert(a, a_layout, 1), upsert(b, b_layout, 2)],
        );
        let b_frame_material = app.world().resource::<SurfaceEntities>().surfaces[&b]
            .decoration
            .as_ref()
            .expect("B decoration")
            .frame_material
            .clone();
        let b_before = chrome_frame_material(app.world(), b);
        let focused_a = SurfaceLayout {
            toplevel: Some(ToplevelSceneState {
                focused: true,
                ..a_layout.toplevel.expect("toplevel")
            }),
            ..a_layout
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id: a,
                scene: scene(focused_a),
            }],
        );
        assert_eq!(
            app.world().resource::<SurfaceEntities>().surfaces[&b]
                .decoration
                .as_ref()
                .expect("B decoration")
                .frame_material,
            b_frame_material
        );
        assert_eq!(chrome_frame_material(app.world(), b), b_before);
        assert_ne!(
            chrome_frame_material(app.world(), a).titlebar_color,
            b_before.titlebar_color
        );
    }

    #[test]
    fn resizing_chrome_does_not_grow_the_colour_material_palette() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let initial = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, initial, 1)]);
        let palette_size = app
            .world()
            .resource::<DecorationRenderAssets>()
            .materials
            .len();
        for width in 321..=480 {
            let layout = SurfaceLayout {
                width: width as f32,
                toplevel: Some(ToplevelSceneState {
                    window_geometry: SceneWindowGeometry {
                        width: width as f32,
                        ..initial.toplevel.expect("toplevel").window_geometry
                    },
                    ..initial.toplevel.expect("toplevel")
                }),
                ..initial
            };
            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::SurfaceRelayout {
                    id,
                    scene: scene(layout),
                }],
            );
        }
        assert_eq!(
            app.world()
                .resource::<DecorationRenderAssets>()
                .materials
                .len(),
            palette_size
        );
    }

    #[test]
    fn decorated_windows_popups_and_shadow_lanes_fit_inside_rank_gap() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let a = SurfaceId(1);
        let popup = SurfaceId(2);
        let b = SurfaceId(3);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(
                    a,
                    toplevel_layout(20.0, 20.0, 1.0, SceneDecorationMode::ServerSide, false),
                    1,
                ),
                upsert_kind(popup, popup_layout(a, 2.0), SceneSurfaceKind::Popup, 2),
                upsert(
                    b,
                    toplevel_layout(220.0, 120.0, 3.0, SceneDecorationMode::ServerSide, true),
                    3,
                ),
            ],
        );
        let surfaces = &app.world().resource::<SurfaceEntities>().surfaces;
        let a_z = surfaces[&a].renderer_z;
        let popup_z = surfaces[&popup].renderer_z;
        let b_z = surfaces[&b].renderer_z;
        assert!(a_z + 3.0 * DECO_Z_EPSILON < popup_z);
        assert!(popup_z < b_z - 2.0 * DECO_Z_EPSILON);
        assert!(
            5.0 * std::hint::black_box(DECO_Z_EPSILON) < std::hint::black_box(MIN_CLIENT_Z_GAP)
        );
        let a_root = surfaces[&a].decoration.as_ref().expect("A decoration").root;
        let b_root = surfaces[&b].decoration.as_ref().expect("B decoration").root;
        assert_eq!(
            app.world()
                .get::<Transform>(a_root)
                .expect("A root")
                .translation
                .z,
            a_z
        );
        assert_eq!(
            app.world()
                .get::<Transform>(b_root)
                .expect("B root")
                .translation
                .z,
            b_z
        );
        assert_eq!(
            app.world()
                .get::<ChildOf>(surfaces[&popup].entity)
                .map(ChildOf::parent),
            Some(surfaces[&a].entity)
        );
    }

    #[test]
    fn lower_decorated_chrome_stays_below_an_undecorated_window_and_its_popup() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let decorated = SurfaceId(1);
        let undecorated = SurfaceId(2);
        let popup = SurfaceId(3);
        publish(
            &mut app,
            &sender,
            vec![
                upsert(
                    decorated,
                    toplevel_layout(20.0, 20.0, 1.0, SceneDecorationMode::ServerSide, false),
                    1,
                ),
                upsert(
                    undecorated,
                    toplevel_layout(60.0, 50.0, 2.0, SceneDecorationMode::ClientSide, true),
                    2,
                ),
                upsert_kind(
                    popup,
                    popup_layout(undecorated, 3.0),
                    SceneSurfaceKind::Popup,
                    3,
                ),
            ],
        );
        let surfaces = &app.world().resource::<SurfaceEntities>().surfaces;
        let decorated_z = surfaces[&decorated].renderer_z;
        let undecorated_z = surfaces[&undecorated].renderer_z;
        let popup_z = surfaces[&popup].renderer_z;
        assert!(surfaces[&undecorated].decoration.is_none());
        assert!(decorated_z + 3.0 * DECO_Z_EPSILON < undecorated_z);
        assert!(decorated_z + 3.0 * DECO_Z_EPSILON < popup_z);
        assert_eq!(
            app.world()
                .get::<ChildOf>(surfaces[&popup].entity)
                .map(ChildOf::parent),
            Some(surfaces[&undecorated].entity)
        );
    }

    #[test]
    fn scale120_one_and_two_point_five_align_outer_chrome_and_client_edges() {
        for scale120 in [120_u32, 300_u32] {
            let (mut app, sender) = scene_app(ChromeStyle::Win11);
            set_compositor_logical_output_geometry(
                app.world_mut(),
                960,
                640,
                OutputScale120::new(scale120).expect("positive exact scale"),
            );
            let id = SurfaceId(1);
            publish(
                &mut app,
                &sender,
                vec![upsert(
                    id,
                    toplevel_layout(100.2, 80.2, 1.0, SceneDecorationMode::ServerSide, true),
                    1,
                )],
            );
            let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
            let decoration = surface.decoration.as_ref().expect("decoration");
            let frame = local_frame_edges(app.world(), decoration);
            let frame_material = app
                .world()
                .resource::<Assets<ChromeFrameMaterial>>()
                .get(&decoration.frame_material)
                .expect("frame material");
            let title = (
                frame.0 + frame_material.border_insets.x,
                frame.1 + frame_material.border_insets.y,
                frame.2 - frame_material.border_insets.z,
                frame.1 + frame_material.titlebar_bottom,
            );
            let client_transform = decoration.client_transform;
            let client_size = app
                .world()
                .resource::<Assets<ClientSurfaceMaterial>>()
                .get(&surface.material)
                .map(|material| material.custom_size)
                .expect("client size");
            let client = (
                client_transform.translation.x - client_size.x / 2.0,
                -client_transform.translation.y - client_size.y / 2.0,
                client_transform.translation.x + client_size.x / 2.0,
                -client_transform.translation.y + client_size.y / 2.0,
            );
            let scale = scale120 as f32 / 120.0;
            let assert_same_physical_edge = |left: f32, right: f32| {
                assert_eq!(
                    (left * scale).round() as i64,
                    (right * scale).round() as i64,
                    "scale120={scale120}: {left} and {right} must share one physical edge"
                );
            };

            assert_eq!((frame.0, frame.1), (0.0, 0.0));
            assert_same_physical_edge(title.3, client.1);
            assert_same_physical_edge(client.0, title.0);
            assert_same_physical_edge(client.2, title.2);
            assert_same_physical_edge(frame.3 - frame_material.border_insets.w, client.3);
            let mut buttons = decoration
                .buttons
                .iter()
                .map(|(_, entity)| local_quad_edges(app.world(), *entity))
                .collect::<Vec<_>>();
            buttons.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
            assert_same_physical_edge(buttons[0].2, buttons[1].0);
            assert_same_physical_edge(buttons[1].2, buttons[2].0);

            for edge in [
                frame.0,
                frame.1,
                frame.2,
                frame.3,
                title.0,
                title.1,
                title.2,
                title.3,
                client.0,
                client.1,
                client.2,
                client.3,
                buttons[0].0,
                buttons[0].1,
                buttons[0].2,
                buttons[0].3,
                buttons[1].0,
                buttons[1].1,
                buttons[1].2,
                buttons[1].3,
                buttons[2].0,
                buttons[2].1,
                buttons[2].2,
                buttons[2].3,
            ] {
                let physical = edge * scale;
                assert!(
                    (physical - physical.round()).abs() < 0.001,
                    "scale120={scale120} leaves edge {edge} between physical pixels"
                );
            }
        }
    }

    /// Every mesh a decorated surface owns, found the way the fix finds them:
    /// by walking the ECS hierarchy from the decoration root and the client
    /// entity, not by listing the parts this decoration happens to have today.
    fn meshes_under_surface(world: &World, id: SurfaceId) -> Vec<Entity> {
        let surface = &world.resource::<SurfaceEntities>().surfaces[&id];
        let mut stack = vec![surface.entity];
        if let Some(decoration) = surface.decoration.as_ref() {
            stack.push(decoration.root);
        }
        let mut seen = std::collections::HashSet::new();
        let mut meshes = Vec::new();
        while let Some(entity) = stack.pop() {
            if !seen.insert(entity) {
                continue;
            }
            if world.get::<Mesh2d>(entity).is_some() {
                meshes.push(entity);
            }
            if let Some(children) = world.get::<Children>(entity) {
                stack.extend(children.iter());
            }
        }
        meshes
    }

    /// The unit-level guard for this fix lives in `compositor_scene` and watches
    /// only the client quad in a bare world. That is not enough: the visible
    /// fault was a stale *chrome frame* drawing over a correctly-ordered title,
    /// so the parts that must requeue are the shadow, the frame, the caption
    /// buttons and their glyphs -- none of which exist in a bare world, and all
    /// of which the fix reaches only through the hierarchy walk. Run the real
    /// SSD scene so that a walk which stops covering the decoration root fails
    /// here even while the unit test stays green.
    #[test]
    fn opening_a_window_requeues_every_decoration_mesh_of_the_windows_it_reranks() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        for id in [1u64, 2] {
            publish(
                &mut app,
                &sender,
                vec![upsert(
                    SurfaceId(id),
                    toplevel_layout(
                        40.0 * id as f32,
                        30.0 * id as f32,
                        id as f32,
                        SceneDecorationMode::ServerSide,
                        id == 2,
                    ),
                    id as u8,
                )],
            );
        }

        // Compare change ticks rather than `Ref::is_changed`: `App::update` ends
        // by clearing trackers, so a direct world query after it reports nothing
        // as changed and the assertion would pass vacuously.
        let tick = |world: &World, entity: Entity| {
            world
                .entity(entity)
                .get_change_ticks::<Mesh2d>()
                .expect("a decoration mesh keeps its Mesh2d")
                .changed
                .get()
        };

        let existing = [SurfaceId(1), SurfaceId(2)].map(|id| {
            let renderer_z = app.world().resource::<SurfaceEntities>().surfaces[&id].renderer_z;
            let meshes = meshes_under_surface(app.world(), id);
            assert!(
                meshes.len() > 3,
                "surface {} should own a shadow, a frame, a client quad and the caption \
                 buttons, but the walk found {} meshes -- this test would prove nothing",
                id.0,
                meshes.len()
            );
            let before = meshes
                .iter()
                .map(|entity| tick(app.world(), *entity))
                .collect::<Vec<_>>();
            (id, meshes, before, renderer_z)
        });

        publish(
            &mut app,
            &sender,
            vec![upsert(
                SurfaceId(3),
                toplevel_layout(120.0, 90.0, 3.0, SceneDecorationMode::ServerSide, true),
                3,
            )],
        );

        for (id, meshes, before, previous_z) in &existing {
            let current_z = app.world().resource::<SurfaceEntities>().surfaces[id].renderer_z;
            assert_ne!(
                *previous_z, current_z,
                "surface {} must be re-ranked by the third window or this test proves nothing",
                id.0
            );
            for (entity, before) in meshes.iter().zip(before) {
                assert!(
                    tick(app.world(), *entity) > *before,
                    "surface {}'s mesh {entity} was not marked changed when its Z moved from \
                     {previous_z} to {current_z}, so Bevy keeps that part's old Transparent2d \
                     sort key and composites it in the window's previous stacking position",
                    id.0
                );
            }
        }
    }

    #[test]
    fn pure_window_translation_does_not_mutate_client_material() {
        let (mut app, sender) = scene_app(ChromeStyle::Win11);
        let id = SurfaceId(1);
        let initial = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, initial, 1)]);
        let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let material_handle = surface.material.clone();
        let material_before = app
            .world()
            .resource::<Assets<ClientSurfaceMaterial>>()
            .get(&material_handle)
            .expect("client material exists")
            .clone();
        app.world_mut().clear_trackers();
        let moved = SurfaceLayout {
            x: 140.0,
            y: 110.0,
            ..initial
        };
        sender
            .send(vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(moved),
            }])
            .expect("scene feed accepts move");
        drain_protocol_events(app.world_mut());
        sync_static_decorations(app.world_mut());
        let root = root_and_client(app.world(), id).0;
        let world = app.world_mut();
        let mut changed = std::collections::HashSet::new();
        changed.extend(
            world
                .query::<(Entity, Ref<Transform>)>()
                .iter(world)
                .filter_map(|(entity, value)| value.is_changed().then_some(entity)),
        );
        changed.extend(
            world
                .query::<(Entity, Ref<MeshMaterial2d<ClientSurfaceMaterial>>)>()
                .iter(world)
                .filter_map(|(entity, value)| value.is_changed().then_some(entity)),
        );
        changed.extend(
            world
                .query::<(Entity, Ref<Visibility>)>()
                .iter(world)
                .filter_map(|(entity, value)| value.is_changed().then_some(entity)),
        );
        changed.extend(
            world
                .query::<(Entity, Ref<ChildOf>)>()
                .iter(world)
                .filter_map(|(entity, value)| value.is_changed().then_some(entity)),
        );
        changed.extend(
            world
                .query::<(Entity, Ref<MeshMaterial2d<ColorMaterial>>)>()
                .iter(world)
                .filter_map(|(entity, value)| value.is_changed().then_some(entity)),
        );
        assert_eq!(changed, std::collections::HashSet::from([root]));
        let surface = &world.resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(surface.material, material_handle);
        assert_eq!(
            world
                .resource::<Assets<ClientSurfaceMaterial>>()
                .get(&material_handle),
            Some(&material_before)
        );
    }

    #[test]
    fn minimized_visibility_relayout_hides_and_restores_the_root_and_client() {
        let (mut app, sender) = scene_app(ChromeStyle::Mac);
        let id = SurfaceId(1);
        let visible = toplevel_layout(100.0, 80.0, 1.0, SceneDecorationMode::ServerSide, true);
        publish(&mut app, &sender, vec![upsert(id, visible, 1)]);
        let (root, client) = root_and_client(app.world(), id);

        let hidden = SurfaceLayout {
            visible: false,
            ..visible
        };
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(hidden),
            }],
        );
        assert_eq!(
            app.world().get::<Visibility>(root),
            Some(&Visibility::Hidden)
        );
        assert_eq!(
            app.world().get::<Visibility>(client),
            Some(&Visibility::Hidden)
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(visible),
            }],
        );
        assert_eq!(
            app.world().get::<Visibility>(root),
            Some(&Visibility::Inherited)
        );
        assert_eq!(
            app.world().get::<Visibility>(client),
            Some(&Visibility::Inherited)
        );
    }

    #[test]
    fn decoration_z_sublanes_fit_inside_the_minimum_client_rank_gap() {
        let epsilon = std::hint::black_box(DECO_Z_EPSILON);
        let minimum_gap = std::hint::black_box(MIN_CLIENT_Z_GAP);
        assert!(5.0 * epsilon < minimum_gap);
    }
}
