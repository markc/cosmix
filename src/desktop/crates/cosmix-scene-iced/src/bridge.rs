//! Main-world half: mounts, input routing, focus, drawing and upload plans.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use bevy::asset::RenderAssetUsages;
use bevy::camera::{Camera, NormalizedRenderTarget, RenderTarget};
use bevy::image::ImageSampler;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key as BevyKey, KeyCode, KeyboardInput};
use bevy::input::mouse::MouseScrollUnit;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::picking::Pickable;
use bevy::picking::hover::HoverMap;
use bevy::picking::pointer::{PointerAction, PointerButton as BevyButton, PointerId, PointerInput};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::ui::widget::NodeImageMode;
use bevy::ui::{ComputedUiRenderTargetInfo, ComputedUiTargetCamera, UiGlobalTransform};
use bevy::window::{RequestRedraw, WindowRef};
use cosmix_scene::ResolvedScene;
use cosmix_scene_bevy::{SceneStore, register_scene_page, scene_edge, scene_page_id};
use cosmix_shell::core::Edge;
use cosmix_shell::runtime::{
    CursorShape, CursorShapeRequest, ExternalImeEvent, ExternalImeKind, ExternalImeTarget,
    ImePurpose,
};
use serde_json::{Value, json};

use crate::gpu::{GpuChannel, SurfaceUpload};
use crate::surface::{
    CursorIcon, ImeEvent, ImeRequest, Key, Modifiers, NamedKey, PointerButton, Processed, Rect,
    ScrollUnit, SurfaceEvent, SurfaceRenderer,
};
use crate::upload;
use crate::{ADAPTER, standin::StandIn};

/// The page entity of one iced-backed scene.
#[derive(Component, Debug)]
pub struct IcedSurface {
    pub scene: String,
}

/// Where the surface is, in its window's physical pixels. Written from UI
/// layout; tests set it directly.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct IcedSurfaceGeometry {
    pub size: UVec2,
    /// UI target scale (window scale x `UiScale`): the surface's own pixels.
    pub scale: f32,
    /// The camera's target scale, which is what pointer positions are in.
    /// A host that pre-multiplies `UiScale` into pointer positions (comp's
    /// native shell does) would otherwise have it counted twice.
    pub pointer_scale: f32,
    pub origin: Vec2,
    /// The window the surface is shown in; pointer events from other windows
    /// are in another coordinate space.
    pub window: Option<Entity>,
}

#[derive(Component)]
pub(crate) struct SurfaceState {
    view: Entity,
    image: Option<Handle<Image>>,
    /// Allocated texture size; the surface uses its top-left `size`.
    texture: UVec2,
    buffer: Vec<u8>,
    size: UVec2,
    scale: f32,
    events: Vec<SurfaceEvent>,
    hovered: bool,
    repaint: bool,
    /// Times the render world gave up on this surface's texture without the
    /// geometry changing. Repaint cycles do not clear it.
    giveups: u32,
    last: Processed,
}

/// Builds the renderer for a newly mounted scene.
pub struct SceneIcedFactory(pub RendererFactory);

pub type RendererFactory = Box<dyn Fn(&ResolvedScene) -> Box<dyn SurfaceRenderer>>;

impl Default for SceneIcedFactory {
    fn default() -> Self {
        Self(Box::new(|_| Box::new(StandIn::default())))
    }
}

#[derive(Default)]
pub(crate) struct Renderers(HashMap<Entity, Box<dyn SurfaceRenderer>>);

struct Mount {
    page: Entity,
    edge: Edge,
    tree: ResolvedScene,
    revision: u64,
    registered: bool,
}

#[derive(Resource, Default)]
pub(crate) struct Mounts(BTreeMap<String, Mount>);

/// The next time any surface needs an update without input, in the
/// `Time<Real>::elapsed()` domain. Hosts turn this into a timer wake.
#[derive(Resource, Default, Debug, PartialEq, Eq)]
pub struct SceneIcedWake(pub Option<Duration>);

/// What a host does with a pending wake: the layer host folds it into its
/// one-shot deadline, comp arms its own. Called every update while a wake is
/// pending, so a host that consumes its deadline re-arms on the next one.
pub type SceneIcedWakeHook = Box<dyn Fn(&mut World, Duration) + Send + Sync>;

#[derive(Resource, Default)]
pub struct SceneIcedWaker(pub Option<SceneIcedWakeHook>);

impl SceneIcedWaker {
    pub fn set(&mut self, hook: impl Fn(&mut World, Duration) + Send + Sync + 'static) {
        self.0 = Some(Box::new(hook));
    }
}

