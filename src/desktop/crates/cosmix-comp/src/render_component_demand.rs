//! Observe the installed 2D scene's renderer-facing components after Main work.
//! This complements protocol and asset demand; it does not certify GPU readiness.

use bevy::{
    camera::{
        CameraMainTextureUsages, RenderTarget,
        visibility::{NoFrustumCulling, RenderLayers},
    },
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    prelude::*,
    render::camera::CameraRenderGraph,
    sprite::{Anchor, SpriteMesh},
    sprite_render::{MeshMaterial2d, SpriteMaterial},
    text::{ComputedTextBlock, FontHinting, LetterSpacing, LineHeight, TextBounds, TextLayoutInfo},
};

use crate::{compositor_scene::SceneContentRevision, render_asset_demand::RenderDemandObservation};
use std::collections::HashMap;

/// Keep this inventory aligned with the compositor's installed scene/extractors.
/// It intentionally observes changes even on currently hidden entities: visibility
/// can change later in the same Main turn, and removals no longer have components
/// available for filtering. New render features must extend this inventory.
pub(crate) fn configure(app: &mut App) {
    macro_rules! track {
        ($($component:ty),+ $(,)?) => {
            $(app.add_systems(RenderDemandObservation, observe::<$component>);)+
        };
    }
    track!(
        Transform,
        GlobalTransform,
        Visibility,
        InheritedVisibility,
        ViewVisibility,
        ChildOf,
        Children,
        RenderLayers,
        NoFrustumCulling,
        Sprite,
        SpriteMesh,
        Anchor,
        Mesh2d,
        MeshMaterial2d<SpriteMaterial>,
        MeshMaterial2d<ColorMaterial>,
        MeshMaterial2d<crate::client_surface_material::ClientSurfaceMaterial>,
        MeshMaterial2d<crate::chrome_frame_material::ChromeFrameMaterial>,
        MeshMaterial2d<crate::shadow_material::ShadowMaterial>,
        Text2d,
        TextFont,
        TextColor,
        TextLayout,
        TextBounds,
        LineHeight,
        LetterSpacing,
        FontHinting,
        ComputedTextBlock,
        TextLayoutInfo,
        Camera2d,
        RenderTarget,
        CameraRenderGraph,
        CameraMainTextureUsages,
        Msaa,
        Tonemapping,
        DebandDither,
    );
    app.add_systems(
        RenderDemandObservation,
        (
            observe_values::<Camera>,
            observe_values::<Projection>,
            observe_clear_color,
        ),
    );
}

fn observe<C: Component>(
    changed: Query<(), Changed<C>>,
    mut removed: RemovedComponents<C>,
    mut revision: ResMut<SceneContentRevision>,
) {
    let has_removals = !removed.is_empty();
    // Advance this reader only. Extraction and other observers keep their own
    // cursors, and removals remain in the World until normal end-of-update cleanup.
    removed.clear();
    if !changed.is_empty() || has_removals {
        revision.advance();
    }
}

// KMS refreshes ManualTextureViews each acquired frame. Bevy consequently
// rewrites camera/projection bookkeeping even when every rendered value stays
// identical. Comparing values breaks that self-sustaining render demand cycle.
trait RenderDemandValue: Component + Clone {
    fn equivalent(&self, other: &Self) -> bool;
}

fn observe_values<C: RenderDemandValue>(
    changed: Query<(Entity, &C), Changed<C>>,
    mut removed: RemovedComponents<C>,
    mut previous: Local<HashMap<Entity, C>>,
    mut revision: ResMut<SceneContentRevision>,
) {
    let mut dirty = false;
    for entity in removed.read() {
        previous.remove(&entity);
        dirty = true;
    }
    for (entity, value) in &changed {
        if previous
            .get(&entity)
            .is_none_or(|old| !value.equivalent(old))
        {
            previous.insert(entity, value.clone());
            dirty = true;
        }
    }
    if dirty {
        revision.advance();
    }
}

fn clear_color_value(value: bevy::camera::ClearColorConfig) -> Option<Option<Color>> {
    match value {
        bevy::camera::ClearColorConfig::Default => None,
        bevy::camera::ClearColorConfig::Custom(color) => Some(Some(color)),
        bevy::camera::ClearColorConfig::None => Some(None),
    }
}

