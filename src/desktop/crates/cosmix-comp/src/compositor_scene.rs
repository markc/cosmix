//! Shared protocol-event to Bevy scene projection.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::NoFrustumCulling,
    image::ImageSampler,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    sprite::SpriteAlphaMode,
    sprite_render::{MeshMaterial2d, SpriteMaterial},
    window::{CursorIcon, PrimaryWindow, SystemCursorIcon},
};
use cosmix_wgpu_dmabuf::{DmabufRelease, ImportedDmabufImages, ReleaseCallback};
use smithay::reexports::wayland_server::backend::ObjectId;

use crate::protocol::{
    ChromeCursorIcon, ClientSceneFeed, CursorImage, CursorPositionSnapshot, CursorPresentation,
    DmabufUseId, MAX_GLOBAL_SURFACES, ProtocolEvent, SceneSurfaceKind, ShmFrame, SurfaceFrame,
    SurfaceId, SurfaceLayout, SurfaceSceneSnapshot, SurfaceTransform,
};
use crate::{
    client_surface_material::{
        ClientSurfaceImage, ClientSurfaceMaterial, ClientSurfaceMaterialPlugin,
        ClientSurfaceRenderAssets, ClientSurfaceSamplingContract,
    },
    decoration::DecorationStartup,
    decoration_scene::{
        DecorationDirtySurfaceIds, DecorationEntities, DecorationPlugin, remove_static_decoration,
    },
};

pub(crate) const CLIENT_CONTENT_Z_MIN: f32 = 0.0;
pub(crate) const CLIENT_CONTENT_Z_MAX: f32 = 900.0;
const CURSOR_Z: f32 = 950.0;
const DEFAULT_CURSOR_WIDTH: u32 = 16;
const DEFAULT_CURSOR_HEIGHT: u32 = 20;
const DEFAULT_CURSOR_MASTER_SCALE: u32 = 3;
const RESIZE_CURSOR_SIZE: u32 = 21;
const RESIZE_CURSOR_MASTER_SCALE: u32 = 2;
const RESIZE_CURSOR_HOTSPOT: (i32, i32) = (10, 10);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RendererRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClientSurfaceClip {
    pub(crate) clip_from_uv: Mat3,
    pub(crate) clip_size: Vec2,
    pub(crate) corner_radius: f32,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DmabufOutputProbeSurface {
    pub(crate) surface_id: SurfaceId,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) opaque: bool,
}

#[cfg(all(feature = "kms-live", not(test)))]
#[derive(
    Clone, Debug, Default, PartialEq, Resource, bevy::render::extract_resource::ExtractResource,
)]
pub(crate) struct DmabufOutputProbeSurfaces {
    pub(crate) canvas: Vec2,
    pub(crate) surfaces: Vec<DmabufOutputProbeSurface>,
}

fn round_half_away(value: f64) -> i64 {
    if value.is_sign_negative() {
        (value - 0.5).ceil() as i64
    } else {
        (value + 0.5).floor() as i64
    }
}

/// Project one logical edge to its sole physical pixel edge.
///
/// Edges, rather than origins and sizes, are projected so adjacent rectangles
/// calculate their shared boundary from the same input and cannot open a seam.
fn project_logical_edge(edge: f32, scale120: u32) -> i64 {
    round_half_away(f64::from(edge) * f64::from(scale120) / 120.0)
}

pub(crate) fn projected_renderer_physical_edges(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    scale120: u32,
) -> (i64, i64, i64, i64) {
    (
        project_logical_edge(x, scale120),
        project_logical_edge(y, scale120),
        project_logical_edge(x + width, scale120),
        project_logical_edge(y + height, scale120),
    )
}

pub(crate) fn renderer_rect(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    scale120: u32,
) -> RendererRect {
    if scale120 == crate::backend::kms::OutputScale120::ONE.get() {
        return RendererRect {
            x,
            y,
            width,
            height,
        };
    }
    let (left, top, right, bottom) =
        projected_renderer_physical_edges(x, y, width, height, scale120);
    let physical_to_logical = 120.0 / scale120 as f32;
    RendererRect {
        x: left as f32 * physical_to_logical,
        y: top as f32 * physical_to_logical,
        width: (right - left) as f32 * physical_to_logical,
        height: (bottom - top) as f32 * physical_to_logical,
    }
}

pub(crate) fn client_surface_clip(
    layout: SurfaceLayout,
    window_x: f32,
    window_y: f32,
    window_width: f32,
    window_height: f32,
    corner_radius: f32,
    scale120: u32,
) -> ClientSurfaceClip {
    let surface = renderer_rect(layout.x, layout.y, layout.width, layout.height, scale120);
    let window = renderer_rect(window_x, window_y, window_width, window_height, scale120);
    let swaps_axes = surface_transform_swaps_axes(layout.transform);
    let local_size = if swaps_axes {
        Vec2::new(surface.height, surface.width)
    } else {
        Vec2::new(surface.width, surface.height)
    };
    let angle = match layout.transform {
        SurfaceTransform::Normal | SurfaceTransform::Flipped => 0.0,
        SurfaceTransform::Rotate90 | SurfaceTransform::Flipped90 => std::f32::consts::FRAC_PI_2,
        SurfaceTransform::Rotate180 | SurfaceTransform::Flipped180 => std::f32::consts::PI,
        SurfaceTransform::Rotate270 | SurfaceTransform::Flipped270 => {
            3.0 * std::f32::consts::FRAC_PI_2
        }
    };
    let surface_center = Vec2::new(
        surface.x + surface.width / 2.0,
        surface.y + surface.height / 2.0,
    );
    let uv_to_renderer = |uv: Vec2| {
        let local = Vec2::new((uv.x - 0.5) * local_size.x, (0.5 - uv.y) * local_size.y);
        let (sin, cos) = angle.sin_cos();
        let rotated = Vec2::new(cos * local.x - sin * local.y, sin * local.x + cos * local.y);
        Vec2::new(surface_center.x + rotated.x, surface_center.y - rotated.y)
    };
    let origin = uv_to_renderer(Vec2::ZERO) - Vec2::new(window.x, window.y);
    let x_axis = uv_to_renderer(Vec2::X) - Vec2::new(window.x, window.y) - origin;
    let y_axis = uv_to_renderer(Vec2::Y) - Vec2::new(window.x, window.y) - origin;
    let physical_to_logical = 120.0 / scale120 as f32;
    let projected_radius =
        (corner_radius.max(0.0) * scale120 as f32 / 120.0).round() * physical_to_logical;
    ClientSurfaceClip {
        clip_from_uv: Mat3::from_cols(x_axis.extend(0.0), y_axis.extend(0.0), origin.extend(1.0)),
        clip_size: Vec2::new(window.width, window.height),
        corner_radius: projected_radius
            .min(window.width / 2.0)
            .min(window.height / 2.0),
    }
}

/// Installs the client-content projection without choosing a camera,
/// background, input transport, or frame boundary.
pub(crate) struct CompositorScenePlugin {
    initial_canvas: Vec2,
    cursor_mode: SceneCursorMode,
}

impl CompositorScenePlugin {
    pub(crate) fn new(width: u32, height: u32, cursor_mode: SceneCursorMode) -> Self {
        Self {
            initial_canvas: Vec2::new(width as f32, height as f32),
            cursor_mode,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneCursorMode {
    HostCursor,
    SoftwareCursor,
}

impl Plugin for CompositorScenePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SurfaceEntities>()
            .init_resource::<ClientSamplingContractLog>()
            .init_resource::<DecorationStartup>()
            .insert_resource(LogicalCanvasSize(self.initial_canvas))
            .insert_resource(RendererOutputScale120(
                crate::backend::kms::OutputScale120::ONE.get(),
            ))
            .insert_resource(CursorScene::new(self.cursor_mode))
            .add_systems(First, drain_protocol_events.in_set(CompositorSceneSet))
            .add_systems(Last, log_settled_client_sampling_contracts)
            .add_plugins(ClientSurfaceMaterialPlugin)
            .add_plugins(DecorationPlugin);
        if self.cursor_mode == SceneCursorMode::SoftwareCursor {
            app.add_systems(Startup, spawn_software_cursor);
        } else {
            app.init_resource::<HostCursor>()
                .add_systems(Last, project_host_cursor);
        }
    }
}

/// Install the opt-in physical-output probe's main-to-render-world surface map.
#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) fn install_dmabuf_output_probe(app: &mut App) {
    app.init_resource::<DmabufOutputProbeSurfaces>()
        .add_plugins(bevy::render::extract_resource::ExtractResourcePlugin::<
            DmabufOutputProbeSurfaces,
        >::default())
        .add_systems(
            First,
            refresh_dmabuf_output_probe_surfaces.after(CompositorSceneSet),
        );
}

#[cfg(all(feature = "kms-live", not(test)))]
fn refresh_dmabuf_output_probe_surfaces(world: &mut World) {
    let canvas = world.resource::<LogicalCanvasSize>().0;
    let scale120 = world.resource::<RendererOutputScale120>().0;
    let mut surfaces = world
        .resource::<SurfaceEntities>()
        .surfaces
        .iter()
        .filter(|(_, surface)| {
            surface.buffer_kind == SurfaceBufferKind::Dmabuf && surface.layout.visible
        })
        .map(|(surface_id, surface)| {
            let rect = renderer_rect(
                surface.layout.x,
                surface.layout.y,
                surface.layout.width,
                surface.layout.height,
                scale120,
            );
            DmabufOutputProbeSurface {
                surface_id: *surface_id,
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                opaque: surface.opaque,
            }
        })
        .collect::<Vec<_>>();
    surfaces.sort_unstable_by_key(|surface| surface.surface_id.0);
    let updated = DmabufOutputProbeSurfaces { canvas, surfaces };
    let mut current = world.resource_mut::<DmabufOutputProbeSurfaces>();
    if *current != updated {
        *current = updated;
    }
}

/// Lets a host preserve the nested ordering between scene input and its own
/// protocol-thread command adapters.
#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CompositorSceneSet;

/// Prevents host adapters later in the same Bevy update from obscuring the
/// protocol failure that stopped scene draining.
#[derive(Resource)]
pub(crate) struct CompositorSceneFailed;

#[derive(Resource)]
pub(crate) struct LogicalCanvasSize(pub(crate) Vec2);

/// Exact physical pixels per 120 logical units, used only to place renderer
/// geometry on shared physical pixel edges. Protocol and layout state remain
/// unsnapped logical coordinates.
#[derive(Resource)]
pub(crate) struct RendererOutputScale120(pub(crate) u32);

#[derive(Resource, Default)]
pub(crate) struct SurfaceEntities {
    pub(crate) surfaces: HashMap<SurfaceId, SurfaceEntity>,
    pub(crate) children: HashMap<SurfaceId, HashSet<SurfaceId>>,
}

impl SurfaceEntities {
    fn update_parent(
        &mut self,
        id: SurfaceId,
        old_parent: Option<SurfaceId>,
        new_parent: Option<SurfaceId>,
    ) {
        if old_parent == new_parent {
            return;
        }
        if let Some(parent) = old_parent
            && let Some(children) = self.children.get_mut(&parent)
        {
            children.remove(&id);
            if children.is_empty() {
                self.children.remove(&parent);
            }
        }
        if let Some(parent) = new_parent {
            self.children.entry(parent).or_default().insert(id);
        }
    }
}

pub(crate) struct SurfaceEntity {
    pub(crate) entity: Entity,
    image: ClientSurfaceImage,
    buffer_kind: SurfaceBufferKind,
    opaque: bool,
    pub(crate) layout: SurfaceLayout,
    pub(crate) kind: SceneSurfaceKind,
    pub(crate) title: Option<Arc<str>>,
    pub(crate) material: Handle<ClientSurfaceMaterial>,
    pub(crate) renderer_z: f32,
    pub(crate) decoration: Option<DecorationEntities>,
}

#[derive(Resource)]
struct ClientSamplingContractLog {
    enabled: bool,
    surfaces: HashMap<SurfaceId, ClientSurfaceSamplingContract>,
    cursor: Option<(ObjectId, ClientSurfaceSamplingContract)>,
}

impl Default for ClientSamplingContractLog {
    fn default() -> Self {
        Self {
            enabled: std::env::var_os("COSMIX_DMABUF_LOG_IMPORTS")
                .is_some_and(|value| value == "1"),
            surfaces: HashMap::new(),
            cursor: None,
        }
    }
}

#[derive(Resource)]
struct CursorScene {
    mode: SceneCursorMode,
    position: CursorPositionSnapshot,
    selection: ProjectedCursorSelection,
    client: Option<ProjectedClientCursor>,
    entity: Option<Entity>,
    default_image: Option<Handle<Image>>,
    resize_images: Option<ResizeCursorImages>,
}

impl CursorScene {
    fn new(mode: SceneCursorMode) -> Self {
        Self {
            mode,
            position: CursorPositionSnapshot::default(),
            selection: ProjectedCursorSelection::Default,
            client: None,
            entity: None,
            default_image: None,
            resize_images: None,
        }
    }
}

#[derive(Clone)]
struct ResizeCursorImages {
    horizontal: Handle<Image>,
    vertical: Handle<Image>,
    ne_sw: Handle<Image>,
    nw_se: Handle<Image>,
}

#[derive(Resource)]
struct HostCursor {
    icon: SystemCursorIcon,
}