/// Hands a pending wake to the host hook. Exclusive so the hook can reach
/// whatever the host keeps its deadline in.
pub(crate) fn apply_wake(world: &mut World) {
    let Some(at) = world.resource::<SceneIcedWake>().0 else {
        return;
    };
    let Some(waker) = world.remove_resource::<SceneIcedWaker>() else {
        return;
    };
    if let Some(hook) = &waker.0 {
        hook(world, at);
    }
    world.insert_resource(waker);
}

/// What the input method should do for the focused surface.
#[derive(Clone, Debug, PartialEq)]
pub struct ImeOutput {
    pub purpose: ImePurpose,
    pub window_scale: f32,
    /// The caret in window-logical coordinates, which is what
    /// text-input-v3's `set_cursor_rectangle` takes for a layer surface.
    pub cursor: bevy::math::Rect,
    /// The same caret in the window's physical pixels, for a host whose
    /// input method wants another space: dividing by
    /// `IcedSurfaceGeometry::pointer_scale` gives output-logical
    /// coordinates (what comp's native IME takes), and by `window_scale`
    /// gives `cursor` back.
    pub cursor_physical: bevy::math::Rect,
}

/// Keyboard focus and IME state of the iced surfaces.
///
/// `owner` is the surface holding Bevy's `InputFocus`. Keyboard input and
/// `ExternalImeEvent`s are routed to it only. `ime` is its current request
/// (`None` means disabled); the same request is published on the surface as
/// `ExternalImeTarget`, which the layer host's text-input-v3 bridge serves.
#[derive(Resource, Default, Debug)]
pub struct SceneIcedFocus {
    pub owner: Option<Entity>,
    pub scene: Option<String>,
    pub ime: Option<ImeOutput>,
    /// The cursor shape asked for by the hovered surface; it is published as
    /// `CursorShapeRequest` with that surface as owner.
    pub cursor: Option<CursorIcon>,
    cursor_owner: Option<Entity>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCounters {
    /// `AssetEvent<Image>` of any image in the app.
    pub image_added: u64,
    pub image_modified: u64,
    /// The same, for surface textures only.
    pub own_added: u64,
    pub own_modified: u64,
    pub allocations: u64,
    /// Surface size changes served by the existing texture.
    pub resizes: u64,
    pub draws: u64,
    pub rects_queued: u64,
    pub bytes_queued: u64,
    /// Written by the render world (the previous frame's uploads).
    pub rects_written: u64,
    pub bytes_written: u64,
}

impl FrameCounters {
    fn add(&mut self, other: &Self) {
        self.image_added += other.image_added;
        self.image_modified += other.image_modified;
        self.own_added += other.own_added;
        self.own_modified += other.own_modified;
        self.allocations += other.allocations;
        self.resizes += other.resizes;
        self.draws += other.draws;
        self.rects_queued += other.rects_queued;
        self.bytes_queued += other.bytes_queued;
        self.rects_written += other.rects_written;
        self.bytes_written += other.bytes_written;
    }