impl RenderDemandValue for Camera {
    fn equivalent(&self, other: &Self) -> bool {
        let viewport = |camera: &Camera| {
            camera
                .viewport
                .as_ref()
                .map(|v| (v.physical_position, v.physical_size, v.depth.clone()))
        };
        let target = |camera: &Camera| {
            camera
                .computed
                .target_info
                .as_ref()
                .map(|v| (v.physical_size, v.scale_factor))
        };
        let output = |camera: &Camera| match camera.output_mode {
            bevy::camera::CameraOutputMode::Skip => None,
            bevy::camera::CameraOutputMode::Write {
                blend_state,
                clear_color,
            } => Some((blend_state, clear_color_value(clear_color))),
        };
        viewport(self) == viewport(other)
            && target(self) == target(other)
            && self.order == other.order
            && self.is_active == other.is_active
            && self.computed.clip_from_view == other.computed.clip_from_view
            && output(self) == output(other)
            && self.msaa_writeback == other.msaa_writeback
            && clear_color_value(self.clear_color) == clear_color_value(other.clear_color)
            && self.invert_culling == other.invert_culling
            && self.sub_camera_view == other.sub_camera_view
    }
}

fn scaling_value(value: bevy::camera::ScalingMode) -> (u8, f32, f32) {
    use bevy::camera::ScalingMode::*;
    match value {
        WindowSize => (0, 0.0, 0.0),
        Fixed { width, height } => (1, width, height),
        AutoMin {
            min_width,
            min_height,
        } => (2, min_width, min_height),
        AutoMax {
            max_width,
            max_height,
        } => (3, max_width, max_height),
        FixedVertical { viewport_height } => (4, 0.0, viewport_height),
        FixedHorizontal { viewport_width } => (5, viewport_width, 0.0),
    }
}

impl RenderDemandValue for Projection {
    fn equivalent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Orthographic(a), Self::Orthographic(b)) => {
                a.near == b.near
                    && a.far == b.far
                    && a.viewport_origin == b.viewport_origin
                    && scaling_value(a.scaling_mode) == scaling_value(b.scaling_mode)
                    && a.scale == b.scale
                    && a.area == b.area
            }
            (Self::Perspective(a), Self::Perspective(b)) => {
                a.fov == b.fov
                    && a.aspect_ratio == b.aspect_ratio
                    && a.near == b.near
                    && a.far == b.far
                    && a.near_clip_plane == b.near_clip_plane
            }
            // Custom implementations may contain additional culling behaviour.
            _ => false,
        }
    }
}