impl Default for HostCursor {
    fn default() -> Self {
        Self {
            icon: SystemCursorIcon::Default,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectedCursorSelection {
    Default,
    Hidden,
    Chrome(ChromeCursorIcon),
    Surface,
}

struct ProjectedClientCursor {
    id: ObjectId,
    hotspot: (i32, i32),
    presentation: CursorPresentation,
    image: ClientSurfaceImage,
    buffer_kind: SurfaceBufferKind,
    opaque: bool,
}

#[derive(Component)]
struct SoftwareCursorEntity;

#[derive(Clone, Copy, Eq, PartialEq)]
enum SurfaceBufferKind {
    Shm,
    Dmabuf,
}

pub(crate) fn drain_protocol_events(world: &mut World) {
    let (result, cursor_position) = {
        let feed = world.resource::<ClientSceneFeed>();
        let result = feed.drain_events();
        let cursor_position = feed.cursor_position();
        (result, cursor_position)
    };
    match result {
        Ok(events) => {
            apply_protocol_events(world, events);
            sample_cursor_position(world, cursor_position);
        }
        Err(error) => fail_compositor_scene(world, error),
    }
}

fn fail_compositor_scene(world: &mut World, error: String) -> ! {
    world.insert_resource(CompositorSceneFailed);
    panic!("{error}");
}

/// Log only contracts that survived the whole frame, including decoration clip sync.
fn log_settled_client_sampling_contracts(world: &mut World) {
    if !world.resource::<ClientSamplingContractLog>().enabled {
        return;
    }

    let mut surface_materials = {
        let surfaces = world.resource::<SurfaceEntities>();
        let materials = world.resource::<Assets<ClientSurfaceMaterial>>();
        surfaces
            .surfaces
            .iter()
            .filter_map(|(id, surface)| {
                materials
                    .get(&surface.material)
                    .cloned()
                    .map(|material| (*id, material))
            })
            .collect::<Vec<_>>()
    };
    surface_materials.sort_unstable_by_key(|(id, _)| id.0);
    let live_surface_ids = world
        .resource::<SurfaceEntities>()
        .surfaces
        .keys()
        .copied()
        .collect::<HashSet<_>>();

    let cursor_material = {
        let cursor = world.resource::<CursorScene>();
        if cursor.selection != ProjectedCursorSelection::Surface {
            None
        } else {
            cursor.client.as_ref().and_then(|client| {
                let entity = cursor.entity?;
                let handle = world.get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)?;
                let material = world
                    .resource::<Assets<ClientSurfaceMaterial>>()
                    .get(&handle.0)?
                    .clone();
                Some((client.id.clone(), material))
            })
        }
    };

    let (changed_surfaces, changed_cursor) = {
        let mut state = world.resource_mut::<ClientSamplingContractLog>();
        state.surfaces.retain(|id, _| live_surface_ids.contains(id));
        let changed_surfaces = surface_materials
            .into_iter()
            .filter(|(id, material)| {
                state.surfaces.insert(*id, material.sampling_contract())
                    != Some(material.sampling_contract())
            })
            .collect::<Vec<_>>();
        let changed_cursor = cursor_material
            .as_ref()
            .filter(|(id, material)| {
                state.cursor.as_ref() != Some(&(id.clone(), material.sampling_contract()))
            })
            .cloned();
        state.cursor = cursor_material
            .as_ref()
            .map(|(id, material)| (id.clone(), material.sampling_contract()));
        (changed_surfaces, changed_cursor)
    };

    for (id, material) in changed_surfaces {
        material.log_surface_sampling_contract(id.0);
    }
    if let Some((id, material)) = changed_cursor {
        material.log_cursor_sampling_contract(id.protocol_id());
    }
}

fn apply_protocol_events(world: &mut World, events: Vec<ProtocolEvent>) {
    let mut z_ranks_dirty = false;
    for event in events {
        match event {
            ProtocolEvent::OutputResized { width, height } => {
                resize_compositor_logical_canvas(world, width, height);
            }
            ProtocolEvent::SurfaceUpserted { id, scene, frame } => {
                z_ranks_dirty |= upsert_surface_snapshot(world, id, scene, frame);
                mark_decoration_dirty(world, id);
            }
            ProtocolEvent::SurfaceRelayout { id, scene } => {
                z_ranks_dirty |= relayout_surface(world, id, scene);
                mark_decoration_dirty(world, id);
            }
            ProtocolEvent::SurfaceUnmapped { id } | ProtocolEvent::SurfaceDestroyed { id } => {
                z_ranks_dirty |= remove_surface(world, id);
            }
            ProtocolEvent::SurfaceRoster { mapped } => {
                // Membership, not a delta: whatever this world holds that the
                // compositor no longer lists goes, however it got here. The
                // protocol thread emits this ahead of every per-surface event
                // in the batch, so a later upsert for a surface that has since
                // been recreated still lands.
                let mapped = mapped.into_iter().collect::<HashSet<_>>();
                let stale = world
                    .resource::<SurfaceEntities>()
                    .surfaces
                    .keys()
                    .copied()
                    .filter(|id| !mapped.contains(id))
                    .collect::<Vec<_>>();
                for id in stale {
                    z_ranks_dirty |= remove_surface(world, id);
                }
            }
            ProtocolEvent::CursorUpdated { image } => apply_cursor_image(world, image),
            ProtocolEvent::DmabufBufferDestroyed { buffer_id } => world
                .resource::<ImportedDmabufImages>()
                .invalidate_buffer(buffer_id),
            ProtocolEvent::DmabufCacheInvalidated => world
                .resource::<ImportedDmabufImages>()
                .invalidate_all_buffers(),
            ProtocolEvent::RuntimeFailed(error) => {
                fail_compositor_scene(world, format!("Wayland protocol thread failed: {error}"));
            }
        }
    }
    if z_ranks_dirty {
        recompute_surface_z_ranks(world);
    }
}

fn spawn_software_cursor(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    canvas: Res<LogicalCanvasSize>,
    output_scale: Res<RendererOutputScale120>,
    mut cursor: ResMut<CursorScene>,
) {
    debug_assert_eq!(cursor.mode, SceneCursorMode::SoftwareCursor);
    let image = images.add(default_cursor_image());
    let resize_images = ResizeCursorImages {
        horizontal: images.add(resize_cursor_image(ResizeCursorAxis::Horizontal)),
        vertical: images.add(resize_cursor_image(ResizeCursorAxis::Vertical)),
        ne_sw: images.add(resize_cursor_image(ResizeCursorAxis::NeSw)),
        nw_se: images.add(resize_cursor_image(ResizeCursorAxis::NwSe)),
    };
    let mut sprite = SpriteMesh::from_image(image.clone());
    sprite.alpha_mode = SpriteAlphaMode::Blend;
    let scale120 = output_scale.0;
    let rendered = cursor_renderer_rect(
        cursor.position,
        (0, 0),
        Vec2::new(DEFAULT_CURSOR_WIDTH as f32, DEFAULT_CURSOR_HEIGHT as f32),
        scale120,
    );
    sprite.custom_size = Some(Vec2::new(rendered.width, rendered.height));
    let transform = cursor_transform(
        cursor.position,
        (0, 0),
        Vec2::new(DEFAULT_CURSOR_WIDTH as f32, DEFAULT_CURSOR_HEIGHT as f32),
        SurfaceTransform::Normal,
        canvas.0,
        scale120,
    );
    let visible = cursor.selection == ProjectedCursorSelection::Default;
    let entity = commands
        .spawn((
            Name::new("CosMix software cursor"),
            SoftwareCursorEntity,
            sprite,
            transform,
            if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        ))
        .id();
    cursor.entity = Some(entity);
    cursor.default_image = Some(image);
    cursor.resize_images = Some(resize_images);
}

fn project_host_cursor(
    host: Res<HostCursor>,
    mut commands: Commands,
    windows: Query<Entity, With<PrimaryWindow>>,
) {
    if !host.is_changed() {
        return;
    }
    for window in &windows {
        commands
            .entity(window)
            .insert(CursorIcon::System(host.icon));
    }
}

fn sample_cursor_position(world: &mut World, position: CursorPositionSnapshot) {
    world.resource_mut::<CursorScene>().position = position;
    refresh_cursor_entity(world);
}

fn apply_cursor_image(world: &mut World, image: CursorImage) {
    if world.resource::<CursorScene>().mode == SceneCursorMode::HostCursor {
        let icon = match &image {
            CursorImage::Chrome(cursor) => host_cursor_icon(*cursor),
            CursorImage::Default | CursorImage::Hidden | CursorImage::Surface { .. } => {
                SystemCursorIcon::Default
            }
        };
        world.resource_mut::<HostCursor>().icon = icon;
        if let CursorImage::Surface {
            frame: Some(SurfaceFrame::Dmabuf(frame)),
            ..
        } = image
        {
            world
                .resource::<ClientSceneFeed>()
                .dmabuf_release_callback(frame.token)();
        }
        return;
    }

    match image {
        CursorImage::Default => {
            clear_client_cursor(world);
            world.resource_mut::<CursorScene>().selection = ProjectedCursorSelection::Default;
        }
        CursorImage::Hidden => {
            clear_client_cursor(world);
            world.resource_mut::<CursorScene>().selection = ProjectedCursorSelection::Hidden;
        }
        CursorImage::Chrome(cursor) => {
            world.resource_mut::<CursorScene>().selection =
                ProjectedCursorSelection::Chrome(cursor);
        }
        CursorImage::Surface {
            id,
            hotspot,
            presentation,
            frame,
        } => match frame {
            Some(frame) => upsert_client_cursor(world, id, hotspot, presentation, frame),
            None => {
                let mut cursor = world.resource_mut::<CursorScene>();
                if let Some(client) = cursor.client.as_mut().filter(|client| client.id == id) {
                    client.hotspot = hotspot;
                    client.presentation = presentation;
                    cursor.selection = ProjectedCursorSelection::Surface;
                } else {
                    cursor.selection = ProjectedCursorSelection::Hidden;
                }
            }
        },
    }
    refresh_cursor_entity(world);
}

fn host_cursor_icon(cursor: ChromeCursorIcon) -> SystemCursorIcon {
    match cursor {
        ChromeCursorIcon::Move => SystemCursorIcon::Move,
        ChromeCursorIcon::NResize => SystemCursorIcon::NResize,
        ChromeCursorIcon::NeResize => SystemCursorIcon::NeResize,
        ChromeCursorIcon::EResize => SystemCursorIcon::EResize,
        ChromeCursorIcon::SeResize => SystemCursorIcon::SeResize,
        ChromeCursorIcon::SResize => SystemCursorIcon::SResize,
        ChromeCursorIcon::SwResize => SystemCursorIcon::SwResize,
        ChromeCursorIcon::WResize => SystemCursorIcon::WResize,
        ChromeCursorIcon::NwResize => SystemCursorIcon::NwResize,
    }
}

fn upsert_client_cursor(
    world: &mut World,
    id: ObjectId,
    hotspot: (i32, i32),
    presentation: CursorPresentation,
    frame: SurfaceFrame,
) {
    let existing = world
        .resource::<CursorScene>()
        .client
        .as_ref()
        .map(|client| (client.id.clone(), client.image.clone(), client.buffer_kind));
    let (image, buffer_kind, opaque) = match frame {
        SurfaceFrame::Shm(frame) => {
            let opaque = frame.opaque;
            if let Some((existing_id, image, SurfaceBufferKind::Shm)) = existing.as_ref()
                && *existing_id == id
            {
                if let Some(mut current) = world
                    .resource_mut::<Assets<Image>>()
                    .get_mut(image.handle())
                {
                    *current = frame.into_image();
                }
                (image.clone(), SurfaceBufferKind::Shm, opaque)
            } else {
                let image = world
                    .resource_mut::<Assets<Image>>()
                    .add(frame.into_image());
                (
                    ClientSurfaceImage::encoded_premultiplied_unorm(image),
                    SurfaceBufferKind::Shm,
                    opaque,
                )
            }
        }
        SurfaceFrame::Dmabuf(frame) => {
            let opaque = frame.descriptor.is_opaque();
            let release_callback = world
                .resource::<ClientSceneFeed>()
                .dmabuf_release_callback(frame.token);
            let release = dmabuf_release_mode(frame.use_id, release_callback);
            let importer = world.resource::<ImportedDmabufImages>().clone();
            if let Some((existing_id, image, SurfaceBufferKind::Dmabuf)) = existing.as_ref()
                && *existing_id == id
            {
                match importer.replace(
                    image.handle(),
                    frame.buffer_id,
                    frame.cacheable,
                    frame.descriptor,
                    release,
                ) {
                    Ok(()) => {
                        set_client_image_linear(world, image.handle());
                        (image.clone(), SurfaceBufferKind::Dmabuf, opaque)
                    }
                    Err(error) => {
                        error!(surface = ?id, %error, "rejected replacement cursor DMA-BUF");
                        return;
                    }
                }
            } else {
                let imported = {
                    let mut images = world.resource_mut::<Assets<Image>>();
                    importer.import(
                        &mut images,
                        frame.buffer_id,
                        frame.cacheable,
                        frame.descriptor,
                        release,
                    )
                };
                match imported {
                    Ok(image) => {
                        set_client_image_linear(world, &image);
                        (
                            ClientSurfaceImage::encoded_premultiplied_unorm(image),
                            SurfaceBufferKind::Dmabuf,
                            opaque,
                        )
                    }
                    Err(error) => {
                        error!(surface = ?id, %error, "rejected committed cursor DMA-BUF");
                        return;
                    }
                }
            }
        }
    };

    let reused = existing
        .as_ref()
        .is_some_and(|(existing_id, existing_image, existing_kind)| {
            *existing_id == id && *existing_kind == buffer_kind && *existing_image == image
        });
    if !reused {
        clear_client_cursor(world);
    }
    let mut cursor = world.resource_mut::<CursorScene>();
    cursor.client = Some(ProjectedClientCursor {
        id,
        hotspot,
        presentation,
        image,
        buffer_kind,
        opaque,
    });
    cursor.selection = ProjectedCursorSelection::Surface;
}

fn clear_client_cursor(world: &mut World) {
    let client = world.resource_mut::<CursorScene>().client.take();
    let Some(client) = client else {
        return;
    };
    if client.buffer_kind == SurfaceBufferKind::Dmabuf {
        world
            .resource::<ImportedDmabufImages>()
            .unregister(client.image.handle());
    }
    world
        .resource_mut::<Assets<Image>>()
        .remove(client.image.id());
}

fn set_client_image_linear(world: &mut World, image: &Handle<Image>) {
    if let Some(mut image) = world.resource_mut::<Assets<Image>>().get_mut(image) {
        // Phase 3b intentionally gives SHM and DMA-BUF the same linear sampler over
        // encoded-sRGB, encoded-premultiplied UNORM bytes. Fractional samples across
        // high-contrast edges are therefore too dark after the material's EOTF
        // (black/white halfway is 0.214 linear rather than 0.5); texel-centre and
        // nearest samples are exact. Phase 4 must convert each texel to a
        // linear-premultiplied intermediate, filter it, multiply both premultiplied
        // RGB and alpha by rounded coverage, then blend One/OneMinusSrcAlpha.
        // Bevy 0.19 has no AlphaMode2d::Premultiplied, but Material2d::specialize
        // can set that blend state directly on the colour target.
        image.sampler = ImageSampler::linear();
    }
}

fn refresh_cursor_entity(world: &mut World) {
    let (mode, entity, selection, position) = {
        let cursor = world.resource::<CursorScene>();
        (
            cursor.mode,
            cursor.entity,
            cursor.selection,
            cursor.position,
        )
    };
    if mode != SceneCursorMode::SoftwareCursor {
        return;
    }
    let Some(entity) = entity else {
        return;
    };
    match selection {
        ProjectedCursorSelection::Hidden => {
            if let Ok(mut cursor_entity) = world.get_entity_mut(entity) {
                cursor_entity.insert(Visibility::Hidden);
            }
        }
        ProjectedCursorSelection::Default => {
            let image = world
                .resource::<CursorScene>()
                .default_image
                .clone()
                .expect("software cursor owns its default image");
            let mut sprite = SpriteMesh::from_image(image);
            sprite.alpha_mode = SpriteAlphaMode::Blend;
            let scale120 = world.resource::<RendererOutputScale120>().0;
            let image_size = Vec2::new(DEFAULT_CURSOR_WIDTH as f32, DEFAULT_CURSOR_HEIGHT as f32);
            let rendered = cursor_renderer_rect(position, (0, 0), image_size, scale120);
            sprite.custom_size = Some(Vec2::new(rendered.width, rendered.height));
            let transform = cursor_transform(
                position,
                (0, 0),
                image_size,
                SurfaceTransform::Normal,
                world.resource::<LogicalCanvasSize>().0,
                scale120,
            );
            replace_cursor_with_sprite(world, entity, sprite, transform);
        }
        ProjectedCursorSelection::Chrome(cursor_icon) => {
            let (image, hotspot, image_size) = {
                let cursor = world.resource::<CursorScene>();
                match cursor_icon {
                    ChromeCursorIcon::Move => (
                        cursor
                            .default_image
                            .clone()
                            .expect("software cursor owns its default image"),
                        (0, 0),
                        Vec2::new(DEFAULT_CURSOR_WIDTH as f32, DEFAULT_CURSOR_HEIGHT as f32),
                    ),
                    _ => {
                        let images = cursor
                            .resize_images
                            .as_ref()
                            .expect("software cursor owns its resize images");
                        let image = resize_cursor_handle(images, cursor_icon);
                        (
                            image,
                            RESIZE_CURSOR_HOTSPOT,
                            Vec2::splat(RESIZE_CURSOR_SIZE as f32),
                        )
                    }
                }
            };
            let mut sprite = SpriteMesh::from_image(image);
            sprite.alpha_mode = SpriteAlphaMode::Blend;
            let scale120 = world.resource::<RendererOutputScale120>().0;
            let rendered = cursor_renderer_rect(position, hotspot, image_size, scale120);
            sprite.custom_size = Some(Vec2::new(rendered.width, rendered.height));
            let transform = cursor_transform(
                position,
                hotspot,
                image_size,
                SurfaceTransform::Normal,
                world.resource::<LogicalCanvasSize>().0,
                scale120,
            );
            replace_cursor_with_sprite(world, entity, sprite, transform);
        }
        ProjectedCursorSelection::Surface => {
            let (image, opaque, hotspot, presentation) = {
                let cursor = world.resource::<CursorScene>();
                let client = cursor
                    .client
                    .as_ref()
                    .expect("surface cursor selection owns an imported image");
                (
                    client.image.clone(),
                    client.opaque,
                    client.hotspot,
                    client.presentation,
                )
            };
            let scale120 = world.resource::<RendererOutputScale120>().0;
            let material =
                client_cursor_material(image, opaque, presentation, position, hotspot, scale120);
            let transform = cursor_transform(
                position,
                hotspot,
                Vec2::new(presentation.width, presentation.height),
                presentation.transform,
                world.resource::<LogicalCanvasSize>().0,
                scale120,
            );
            replace_cursor_with_client_material(world, entity, material, transform);
        }
    }
}

fn resize_cursor_handle(images: &ResizeCursorImages, cursor: ChromeCursorIcon) -> Handle<Image> {
    match cursor {
        ChromeCursorIcon::EResize | ChromeCursorIcon::WResize => images.horizontal.clone(),
        ChromeCursorIcon::NResize | ChromeCursorIcon::SResize => images.vertical.clone(),
        ChromeCursorIcon::NeResize | ChromeCursorIcon::SwResize => images.ne_sw.clone(),
        ChromeCursorIcon::NwResize | ChromeCursorIcon::SeResize => images.nw_se.clone(),
        ChromeCursorIcon::Move => unreachable!(),
    }
}

fn cursor_transform(
    position: CursorPositionSnapshot,
    hotspot: (i32, i32),
    image_size: Vec2,
    image_transform: SurfaceTransform,
    canvas: Vec2,
    scale120: u32,
) -> Transform {
    let rendered = cursor_renderer_rect(position, hotspot, image_size, scale120);
    Transform::from_xyz(
        rendered.x + rendered.width / 2.0 - canvas.x / 2.0,
        canvas.y / 2.0 - rendered.y - rendered.height / 2.0,
        CURSOR_Z,
    )
    .with_rotation(surface_rotation(image_transform))
}

fn cursor_renderer_rect(
    position: CursorPositionSnapshot,
    hotspot: (i32, i32),
    image_size: Vec2,
    scale120: u32,
) -> RendererRect {
    renderer_rect(
        position.x as f32 - hotspot.0 as f32,
        position.y as f32 - hotspot.1 as f32,
        image_size.x,
        image_size.y,
        scale120,
    )
}

#[derive(Clone, Copy)]
enum ResizeCursorAxis {
    Horizontal,
    Vertical,
    NeSw,
    NwSe,
}

fn resize_cursor_image(axis: ResizeCursorAxis) -> Image {
    let segments: [((i32, i32), (i32, i32)); 5] = match axis {
        ResizeCursorAxis::Horizontal => [
            ((3, 10), (17, 10)),
            ((3, 10), (7, 6)),
            ((3, 10), (7, 14)),
            ((17, 10), (13, 6)),
            ((17, 10), (13, 14)),
        ],
        ResizeCursorAxis::Vertical => [
            ((10, 3), (10, 17)),
            ((10, 3), (6, 7)),
            ((10, 3), (14, 7)),
            ((10, 17), (6, 13)),
            ((10, 17), (14, 13)),
        ],
        ResizeCursorAxis::NeSw => [
            ((3, 17), (17, 3)),
            ((3, 17), (3, 11)),
            ((3, 17), (9, 17)),
            ((17, 3), (11, 3)),
            ((17, 3), (17, 9)),
        ],
        ResizeCursorAxis::NwSe => [
            ((3, 3), (17, 17)),
            ((3, 3), (3, 9)),
            ((3, 3), (9, 3)),
            ((17, 17), (11, 17)),
            ((17, 17), (17, 11)),
        ],
    };
    let master_size = RESIZE_CURSOR_SIZE * RESIZE_CURSOR_MASTER_SCALE;
    let mut rgba = vec![0; master_size as usize * master_size as usize * 4];
    for (start, end) in segments {
        paint_cursor_line(&mut rgba, master_size, start, end, 3, [30, 32, 38, 255]);
    }
    for (start, end) in segments {
        paint_cursor_line(&mut rgba, master_size, start, end, 1, [248, 248, 245, 255]);
    }
    let mut image = Image::new(
        Extent3d {
            width: master_size,
            height: master_size,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::linear();
    image
}

fn paint_cursor_line(
    rgba: &mut [u8],
    size: u32,
    start: (i32, i32),
    end: (i32, i32),
    logical_width: i32,
    colour: [u8; 4],
) {
    let scale = RESIZE_CURSOR_MASTER_SCALE as i32;
    let (mut x, mut y) = (start.0 * scale, start.1 * scale);
    let (end_x, end_y) = (end.0 * scale, end.1 * scale);
    let dx = (end_x - x).abs();
    let step_x = if x < end_x { 1 } else { -1 };
    let dy = -(end_y - y).abs();
    let step_y = if y < end_y { 1 } else { -1 };
    let mut error = dx + dy;
    let radius = logical_width * scale / 2;
    loop {
        for offset_y in -radius..=radius {
            for offset_x in -radius..=radius {
                if offset_x * offset_x + offset_y * offset_y > radius * radius {
                    continue;
                }
                let pixel_x = x + offset_x;
                let pixel_y = y + offset_y;
                if pixel_x < 0 || pixel_y < 0 || pixel_x >= size as i32 || pixel_y >= size as i32 {
                    continue;
                }
                let index = (pixel_y as usize * size as usize + pixel_x as usize) * 4;
                rgba[index..index + 4].copy_from_slice(&colour);
            }
        }
        if x == end_x && y == end_y {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += step_x;
        }
        if doubled <= dx {
            error += dx;
            y += step_y;
        }
    }
}

fn default_cursor_image() -> Image {
    const ROWS: [&str; DEFAULT_CURSOR_HEIGHT as usize] = [
        "D...............",
        "DD..............",
        "DWD.............",
        "DWWD............",
        "DWWWD...........",
        "DWWWWD..........",
        "DWWWWWD.........",
        "DWWWWWWD........",
        "DWWWWWWWD.......",
        "DWWWWWWWWD......",
        "DWWWWDDDDD......",
        "DWWDWD..........",
        "DWD.DWD.........",
        "DD..DWD.........",
        "D...DWWD........",
        "....DWWD........",
        "....DWWD........",
        ".....DD.........",
        "................",
        "................",
    ];
    let mut rgba = Vec::with_capacity(
        (DEFAULT_CURSOR_WIDTH * DEFAULT_CURSOR_MASTER_SCALE) as usize
            * (DEFAULT_CURSOR_HEIGHT * DEFAULT_CURSOR_MASTER_SCALE) as usize
            * 4,
    );
    for row in ROWS {
        debug_assert_eq!(row.len(), DEFAULT_CURSOR_WIDTH as usize);
        for _ in 0..DEFAULT_CURSOR_MASTER_SCALE {
            for pixel in row.bytes() {
                let rgba_pixel = match pixel {
                    b'D' => &[30, 32, 38, 255],
                    b'W' => &[248, 248, 245, 255],
                    _ => &[0, 0, 0, 0],
                };
                for _ in 0..DEFAULT_CURSOR_MASTER_SCALE {
                    rgba.extend_from_slice(rgba_pixel);
                }
            }
        }
    }
    let mut image = Image::new(
        Extent3d {
            width: DEFAULT_CURSOR_WIDTH * DEFAULT_CURSOR_MASTER_SCALE,
            height: DEFAULT_CURSOR_HEIGHT * DEFAULT_CURSOR_MASTER_SCALE,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::linear();
    image
}

/// Match client layout coordinates to the host output before its next frame.
///
/// The live path calls this after selecting its sole output and before the
/// first App update. Relayout also keeps the shared nested path correct when a
/// later output-size event changes the canvas around retained surfaces.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) fn set_compositor_logical_output_geometry(
    world: &mut World,
    logical_width: u32,
    logical_height: u32,
    output_scale: crate::backend::kms::OutputScale120,
) {
    world.resource_mut::<RendererOutputScale120>().0 = output_scale.get();
    resize_compositor_logical_canvas(world, logical_width, logical_height);
}

fn resize_compositor_logical_canvas(world: &mut World, width: u32, height: u32) {
    world.resource_mut::<LogicalCanvasSize>().0 = Vec2::new(width as f32, height as f32);
    let surfaces = world
        .resource::<SurfaceEntities>()
        .surfaces
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for id in surfaces {
        relayout_surface_entity(world, id);
        mark_decoration_dirty(world, id);
    }
    refresh_cursor_entity(world);
}

#[cfg(test)]
fn upsert_surface(
    world: &mut World,
    id: SurfaceId,
    layout: SurfaceLayout,
    frame: SurfaceFrame,
) -> bool {
    upsert_surface_snapshot(
        world,
        id,
        SurfaceSceneSnapshot {
            layout,
            kind: if layout.toplevel.is_some() {
                SceneSurfaceKind::Toplevel
            } else {
                SceneSurfaceKind::Subsurface
            },
            title: None,
        },
        frame,
    )
}

fn upsert_surface_snapshot(
    world: &mut World,
    id: SurfaceId,
    scene: SurfaceSceneSnapshot,
    frame: SurfaceFrame,
) -> bool {
    let layout = scene.layout;
    let kind = scene.kind;
    let existing = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| (surface.entity, surface.image.clone(), surface.buffer_kind));

    let z_changed = match frame {
        SurfaceFrame::Shm(frame) => {
            let opaque = frame.opaque;
            if let Some((entity, image_handle, SurfaceBufferKind::Shm)) = existing {
                if let Some(mut current) = world
                    .resource_mut::<Assets<Image>>()
                    .get_mut(image_handle.handle())
                {
                    *current = frame.into_image();
                }
                update_surface_entity(world, id, entity, image_handle, opaque, layout, kind)
            } else {
                let image = world
                    .resource_mut::<Assets<Image>>()
                    .add(frame.into_image());
                replace_surface_image(
                    world,
                    id,
                    layout,
                    kind,
                    ClientSurfaceImage::encoded_premultiplied_unorm(image),
                    SurfaceBufferKind::Shm,
                    opaque,
                )
            }
        }
        SurfaceFrame::Dmabuf(frame) => {
            let opaque = frame.descriptor.is_opaque();
            let release_callback = world
                .resource::<ClientSceneFeed>()
                .dmabuf_release_callback(frame.token);
            let release = dmabuf_release_mode(frame.use_id, release_callback);
            let importer = world.resource::<ImportedDmabufImages>().clone();

            if let Some((entity, image, SurfaceBufferKind::Dmabuf)) = existing {
                match importer.replace(
                    image.handle(),
                    frame.buffer_id,
                    frame.cacheable,
                    frame.descriptor,
                    release,
                ) {
                    Ok(()) => {
                        set_client_image_linear(world, image.handle());
                        update_surface_entity(world, id, entity, image, opaque, layout, kind)
                    }
                    Err(error) => {
                        error!(surface_id = id.0, %error, "rejected replacement DMA-BUF");
                        false
                    }
                }
            } else {
                let imported = {
                    let mut images = world.resource_mut::<Assets<Image>>();
                    importer.import(
                        &mut images,
                        frame.buffer_id,
                        frame.cacheable,
                        frame.descriptor,
                        release,
                    )
                };
                match imported {
                    Ok(image) => {
                        set_client_image_linear(world, &image);
                        replace_surface_image(
                            world,
                            id,
                            layout,
                            kind,
                            ClientSurfaceImage::encoded_premultiplied_unorm(image),
                            SurfaceBufferKind::Dmabuf,
                            opaque,
                        )
                    }
                    Err(error) => {
                        error!(surface_id = id.0, %error, "rejected committed DMA-BUF");
                        false
                    }
                }
            }
        }
    };
    if let Some(surface) = world
        .resource_mut::<SurfaceEntities>()
        .surfaces
        .get_mut(&id)
    {
        surface.title = scene.title;
        surface.kind = kind;
    }
    z_changed
}

fn dmabuf_release_mode(use_id: Option<DmabufUseId>, callback: ReleaseCallback) -> DmabufRelease {
    if use_id.is_some() {
        DmabufRelease::Explicit(callback)
    } else {
        DmabufRelease::Implicit(callback)
    }
}

fn replace_surface_image(
    world: &mut World,
    id: SurfaceId,
    layout: SurfaceLayout,
    kind: SceneSurfaceKind,
    image: ClientSurfaceImage,
    buffer_kind: SurfaceBufferKind,
    opaque: bool,
) -> bool {
    let existing = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| (surface.entity, surface.image.clone(), surface.buffer_kind));
    if let Some((entity, previous_image, previous_buffer_kind)) = existing {
        let z_changed =
            update_surface_entity(world, id, entity, image.clone(), opaque, layout, kind);
        if let Some(surface) = world
            .resource_mut::<SurfaceEntities>()
            .surfaces
            .get_mut(&id)
        {
            surface.image = image;
            surface.buffer_kind = buffer_kind;
        }
        if previous_buffer_kind == SurfaceBufferKind::Dmabuf {
            world
                .resource::<ImportedDmabufImages>()
                .unregister(previous_image.handle());
        }
        world
            .resource_mut::<Assets<Image>>()
            .remove(previous_image.id());
        return z_changed;
    }

    let scale120 = world.resource::<RendererOutputScale120>().0;
    let initial_material = client_surface_material(image.clone(), opaque, layout, scale120);
    let material = world
        .resource_mut::<Assets<ClientSurfaceMaterial>>()
        .add(initial_material);
    let mesh = world.resource::<ClientSurfaceRenderAssets>().mesh();
    let entity = world
        .spawn((
            Name::new(format!("Wayland surface {}", id.0)),
            WaylandSurfaceEntity { id },
            mesh,
            MeshMaterial2d(material.clone()),
            NoFrustumCulling,
            Transform::default(),
            if layout.visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        ))
        .id();
    if let Some(parent) = layout.parent.and_then(|parent_id| {
        world
            .resource::<SurfaceEntities>()
            .surfaces
            .get(&parent_id)
            .map(|surface| surface.entity)
    }) {
        world.entity_mut(entity).insert(ChildOf(parent));
    }
    world.resource_mut::<SurfaceEntities>().surfaces.insert(
        id,
        SurfaceEntity {
            entity,
            image,
            buffer_kind,
            opaque,
            layout,
            kind,
            title: None,
            material,
            renderer_z: CLIENT_CONTENT_Z_MIN,
            decoration: None,
        },
    );
    world
        .resource_mut::<SurfaceEntities>()
        .update_parent(id, None, layout.parent);
    sync_surface_parent(world, id);
    sync_surface_children(world, id);
    true
}

#[derive(Component)]
struct WaylandSurfaceEntity {
    #[allow(dead_code)]
    id: SurfaceId,
}

fn update_surface_entity(
    world: &mut World,
    id: SurfaceId,
    entity: Entity,
    image: ClientSurfaceImage,
    opaque: bool,
    layout: SurfaceLayout,
    kind: SceneSurfaceKind,
) -> bool {
    let scale120 = world.resource::<RendererOutputScale120>().0;
    refresh_client_surface_material(world, id, image, opaque, layout, scale120, true);
    world.entity_mut(entity).insert(if layout.visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    });
    let z_changed = {
        let mut entities = world.resource_mut::<SurfaceEntities>();
        let (old_parent, old_z) = entities
            .surfaces
            .get(&id)
            .map(|surface| (surface.layout.parent, surface.layout.z))
            .unwrap_or((None, layout.z));
        if let Some(surface) = entities.surfaces.get_mut(&id) {
            surface.opaque = opaque;
            surface.layout = layout;
            surface.kind = kind;
        }
        entities.update_parent(id, old_parent, layout.parent);
        old_z != layout.z
    };
    sync_surface_parent(world, id);
    relayout_surface_descendants(world, id);
    z_changed
}

fn relayout_surface(world: &mut World, id: SurfaceId, scene: SurfaceSceneSnapshot) -> bool {
    let layout = scene.layout;
    let Some((entity, image, opaque)) = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| (surface.entity, surface.image.clone(), surface.opaque))
    else {
        return false;
    };
    let scale120 = world.resource::<RendererOutputScale120>().0;
    let visibility = if layout.visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    if world.get::<Visibility>(entity) != Some(&visibility) {
        world.entity_mut(entity).insert(visibility);
    }
    refresh_client_surface_material(world, id, image, opaque, layout, scale120, false);
    let z_changed = {
        let mut entities = world.resource_mut::<SurfaceEntities>();
        let (old_parent, old_z) = entities
            .surfaces
            .get(&id)
            .map(|surface| (surface.layout.parent, surface.layout.z))
            .unwrap_or((None, layout.z));
        if let Some(surface) = entities.surfaces.get_mut(&id) {
            surface.layout = layout;
            surface.kind = scene.kind;
            surface.title = scene.title;
        }
        entities.update_parent(id, old_parent, layout.parent);
        old_z != layout.z
    };
    sync_surface_parent(world, id);
    relayout_surface_descendants(world, id);
    z_changed
}

fn client_surface_material(
    image: ClientSurfaceImage,
    opaque: bool,
    layout: SurfaceLayout,
    scale120: u32,
) -> ClientSurfaceMaterial {
    let mut material = ClientSurfaceMaterial::new(&image, opaque);
    apply_surface_layout_to_client_material(&mut material, layout, scale120);
    material
}

fn refresh_client_surface_material(
    world: &mut World,
    id: SurfaceId,
    image: ClientSurfaceImage,
    opaque: bool,
    layout: SurfaceLayout,
    scale120: u32,
    force_rebind: bool,
) {
    let handle = world.resource::<SurfaceEntities>().surfaces[&id]
        .material
        .clone();
    let desired = client_surface_material(image, opaque, layout, scale120);
    if !force_rebind
        && world
            .resource::<Assets<ClientSurfaceMaterial>>()
            .get(&handle)
            == Some(&desired)
    {
        return;
    }
    let mut materials = world.resource_mut::<Assets<ClientSurfaceMaterial>>();
    let mut current = materials
        .get_mut(&handle)
        .expect("tracked client surface owns its material asset");
    *current = desired;
}

pub(crate) fn set_surface_client_clip(
    world: &mut World,
    id: SurfaceId,
    clip: Option<ClientSurfaceClip>,
) {
    let Some(handle) = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| surface.material.clone())
    else {
        return;
    };
    let (clip_from_uv, clip_size, corner_radius) = clip
        .map_or((Mat3::IDENTITY, Vec2::ZERO, 0.0), |clip| {
            (clip.clip_from_uv, clip.clip_size, clip.corner_radius)
        });
    let mut materials = world.resource_mut::<Assets<ClientSurfaceMaterial>>();
    let Some(current) = materials.get(&handle) else {
        return;
    };
    let desired_alpha = if current.opaque && corner_radius == 0.0 {
        bevy::sprite_render::AlphaMode2d::Opaque
    } else {
        bevy::sprite_render::AlphaMode2d::Blend
    };
    if current.clip_from_uv == clip_from_uv
        && current.clip_size == clip_size
        && current.corner_radius == corner_radius
        && current.alpha_mode == desired_alpha
    {
        return;
    }
    let mut current = materials
        .get_mut(&handle)
        .expect("tracked client surface owns its material asset");
    current.set_rounded_clip(clip_from_uv, clip_size, corner_radius);
}