    fn json(&self) -> Value {
        json!({
            "image_added": self.image_added,
            "image_modified": self.image_modified,
            "own_added": self.own_added,
            "own_modified": self.own_modified,
            "allocations": self.allocations,
            "resizes": self.resizes,
            "draws": self.draws,
            "rects_queued": self.rects_queued,
            "bytes_queued": self.bytes_queued,
            "rects_written": self.rects_written,
            "bytes_written": self.bytes_written,
        })
    }
}

#[derive(Resource, Default)]
pub struct SceneIcedCounters {
    pub frames: u64,
    pub current: FrameCounters,
    pub last: FrameCounters,
    pub totals: FrameCounters,
    /// The rectangles of the most recent upload plan.
    pub last_rects: Vec<Rect>,
    pub surfaces: usize,
    own: HashSet<AssetId<Image>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneIcedStats {
    pub frames: u64,
    pub last: FrameCounters,
    pub totals: FrameCounters,
    pub surfaces: usize,
    pub focus: Option<String>,
}

impl SceneIcedStats {
    pub fn json(&self) -> Value {
        json!({
            "frames": self.frames,
            "surfaces": self.surfaces,
            "focus": self.focus,
            "last": self.last.json(),
            "totals": self.totals.json(),
        })
    }
}

impl SceneIcedCounters {
    pub fn snapshot(&self, focus: &SceneIcedFocus) -> SceneIcedStats {
        SceneIcedStats {
            frames: self.frames,
            last: self.last,
            totals: self.totals,
            surfaces: self.surfaces,
            focus: focus.scene.clone(),
        }
    }
}

const TRACE_EVERY: u64 = 120;

pub(crate) fn roll_counters(
    mut counters: ResMut<SceneIcedCounters>,
    channel: Option<Res<GpuChannel>>,
    trace: Local<Trace>,
    focus: Res<SceneIcedFocus>,
) {
    let mut done = std::mem::take(&mut counters.current);
    if let Some(channel) = channel {
        done.bytes_written = channel.0.bytes_written.swap(0, Ordering::Relaxed);
        done.rects_written = channel.0.rects_written.swap(0, Ordering::Relaxed);
    }
    counters.totals.add(&done);
    counters.last = done;
    counters.frames += 1;
    if trace.0 && counters.frames.is_multiple_of(TRACE_EVERY) {
        info!("SCENE_ICED_STATS {}", counters.snapshot(&focus).json());
    }
}

pub(crate) struct Trace(bool);

impl Default for Trace {
    fn default() -> Self {
        Self(std::env::var_os("COSMIX_SCENE_ICED_TRACE").is_some_and(|v| v == "1"))
    }
}

pub(crate) fn count_asset_events(
    mut events: MessageReader<AssetEvent<Image>>,
    mut counters: ResMut<SceneIcedCounters>,
) {
    for event in events.read() {
        match event {
            AssetEvent::Added { id } => {
                counters.current.image_added += 1;
                if counters.own.contains(id) {
                    counters.current.own_added += 1;
                }
            }
            AssetEvent::Modified { id } => {
                counters.current.image_modified += 1;
                if counters.own.contains(id) {
                    counters.current.own_modified += 1;
                }
            }
            AssetEvent::Removed { id } => {
                counters.own.remove(id);
            }
            _ => {}
        }
    }
}

pub(crate) fn reconcile(world: &mut World) {
    world.resource_scope(|world, mut mounts: Mut<Mounts>| {
        let (gone, changed): (Vec<String>, Vec<(ResolvedScene, u64)>) = {
            let store = world.resource::<SceneStore>();
            let live: HashMap<&str, u64> = store
                .adapter_scenes(ADAPTER)
                .map(|(tree, revision)| (tree.name.as_str(), revision))
                .collect();
            let gone = mounts
                .0
                .keys()
                .filter(|name| !live.contains_key(name.as_str()))
                .cloned()
                .collect();
            // Trees are cloned only when their revision moved; idle frames copy nothing.
            let changed = store
                .adapter_scenes(ADAPTER)
                .filter(|(tree, revision)| {
                    mounts
                        .0
                        .get(&tree.name)
                        .is_none_or(|mount| mount.revision != *revision)
                })
                .map(|(tree, revision)| (tree.clone(), revision))
                .collect();
            (gone, changed)
        };
        for name in gone {
            let mount = mounts.0.remove(&name).unwrap();
            unmount(world, &mount);
            world.non_send_mut::<Renderers>().0.remove(&mount.page);
            if world.get_entity(mount.page).is_ok() {
                world.despawn(mount.page);
            }
        }
        for (tree, revision) in changed {
            if let Some(mount) = mounts.0.get_mut(&tree.name) {
                let edge = scene_edge(&tree);
                if edge != mount.edge {
                    world.entity_mut(mount.page).remove::<ChildOf>();
                    unmount(world, mount);
                    mount.edge = edge;
                    mount.registered = false;
                }
                if let Some(renderer) = world.non_send_mut::<Renderers>().0.get_mut(&mount.page) {
                    renderer.set_scene(&tree);
                }
                mount.tree = tree;
                mount.revision = revision;
                continue;
            }
            let page = spawn_surface(world, &tree.name);
            let mut renderer = (world.non_send::<SceneIcedFactory>().0)(&tree);
            renderer.set_scene(&tree);
            world.non_send_mut::<Renderers>().0.insert(page, renderer);
            mounts.0.insert(
                tree.name.clone(),
                Mount {
                    page,
                    edge: scene_edge(&tree),
                    tree,
                    revision,
                    registered: false,
                },
            );
        }
    });
}

/// Runs after the CTK pass: `mount_page` reports an existing page id as
/// mounted without attaching new content, so a CTK wrapper for the same scene
/// must be gone before this registers (and `reconcile` releases ours before
/// CTK registers).
pub(crate) fn register(world: &mut World) {
    world.resource_scope(|world, mut mounts: Mut<Mounts>| {
        for mount in mounts.0.values_mut().filter(|mount| !mount.registered) {
            mount.registered = register_scene_page(world, &mount.tree, mount.page);
        }
    });
}

fn unmount(world: &mut World, mount: &Mount) {
    if mount.registered {
        cosmix_shell::chrome::unmount_page(world, mount.edge, &scene_page_id(&mount.tree));
    }
}

pub(crate) fn spawn_surface(world: &mut World, scene: &str) -> Entity {
    let view = world
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                ..default()
            },
            ImageNode {
                image_mode: NodeImageMode::Stretch,
                ..default()
            },
            Visibility::Hidden,
            Pickable::IGNORE,
        ))
        .id();
    let mut page = world.spawn((
        Node {
            width: percent(100),
            height: percent(100),
            min_width: px(0),
            min_height: px(0),
            ..default()
        },
        IcedSurface {
            scene: scene.to_owned(),
        },
        IcedSurfaceGeometry::default(),
        ExternalImeTarget::default(),
        SurfaceState {
            view,
            image: None,
            texture: UVec2::ZERO,
            buffer: Vec::new(),
            size: UVec2::ZERO,
            scale: 0.0,
            events: Vec::new(),
            hovered: false,
            repaint: false,
            giveups: 0,
            last: Processed::default(),
        },
    ));
    page.add_child(view);
    page.id()
}