fn observe_clear_color(
    color: Option<Res<ClearColor>>,
    mut previous: Local<Option<Color>>,
    mut revision: ResMut<SceneContentRevision>,
) {
    let current = color.as_ref().map(|color| color.0);
    if *previous != current {
        *previous = current;
        revision.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(app: &mut App) {
        app.init_resource::<SceneContentRevision>();
        crate::render_asset_demand::configure(app);
        configure(app);
    }

    fn revision(app: &App) -> u64 {
        app.world().resource::<SceneContentRevision>().0.unwrap()
    }

    #[test]
    fn camera_bookkeeping_is_quiet_but_view_projection_and_clear_changes_wake() {
        let mut app = App::new();
        install(&mut app);
        let entity = app
            .world_mut()
            .spawn((
                Camera2d,
                Projection::Orthographic(OrthographicProjection::default_2d()),
            ))
            .id();
        app.update();
        app.update();
        let before = revision(&app);
        app.world_mut()
            .get_mut::<Camera>(entity)
            .unwrap()
            .computed
            .old_viewport_size = Some(UVec2::new(320, 240));
        app.update();
        assert_eq!(
            revision(&app),
            before,
            "internal bookkeeping does not change pixels"
        );
        app.world_mut().get_mut::<Camera>(entity).unwrap().viewport =
            Some(bevy::camera::Viewport {
                physical_size: UVec2::new(320, 240),
                ..Default::default()
            });
        app.update();
        assert!(revision(&app) > before);
        let before = revision(&app);
        app.world_mut()
            .get_mut::<Camera>(entity)
            .unwrap()
            .computed
            .clip_from_view = Mat4::from_scale(Vec3::splat(2.0));
        app.update();
        assert!(revision(&app) > before);
        let before = revision(&app);
        if let Projection::Orthographic(projection) =
            &mut *app.world_mut().get_mut::<Projection>(entity).unwrap()
        {
            projection.scale = 2.0;
        }
        app.update();
        assert!(revision(&app) > before);
        let before = revision(&app);
        app.world_mut().entity_mut(entity).remove::<Projection>();
        app.world_mut()
            .entity_mut(entity)
            .insert(Projection::Orthographic(
                OrthographicProjection::default_2d(),
            ));
        app.update();
        assert!(
            revision(&app) > before,
            "removal/reinsertion retains demand"
        );
        let before = revision(&app);
        app.insert_resource(ClearColor(Color::BLACK));
        app.update();
        assert!(revision(&app) > before);
        let before = revision(&app);
        app.world_mut().resource_mut::<ClearColor>().0 = Color::BLACK;
        app.update();
        assert_eq!(revision(&app), before);
        app.world_mut().resource_mut::<ClearColor>().0 = Color::WHITE;
        app.update();
        assert!(revision(&app) > before);
    }

    #[test]
    fn late_layout_and_style_changes_wake_without_protocol_or_asset_events() {
        let mut app = App::new();
        install(&mut app);
        let title = app.world_mut().spawn(TextLayoutInfo::default()).id();
        app.update();
        let initial = revision(&app);
        app.update();
        assert_eq!(revision(&app), initial);
        // Represents layout completing with glyphs already in an existing atlas:
        // there need not be any Image or Font mutation to wake the renderer.
        app.add_systems(
            Last,
            move |mut commands: Commands, mut once: Local<bool>| {
                if !*once {
                    commands.entity(title).insert(TextLayoutInfo {
                        size: Vec2::new(80.0, 20.0),
                        ..Default::default()
                    });
                    *once = true;
                }
            },
        );
        app.update();
        assert!(revision(&app) > initial);
        let laid_out = revision(&app);
        app.update();
        assert_eq!(revision(&app), laid_out);
        app.world_mut()
            .entity_mut(title)
            .insert(TextColor(Color::WHITE));
        app.update();
        assert!(revision(&app) > laid_out);
        let coloured = revision(&app);
        app.world_mut().get_mut::<TextColor>(title).unwrap().0 = Color::BLACK;
        app.update();
        assert!(revision(&app) > coloured);
    }

    #[test]
    fn removal_wakes_once_and_remains_available_to_extraction() {
        let mut app = App::new();
        install(&mut app);
        let entity = app
            .world_mut()
            .spawn((Transform::default(), Sprite::default()))
            .id();
        app.update();
        app.update();
        let initial = revision(&app);
        app.world_mut().entity_mut(entity).remove::<Sprite>();
        app.main_mut().run_default_schedule();
        assert!(revision(&app) > initial);
        assert_eq!(
            app.world().removed::<Sprite>().collect::<Vec<_>>(),
            vec![entity]
        );
        app.world_mut().clear_trackers();
        let removed = revision(&app);
        for _ in 0..4 {
            app.update();
            assert_eq!(revision(&app), removed);
        }
        app.world_mut().despawn(entity);
        app.update();
        assert!(revision(&app) > removed);
        app.world_mut().resource_mut::<SceneContentRevision>().0 = Some(u64::MAX);
        app.world_mut().spawn(Transform::default());
        app.update();
        assert_eq!(app.world().resource::<SceneContentRevision>().0, None);
    }

    #[test]
    fn delayed_real_text_layout_wakes_with_unchanged_cached_atlas_pixels() {
        use bevy::{
            app::MainScheduleOrder,
            asset::AssetId,
            camera::{ComputedCameraValues, RenderTargetInfo, visibility::VisibleEntities},
            text::{
                FontAtlasSet, FontCx, LayoutCx, RemSize, ScaleCx, TextIterScratch, TextPipeline,
            },
        };
        #[derive(Resource)]
        struct LayoutEnabled(bool);
        let mut app = App::new();
        app.init_resource::<Assets<Font>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<Assets<TextureAtlasLayout>>()
            .init_resource::<FontAtlasSet>()
            .init_resource::<TextPipeline>()
            .init_resource::<FontCx>()
            .init_resource::<LayoutCx>()
            .init_resource::<ScaleCx>()
            .init_resource::<TextIterScratch>()
            .init_resource::<RemSize>()
            .init_resource::<SceneContentRevision>()
            .insert_resource(LayoutEnabled(true))
            .add_systems(
                PostUpdate,
                (
                    bevy::text::detect_text_needs_rerender,
                    bevy::sprite::update_text2d_layout
                        .run_if(|enabled: Res<LayoutEnabled>| enabled.0),
                )
                    .chain(),
            );
        // Isolate component demand: actual asset stores exist, but this test
        // does not install the separate resource/event observers.
        app.init_schedule(RenderDemandObservation);
        app.world_mut()
            .resource_mut::<MainScheduleOrder>()
            .insert_after(Last, RenderDemandObservation);
        configure(&mut app);
        let mut font = Font::from_bytes(include_bytes!("../assets/fonts/DejaVuSans.ttf").to_vec());
        font.alias = "DejaVu Sans".into();
        app.world_mut()
            .resource_mut::<FontCx>()
            .collection
            .register_fonts(font.data.clone(), None);
        app.world_mut()
            .resource_mut::<Assets<Font>>()
            .insert(AssetId::default(), font)
            .unwrap();
        let mut visible = VisibleEntities::default();
        visible.push(Entity::PLACEHOLDER, std::any::TypeId::of::<Sprite>());
        app.world_mut().spawn((
            Camera {
                computed: ComputedCameraValues {
                    target_info: Some(RenderTargetInfo {
                        physical_size: UVec2::splat(1000),
                        scale_factor: 1.0,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            visible,
        ));
        let title = app
            .world_mut()
            .spawn(Text2d::new("idle text idle text"))
            .id();
        for _ in 0..3 {
            app.update();
        }
        let initial_size = app.world().get::<TextLayoutInfo>(title).unwrap().size;
        assert!(initial_size.x > 50.0 && initial_size.y > 0.0);
        let settled = revision(&app);
        for _ in 0..8 {
            app.update();
            assert_eq!(revision(&app), settled);
        }
        let pixels = |app: &App| {
            app.world()
                .resource::<Assets<Image>>()
                .iter()
                .map(|(id, image)| (id, image.data.clone()))
                .collect::<Vec<_>>()
        };
        let atlas = pixels(&app);
        assert!(!atlas.is_empty());
        app.world_mut().resource_mut::<LayoutEnabled>().0 = false;
        app.world_mut().entity_mut(title).insert(TextBounds {
            width: Some(50.0),
            height: None,
        });
        app.update();
        assert_eq!(
            app.world().get::<TextLayoutInfo>(title).unwrap().size,
            initial_size
        );
        let awaiting_layout = revision(&app);
        app.world_mut().resource_mut::<LayoutEnabled>().0 = true;
        app.update();
        assert!(app.world().get::<TextLayoutInfo>(title).unwrap().size.y > initial_size.y);
        assert!(revision(&app) > awaiting_layout);
        assert_eq!(
            pixels(&app),
            atlas,
            "existing glyph pixels suffice for relayout"
        );
        let completed = revision(&app);
        for _ in 0..4 {
            app.update();
            assert_eq!(revision(&app), completed);
        }
    }

    #[test]
    fn real_camera_transform_and_visibility_maintenance_settle() {
        use bevy::{
            asset::{AssetApp, AssetPlugin},
            camera::visibility::{NoFrustumCulling, VisibilityPlugin},
            mesh::MeshPlugin,
            render::{camera::camera_system, texture::ManualTextureViews},
            transform::TransformPlugin,
            window::{WindowCreated, WindowResized, WindowScaleFactorChanged},
        };
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .init_asset::<Image>()
        .init_resource::<ManualTextureViews>()
        .add_message::<WindowCreated>()
        .add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_systems(
            PostUpdate,
            camera_system.before(bevy::camera::visibility::update_frusta),
        );
        install(&mut app);
        let image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::default());
        app.world_mut()
            .spawn((Camera2d, RenderTarget::Image(image.into())));
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        let entity = app.world_mut().spawn((Mesh2d(mesh), NoFrustumCulling)).id();
        for _ in 0..3 {
            app.update();
        }
        let settled = revision(&app);
        assert!(app.world().get::<ViewVisibility>(entity).unwrap().get());
        for _ in 0..8 {
            app.update();
            assert_eq!(revision(&app), settled);
        }
        *app.world_mut().get_mut::<Visibility>(entity).unwrap() = Visibility::Hidden;
        app.update();
        assert!(!app.world().get::<ViewVisibility>(entity).unwrap().get());
        assert!(revision(&app) > settled);
        let hidden = revision(&app);
        for _ in 0..4 {
            app.update();
            assert_eq!(revision(&app), hidden);
        }
        *app.world_mut().get_mut::<Visibility>(entity).unwrap() = Visibility::Visible;
        app.world_mut()
            .get_mut::<Transform>(entity)
            .unwrap()
            .translation
            .x = 2.0;
        app.update();
        assert!(revision(&app) > hidden);
        assert!(app.world().get::<ViewVisibility>(entity).unwrap().get());
    }
}