pub(crate) fn refresh_sprite_material(world: &mut World, entity: Entity, sprite: &SpriteMesh) {
    let material = world
        .get::<MeshMaterial2d<SpriteMaterial>>(entity)
        .map(|material| material.0.clone());
    if let Some(material) = material
        && let Some(mut current) = world
            .resource_mut::<Assets<SpriteMaterial>>()
            .get_mut(&material)
    {
        *current = SpriteMaterial::from_sprite_mesh(sprite.clone());
    }
}

fn replace_cursor_with_sprite(
    world: &mut World,
    entity: Entity,
    sprite: SpriteMesh,
    transform: Transform,
) {
    let client_material = world
        .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)
        .map(|material| material.0.clone());
    if let Some(material) = client_material {
        world
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .remove(material.id());
    }
    world
        .entity_mut(entity)
        .remove::<(MeshMaterial2d<ClientSurfaceMaterial>, NoFrustumCulling)>();
    refresh_sprite_material(world, entity, &sprite);
    world
        .entity_mut(entity)
        .insert((sprite, transform, Visibility::Inherited));
}

fn replace_cursor_with_client_material(
    world: &mut World,
    entity: Entity,
    material: ClientSurfaceMaterial,
    transform: Transform,
) {
    let handle = world
        .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)
        .map(|material| material.0.clone());
    let handle = if let Some(handle) = handle {
        *world
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .get_mut(&handle)
            .expect("client cursor owns its material asset") = material;
        handle
    } else {
        world
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .add(material)
    };
    let mesh = world.resource::<ClientSurfaceRenderAssets>().mesh();
    world
        .entity_mut(entity)
        .remove::<(SpriteMesh, MeshMaterial2d<SpriteMaterial>)>()
        .insert((
            mesh,
            MeshMaterial2d(handle),
            NoFrustumCulling,
            transform,
            Visibility::Inherited,
        ));
}