/// The surface's own scale and the scale pointer positions arrive in.
///
/// UI layout works in window scale x `UiScale` (`bevy_ui`'s
/// `ui_layout_system`), which is what the surface renders at. Bevy's UI
/// picking backend converts pointer positions with the camera's target scale
/// alone, so that is what the bridge must use for hit coordinates: with
/// `UiScale` 1 they are equal, and where they differ the camera's is right.
pub fn scales(target_scale: f32, camera_scale: Option<f32>) -> (f32, f32) {
    (target_scale, camera_scale.unwrap_or(target_scale))
}

/// Surface placement from UI layout. The window is resolved only for cameras
/// targeting `WindowRef::Entity`, as Quoin's panels do; a primary-window
/// camera leaves `window` unset and disables the capture window check.
pub(crate) fn geometry(
    mut surfaces: Query<
        (
            &ComputedNode,
            &UiGlobalTransform,
            &ComputedUiRenderTargetInfo,
            &ComputedUiTargetCamera,
            &mut IcedSurfaceGeometry,
        ),
        With<IcedSurface>,
    >,
    cameras: Query<(&RenderTarget, &Camera)>,
) {
    for (node, transform, target, camera, mut geometry) in &mut surfaces {
        let camera = camera.get().and_then(|camera| cameras.get(camera).ok());
        let window = camera.and_then(|(target, _)| match target {
            RenderTarget::Window(WindowRef::Entity(window)) => Some(*window),
            _ => None,
        });
        let (scale, pointer_scale) = scales(
            target.scale_factor(),
            camera.and_then(|(_, camera)| camera.target_scaling_factor()),
        );
        let size = node.size();
        if size.x < 1.0 || size.y < 1.0 {
            // Not laid out (or collapsed): keep the last texture.
            continue;
        }
        geometry.set_if_neq(IcedSurfaceGeometry {
            size: size.round().as_uvec2(),
            scale,
            pointer_scale,
            origin: transform.affine().translation - size / 2.0,
            window,
        });
    }
}

#[derive(Default)]
pub(crate) struct PointerRoute {
    capture: HashMap<PointerId, Entity>,
    over: HashMap<PointerId, Entity>,
}