fn apply_surface_layout_to_client_material(
    material: &mut ClientSurfaceMaterial,
    layout: SurfaceLayout,
    scale120: u32,
) {
    let rendered = renderer_rect(layout.x, layout.y, layout.width, layout.height, scale120);
    material.custom_size = if surface_transform_swaps_axes(layout.transform) {
        Vec2::new(rendered.height, rendered.width)
    } else {
        Vec2::new(rendered.width, rendered.height)
    };
    material.flip_x = matches!(
        layout.transform,
        SurfaceTransform::Flipped
            | SurfaceTransform::Flipped90
            | SurfaceTransform::Flipped180
            | SurfaceTransform::Flipped270
    );
    material.source_rect = layout.source.map(|source| {
        Rect::new(
            source.x,
            source.y,
            source.x + source.width,
            source.y + source.height,
        )
    });
}

fn client_cursor_material(
    image: ClientSurfaceImage,
    opaque: bool,
    presentation: CursorPresentation,
    position: CursorPositionSnapshot,
    hotspot: (i32, i32),
    scale120: u32,
) -> ClientSurfaceMaterial {
    let mut material = ClientSurfaceMaterial::new(&image, opaque);
    let rendered = cursor_renderer_rect(
        position,
        hotspot,
        Vec2::new(presentation.width, presentation.height),
        scale120,
    );
    material.custom_size = if surface_transform_swaps_axes(presentation.transform) {
        Vec2::new(rendered.height, rendered.width)
    } else {
        Vec2::new(rendered.width, rendered.height)
    };
    material.flip_x = matches!(
        presentation.transform,
        SurfaceTransform::Flipped
            | SurfaceTransform::Flipped90
            | SurfaceTransform::Flipped180
            | SurfaceTransform::Flipped270
    );
    material.source_rect = presentation.source.map(|source| {
        Rect::new(
            source.x,
            source.y,
            source.x + source.width,
            source.y + source.height,
        )
    });
    material
}

fn remove_surface(world: &mut World, id: SurfaceId) -> bool {
    let children = world
        .resource::<SurfaceEntities>()
        .children
        .get(&id)
        .map(|children| children.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for child in children {
        if let Some(entity) = world
            .resource::<SurfaceEntities>()
            .surfaces
            .get(&child)
            .map(|surface| surface.entity)
            && let Ok(mut entity) = world.get_entity_mut(entity)
        {
            entity.remove::<ChildOf>();
            entity.insert(Visibility::Hidden);
        }
        if let Some(surface) = world
            .resource_mut::<SurfaceEntities>()
            .surfaces
            .get_mut(&child)
        {
            surface.layout.parent = None;
        }
        relayout_surface_entity(world, child);
        mark_decoration_dirty(world, child);
    }
    remove_static_decoration(world, id);

    let surface = {
        let mut entities = world.resource_mut::<SurfaceEntities>();
        let old_parent = entities
            .surfaces
            .get(&id)
            .and_then(|surface| surface.layout.parent);
        entities.update_parent(id, old_parent, None);
        entities.children.remove(&id);
        entities.surfaces.remove(&id)
    };
    let Some(surface) = surface else {
        return false;
    };
    if let Ok(entity) = world.get_entity_mut(surface.entity) {
        entity.despawn();
    }
    if surface.buffer_kind == SurfaceBufferKind::Dmabuf {
        world
            .resource::<ImportedDmabufImages>()
            .unregister(surface.image.handle());
    }
    world
        .resource_mut::<Assets<Image>>()
        .remove(surface.image.id());
    world
        .resource_mut::<Assets<ClientSurfaceMaterial>>()
        .remove(surface.material.id());
    true
}

fn relayout_surface_descendants(world: &mut World, parent: SurfaceId) {
    let descendants = descendant_surface_ids(&world.resource::<SurfaceEntities>().children, parent);
    for id in descendants {
        let Some((entity, layout)) = world
            .resource::<SurfaceEntities>()
            .surfaces
            .get(&id)
            .map(|surface| (surface.entity, surface.layout))
        else {
            continue;
        };
        let transform = surface_entity_transform(world, id, layout);
        if let Ok(mut child) = world.get_entity_mut(entity)
            && child.get::<Transform>() != Some(&transform)
        {
            child.insert(transform);
        }
    }
}

pub(crate) fn descendant_surface_ids(
    children: &HashMap<SurfaceId, HashSet<SurfaceId>>,
    parent: SurfaceId,
) -> Vec<SurfaceId> {
    let mut visited = HashSet::new();
    let mut descendants = Vec::new();
    let mut stack = children
        .get(&parent)
        .map(|children| children.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        descendants.push(id);
        if let Some(grandchildren) = children.get(&id) {
            stack.extend(grandchildren.iter().copied());
        }
    }
    descendants
}

pub(crate) fn sync_surface_parent(world: &mut World, id: SurfaceId) {
    let relationship = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| {
            let parent_entity = surface
                .layout
                .parent
                .and_then(|parent| {
                    world
                        .resource::<SurfaceEntities>()
                        .surfaces
                        .get(&parent)
                        .map(|parent| parent.entity)
                })
                .or_else(|| {
                    surface
                        .decoration
                        .as_ref()
                        .map(|decoration| decoration.root)
                });
            (surface.entity, parent_entity)
        });
    if let Some((entity, parent)) = relationship
        && let Ok(mut entity) = world.get_entity_mut(entity)
    {
        if let Some(parent) = parent {
            if entity.get::<ChildOf>().map(ChildOf::parent) != Some(parent) {
                entity.insert(ChildOf(parent));
            }
        } else if entity.contains::<ChildOf>() {
            entity.remove::<ChildOf>();
        }
    }
    relayout_surface_entity(world, id);
}

fn sync_surface_children(world: &mut World, parent: SurfaceId) {
    let children = world
        .resource::<SurfaceEntities>()
        .children
        .get(&parent)
        .map(|children| children.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for child in children {
        sync_surface_parent(world, child);
        relayout_surface_descendants(world, child);
    }
}

pub(crate) fn relayout_surface_entity(world: &mut World, id: SurfaceId) {
    let Some((entity, layout)) = world
        .resource::<SurfaceEntities>()
        .surfaces
        .get(&id)
        .map(|surface| (surface.entity, surface.layout))
    else {
        return;
    };
    let transform = surface_entity_transform(world, id, layout);
    if let Ok(mut entity) = world.get_entity_mut(entity)
        && entity.get::<Transform>() != Some(&transform)
    {
        entity.insert(transform);
    }
}

fn surface_transform(
    layout: SurfaceLayout,
    canvas: Vec2,
    renderer_z: f32,
    scale120: u32,
) -> Transform {
    let rendered = renderer_rect(layout.x, layout.y, layout.width, layout.height, scale120);
    Transform::from_xyz(
        rendered.x + rendered.width / 2.0 - canvas.x / 2.0,
        canvas.y / 2.0 - rendered.y - rendered.height / 2.0,
        renderer_z,
    )
    .with_rotation(surface_rotation(layout.transform))
}

pub(crate) fn surface_rotation(transform: SurfaceTransform) -> Quat {
    Quat::from_rotation_z(match transform {
        SurfaceTransform::Normal | SurfaceTransform::Flipped => 0.0,
        SurfaceTransform::Rotate90 | SurfaceTransform::Flipped90 => std::f32::consts::FRAC_PI_2,
        SurfaceTransform::Rotate180 | SurfaceTransform::Flipped180 => std::f32::consts::PI,
        SurfaceTransform::Rotate270 | SurfaceTransform::Flipped270 => {
            3.0 * std::f32::consts::FRAC_PI_2
        }
    })
}

fn surface_transform_swaps_axes(transform: SurfaceTransform) -> bool {
    matches!(
        transform,
        SurfaceTransform::Rotate90
            | SurfaceTransform::Rotate270
            | SurfaceTransform::Flipped90
            | SurfaceTransform::Flipped270
    )
}

fn surface_entity_transform(world: &World, id: SurfaceId, layout: SurfaceLayout) -> Transform {
    let canvas = world.resource::<LogicalCanvasSize>().0;
    let scale120 = world.resource::<RendererOutputScale120>().0;
    let surface = world.resource::<SurfaceEntities>().surfaces.get(&id);
    if layout.parent.is_none()
        && let Some(transform) = surface
            .and_then(|surface| surface.decoration.as_ref())
            .map(|decoration| decoration.client_transform)
    {
        return transform;
    }
    let renderer_z = surface.map_or(CLIENT_CONTENT_Z_MIN, |surface| surface.renderer_z);
    let transform = surface_transform(layout, canvas, renderer_z, scale120);
    if let Some((parent_layout, parent_z)) = layout.parent.and_then(|parent_id| {
        world
            .resource::<SurfaceEntities>()
            .surfaces
            .get(&parent_id)
            .map(|surface| (surface.layout, surface.renderer_z))
    }) {
        let parent = surface_transform(parent_layout, canvas, parent_z, scale120).to_matrix();
        return Transform::from_matrix(parent.inverse() * transform.to_matrix());
    }
    transform
}

fn recompute_surface_z_ranks(world: &mut World) {
    let mut ordered = world
        .resource::<SurfaceEntities>()
        .surfaces
        .iter()
        .map(|(id, surface)| (*id, surface.layout.z))
        .collect::<Vec<_>>();
    debug_assert!(ordered.len() <= MAX_GLOBAL_SURFACES);
    ordered.sort_unstable_by(|(left_id, left_z), (right_id, right_z)| {
        left_z
            .total_cmp(right_z)
            .then_with(|| left_id.0.cmp(&right_id.0))
    });
    let changed = {
        let mut entities = world.resource_mut::<SurfaceEntities>();
        let mut changed = HashSet::new();
        for (rank, (id, _)) in ordered.iter().enumerate() {
            if let Some(surface) = entities.surfaces.get_mut(id) {
                let renderer_z = client_content_z(rank, ordered.len());
                if surface.renderer_z != renderer_z {
                    surface.renderer_z = renderer_z;
                    changed.insert(*id);
                }
            }
        }
        changed
    };

    // Live z-order forensics. A two-window draw-order fault reported on
    // 2026-08-11 (window 1's content above window 2's, window 2's content
    // above window 1's titlebar panel) survived ten separate static
    // hypotheses and does not reproduce against synthetic clients -- see
    // `later_mapped_ssd_subsurface_stack_is_globally_above_the_earlier_window`,
    // which passes. This is the single function that decides which window
    // draws over which, so dump the resolved ordering it just committed to.
    // Debug-level: silent under the default INFO filter.
    let ordering = {
        let entities = world.resource::<SurfaceEntities>();
        ordered
            .iter()
            .filter_map(|(id, _)| entities.surfaces.get(id).map(|surface| (id, surface)))
            .map(|(id, surface)| {
                format!(
                    "{}(z={:.3} rz={:.1} parent={:?} {:?})",
                    id.0,
                    surface.layout.z,
                    surface.renderer_z,
                    surface.layout.parent.map(|parent| parent.0),
                    surface.kind,
                )
            })
            .collect::<Vec<_>>()
            .join(" < ")
    };
    tracing::debug!(order = %ordering, "recomputed global surface z ranks");

    // A child's local Z compensates for its parent's absolute renderer rank.
    // When an ancestor moves, descendants therefore need a local transform
    // refresh even if their own absolute rank stayed put.
    let transform_updates = {
        let entities = world.resource::<SurfaceEntities>();
        let mut transform_updates = changed;
        let mut stack = transform_updates.iter().copied().collect::<Vec<_>>();
        while let Some(parent) = stack.pop() {
            if let Some(children) = entities.children.get(&parent) {
                for child in children {
                    if transform_updates.insert(*child) {
                        stack.push(*child);
                    }
                }
            }
        }
        transform_updates
    };
    #[cfg(test)]
    if let Some(mut probe) = world.get_resource_mut::<ZRankRecomputeProbe>() {
        probe.passes += 1;
        probe.transform_writes += transform_updates.len();
    }
    let mut requeued = HashSet::new();
    for id in transform_updates {
        relayout_surface_entity(world, id);
        mark_decoration_dirty(world, id);
        mark_surface_meshes_for_requeue(world, id, &mut requeued);
    }
}

pub(crate) fn client_content_z(rank: usize, surface_count: usize) -> f32 {
    debug_assert!(rank < surface_count);
    let step = (CLIENT_CONTENT_Z_MAX - CLIENT_CONTENT_Z_MIN) / (surface_count + 1) as f32;
    CLIENT_CONTENT_Z_MIN + step * (rank + 1) as f32
}

fn mark_decoration_dirty(world: &mut World, id: SurfaceId) {
    if let Some(mut dirty) = world.get_resource_mut::<DecorationDirtySurfaceIds>() {
        dirty.0.insert(id);
    }
}

/// Force Bevy to requeue this surface's meshes so their draw order follows the
/// Z we just assigned.
///
/// Bevy 0.19 captures a `Transparent2d` item's `sort_key` at queue time and
/// never refreshes it: `Transparent2d::recalculate_sort_keys` is deliberately a
/// no-op, and the 2D material queue loop revisits only newly-visible entities
/// and those needing specialization -- a set triggered by `Changed`/
/// `AssetChanged` on `Mesh2d` or `MeshMaterial2d`, in which `GlobalTransform`
/// does not appear. The item itself is retained across frames via
/// `add_retained`. Meanwhile the current transform *is* re-extracted to the GPU
/// every frame, so a transform-only Z change renders each mesh in the right
/// place in the wrong order: a window composites at its previous stacking
/// position while every transform in this world reads correct. That is why
/// three rounds of transform instrumentation found nothing wrong -- the scene
/// graph was never the thing that was stale.
///
/// A rank recompute changes `renderer_z` and nothing else, which is exactly
/// that case, so touch `Mesh2d` on every mesh this surface owns. Ordinary
/// window moves do not come through here and keep their existing phase items;
/// only a Z change pays this cost.
///
/// Walk the hierarchy rather than listing the known parts: a decoration part
/// added later must not silently reintroduce the fault. The title is `Text2d`,
/// re-extracted as a sprite every frame, so it is never stale -- which is
/// precisely why a stale frame quad drawing over a correctly-ordered title is
/// what blanks a titlebar while its caption buttons survive.
///
/// `visited` is owned by the caller and shared across the whole re-rank, because
/// the set being walked is not an antichain: `transform_updates` holds every
/// descendant of a re-ranked surface as well, and a child surface's entity is an
/// ECS child of its parent's, so a per-call set would re-walk the same subtree
/// once per ancestor. A client is allowed to nest subsurfaces
/// `MAX_SUBSURFACE_DEPTH` deep, so that redundancy is a depth multiplier on the
/// whole traversal, not a constant. Sharing it makes one recompute visit each
/// entity exactly once, and skipping an already-visited entity is safe in either
/// arrival order: whichever walk reached it first also marked its meshes.
fn mark_surface_meshes_for_requeue(
    world: &mut World,
    id: SurfaceId,
    visited: &mut HashSet<Entity>,
) {
    let mut stack = {
        let Some(surface) = world.resource::<SurfaceEntities>().surfaces.get(&id) else {
            return;
        };
        let mut roots = vec![surface.entity];
        if let Some(decoration) = surface.decoration.as_ref() {
            roots.push(decoration.root);
        }
        roots
    };
    let mut meshes = Vec::new();
    while let Some(entity) = stack.pop() {
        if !visited.insert(entity) {
            continue;
        }
        if world.get::<Mesh2d>(entity).is_some() {
            meshes.push(entity);
        }
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter());
        }
    }
    for entity in meshes {
        if let Some(mut mesh) = world.get_mut::<Mesh2d>(entity) {
            mesh.set_changed();
        }
    }
}