pub(crate) fn route_pointer(
    mut inputs: MessageReader<PointerInput>,
    hover: Option<Res<HoverMap>>,
    mut surfaces: Query<(&IcedSurfaceGeometry, &mut SurfaceState)>,
    mut focus: Option<ResMut<InputFocus>>,
    mut route: Local<PointerRoute>,
) {
    for input in inputs.read() {
        let pointer = input.pointer_id;
        let hovered = hover
            .as_ref()
            .and_then(|map| map.get(&pointer))
            .and_then(|hits| hits.keys().copied().find(|e| surfaces.contains(*e)));
        let captured = route.capture.get(&pointer).copied();
        let event_window = match &input.location.target {
            NormalizedRenderTarget::Window(window) => Some(window.entity()),
            _ => None,
        };
        let foreign = captured.filter(|entity| {
            surfaces.get(*entity).is_ok_and(|(geometry, _)| {
                geometry
                    .window
                    .is_some_and(|window| event_window != Some(window))
            })
        });
        if let Some(entity) = foreign {
            // A captured pointer now reports positions in another window's
            // space: leave, and release the button without a position.
            match input.action {
                PointerAction::Move { .. } | PointerAction::Scroll { .. } => {
                    if surfaces.get(entity).is_ok_and(|(_, state)| state.hovered) {
                        route.over.remove(&pointer);
                        push(
                            &mut surfaces,
                            entity,
                            SurfaceEvent::PointerLeft,
                            Some(false),
                        );
                    }
                    continue;
                }
                PointerAction::Release(button) => {
                    if let Some(button) = convert_button(button) {
                        let event = SurfaceEvent::PointerButton {
                            button,
                            pressed: false,
                        };
                        push(&mut surfaces, entity, event, None);
                    }
                    if surfaces.get(entity).is_ok_and(|(_, state)| state.hovered) {
                        push(
                            &mut surfaces,
                            entity,
                            SurfaceEvent::PointerLeft,
                            Some(false),
                        );
                    }
                    route.over.remove(&pointer);
                    route.capture.remove(&pointer);
                    continue;
                }
                PointerAction::Press(_) | PointerAction::Cancel => {}
            }
        }
        let captured = captured.filter(|entity| Some(*entity) != foreign);
        let target = captured.or(hovered);
        if let Some(previous) = route.over.get(&pointer).copied()
            && Some(previous) != hovered
            && Some(previous) != captured
        {
            route.over.remove(&pointer);
            push(
                &mut surfaces,
                previous,
                SurfaceEvent::PointerLeft,
                Some(false),
            );
        }
        if let Some(entity) = hovered {
            route.over.insert(pointer, entity);
        }
        let local = |surfaces: &Query<(&IcedSurfaceGeometry, &mut SurfaceState)>, entity| {
            surfaces.get(entity).ok().map(|(geometry, _)| {
                let at = input.location.position * geometry.pointer_scale - geometry.origin;
                SurfaceEvent::PointerMoved { x: at.x, y: at.y }
            })
        };
        match input.action {
            PointerAction::Move { .. } => {
                if let Some(entity) = target
                    && let Some(event) = local(&surfaces, entity)
                {
                    push(&mut surfaces, entity, event, hovered.map(|h| h == entity));
                }
            }
            PointerAction::Press(button) => {
                if let Some(entity) = hovered {
                    route.capture.insert(pointer, entity);
                    if let Some(event) = local(&surfaces, entity) {
                        push(&mut surfaces, entity, event, Some(true));
                    }
                    if let Some(button) = convert_button(button) {
                        let event = SurfaceEvent::PointerButton {
                            button,
                            pressed: true,
                        };
                        push(&mut surfaces, entity, event, None);
                    }
                    if let Some(focus) = focus.as_mut()
                        && focus.get() != Some(entity)
                    {
                        focus.set(entity, FocusCause::Pressed);
                    }
                } else if let Some(focus) = focus.as_mut()
                    && focus.get().is_some_and(|owner| surfaces.contains(owner))
                {
                    focus.clear();
                }
            }
            PointerAction::Release(button) => {
                if let Some(entity) = target
                    && let Some(button) = convert_button(button)
                {
                    let event = SurfaceEvent::PointerButton {
                        button,
                        pressed: false,
                    };
                    push(&mut surfaces, entity, event, None);
                    if hovered != Some(entity) {
                        // Released outside: this is the leave, so the next
                        // motion must not produce another one.
                        route.over.remove(&pointer);
                        push(
                            &mut surfaces,
                            entity,
                            SurfaceEvent::PointerLeft,
                            Some(false),
                        );
                    }
                }
                route.capture.remove(&pointer);
            }
            PointerAction::Scroll { unit, x, y, .. } => {
                if let Some(entity) = target {
                    let unit = match unit {
                        MouseScrollUnit::Line => ScrollUnit::Line,
                        MouseScrollUnit::Pixel => ScrollUnit::Pixel,
                    };
                    push(
                        &mut surfaces,
                        entity,
                        SurfaceEvent::Scroll { unit, x, y },
                        None,
                    );
                }
            }
            PointerAction::Cancel => {
                if let Some(entity) = target {
                    push(
                        &mut surfaces,
                        entity,
                        SurfaceEvent::PointerLeft,
                        Some(false),
                    );
                }
                route.capture.remove(&pointer);
                route.over.remove(&pointer);
            }
        }
    }
}

fn push(
    surfaces: &mut Query<(&IcedSurfaceGeometry, &mut SurfaceState)>,
    entity: Entity,
    event: SurfaceEvent,
    hovered: Option<bool>,
) {
    if let Ok((_, mut state)) = surfaces.get_mut(entity) {
        state.events.push(event);
        if let Some(hovered) = hovered {
            state.hovered = hovered;
        }
    }
}

fn convert_button(button: BevyButton) -> Option<PointerButton> {
    match button {
        BevyButton::Primary => Some(PointerButton::Primary),
        BevyButton::Secondary => Some(PointerButton::Secondary),
        BevyButton::Middle => Some(PointerButton::Middle),
    }
}

/// Copies keyboard input to the focused surface; it does not consume it.
/// Every other `KeyboardInput` reader still sees each key: `ButtonInput<KeyCode>`,
/// Quoin's and the layer host's own handlers, and any global shortcut system.
/// CTK text fields act only on keys while they hold `InputFocus`, which they
/// cannot while a surface does, so text never reaches both.
pub(crate) fn route_keyboard(
    mut keys: MessageReader<KeyboardInput>,
    mut ime: MessageReader<ExternalImeEvent>,
    input_focus: Option<Res<InputFocus>>,
    buttons: Option<Res<ButtonInput<KeyCode>>>,
    mut surfaces: Query<(&IcedSurface, &mut SurfaceState)>,
    mut focus: ResMut<SceneIcedFocus>,
) {
    let current = input_focus
        .and_then(|focus| focus.get())
        .filter(|entity| surfaces.contains(*entity));
    if current != focus.owner {
        if let Some(old) = focus.owner
            && let Ok((_, mut state)) = surfaces.get_mut(old)
        {
            state.events.push(SurfaceEvent::Focus(false));
        }
        focus.scene = None;
        if let Some(new) = current
            && let Ok((surface, mut state)) = surfaces.get_mut(new)
        {
            state.events.push(SurfaceEvent::Focus(true));
            focus.scene = Some(surface.scene.clone());
        }
        focus.owner = current;
        focus.ime = None;
    }
    let owner = focus.owner;
    let Some((_, mut state)) = owner.and_then(|owner| surfaces.get_mut(owner).ok()) else {
        keys.clear();
        ime.clear();
        return;
    };
    let modifiers = buttons.map_or_else(Modifiers::default, |b| Modifiers {
        shift: b.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        control: b.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
        alt: b.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]),
        logo: b.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]),
    });
    for input in keys.read() {
        state.events.push(SurfaceEvent::Key {
            key: convert_key(&input.logical_key),
            latin: latin(input.key_code),
            text: input.text.as_ref().map(ToString::to_string),
            pressed: input.state == ButtonState::Pressed,
            repeat: input.repeat,
            modifiers,
        });
    }
    for event in ime.read().filter(|event| Some(event.target) == owner) {
        let event = match &event.kind {
            ExternalImeKind::Preedit { text, cursor } => ImeEvent::Preedit {
                text: text.clone(),
                cursor: *cursor,
            },
            ExternalImeKind::Commit(text) => ImeEvent::Commit(text.clone()),
            ExternalImeKind::Disabled => ImeEvent::Disabled,
            // Focus already told the renderer; iced 0.14 has no deletion of
            // surrounding text, and no surrounding text is ever sent.
            ExternalImeKind::Enabled | ExternalImeKind::DeleteSurrounding { .. } => continue,
        };
        state.events.push(SurfaceEvent::Ime(event));
    }
}

pub(crate) fn convert_key(key: &BevyKey) -> Key {
    let named = match key {
        BevyKey::Character(text) => return Key::Character(text.to_string()),
        BevyKey::Enter => NamedKey::Enter,
        BevyKey::Tab => NamedKey::Tab,
        BevyKey::Space => NamedKey::Space,
        BevyKey::Backspace => NamedKey::Backspace,
        BevyKey::Delete => NamedKey::Delete,
        BevyKey::Escape => NamedKey::Escape,
        BevyKey::ArrowLeft => NamedKey::ArrowLeft,
        BevyKey::ArrowRight => NamedKey::ArrowRight,
        BevyKey::ArrowUp => NamedKey::ArrowUp,
        BevyKey::ArrowDown => NamedKey::ArrowDown,
        BevyKey::Home => NamedKey::Home,
        BevyKey::End => NamedKey::End,
        BevyKey::PageUp => NamedKey::PageUp,
        BevyKey::PageDown => NamedKey::PageDown,
        BevyKey::Shift => NamedKey::Shift,
        BevyKey::Control => NamedKey::Control,
        BevyKey::Alt => NamedKey::Alt,
        BevyKey::Super | BevyKey::Meta => NamedKey::Super,
        _ => return Key::Unidentified,
    };
    Key::Named(named)
}