#[cfg(test)]
#[derive(Resource, Default)]
struct ZRankRecomputeProbe {
    passes: usize,
    transform_writes: usize,
}

impl ShmFrame {
    fn into_image(self) -> Image {
        // ShmBacking deliberately retains the other Arc owner so incremental
        // commits can convert only damaged rows. Bevy Image requires an owned
        // Vec, therefore this is always one bounded full-frame copy on the
        // render thread, never a uniqueness fast path. A lagging renderer can
        // still trigger Arc::make_mut's bounded COW copy on a later protocol
        // commit; direct write_texture subregions are the route to removing
        // both copies once SHM upload ownership moves into the render bridge.
        let rgba = (*self.rgba).clone();
        let mut image = Image::new(
            Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            rgba,
            TextureFormat::Rgba8Unorm,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        );
        // This is implicit-sRGB, encoded-space-premultiplied client data, not a
        // general-purpose linear RGBA image. The Handle is wrapped in
        // ClientSurfaceImage immediately after insertion into Assets<Image>.
        // Phase 3b filters those bytes in encoded space. Phase 4 must instead
        // filter a linear-premultiplied intermediate, multiply both premultiplied
        // RGB and alpha by rounded coverage, then blend One/OneMinusSrcAlpha.
        // Bevy 0.19 has no AlphaMode2d::Premultiplied, but Material2d::specialize
        // can set that blend state directly on the colour target.
        image.sampler = ImageSampler::linear();
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        fs::File,
        io::{self, Write},
        sync::{Arc, Mutex, mpsc::SyncSender},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use bevy::log::tracing_subscriber::fmt::MakeWriter;

    use cosmix_wgpu_dmabuf::{DmabufDescriptor, DmabufPlane};
    use smithay::backend::allocator::{Fourcc, Modifier};

    use crate::{
        backend::render::tests::live_client_scene_app_for_test,
        protocol::{
            ChromePointerSceneState, DmabufFrame, HostButtonState, HostInput,
            PendingSsdSubsurfaceSceneClient, RealCursorSceneClient, RealShmSceneClient,
            SceneDecorationMode, SceneWindowGeometry, TextureSourceRect, ToplevelSceneState,
            WaylandRuntime, real_shm_scene_runtime, real_shm_scene_runtime_with_decoration,
        },
    };
    use cosmix_deco::ChromeStyle;

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("log capture mutex is available")
                .write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for LogCapture {
        type Writer = LogCaptureWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            LogCaptureWriter(Arc::clone(&self.0))
        }
    }