fn latin(code: KeyCode) -> Option<char> {
    const LETTERS: [KeyCode; 26] = [
        KeyCode::KeyA,
        KeyCode::KeyB,
        KeyCode::KeyC,
        KeyCode::KeyD,
        KeyCode::KeyE,
        KeyCode::KeyF,
        KeyCode::KeyG,
        KeyCode::KeyH,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::KeyM,
        KeyCode::KeyN,
        KeyCode::KeyO,
        KeyCode::KeyP,
        KeyCode::KeyQ,
        KeyCode::KeyR,
        KeyCode::KeyS,
        KeyCode::KeyT,
        KeyCode::KeyU,
        KeyCode::KeyV,
        KeyCode::KeyW,
        KeyCode::KeyX,
        KeyCode::KeyY,
        KeyCode::KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    if let Some(i) = LETTERS.iter().position(|k| *k == code) {
        return Some((b'a' + i as u8) as char);
    }
    DIGITS
        .iter()
        .position(|k| *k == code)
        .map(|i| (b'0' + i as u8) as char)
}

/// Texture side lengths grow in steps of this many pixels.
pub const BUCKET: u32 = 128;

/// The texture size to allocate for a surface of `size`, or `None` when the
/// current texture (`current`) still serves: it must be at least as large,
/// and less than four times the bucketed need in each dimension, so a size
/// wobbling across a bucket edge does not reallocate back and forth.
pub fn texture_size(size: UVec2, current: Option<UVec2>) -> Option<UVec2> {
    let bucket = |v: u32| v.div_ceil(BUCKET).max(1) * BUCKET;
    let want = UVec2::new(bucket(size.x), bucket(size.y));
    match current {
        Some(tex)
            if size.x <= tex.x && size.y <= tex.y && want.x * 4 > tex.x && want.y * 4 > tex.y =>
        {
            None
        }
        _ => Some(want),
    }
}

fn cursor_shape(icon: CursorIcon) -> CursorShape {
    match icon {
        CursorIcon::Default => CursorShape::Default,
        CursorIcon::Pointer => CursorShape::Pointer,
        CursorIcon::Text => CursorShape::Text,
        CursorIcon::Grab => CursorShape::Grab,
        CursorIcon::Grabbing => CursorShape::Grabbing,
        CursorIcon::NotAllowed => CursorShape::NotAllowed,
        CursorIcon::ResizeHorizontal => CursorShape::EwResize,
        CursorIcon::ResizeVertical => CursorShape::NsResize,
    }
}

/// Frames the host is kept awake for staged uploads before the wait is
/// abandoned: twice the render world's own `MAX_WAIT_FRAMES`. It bounds one
/// stretch of waiting; [`MAX_GIVEUPS`] bounds the repaint cycles themselves.
pub const MAX_KEEP_AWAKE_FRAMES: u32 = 240;

/// Times a surface's texture may be given up on before the surface stops
/// uploading. Each cycle costs `MAX_WAIT_FRAMES` in the render world, so a
/// texture that never prepares stops the traffic in a few seconds instead of
/// re-arming for ever. A geometry change clears the count.
pub const MAX_GIVEUPS: u32 = 3;

#[allow(clippy::too_many_arguments)]
pub(crate) fn frame(
    mut renderers: NonSendMut<Renderers>,
    mut surfaces: Query<(
        Entity,
        &IcedSurfaceGeometry,
        &mut SurfaceState,
        &mut ExternalImeTarget,
    )>,
    mut views: Query<(&mut ImageNode, &mut Visibility)>,
    mut images: ResMut<Assets<Image>>,
    mut counters: ResMut<SceneIcedCounters>,
    mut focus: ResMut<SceneIcedFocus>,
    mut wake: ResMut<SceneIcedWake>,
    mut cursor_request: ResMut<CursorShapeRequest>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut keeping_awake: Local<u32>,
    channel: Option<Res<GpuChannel>>,
    time: Res<Time<Real>>,
) {
    let now = time.elapsed();
    // `waiting` is set by the render world with a Relaxed store, so this read
    // is one frame stale under pipelined rendering; it only gates a redraw
    // request, never correctness.
    if channel
        .as_ref()
        .is_some_and(|channel| channel.0.waiting.load(Ordering::Relaxed))
    {
        // Uploads are staged for a texture Bevy has not prepared yet; an
        // idle host would otherwise never render them. A texture that never
        // prepares (device loss) must not keep the desktop at full frame
        // rate for ever: give up after twice the render world's own wait,
        // leaving the surface blank until something else wakes the host.
        *keeping_awake += 1;
        if *keeping_awake <= MAX_KEEP_AWAKE_FRAMES {
            redraw.write(RequestRedraw);
        } else if *keeping_awake == MAX_KEEP_AWAKE_FRAMES + 1 {
            warn!("scene-iced: a surface texture never prepared; stopped waking the host");
        }
    } else {
        *keeping_awake = 0;
    }
    let repaints = channel
        .as_ref()
        .map(|channel| channel.0.take_repaints())
        .unwrap_or_default();
    let mut next_wake: Option<Duration> = None;
    let mut cursor = None;
    counters.surfaces = surfaces.iter().count();
    for (entity, geometry, mut state, mut ime_target) in &mut surfaces {
        if state
            .image
            .as_ref()
            .is_some_and(|image| repaints.contains(&image.id()))
        {
            // Consumed from the channel now; kept here until the surface
            // next draws.
            state.size = UVec2::ZERO;
            state.giveups += 1;
            if state.giveups == MAX_GIVEUPS {
                warn!(
                    "scene-iced: a surface texture was abandoned {} times; it stays blank until \
                     its geometry changes",
                    state.giveups
                );
            }
        }
        let size = geometry.size;
        if state.size != UVec2::ZERO && (state.size != size || state.scale != geometry.scale) {
            // Real geometry movement, not a repaint cycle: start counting again.
            state.giveups = 0;
        }
        let renderer = renderers.0.get_mut(&entity);
        let Some(renderer) = renderer.filter(|_| size.x > 0 && size.y > 0 && geometry.scale > 0.0)
        else {
            // A surface that cannot draw cannot show a caret either.
            ime_target.set_if_neq(ExternalImeTarget::default());
            if focus.owner == Some(entity) {
                focus.ime = None;
            }
            continue;
        };
        let current = state.image.as_ref().map(|_| state.texture);
        if let Some(texture) = texture_size(size, current) {
            let mut image = Image::new_uninit(
                Extent3d {
                    width: texture.x,
                    height: texture.y,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::RENDER_WORLD,
            );
            // The visible part maps 1:1 onto physical pixels.
            image.sampler = ImageSampler::nearest();
            let handle = images.add(image);
            counters.own.insert(handle.id());
            counters.current.allocations += 1;
            if let Ok((mut node, mut visibility)) = views.get_mut(state.view) {
                node.image = handle.clone();
                *visibility = Visibility::Inherited;
            }
            state.image = Some(handle);
            state.buffer = vec![0; texture.x as usize * texture.y as usize * 4];
            state.texture = texture;
            state.size = UVec2::ZERO;
        }
        if state.size != size || state.scale != geometry.scale {
            if let Ok((mut node, _)) = views.get_mut(state.view) {
                node.rect = Some(bevy::math::Rect::new(
                    0.0,
                    0.0,
                    size.x as f32,
                    size.y as f32,
                ));
            }
            if state.size != UVec2::ZERO {
                counters.current.resizes += 1;
            }
            state.size = size;
            state.scale = geometry.scale;
            state.repaint = true;
            renderer.resize(size.x, size.y, geometry.scale);
            renderer.set_pointer_scale(geometry.pointer_scale);
        }
        for event in std::mem::take(&mut state.events) {
            renderer.queue(event);
        }
        let processed = renderer.process(now);
        if let Some(at) = processed.wake_at {
            next_wake = Some(next_wake.map_or(at, |next| next.min(at)));
        }
        if state.hovered {
            cursor = Some((entity, processed.cursor));
        }
        let owner = focus.owner == Some(entity);
        let request = match (&processed.ime, owner) {
            (ImeRequest::Enabled { cursor, purpose }, true) => {
                let at = geometry.origin + Vec2::new(cursor.x as f32, cursor.y as f32);
                let size = Vec2::new(cursor.w as f32, cursor.h as f32);
                let min = at / geometry.scale;
                let extent = size / geometry.scale;
                Some(ImeOutput {
                    purpose: match purpose {
                        crate::surface::ImePurpose::Normal => ImePurpose::Normal,
                        crate::surface::ImePurpose::Secure => ImePurpose::Password,
                        crate::surface::ImePurpose::Terminal => ImePurpose::Terminal,
                    },
                    window_scale: geometry.scale,
                    cursor: bevy::math::Rect::from_corners(min, min + extent),
                    cursor_physical: bevy::math::Rect::from_corners(at, at + size),
                })
            }
            _ => None,
        };
        ime_target.set_if_neq(ExternalImeTarget {
            enabled: request.is_some(),
            purpose: request
                .as_ref()
                .map_or(ImePurpose::Normal, |ime| ime.purpose),
            cursor: request.as_ref().map(|ime| ime.cursor),
        });
        if owner {
            focus.ime = request;
        }
        let redraw = processed.needs_redraw || state.repaint;
        state.last = processed;
        if !redraw {
            continue;
        }
        let state = &mut *state;
        let stride = state.texture.x * 4;
        let damage = renderer.draw(&mut state.buffer, size.x, size.y, stride);
        let damage = if state.repaint {
            vec![Rect::new(0, 0, size.x, size.y)]
        } else {
            damage
        };
        state.repaint = false;
        let rects = upload::plan(&damage, size.x, size.y);
        counters.current.draws += 1;
        counters.current.rects_queued += rects.len() as u64;
        counters.current.bytes_queued += upload::byte_len(&rects);
        if let Some(channel) = channel.as_ref()
            && !rects.is_empty()
            && state.giveups < MAX_GIVEUPS
        {
            channel.0.push(SurfaceUpload {
                image: state.image.as_ref().unwrap().id(),
                texture: state.texture,
                visible: size,
                ops: rects
                    .iter()
                    .map(|rect| upload::extract(&state.buffer, stride, *rect))
                    .collect(),
            });
        }
        counters.last_rects = rects;
    }
    let owner = cursor.map(|(entity, _)| entity);
    let cursor = cursor.map(|(_, icon)| icon);
    if (focus.cursor, focus.cursor_owner) != (cursor, owner) {
        match (owner, cursor) {
            (Some(owner), Some(icon)) => {
                let mut request = *cursor_request;
                request.set(owner, cursor_shape(icon));
                cursor_request.set_if_neq(request);
            }
            // Leaving every surface releases only our own request.
            _ => {
                if let Some(previous) = focus.cursor_owner {
                    let mut request = *cursor_request;
                    request.clear(previous);
                    cursor_request.set_if_neq(request);
                }
            }
        }
        focus.cursor = cursor;
        focus.cursor_owner = owner;
    }
    wake.set_if_neq(SceneIcedWake(next_wake));
}