    impl LogCapture {
        fn output(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("log capture mutex is available")
                    .clone(),
            )
            .expect("tracing output is UTF-8")
        }
    }

    fn frame(value: u8) -> SurfaceFrame {
        SurfaceFrame::Shm(ShmFrame {
            width: 2,
            height: 2,
            opaque: true,
            rgba: Arc::new(vec![value; 16]),
        })
    }

    fn layout(id: u64) -> SurfaceLayout {
        SurfaceLayout {
            x: id as f32 * 10.0,
            y: id as f32 * 5.0,
            width: 8.0,
            height: 8.0,
            z: id as f32,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
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

    fn insert_client_surface_render_resources(world: &mut World) {
        world.insert_resource(Assets::<ClientSurfaceMaterial>::default());
        world.insert_resource(ClientSurfaceRenderAssets {
            quad: Handle::default(),
        });
    }

    fn scene_app() -> (App, SyncSender<Vec<ProtocolEvent>>) {
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .add_plugins(CompositorScenePlugin::new(
                960,
                640,
                SceneCursorMode::HostCursor,
            ));
        (app, sender)
    }

    fn software_cursor_scene_app() -> (App, SyncSender<Vec<ProtocolEvent>>) {
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .add_plugins(CompositorScenePlugin::new(
                320,
                240,
                SceneCursorMode::SoftwareCursor,
            ));
        (app, sender)
    }

    struct RealProtocolScene {
        _runtime: WaylandRuntime,
        client: RealShmSceneClient,
        app: App,
    }

    fn real_protocol_scene(label: &str) -> RealProtocolScene {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let socket_name = format!("cosmix-live-scene-{}-{label}-{unique}", std::process::id());
        let mut runtime = real_shm_scene_runtime(&socket_name, (320, 240));
        let feed = runtime
            .take_client_scene_feed()
            .expect("the live scene exclusively takes the protocol feed");
        let app = live_client_scene_app_for_test(feed, (320, 240));
        let client = RealShmSceneClient::connect(&socket_name, label);
        RealProtocolScene {
            _runtime: runtime,
            client,
            app,
        }
    }

    fn wait_for_real_surface(app: &mut App) -> SurfaceId {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.update();
            if let Some(id) = app
                .world()
                .resource::<SurfaceEntities>()
                .surfaces
                .keys()
                .next()
                .copied()
            {
                return id;
            }
            assert!(
                Instant::now() < deadline,
                "real SHM client did not reach the live-shaped scene App"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn dmabuf_frame(token: u64, use_id: u64) -> SurfaceFrame {
        let fd: std::os::fd::OwnedFd = File::open("/dev/null")
            .expect("/dev/null is available")
            .into();
        SurfaceFrame::Dmabuf(DmabufFrame {
            buffer_id: cosmix_wgpu_dmabuf::DmabufBufferId(token),
            cacheable: true,
            token,
            descriptor: DmabufDescriptor {
                width: 8,
                height: 8,
                fourcc: Fourcc::Argb8888 as u32,
                modifier: u64::from(Modifier::Linear),
                planes: vec![DmabufPlane {
                    fd,
                    offset: 0,
                    stride: 32,
                }],
            },
            use_id: Some(DmabufUseId::for_test(use_id)),
        })
    }

    fn publish(app: &mut App, sender: &SyncSender<Vec<ProtocolEvent>>, events: Vec<ProtocolEvent>) {
        sender
            .send(events)
            .expect("shared scene protocol channel remains connected");
        app.update();
    }

    fn world_position(world: &World, entity: Entity) -> Vec3 {
        let mut current = entity;
        let mut transform = world
            .get::<Transform>(current)
            .expect("surface has a transform")
            .to_matrix();
        while let Some(parent) = world.get::<ChildOf>(current) {
            current = parent.parent();
            transform = world
                .get::<Transform>(current)
                .expect("parent has a transform")
                .to_matrix()
                * transform;
        }
        transform.transform_point3(Vec3::ZERO)
    }

    fn pump_real_protocol_into_scene(
        runtime: &WaylandRuntime,
        app: &mut App,
        sender: &SyncSender<Vec<ProtocolEvent>>,
        surface_count: usize,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let events = runtime
                .drain_events()
                .expect("real protocol publication remains connected");
            if !events.is_empty() {
                sender
                    .send(events)
                    .expect("real protocol batch reaches the test scene feed");
            }
            app.update();
            if app.world().resource::<SurfaceEntities>().surfaces.len() == surface_count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "real protocol scene did not reach {surface_count} mapped surfaces"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn implicit_frames_keep_physical_release_and_explicit_uses_select_logical_eviction() {
        assert!(matches!(
            dmabuf_release_mode(None, Box::new(|| {})),
            DmabufRelease::Implicit(_)
        ));
        assert!(matches!(
            dmabuf_release_mode(Some(DmabufUseId::for_test(7)), Box::new(|| {})),
            DmabufRelease::Explicit(_)
        ));
    }

    #[test]
    fn host_cursor_mode_never_spawns_a_software_cursor_entity() {
        let (mut app, _sender) = scene_app();
        app.update();
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<SoftwareCursorEntity>>()
                .iter(app.world())
                .count(),
            0
        );
        assert_eq!(
            app.world().resource::<CursorScene>().mode,
            SceneCursorMode::HostCursor
        );
    }

    #[test]
    fn nested_chrome_cursors_map_every_edge_and_restore_host_default() {
        let (mut app, sender) = scene_app();
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.update();

        for (chrome, expected) in [
            (ChromeCursorIcon::Move, SystemCursorIcon::Move),
            (ChromeCursorIcon::NResize, SystemCursorIcon::NResize),
            (ChromeCursorIcon::NeResize, SystemCursorIcon::NeResize),
            (ChromeCursorIcon::EResize, SystemCursorIcon::EResize),
            (ChromeCursorIcon::SeResize, SystemCursorIcon::SeResize),
            (ChromeCursorIcon::SResize, SystemCursorIcon::SResize),
            (ChromeCursorIcon::SwResize, SystemCursorIcon::SwResize),
            (ChromeCursorIcon::WResize, SystemCursorIcon::WResize),
            (ChromeCursorIcon::NwResize, SystemCursorIcon::NwResize),
        ] {
            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::CursorUpdated {
                    image: CursorImage::Chrome(chrome),
                }],
            );
            assert_eq!(app.world().resource::<HostCursor>().icon, expected);
        }

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Default,
            }],
        );
        assert_eq!(
            app.world().resource::<HostCursor>().icon,
            SystemCursorIcon::Default
        );
        let window = app
            .world_mut()
            .query_filtered::<Entity, With<PrimaryWindow>>()
            .single(app.world())
            .expect("test owns one primary window");
        assert_eq!(
            app.world()
                .get::<CursorIcon>(window)
                .and_then(CursorIcon::as_system),
            Some(&SystemCursorIcon::Default)
        );
    }

    #[test]
    fn software_cursor_defaults_visible_and_hidden_hides_it() {
        let (mut app, sender) = software_cursor_scene_app();
        app.update();
        let entity = app
            .world()
            .resource::<CursorScene>()
            .entity
            .expect("software mode owns one cursor entity");
        assert_eq!(
            app.world().get::<Visibility>(entity),
            Some(&Visibility::Inherited)
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Hidden,
            }],
        );
        assert_eq!(
            app.world().get::<Visibility>(entity),
            Some(&Visibility::Hidden)
        );
        assert_eq!(
            app.world().resource::<CursorScene>().selection,
            ProjectedCursorSelection::Hidden
        );

        let id = app
            .world()
            .resource::<SurfaceEntities>()
            .surfaces
            .keys()
            .next()
            .copied();
        assert!(id.is_none(), "cursor singleton is outside SurfaceEntities");

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Default,
            }],
        );
        assert_eq!(
            app.world().get::<Visibility>(entity),
            Some(&Visibility::Inherited)
        );
    }

    #[test]
    fn kms_chrome_cursors_share_four_bounded_resize_images_and_centred_hotspots() {
        let (mut app, sender) = software_cursor_scene_app();
        app.init_resource::<Assets<SpriteMaterial>>();
        app.update();
        let entity = app
            .world()
            .resource::<CursorScene>()
            .entity
            .expect("software cursor entity exists");
        let images_before = app.world().resource::<Assets<Image>>().len();
        let materials_before = app.world().resource::<Assets<SpriteMaterial>>().len();
        assert_eq!(images_before, 5, "default plus four resize images");

        let (horizontal, vertical, ne_sw, nw_se) = {
            let cursor = app.world().resource::<CursorScene>();
            let resize = cursor
                .resize_images
                .as_ref()
                .expect("software cursor owns resize images");
            (
                resize.horizontal.id(),
                resize.vertical.id(),
                resize.ne_sw.id(),
                resize.nw_se.id(),
            )
        };
        let cases = [
            (ChromeCursorIcon::NResize, vertical),
            (ChromeCursorIcon::NeResize, ne_sw),
            (ChromeCursorIcon::EResize, horizontal),
            (ChromeCursorIcon::SeResize, nw_se),
            (ChromeCursorIcon::SResize, vertical),
            (ChromeCursorIcon::SwResize, ne_sw),
            (ChromeCursorIcon::WResize, horizontal),
            (ChromeCursorIcon::NwResize, nw_se),
        ];
        for (chrome, expected) in cases {
            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::CursorUpdated {
                    image: CursorImage::Chrome(chrome),
                }],
            );
            assert_eq!(
                app.world()
                    .get::<SpriteMesh>(entity)
                    .map(|sprite| sprite.image.id()),
                Some(expected),
                "{chrome:?} selects its shared directional image"
            );
        }
        for _ in 0..32 {
            for (chrome, _) in cases {
                publish(
                    &mut app,
                    &sender,
                    vec![ProtocolEvent::CursorUpdated {
                        image: CursorImage::Chrome(chrome),
                    }],
                );
            }
        }

        let cursor = app.world().resource::<CursorScene>();
        let resize = cursor
            .resize_images
            .as_ref()
            .expect("software cursor owns resize images");
        let unique = [
            resize.horizontal.id(),
            resize.vertical.id(),
            resize.ne_sw.id(),
            resize.nw_se.id(),
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        assert_eq!(unique.len(), 4);
        assert_eq!(app.world().resource::<Assets<Image>>().len(), images_before);
        assert_eq!(
            app.world().resource::<Assets<SpriteMaterial>>().len(),
            materials_before
        );
        assert_eq!(
            app.world()
                .get::<SpriteMesh>(entity)
                .map(|sprite| sprite.image.id()),
            Some(resize.nw_se.id()),
            "north-west uses the NW/SE shared image"
        );

        let position = CursorPositionSnapshot {
            x: 100.0,
            y: 80.0,
            revision: 1,
        };
        let rect = cursor_renderer_rect(
            position,
            RESIZE_CURSOR_HOTSPOT,
            Vec2::splat(RESIZE_CURSOR_SIZE as f32),
            120,
        );
        assert_eq!((rect.x, rect.y), (90.0, 70.0));
    }

    #[test]
    fn kms_chrome_leave_restores_retained_shm_and_dmabuf_client_cursors() {
        for buffer_kind in [SurfaceBufferKind::Shm, SurfaceBufferKind::Dmabuf] {
            let (mut app, sender) = software_cursor_scene_app();
            app.update();
            let id = ObjectId::null();
            let image = app
                .world_mut()
                .resource_mut::<Assets<Image>>()
                .add(default_cursor_image());
            app.world_mut().resource_mut::<CursorScene>().client = Some(ProjectedClientCursor {
                id: id.clone(),
                hotspot: (4, 5),
                presentation: CursorPresentation {
                    width: 16.0,
                    height: 20.0,
                    source: None,
                    transform: SurfaceTransform::Normal,
                },
                image: ClientSurfaceImage::encoded_premultiplied_unorm(image.clone()),
                buffer_kind,
                opaque: false,
            });
            app.world_mut().resource_mut::<CursorScene>().selection =
                ProjectedCursorSelection::Surface;

            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::CursorUpdated {
                    image: CursorImage::Chrome(ChromeCursorIcon::EResize),
                }],
            );
            assert!(app.world().resource::<Assets<Image>>().contains(&image));
            assert_eq!(
                app.world()
                    .resource::<CursorScene>()
                    .client
                    .as_ref()
                    .map(|client| client.image.id()),
                Some(image.id())
            );

            publish(
                &mut app,
                &sender,
                vec![ProtocolEvent::CursorUpdated {
                    image: CursorImage::Surface {
                        id,
                        hotspot: (4, 5),
                        presentation: CursorPresentation {
                            width: 16.0,
                            height: 20.0,
                            source: None,
                            transform: SurfaceTransform::Normal,
                        },
                        frame: None,
                    },
                }],
            );
            assert_eq!(
                app.world().resource::<CursorScene>().selection,
                ProjectedCursorSelection::Surface
            );
            assert!(app.world().resource::<Assets<Image>>().contains(&image));
        }
    }

    #[test]
    fn cursor_position_and_hotspot_math_preserve_top_left_at_corners_and_offscreen() {
        let canvas = Vec2::new(320.0, 240.0);
        let size = Vec2::new(16.0, 20.0);
        for (position, hotspot) in [
            (
                CursorPositionSnapshot {
                    x: 0.0,
                    y: 0.0,
                    revision: 1,
                },
                (0, 0),
            ),
            (
                CursorPositionSnapshot {
                    x: 319.999,
                    y: 239.999,
                    revision: 2,
                },
                (0, 0),
            ),
            (
                CursorPositionSnapshot {
                    x: 2.0,
                    y: 3.0,
                    revision: 3,
                },
                (7, 9),
            ),
        ] {
            let transform = cursor_transform(
                position,
                hotspot,
                size,
                SurfaceTransform::Normal,
                canvas,
                120,
            );
            let projected_top_left = (
                transform.translation.x + canvas.x / 2.0 - size.x / 2.0,
                canvas.y / 2.0 - transform.translation.y - size.y / 2.0,
            );
            assert!((projected_top_left.0 - (position.x as f32 - hotspot.0 as f32)).abs() < 0.001);
            assert!((projected_top_left.1 - (position.y as f32 - hotspot.1 as f32)).abs() < 0.001);
            assert_eq!(transform.translation.z, CURSOR_Z);
        }
    }

    #[test]
    fn cursor_snapshot_is_sampled_after_the_protocol_event_drain() {
        let (sender, feed) = ClientSceneFeed::test_channel();
        feed.set_cursor_position_for_test(CursorPositionSnapshot {
            x: 123.5,
            y: 77.25,
            revision: 44,
        });
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .add_plugins(CompositorScenePlugin::new(
                320,
                240,
                SceneCursorMode::SoftwareCursor,
            ));
        sender
            .send(vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Default,
            }])
            .expect("cursor image event reaches scene");
        app.update();

        assert_eq!(
            app.world().resource::<CursorScene>().position,
            CursorPositionSnapshot {
                x: 123.5,
                y: 77.25,
                revision: 44,
            }
        );
    }

    #[test]
    fn default_arrow_is_rgba_with_dark_outline_white_fill_and_transparency() {
        let image = default_cursor_image();
        let data = image
            .data
            .as_ref()
            .expect("built-in cursor has owned RGBA data");
        assert_eq!(
            data.len(),
            (DEFAULT_CURSOR_WIDTH
                * DEFAULT_CURSOR_MASTER_SCALE
                * DEFAULT_CURSOR_HEIGHT
                * DEFAULT_CURSOR_MASTER_SCALE
                * 4) as usize
        );
        assert_eq!(image.width(), DEFAULT_CURSOR_WIDTH * 3);
        assert_eq!(image.height(), DEFAULT_CURSOR_HEIGHT * 3);
        assert_eq!(image.sampler, ImageSampler::linear());
        assert!(data.chunks_exact(4).any(|pixel| pixel == [30, 32, 38, 255]));
        assert!(
            data.chunks_exact(4)
                .any(|pixel| pixel == [248, 248, 245, 255])
        );
        assert!(data.chunks_exact(4).any(|pixel| pixel == [0, 0, 0, 0]));
    }

    #[test]
    fn fractional_edge_projection_is_symmetric_and_adjacent_rectangles_share_one_edge() {
        let scale120 = 300;
        assert_eq!(project_logical_edge(0.2, scale120), 1);
        assert_eq!(project_logical_edge(-0.2, scale120), -1);
        assert_eq!(project_logical_edge(0.6, scale120), 2);
        assert_eq!(project_logical_edge(-0.6, scale120), -2);

        let left = project_logical_edge(7.25, scale120);
        let shared_from_left = project_logical_edge(7.25 + 101.5, scale120);
        let shared_from_right = project_logical_edge(108.75, scale120);
        let right = project_logical_edge(108.75 + 63.1, scale120);
        assert_eq!(shared_from_left, shared_from_right);
        assert_eq!(
            (shared_from_left - left) + (right - shared_from_right),
            right - left,
            "independently projected widths cover the span without a gap or overlap"
        );
    }

    #[test]
    fn renderer_snapping_is_identity_at_scale_one_and_keeps_viewport_sources_in_buffer_pixels() {
        let logical = RendererRect {
            x: -3.125,
            y: 7.75,
            width: 101.375,
            height: 42.625,
        };
        assert_eq!(
            renderer_rect(logical.x, logical.y, logical.width, logical.height, 120),
            logical,
            "the 0.16.0 scale-one renderer geometry is bit-identical"
        );

        let source = TextureSourceRect {
            x: 17.0,
            y: 23.0,
            width: 301.0,
            height: 199.0,
        };
        let mut layout = layout(1);
        layout.x = 0.2;
        layout.y = -0.2;
        layout.width = 1536.0;
        layout.height = 864.0;
        layout.source = Some(source);
        let material = client_surface_material(
            ClientSurfaceImage::encoded_premultiplied_unorm(Handle::default()),
            false,
            layout,
            300,
        );
        assert_eq!(
            material.source_rect,
            Some(Rect::new(17.0, 23.0, 318.0, 222.0)),
            "renderer placement snapping must not scale the buffer-pixel source rectangle"
        );
    }

    #[test]
    fn client_surface_material_preserves_crop_flip_alpha_and_custom_size() {
        let layout = SurfaceLayout {
            x: 11.0,
            y: 13.0,
            width: 80.0,
            height: 40.0,
            z: 1.0,
            source: Some(TextureSourceRect {
                x: 17.0,
                y: 23.0,
                width: 31.0,
                height: 19.0,
            }),
            parent: None,
            transform: SurfaceTransform::Flipped90,
            visible: true,
            toplevel: None,
        };
        let material = client_surface_material(
            ClientSurfaceImage::encoded_premultiplied_unorm(Handle::default()),
            false,
            layout,
            120,
        );

        assert_eq!(material.image_id(), Handle::<Image>::default().id());
        assert!(material.flip_x);
        assert!(!material.flip_y);
        assert_eq!(material.custom_size, Vec2::new(40.0, 80.0));
        assert_eq!(
            material.source_rect,
            Some(Rect::new(17.0, 23.0, 48.0, 42.0))
        );
        assert_eq!(material.corner_radius, 0.0);
        assert_eq!(material.alpha_mode, bevy::sprite_render::AlphaMode2d::Blend);

        let opaque = client_surface_material(
            ClientSurfaceImage::encoded_premultiplied_unorm(Handle::default()),
            true,
            layout,
            120,
        );
        assert_eq!(opaque.alpha_mode, bevy::sprite_render::AlphaMode2d::Opaque);

        let mut rounded_opaque = opaque.clone();
        rounded_opaque.set_rounded_clip(Mat3::IDENTITY, Vec2::new(80.0, 40.0), 8.0);
        assert_eq!(
            rounded_opaque.alpha_mode,
            bevy::sprite_render::AlphaMode2d::Blend,
            "rounded opaque surfaces need coverage blending"
        );
        rounded_opaque.set_rounded_clip(Mat3::IDENTITY, Vec2::new(80.0, 40.0), 0.0);
        assert_eq!(
            rounded_opaque.alpha_mode,
            bevy::sprite_render::AlphaMode2d::Opaque
        );
    }

    #[test]
    fn rounded_surface_repeated_commits_log_one_settled_sampling_contract() {
        let (mut app, sender) = scene_app();
        app.world_mut()
            .resource_mut::<ClientSamplingContractLog>()
            .enabled = true;
        let id = SurfaceId(71);
        let surface_layout = SurfaceLayout {
            x: 100.0,
            y: 80.0,
            width: 320.0,
            height: 200.0,
            z: 1.0,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: Some(ToplevelSceneState {
                decoration: SceneDecorationMode::ServerSide,
                focused: true,
                committed_maximized: false,
                window_geometry: SceneWindowGeometry {
                    x: 0.0,
                    y: 0.0,
                    width: 320.0,
                    height: 200.0,
                },
                chrome_pointer: ChromePointerSceneState::default(),
            }),
        };
        let capture = LogCapture::default();
        let subscriber = bevy::log::tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(capture.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            for value in 0..10 {
                publish(
                    &mut app,
                    &sender,
                    vec![ProtocolEvent::SurfaceUpserted {
                        id,
                        scene: scene(surface_layout),
                        frame: frame(value),
                    }],
                );
            }
        });

        let material = {
            let surface = &app.world().resource::<SurfaceEntities>().surfaces[&id];
            app.world()
                .resource::<Assets<ClientSurfaceMaterial>>()
                .get(&surface.material)
                .expect("rounded surface owns its settled material")
        };
        assert!(
            material.corner_radius > 0.0,
            "test surface must remain rounded"
        );
        let output = capture.output();
        let lines = output
            .lines()
            .filter(|line| line.contains("Client surface material sampling contract"))
            .collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            1,
            "ten commits on one unchanged rounded surface must emit exactly one settled sampling-contract line; output:\n{output}",
        );
        assert!(
            lines[0].contains("rounded_coverage=true")
                && lines[0].contains("alpha_mode=Blend")
                && lines[0].contains("blend=\"standard-alpha\""),
            "the only emitted contract must contain the settled rounded clip and coverage blend, never the transient unclipped material; line: {}",
            lines[0],
        );
        assert!(
            !lines[0].contains("rounded_coverage=false"),
            "the transient unclipped material must never be logged: {}",
            lines[0],
        );
    }

    #[test]
    fn dmabuf_only_client_cursor_logs_one_material_contract() {
        let (mut app, _sender) = software_cursor_scene_app();
        app.update();
        app.world_mut()
            .resource_mut::<ClientSamplingContractLog>()
            .enabled = true;
        let image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(default_cursor_image());
        let cursor = ProjectedClientCursor {
            id: ObjectId::null(),
            hotspot: (0, 0),
            presentation: CursorPresentation {
                width: 16.0,
                height: 20.0,
                source: None,
                transform: SurfaceTransform::Normal,
            },
            image: ClientSurfaceImage::encoded_premultiplied_unorm(image),
            buffer_kind: SurfaceBufferKind::Dmabuf,
            opaque: false,
        };
        app.world_mut().resource_mut::<CursorScene>().client = Some(cursor);
        app.world_mut().resource_mut::<CursorScene>().selection = ProjectedCursorSelection::Surface;
        let capture = LogCapture::default();
        let subscriber = bevy::log::tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(capture.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            app.update();
            app.update();
        });

        let output = capture.output();
        let cursor_lines = output
            .lines()
            .filter(|line| {
                line.contains("Client surface material sampling contract")
                    && line.contains("cursor_id=Some(0)")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cursor_lines.len(),
            1,
            "a stable DMA-BUF-only client cursor must emit one material contract, then remain quiet; output:\n{output}",
        );
    }

    #[test]
    fn client_surface_material_handle_survives_buffer_replacement() {
        let (mut app, sender) = scene_app();
        let id = SurfaceId(1);
        let surface_layout = layout(1);
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(surface_layout),
                frame: frame(1),
            }],
        );
        let first = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let entity = first.entity;
        let image = first.image.clone();
        let material = first.material.clone();
        assert!(app.world().get::<Mesh2d>(entity).is_some());
        assert!(app.world().get::<SpriteMesh>(entity).is_none());
        assert!(app.world().get::<NoFrustumCulling>(entity).is_some());

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(surface_layout),
                frame: frame(2),
            }],
        );

        let replaced = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(replaced.entity, entity);
        assert_eq!(replaced.image, image);
        assert_eq!(replaced.material, material);
        assert_eq!(
            app.world()
                .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity),
            Some(&MeshMaterial2d(material.clone()))
        );
        assert_eq!(
            app.world()
                .resource::<Assets<ClientSurfaceMaterial>>()
                .len(),
            1
        );
    }

    #[test]
    fn client_shm_images_are_buffer_sized_copies_with_explicit_linear_filtering() {
        let frame = ShmFrame {
            width: 4608,
            height: 2592,
            opaque: true,
            rgba: Arc::new(vec![0x5a; 4608 * 2592 * 4]),
        };
        let image = frame.into_image();
        assert_eq!((image.width(), image.height()), (4608, 2592));
        assert_eq!(
            image.data.as_ref().map(Vec::len),
            Some(4608 * 2592 * 4),
            "the renderer copy follows the client buffer, not logical output dimensions"
        );
        assert_eq!(image.sampler, ImageSampler::linear());
        assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn client_cursor_uses_client_material_while_drawn_cursors_stay_sprite_materials() {
        let (mut app, sender) = software_cursor_scene_app();
        app.update();
        let entity = app
            .world()
            .resource::<CursorScene>()
            .entity
            .expect("software cursor entity exists");
        let drawn = app
            .world()
            .get::<SpriteMesh>(entity)
            .expect("compositor-drawn cursor uses the sprite path")
            .clone();
        assert_eq!(
            SpriteMaterial::from_sprite_mesh(drawn).alpha_mode,
            bevy::sprite_render::AlphaMode2d::Blend
        );
        assert!(
            app.world()
                .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)
                .is_none()
        );
        assert_eq!(
            default_cursor_image().texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(
            resize_cursor_image(ResizeCursorAxis::Horizontal)
                .texture_descriptor
                .format,
            TextureFormat::Rgba8UnormSrgb
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Surface {
                    id: ObjectId::null(),
                    hotspot: (2, 3),
                    presentation: CursorPresentation {
                        width: 16.0,
                        height: 20.0,
                        source: None,
                        transform: SurfaceTransform::Normal,
                    },
                    frame: Some(frame(0x80)),
                },
            }],
        );

        assert!(app.world().get::<SpriteMesh>(entity).is_none());
        assert!(app.world().get::<Mesh2d>(entity).is_some());
        let material = app
            .world()
            .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)
            .expect("client cursor uses the shared client material")
            .0
            .clone();
        assert!(
            app.world()
                .resource::<Assets<ClientSurfaceMaterial>>()
                .contains(&material)
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::CursorUpdated {
                image: CursorImage::Default,
            }],
        );
        assert!(app.world().get::<SpriteMesh>(entity).is_some());
        assert!(
            app.world()
                .get::<MeshMaterial2d<ClientSurfaceMaterial>>(entity)
                .is_none()
        );
    }

    #[test]
    fn fractional_cursor_hotspot_and_placement_remain_logical() {
        let position = CursorPositionSnapshot {
            x: 1535.5,
            y: 863.5,
            revision: 7,
        };
        let rendered = cursor_renderer_rect(position, (7, 9), Vec2::new(16.0, 20.0), 300);
        assert_eq!(project_logical_edge(rendered.x, 300), 3821);
        assert_eq!(project_logical_edge(rendered.y, 300), 2136);
        assert_eq!(project_logical_edge(rendered.x + rendered.width, 300), 3861);
        assert_eq!(
            project_logical_edge(rendered.y + rendered.height, 300),
            2186
        );
        let transform = cursor_transform(
            position,
            (7, 9),
            Vec2::new(16.0, 20.0),
            SurfaceTransform::Normal,
            Vec2::new(1536.0, 864.0),
            300,
        );
        assert_eq!(transform.translation.z, CURSOR_Z);
    }

    #[test]
    fn client_z_ranks_stay_bounded_below_the_cursor_band() {
        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(320.0, 240.0)));
        world.insert_resource(RendererOutputScale120(120));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);
        for (id, protocol_z) in [(1, -1.0e30), (2, 9.0e30), (3, 42.0)] {
            let mut layout = layout(id);
            layout.z = protocol_z;
            upsert_surface(&mut world, SurfaceId(id), layout, frame(id as u8));
        }
        recompute_surface_z_ranks(&mut world);
        let entities = world.resource::<SurfaceEntities>();
        let ordered =
            [SurfaceId(1), SurfaceId(3), SurfaceId(2)].map(|id| entities.surfaces[&id].renderer_z);
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            ordered.iter().all(|z| {
                *z > CLIENT_CONTENT_Z_MIN && *z < CLIENT_CONTENT_Z_MAX && *z < CURSOR_Z
            })
        );
    }

    fn mesh_changed_tick(world: &World, entity: Entity) -> u32 {
        world
            .entity(entity)
            .get_change_ticks::<Mesh2d>()
            .expect("a client surface owns a Mesh2d")
            .changed
            .get()
    }

    /// Opening a third window re-ranks the first two, and a re-rank is a
    /// transform-only change. Bevy 0.19 captures a `Transparent2d` item's
    /// `sort_key` when the mesh is queued and never refreshes it -- the queue
    /// loop revisits only newly-visible entities and those whose `Mesh2d` or
    /// `MeshMaterial2d` changed -- while still uploading the new transform every
    /// frame. So unless the Z change marks the mesh, the window draws in the
    /// right place in its *previous* stacking position, which is the two-window
    /// fault reported on 2026-08-11. Assert the marking, because the resulting
    /// draw order is not observable from this world.
    #[test]
    fn opening_a_window_requeues_the_existing_windows_meshes_against_their_new_z() {
        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(320.0, 240.0)));
        world.insert_resource(RendererOutputScale120(120));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);

        for id in [1u64, 2] {
            upsert_surface(&mut world, SurfaceId(id), layout(id), frame(id as u8));
        }
        recompute_surface_z_ranks(&mut world);

        let existing = [SurfaceId(1), SurfaceId(2)].map(|id| {
            let surface = &world.resource::<SurfaceEntities>().surfaces[&id];
            (id, surface.entity, surface.renderer_z)
        });

        // A mutation stamps the world's current change tick, so advance it
        // first: otherwise the requeue would land on the same tick as the setup
        // above and this test could not tell the two apart.
        world.increment_change_tick();
        let before = existing.map(|(_, entity, _)| mesh_changed_tick(&world, entity));

        upsert_surface(&mut world, SurfaceId(3), layout(3), frame(3));
        recompute_surface_z_ranks(&mut world);

        for (index, (id, entity, previous_z)) in existing.iter().enumerate() {
            let current_z = world.resource::<SurfaceEntities>().surfaces[id].renderer_z;
            assert_ne!(
                *previous_z, current_z,
                "surface {} must be re-ranked by the third window or this test proves nothing",
                id.0
            );
            assert!(
                mesh_changed_tick(&world, *entity) > before[index],
                "surface {}'s mesh was not marked changed when its Z moved from {previous_z} to \
                 {current_z}, so Bevy keeps its old Transparent2d sort key and composites the \
                 window in its previous stacking position",
                id.0
            );
        }
    }

    #[test]
    fn every_generated_scene_z_is_strictly_inside_the_host_camera_projection() {
        let projection = bevy::camera::OrthographicProjection {
            scaling_mode: bevy::camera::ScalingMode::Fixed {
                width: 1536.0,
                height: 864.0,
            },
            ..bevy::camera::OrthographicProjection::default_2d()
        };
        let generated = [
            (
                "minimum-rank client",
                client_content_z(0, MAX_GLOBAL_SURFACES),
            ),
            (
                "maximum-rank client",
                client_content_z(MAX_GLOBAL_SURFACES - 1, MAX_GLOBAL_SURFACES),
            ),
            ("cursor", CURSOR_Z),
        ];

        for (name, z) in generated {
            assert!(
                z > projection.near && z < projection.far,
                "{name} Z {z} is clipped by the host Camera2d projection ({}, {})",
                projection.near,
                projection.far
            );
        }
    }

    #[test]
    fn one_large_create_and_churn_batch_runs_one_z_recompute_each_under_the_deadline() {
        const SURFACE_COUNT: usize = 4096;
        const WELL_UNDER_NO_SUBMIT_DEADLINE: Duration = Duration::from_millis(1500);

        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(320.0, 240.0)));
        world.insert_resource(RendererOutputScale120(120));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);
        world.insert_resource(ZRankRecomputeProbe::default());

        let created = (1..=SURFACE_COUNT as u64)
            .map(|id| ProtocolEvent::SurfaceUpserted {
                id: SurfaceId(id),
                scene: scene(layout(id)),
                frame: frame((id & 0xff) as u8),
            })
            .collect();
        let create_started = Instant::now();
        apply_protocol_events(&mut world, created);
        let create_elapsed = create_started.elapsed();
        assert_eq!(
            world.resource::<ZRankRecomputeProbe>().passes,
            1,
            "the 4096-surface creation batch gets one rank pass"
        );
        assert_eq!(
            world.resource::<SurfaceEntities>().surfaces.len(),
            SURFACE_COUNT
        );
        assert!(
            create_elapsed < WELL_UNDER_NO_SUBMIT_DEADLINE,
            "4096-surface creation took {create_elapsed:?}, not well under the two-second KMS no-submit deadline"
        );

        let mut churn = Vec::with_capacity(SURFACE_COUNT);
        for id in 1..=SURFACE_COUNT as u64 / 2 {
            churn.push(ProtocolEvent::SurfaceDestroyed { id: SurfaceId(id) });
        }
        for offset in 1..=SURFACE_COUNT as u64 / 2 {
            let id = SURFACE_COUNT as u64 + offset;
            churn.push(ProtocolEvent::SurfaceUpserted {
                id: SurfaceId(id),
                scene: scene(layout(id)),
                frame: frame((id & 0xff) as u8),
            });
        }
        let churn_started = Instant::now();
        apply_protocol_events(&mut world, churn);
        let churn_elapsed = churn_started.elapsed();
        assert_eq!(
            world.resource::<ZRankRecomputeProbe>().passes,
            2,
            "the 4096-event removal/creation churn batch adds one rank pass"
        );
        assert_eq!(
            world.resource::<SurfaceEntities>().surfaces.len(),
            SURFACE_COUNT
        );
        assert!(
            churn_elapsed < WELL_UNDER_NO_SUBMIT_DEADLINE,
            "4096-event churn took {churn_elapsed:?}, not well under the two-second KMS no-submit deadline"
        );

        let entities = world.resource::<SurfaceEntities>();
        let mut ordered = entities.surfaces.values().collect::<Vec<_>>();
        ordered.sort_unstable_by(|left, right| {
            left.layout
                .z
                .total_cmp(&right.layout.z)
                .then_with(|| left.entity.index().cmp(&right.entity.index()))
        });
        assert!(
            ordered
                .windows(2)
                .all(|pair| pair[0].renderer_z < pair[1].renderer_z),
            "batched churn preserves strict renderer stack order"
        );
    }

    #[test]
    fn rank_recompute_does_not_rewrite_a_transform_when_the_rank_is_unchanged() {
        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(320.0, 240.0)));
        world.insert_resource(RendererOutputScale120(120));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);
        world.insert_resource(ZRankRecomputeProbe::default());
        apply_protocol_events(
            &mut world,
            (1..=3)
                .map(|id| ProtocolEvent::SurfaceUpserted {
                    id: SurfaceId(id),
                    scene: scene(layout(id)),
                    frame: frame(id as u8),
                })
                .collect(),
        );
        let writes_before = world.resource::<ZRankRecomputeProbe>().transform_writes;
        let mut same_rank = layout(2);
        same_rank.z += 0.25;

        apply_protocol_events(
            &mut world,
            vec![ProtocolEvent::SurfaceRelayout {
                id: SurfaceId(2),
                scene: scene(same_rank),
            }],
        );

        let probe = world.resource::<ZRankRecomputeProbe>();
        assert_eq!(
            probe.passes, 2,
            "the changed protocol Z marks the batch dirty"
        );
        assert_eq!(
            probe.transform_writes, writes_before,
            "an unchanged renderer rank causes no rank-pass transform rewrite"
        );
    }

    #[test]
    fn x_format_client_buffers_force_alpha_one_in_the_non_blending_material_path() {
        let layout = SurfaceLayout {
            x: 0.0,
            y: 0.0,
            width: 64.0,
            height: 32.0,
            z: 1.0,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        for fourcc in [Fourcc::Xrgb8888, Fourcc::Xbgr8888] {
            let descriptor = DmabufDescriptor {
                width: 1,
                height: 1,
                fourcc: fourcc as u32,
                modifier: u64::from(Modifier::Linear),
                planes: vec![cosmix_wgpu_dmabuf::DmabufPlane {
                    fd: File::open("/dev/null")
                        .expect("/dev/null is available")
                        .into(),
                    offset: 0,
                    stride: 4,
                }],
            };
            assert!(descriptor.is_opaque());
            let opaque = client_surface_material(
                ClientSurfaceImage::encoded_premultiplied_unorm(Handle::default()),
                descriptor.is_opaque(),
                layout,
                120,
            );
            assert_eq!(opaque.alpha_mode, bevy::sprite_render::AlphaMode2d::Opaque);
        }
        let alpha = client_surface_material(
            ClientSurfaceImage::encoded_premultiplied_unorm(Handle::default()),
            false,
            layout,
            120,
        );

        assert_eq!(alpha.alpha_mode, bevy::sprite_render::AlphaMode2d::Blend);
    }

    #[test]
    fn surface_child_index_tracks_insert_reparent_and_remove() {
        let mut entities = SurfaceEntities::default();
        entities.update_parent(SurfaceId(2), None, Some(SurfaceId(1)));
        assert_eq!(
            entities.children[&SurfaceId(1)],
            HashSet::from([SurfaceId(2)])
        );

        entities.update_parent(SurfaceId(2), Some(SurfaceId(1)), Some(SurfaceId(3)));
        assert!(!entities.children.contains_key(&SurfaceId(1)));
        assert_eq!(
            entities.children[&SurfaceId(3)],
            HashSet::from([SurfaceId(2)])
        );

        entities.update_parent(SurfaceId(2), Some(SurfaceId(3)), None);
        assert!(entities.children.is_empty());
    }

    /// The renderer half of the rejected-removal recovery: a roster is
    /// membership, so it removes whatever this world holds that the compositor
    /// no longer lists, however that entity got here.
    ///
    /// The batch is shaped the way `PendingProtocolEvents::take` emits one —
    /// roster first, per-surface events behind it — and carries an upsert for a
    /// surface created after the roster was snapshotted, which the roster
    /// therefore does not list. That event must still land: membership decides
    /// what is removed, and the events behind it decide what exists.
    #[test]
    fn a_surface_roster_removes_exactly_the_entities_the_compositor_no_longer_lists() {
        let layout = SurfaceLayout {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 8.0,
            z: 1.0,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(960.0, 640.0)));
        world.insert_resource(RendererOutputScale120(120));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);

        let staying = SurfaceId(1);
        // Two of them, because a roster is membership: an implementation that
        // removes the first stale id it finds and stops satisfies every
        // postcondition a single departing surface can state.
        let departing = [SurfaceId(2), SurfaceId(4)];
        let arriving = SurfaceId(3);
        upsert_surface(&mut world, staying, layout, frame(0));
        for id in departing {
            upsert_surface(&mut world, id, layout, frame(0));
        }
        let departing_entities =
            departing.map(|id| world.resource::<SurfaceEntities>().surfaces[&id].entity);

        apply_protocol_events(
            &mut world,
            vec![
                ProtocolEvent::SurfaceRoster {
                    mapped: vec![staying],
                },
                ProtocolEvent::SurfaceUpserted {
                    id: arriving,
                    scene: scene(layout),
                    frame: frame(0),
                },
            ],
        );

        let surfaces = &world.resource::<SurfaceEntities>().surfaces;
        assert!(
            surfaces.contains_key(&staying),
            "a listed surface is left alone"
        );
        assert!(
            departing.iter().all(|id| !surfaces.contains_key(id)),
            "every unlisted one is removed, with no delta ever having said so"
        );
        assert!(
            surfaces.contains_key(&arriving),
            "and an upsert behind the roster still lands, even for a surface \
             the roster does not list"
        );
        assert!(
            departing_entities
                .iter()
                .all(|entity| world.get_entity(*entity).is_err()),
            "the entities are despawned, not merely dropped from the index"
        );
    }

    #[test]
    fn descendant_traversal_visits_deep_and_broad_trees_once() {
        let mut children = HashMap::<SurfaceId, HashSet<SurfaceId>>::new();
        for id in 1..64 {
            children
                .entry(SurfaceId(id))
                .or_default()
                .insert(SurfaceId(id + 1));
        }
        for id in 100..164 {
            children
                .entry(SurfaceId(1))
                .or_default()
                .insert(SurfaceId(id));
        }
        let descendants = descendant_surface_ids(&children, SurfaceId(1));
        let unique = descendants.iter().copied().collect::<HashSet<_>>();
        assert_eq!(descendants.len(), 127);
        assert_eq!(unique.len(), descendants.len());
        assert!(unique.contains(&SurfaceId(64)));
        assert!(unique.contains(&SurfaceId(163)));
    }

    #[test]
    fn late_parent_arrival_matches_parent_first_child_world_position() {
        fn test_world() -> World {
            let mut world = World::new();
            world.insert_resource(LogicalCanvasSize(Vec2::new(960.0, 640.0)));
            world.insert_resource(RendererOutputScale120(120));
            world.insert_resource(SurfaceEntities::default());
            world.insert_resource(Assets::<Image>::default());
            insert_client_surface_render_resources(&mut world);
            world
        }

        let parent_id = SurfaceId(1);
        let child_id = SurfaceId(2);
        let parent_layout = SurfaceLayout {
            x: 100.0,
            y: 80.0,
            width: 300.0,
            height: 200.0,
            z: 10.0,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        let child_layout = SurfaceLayout {
            x: 140.0,
            y: 125.0,
            width: 40.0,
            height: 30.0,
            z: 11.0,
            source: None,
            parent: Some(parent_id),
            transform: SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };

        let mut child_first = test_world();
        upsert_surface(&mut child_first, child_id, child_layout, frame(0));
        upsert_surface(&mut child_first, parent_id, parent_layout, frame(0));
        recompute_surface_z_ranks(&mut child_first);
        let child_first_entity =
            child_first.resource::<SurfaceEntities>().surfaces[&child_id].entity;
        assert!(
            child_first.get::<ChildOf>(child_first_entity).is_some(),
            "late parent arrival attaches the direct child"
        );
        let child_first_position = world_position(&child_first, child_first_entity);

        let mut parent_first = test_world();
        upsert_surface(&mut parent_first, parent_id, parent_layout, frame(0));
        upsert_surface(&mut parent_first, child_id, child_layout, frame(0));
        recompute_surface_z_ranks(&mut parent_first);
        let parent_first_entity =
            parent_first.resource::<SurfaceEntities>().surfaces[&child_id].entity;
        let parent_first_position = world_position(&parent_first, parent_first_entity);
        let expected_position = surface_transform(
            child_layout,
            parent_first.resource::<LogicalCanvasSize>().0,
            parent_first.resource::<SurfaceEntities>().surfaces[&child_id].renderer_z,
            120,
        )
        .translation;

        assert!(
            child_first_position.distance(parent_first_position) < 0.001,
            "late-parent position {child_first_position:?} differs from parent-first position \
             {parent_first_position:?}"
        );
        assert!(
            child_first_position.distance(expected_position) < 0.001,
            "child world position {child_first_position:?} differs from authoritative layout \
             position {expected_position:?}"
        );

        assert!(remove_surface(&mut child_first, parent_id));
        recompute_surface_z_ranks(&mut child_first);
        assert!(
            child_first.get::<ChildOf>(child_first_entity).is_none(),
            "removing the parent detaches the direct child"
        );
        let detached_position = world_position(&child_first, child_first_entity);
        let detached_expected = surface_transform(
            child_layout,
            child_first.resource::<LogicalCanvasSize>().0,
            child_first.resource::<SurfaceEntities>().surfaces[&child_id].renderer_z,
            120,
        )
        .translation;
        assert!(
            detached_position.distance(detached_expected) < 0.001,
            "detached child position {detached_position:?} differs from authoritative layout \
             position {detached_expected:?}"
        );
    }

    #[test]
    fn channel_fed_plugin_upserts_and_replaces_one_shm_surface() {
        let (mut app, sender) = scene_app();
        let id = SurfaceId(1);
        let layout = layout(1);
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(layout),
                frame: frame(3),
            }],
        );

        let first = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let entity = first.entity;
        let image = first.image.clone();
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("upserted SHM image remains registered")
                .data,
            Some(vec![3; 16])
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(layout),
                frame: frame(7),
            }],
        );
        let replaced = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(replaced.entity, entity, "SHM replacement keeps the entity");
        assert_eq!(
            replaced.image, image,
            "SHM replacement keeps the asset handle"
        );
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("replaced SHM image remains registered")
                .data,
            Some(vec![7; 16])
        );
    }

    #[test]
    fn real_protocol_thread_shm_client_reaches_the_live_shaped_scene_texture() {
        let mut scene = real_protocol_scene("pixels");
        scene.client.commit_rgb([0x31, 0x72, 0xb4]);
        let id = wait_for_real_surface(&mut scene.app);

        let world = scene.app.world();
        assert_eq!(
            world.resource::<LogicalCanvasSize>().0,
            Vec2::new(320.0, 240.0)
        );
        let surface = &world.resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(
            world
                .resource::<Assets<Image>>()
                .get(surface.image.handle())
                .expect("real client texture stays registered")
                .data,
            Some(vec![0x31, 0x72, 0xb4, 0xff])
        );
    }

    #[test]
    fn paused_real_client_commit_replaces_retained_surface_on_first_resumed_update() {
        let mut scene = real_protocol_scene("pause-replace");
        scene.client.commit_rgb([0x80, 0x10, 0x20]);
        let id = wait_for_real_surface(&mut scene.app);
        let first = &scene.app.world().resource::<SurfaceEntities>().surfaces[&id];
        let entity = first.entity;
        let image = first.image.clone();

        // A live pause stops pump updates but retains this App and its scene.
        scene.client.commit_rgb([0x20, 0xc0, 0x50]);
        assert_eq!(
            scene
                .app
                .world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("paused SHM texture remains retained")
                .data,
            Some(vec![0x80, 0x10, 0x20, 0xff]),
            "the paused App has not sampled the queued commit"
        );

        scene
            ._runtime
            .kms_topology_client()
            .flush_events(Duration::from_secs(30))
            .expect("resume flushes the compacted client commit before rendering");
        scene.app.update();
        let world = scene.app.world();
        let surfaces = &world.resource::<SurfaceEntities>().surfaces;
        assert_eq!(surfaces.len(), 1, "resume does not duplicate the surface");
        assert_eq!(
            surfaces[&id].entity, entity,
            "resume retains entity identity"
        );
        assert_eq!(surfaces[&id].image, image, "resume retains the SHM handle");
        assert_eq!(
            world
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("resumed SHM texture remains registered")
                .data,
            Some(vec![0x20, 0xc0, 0x50, 0xff]),
            "the first resumed update drains the latest committed pixels"
        );
    }

    #[test]
    fn real_client_cursor_renders_and_first_resumed_update_uses_current_backing() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let socket_name = format!("cosmix-live-cursor-{}-{unique}", std::process::id());
        let mut runtime = real_shm_scene_runtime(&socket_name, (320, 240));
        let feed = runtime
            .take_client_scene_feed()
            .expect("the live cursor scene exclusively takes the protocol feed");
        let mut app = live_client_scene_app_for_test(feed, (320, 240));
        let mut client = RealCursorSceneClient::connect(&runtime, &socket_name, "live-cursor");
        client.commit_rgb([0x80, 0x20, 0x10]);

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.update();
            if app.world().resource::<CursorScene>().selection == ProjectedCursorSelection::Surface
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "real cursor did not reach the live-shaped scene App"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let cursor = app.world().resource::<CursorScene>();
        let entity = cursor
            .entity
            .expect("software cursor entity remains singleton");
        let image = cursor
            .client
            .as_ref()
            .expect("client cursor owns an image")
            .image
            .clone();
        assert_eq!(
            app.world().get::<Visibility>(entity),
            Some(&Visibility::Inherited)
        );
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("real cursor texture is registered")
                .data,
            Some(vec![0x80, 0x20, 0x10, 0xff])
        );
        let transform = app
            .world()
            .get::<Transform>(entity)
            .expect("cursor entity is positioned");
        assert_eq!(transform.translation.z, CURSOR_Z);

        client.commit_rgb([0x10, 0xa0, 0x40]);
        client.commit_rgb([0x20, 0x30, 0xd0]);
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("paused cursor texture remains retained")
                .data,
            Some(vec![0x80, 0x20, 0x10, 0xff]),
            "a paused scene preserves the selected cursor and old GPU asset"
        );
        assert_eq!(
            runtime
                .kms_topology_client()
                .flush_events(Duration::from_secs(30))
                .expect("resume flush converges current cursor state"),
            crate::protocol::EventFlushOutcome::Complete
        );
        app.update();
        let cursor = app.world().resource::<CursorScene>();
        assert_eq!(cursor.selection, ProjectedCursorSelection::Surface);
        assert_eq!(
            cursor
                .client
                .as_ref()
                .expect("cursor selection survives pause")
                .image,
            image,
            "SHM cursor replacement retains the image handle"
        );
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(image.handle())
                .expect("resumed cursor texture remains registered")
                .data,
            Some(vec![0x20, 0x30, 0xd0, 0xff]),
            "the first resumed update shows the current cursor, not the queued stale one"
        );
    }

    #[test]
    fn pause_retains_dmabuf_registry_and_release_tokens_until_replace_or_unmap() {
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = live_client_scene_app_for_test(feed, (320, 240));
        let id = SurfaceId(40);
        let layout = layout(40);
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(layout),
                frame: dmabuf_frame(401, 1),
            }],
        );
        let first = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        let entity = first.entity;
        let image = first.image.clone();
        assert!(
            app.world()
                .resource::<ClientSceneFeed>()
                .released_dmabuf_tokens_for_test()
                .is_empty(),
            "mapping and pausing do not release the compositor-owned buffer"
        );

        // While paused, the protocol-side commit can queue but the App does
        // not drain it and therefore retains the original registry entry.
        sender
            .send(vec![
                ProtocolEvent::DmabufCacheInvalidated,
                ProtocolEvent::SurfaceUpserted {
                    id,
                    scene: scene(layout),
                    frame: dmabuf_frame(402, 2),
                },
            ])
            .expect("paused scene feed remains connected");
        assert!(
            app.world()
                .resource::<ClientSceneFeed>()
                .released_dmabuf_tokens_for_test()
                .is_empty(),
            "a queued paused commit alone releases nothing"
        );

        app.update();
        let replaced = &app.world().resource::<SurfaceEntities>().surfaces[&id];
        assert_eq!(replaced.entity, entity);
        assert_eq!(replaced.image, image, "DMA-BUF registry keeps the asset id");
        assert_eq!(
            app.world()
                .resource::<ClientSceneFeed>()
                .released_dmabuf_tokens_for_test(),
            vec![401],
            "replacement routes the superseded token through the scene feed"
        );

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUnmapped { id }],
        );
        assert_eq!(
            app.world()
                .resource::<ClientSceneFeed>()
                .released_dmabuf_tokens_for_test(),
            vec![402],
            "ordinary unmap releases the retained replacement token"
        );
    }

    #[test]
    fn channel_fed_plugin_relayouts_an_existing_surface() {
        let (mut app, sender) = scene_app();
        let id = SurfaceId(2);
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(layout(2)),
                frame: frame(0),
            }],
        );
        let entity = app.world().resource::<SurfaceEntities>().surfaces[&id].entity;
        let relayout = SurfaceLayout {
            x: 200.0,
            y: 100.0,
            width: 60.0,
            height: 30.0,
            z: 9.0,
            visible: false,
            ..layout(2)
        };

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRelayout {
                id,
                scene: scene(relayout),
            }],
        );

        let world = app.world();
        let renderer_z = world.resource::<SurfaceEntities>().surfaces[&id].renderer_z;
        assert_eq!(
            world.get::<Transform>(entity),
            Some(&surface_transform(
                relayout,
                Vec2::new(960.0, 640.0),
                renderer_z,
                120,
            ))
        );
        assert_eq!(world.get::<Visibility>(entity), Some(&Visibility::Hidden));
        let material = &world.resource::<SurfaceEntities>().surfaces[&id].material;
        assert_eq!(
            world
                .resource::<Assets<ClientSurfaceMaterial>>()
                .get(material)
                .map(|material| material.custom_size),
            Some(Vec2::new(60.0, 30.0)),
        );
    }

    #[test]
    fn channel_fed_plugin_reconciles_the_authoritative_roster() {
        let (mut app, sender) = scene_app();
        let staying = SurfaceId(1);
        let departing = SurfaceId(2);
        publish(
            &mut app,
            &sender,
            vec![
                ProtocolEvent::SurfaceUpserted {
                    id: staying,
                    scene: scene(layout(1)),
                    frame: frame(0),
                },
                ProtocolEvent::SurfaceUpserted {
                    id: departing,
                    scene: scene(layout(2)),
                    frame: frame(0),
                },
            ],
        );
        let departing_entity =
            app.world().resource::<SurfaceEntities>().surfaces[&departing].entity;

        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceRoster {
                mapped: vec![staying],
            }],
        );

        let world = app.world();
        let surfaces = &world.resource::<SurfaceEntities>().surfaces;
        assert!(surfaces.contains_key(&staying));
        assert!(!surfaces.contains_key(&departing));
        assert!(world.get_entity(departing_entity).is_err());
    }

    #[test]
    fn channel_fed_plugin_unmaps_and_despawns_a_surface() {
        let (mut app, sender) = scene_app();
        let id = SurfaceId(4);
        publish(
            &mut app,
            &sender,
            vec![ProtocolEvent::SurfaceUpserted {
                id,
                scene: scene(layout(4)),
                frame: frame(0),
            }],
        );
        let entity = app.world().resource::<SurfaceEntities>().surfaces[&id].entity;

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
        assert!(app.world().get_entity(entity).is_err());
    }

    #[test]
    fn channel_fed_plugin_builds_late_subsurface_hierarchy_and_z_order() {
        let (mut app, sender) = scene_app();
        let parent_id = SurfaceId(1);
        let child_id = SurfaceId(2);
        let parent_layout = SurfaceLayout {
            x: 100.0,
            y: 80.0,
            width: 300.0,
            height: 200.0,
            z: 10.0,
            ..layout(1)
        };
        let child_layout = SurfaceLayout {
            x: 140.0,
            y: 125.0,
            width: 40.0,
            height: 30.0,
            z: 11.0,
            parent: Some(parent_id),
            ..layout(2)
        };

        publish(
            &mut app,
            &sender,
            vec![
                ProtocolEvent::SurfaceUpserted {
                    id: child_id,
                    scene: scene(child_layout),
                    frame: frame(0),
                },
                ProtocolEvent::SurfaceUpserted {
                    id: parent_id,
                    scene: scene(parent_layout),
                    frame: frame(0),
                },
            ],
        );

        let world = app.world();
        let surfaces = &world.resource::<SurfaceEntities>().surfaces;
        let parent = surfaces[&parent_id].entity;
        let child = surfaces[&child_id].entity;
        assert_eq!(
            world.get::<ChildOf>(child).map(ChildOf::parent),
            Some(parent)
        );
        let child_world = world_position(world, child);
        let expected = surface_transform(
            child_layout,
            Vec2::new(960.0, 640.0),
            surfaces[&child_id].renderer_z,
            120,
        )
        .translation;
        assert!(child_world.distance(expected) < 0.001);
        assert!(child_world.z > world_position(world, parent).z);
    }

    #[test]
    fn later_mapped_ssd_subsurface_stack_is_globally_above_the_earlier_window() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let socket_name = format!("cosmix-ssd-stack-scene-{}-{unique}", std::process::id());
        let runtime = real_shm_scene_runtime_with_decoration(
            &socket_name,
            (320, 240),
            DecorationStartup::resolve(true, ChromeStyle::Win11),
        );
        let (sender, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .insert_resource(feed)
            .insert_resource(DecorationStartup::resolve(true, ChromeStyle::Win11))
            .add_plugins(TransformPlugin)
            .add_plugins(CompositorScenePlugin::new(
                320,
                240,
                SceneCursorMode::HostCursor,
            ));

        // Register B's synchronized child before A exists, but do not give B
        // its xdg role or map it yet. B's child therefore gets the lower
        // SurfaceId. If equal/default z reached the SurfaceId tie-break, it
        // would put B's content below A's, the opposite of the ordering below.
        let pending_b = PendingSsdSubsurfaceSceneClient::connect(&socket_name);
        let pending_a = PendingSsdSubsurfaceSceneClient::connect(&socket_name);
        let mut a = pending_a.finish("foot");
        a.map([0x20, 0x20, 0x20], [0x40, 0x40, 0x40]);
        pump_real_protocol_into_scene(&runtime, &mut app, &sender, 2);

        let mut b = pending_b.finish("foot");
        b.map([0x10, 0x10, 0x10], [0x30, 0x30, 0x30]);
        pump_real_protocol_into_scene(&runtime, &mut app, &sender, 4);
        // Let transform propagation and title layout settle after the batch
        // that created B's complete hierarchy.
        app.update();

        let world = app.world();
        let surfaces = &world.resource::<SurfaceEntities>().surfaces;
        let mut roots = surfaces
            .iter()
            .filter_map(|(id, surface)| {
                (surface.kind == SceneSurfaceKind::Toplevel).then_some((*id, surface.layout.x))
            })
            .collect::<Vec<_>>();
        roots.sort_unstable_by(|left, right| left.1.total_cmp(&right.1));
        assert_eq!(roots.len(), 2, "two real clients publish two toplevels");
        let a_id = roots[0].0;
        let b_id = roots[1].0;
        let child_of = |parent| {
            surfaces
                .iter()
                .find_map(|(id, surface)| (surface.layout.parent == Some(parent)).then_some(*id))
                .expect("mapped toplevel owns one synchronized child")
        };
        let a_child_id = child_of(a_id);
        let b_child_id = child_of(b_id);

        assert!(
            b_child_id.0 < a_child_id.0,
            "the fixture's SurfaceId order must contradict the desired content order"
        );
        let z = |id| surfaces[&id].layout.z;
        assert!((z(a_id) - 11.0).abs() < 0.000_1, "A root z = {}", z(a_id));
        assert!(
            (z(a_child_id) - 11.001).abs() < 0.000_1,
            "A child z = {}",
            z(a_child_id)
        );
        assert!((z(b_id) - 12.0).abs() < 0.000_1, "B root z = {}", z(b_id));
        assert!(
            (z(b_child_id) - 12.001).abs() < 0.000_1,
            "B's parent map must replace its provisional child z: {}",
            z(b_child_id)
        );

        let entity = |id| surfaces[&id].entity;
        let decoration_root = |id| {
            surfaces[&id]
                .decoration
                .as_ref()
                .expect("SSD toplevel owns a decoration")
                .root
        };
        let global_z = |entity| {
            world
                .get::<GlobalTransform>(entity)
                .expect("renderer entity has a resolved global transform")
                .translation()
                .z
        };
        let a_root_entity = entity(a_id);
        let a_child_entity = entity(a_child_id);
        let b_root_entity = entity(b_id);
        let b_child_entity = entity(b_child_id);
        let a_deco_root = decoration_root(a_id);
        let b_deco_root = decoration_root(b_id);
        let a_content_z = global_z(a_child_entity);
        let b_content_z = global_z(b_child_entity);
        let a_decoration_z = world
            .get::<Children>(a_deco_root)
            .expect("A decoration root has renderer children")
            .iter()
            .filter(|child| *child != a_root_entity)
            .map(global_z)
            .collect::<Vec<_>>();
        let b_window_z = world
            .get::<Children>(b_deco_root)
            .expect("B decoration root has renderer children")
            .iter()
            .map(global_z)
            .chain(std::iter::once(b_content_z))
            .collect::<Vec<_>>();

        assert!(
            a_decoration_z.iter().all(|part_z| *part_z < a_content_z),
            "A content ({a_content_z}) must be above all A decoration parts: {a_decoration_z:?}"
        );
        assert!(
            b_content_z > a_content_z && a_decoration_z.iter().all(|part_z| b_content_z > *part_z),
            "B content ({b_content_z}) must be above A content ({a_content_z}) and decoration"
        );
        assert!(
            b_window_z.iter().all(|part_z| a_content_z < *part_z),
            "A content ({a_content_z}) must be below every B quad/decoration part: {b_window_z:?}"
        );
        assert!(global_z(b_root_entity) > global_z(a_child_entity));
        assert!(global_z(b_child_entity) > global_z(b_root_entity));
        assert!(global_z(a_child_entity) > global_z(a_root_entity));
        assert_eq!(global_z(a_deco_root), surfaces[&a_id].renderer_z);
        assert_eq!(global_z(b_deco_root), surfaces[&b_id].renderer_z);

        let title = world
            .get::<Children>(b_deco_root)
            .expect("B decoration root has a title child")
            .iter()
            .find(|child| {
                world
                    .get::<Name>(*child)
                    .is_some_and(|name| name.as_str() == "Decoration title")
            })
            .expect("B decoration title entity is present");
        assert_eq!(
            world.get::<Text2d>(title).map(|text| text.0.as_str()),
            Some("foot")
        );
        assert_eq!(world.get::<Visibility>(title), Some(&Visibility::Inherited));
        assert_eq!(
            world.get::<Visibility>(b_deco_root),
            Some(&Visibility::Inherited)
        );

        let a_layout = surfaces[&a_child_id].layout;
        let b_layout = surfaces[&b_child_id].layout;
        let overlap_left = a_layout.x.max(b_layout.x);
        let overlap_top = a_layout.y.max(b_layout.y);
        let overlap_right = (a_layout.x + a_layout.width).min(b_layout.x + b_layout.width);
        let overlap_bottom = (a_layout.y + a_layout.height).min(b_layout.y + b_layout.height);
        assert!(overlap_left < overlap_right && overlap_top < overlap_bottom);
        let pointer = (
            f64::from((overlap_left + overlap_right) / 2.0),
            f64::from((overlap_top + overlap_bottom) / 2.0),
        );
        runtime
            .finish_frame(vec![
                HostInput::PointerMotionAbsolute {
                    x: pointer.0,
                    y: pointer.1,
                    time: 1,
                },
                HostInput::PointerButton {
                    button: 0x110,
                    state: HostButtonState::Pressed,
                    time: 2,
                },
            ])
            .expect("real pointer press reaches protocol input policy");
        assert_eq!(b.pointer_press_count(), 1, "overlap press resolves to B");
        assert_eq!(
            a.pointer_press_count(),
            0,
            "overlap press does not resolve to A"
        );
    }

    #[test]
    fn fractional_subsurface_offset_is_applied_once_in_surface_local_logical_space() {
        let mut world = World::new();
        world.insert_resource(LogicalCanvasSize(Vec2::new(960.0, 640.0)));
        world.insert_resource(RendererOutputScale120(300));
        world.insert_resource(SurfaceEntities::default());
        world.insert_resource(Assets::<Image>::default());
        insert_client_surface_render_resources(&mut world);
        let parent_id = SurfaceId(41);
        let child_id = SurfaceId(42);
        let parent_layout = SurfaceLayout {
            x: 100.2,
            y: 50.2,
            width: 300.0,
            height: 200.0,
            ..layout(41)
        };
        let child_layout = SurfaceLayout {
            x: 113.2,
            y: 57.2,
            width: 80.0,
            height: 40.0,
            parent: Some(parent_id),
            ..layout(42)
        };
        upsert_surface(&mut world, parent_id, parent_layout, frame(1));
        upsert_surface(&mut world, child_id, child_layout, frame(2));
        recompute_surface_z_ranks(&mut world);

        let child = world.resource::<SurfaceEntities>().surfaces[&child_id].entity;
        let child_world = world_position(&world, child);
        let expected = surface_transform(
            child_layout,
            world.resource::<LogicalCanvasSize>().0,
            world.resource::<SurfaceEntities>().surfaces[&child_id].renderer_z,
            300,
        )
        .translation;
        assert!(child_world.distance(expected) < 0.001);
        assert_eq!(
            projected_renderer_physical_edges(
                child_layout.x,
                child_layout.y,
                child_layout.width,
                child_layout.height,
                300,
            ),
            (283, 143, 483, 243)
        );
    }
}
