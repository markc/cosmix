use std::time::Duration;

use bevy::asset::AssetPlugin;
use bevy::camera::NormalizedRenderTarget;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyCode};
use bevy::input_focus::FocusCause;
use bevy::picking::backend::HitData;
use bevy::picking::hover::HoverMap;
use bevy::picking::pointer::{Location, PointerAction, PointerButton, PointerId};
use bevy::time::TimeUpdateStrategy;
use cosmix_shell::runtime::SceneVerb;
use serde_json::{Value, json};

use super::*;
use crate::surface::Rect;

const SCENE: &str = "---\nscene: 1\nname: probe\ncitizen: test\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"left\", w: 200}\n```\n";

struct Harness {
    app: App,
    surface: Entity,
    _peer: ctk::bus::TestBusPeer,
    bridge: ctk::bus::BusBridge,
    window: Option<Entity>,
}

impl Harness {
    fn new() -> Self {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .add_plugins(SceneIcedPlugin)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                16,
            )));
        let (bridge, peer) = ctk::bus::test_bridge("test");
        let mut harness = Self {
            app,
            surface: Entity::PLACEHOLDER,
            _peer: peer,
            bridge,
            window: None,
        };
        harness.request(SceneVerb::Load, SCENE, json!({"adapter": ADAPTER}));
        harness.app.update();
        let mut query = harness
            .app
            .world_mut()
            .query_filtered::<Entity, With<IcedSurface>>();
        harness.surface = query.single(harness.app.world()).unwrap();
        harness.geometry(200, 100, 1.5);
        harness
    }

    fn request(&mut self, verb: SceneVerb, body: &str, args: Value) {
        let (rc, reply) = self.app.world_mut().resource_mut::<SceneStore>().dispatch(
            verb,
            body,
            &args,
            &self.bridge,
        );
        assert_eq!(rc, 0, "{reply}");
    }

    fn geometry(&mut self, width: u32, height: u32, scale: f32) {
        self.app
            .world_mut()
            .entity_mut(self.surface)
            .insert(IcedSurfaceGeometry {
                size: UVec2::new(width, height),
                scale,
                origin: Vec2::new(10.0, 20.0),
                window: self.window,
            });
    }

    fn run(&mut self, frames: usize) {
        for _ in 0..frames {
            self.app.update();
        }
    }

    fn totals(&self) -> FrameCounters {
        self.app.world().resource::<SceneIcedCounters>().totals
    }

    fn pointer(&mut self, over: bool, position: Vec2, action: PointerAction) {
        self.pointer_in(None, over, position, action);
    }

    fn pointer_in(
        &mut self,
        window: Option<Entity>,
        over: bool,
        position: Vec2,
        action: PointerAction,
    ) {
        let camera = Entity::PLACEHOLDER;
        let mut hover = HoverMap::default();
        let mut hits = bevy::ecs::entity::EntityHashMap::default();
        if over {
            hits.insert(self.surface, HitData::new(camera, 0.0, None, None));
        }
        hover.insert(PointerId::Mouse, hits);
        let world = self.app.world_mut();
        world.insert_resource(hover);
        world.write_message(PointerInput::new(
            PointerId::Mouse,
            Location {
                target: match window {
                    Some(window) => NormalizedRenderTarget::Window(
                        bevy::window::WindowRef::Entity(window)
                            .normalize(None)
                            .unwrap(),
                    ),
                    None => NormalizedRenderTarget::None {
                        width: 800,
                        height: 600,
                    },
                },
                position,
            },
            action,
        ));
    }

    fn focus(&self) -> &SceneIcedFocus {
        self.app.world().resource::<SceneIcedFocus>()
    }
}

// Counters are rolled in `First`, so the totals include the previous frame.
#[test]
fn texture_is_allocated_once_and_again_on_resize() {
    let mut h = Harness::new();
    h.run(50);
    assert_eq!(h.totals().allocations, 1);
    assert_eq!(h.totals().own_added, 1);
    h.geometry(300, 100, 1.5);
    h.run(3);
    assert_eq!(h.totals().allocations, 2);
    // A scale change at the same physical size keeps the texture.
    h.geometry(300, 100, 2.0);
    h.run(3);
    assert_eq!(h.totals().allocations, 2);
    assert_eq!(h.totals().resizes, 1);
    assert_eq!(h.totals().own_added, 2);
    assert_eq!(h.totals().own_modified, 0);
    // A full repaint follows every allocation.
    assert_eq!(
        h.app.world().resource::<SceneIcedCounters>().last_rects,
        vec![Rect::new(0, 0, 300, 100)]
    );
}

#[test]
fn idle_frames_modify_no_image_and_upload_nothing() {
    let mut h = Harness::new();
    h.run(3);
    let before = h.totals();
    assert_eq!(before.bytes_queued, 200 * 100 * 4);
    h.run(600);
    let after = h.totals();
    assert_eq!(after.image_modified, 0);
    assert_eq!(after.own_modified, 0);
    assert_eq!(after.allocations, 1);
    assert_eq!(after.bytes_queued, before.bytes_queued);
    assert_eq!(after.draws, before.draws);
    assert_eq!(h.app.world().resource::<SceneIcedWake>().0, None);
}

#[test]
fn one_rect_change_uploads_only_that_rect() {
    let mut h = Harness::new();
    h.run(3);
    let before = h.totals();
    // Logical (40, 60) at 1.5x is physical (60, 90); minus the origin (10, 20).
    h.pointer(
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Move { delta: Vec2::ZERO },
    );
    h.run(2);
    // The stand-in's hover square is 36 px at 1.5x, centred on the pointer.
    let expected = Rect::new(32, 52, 36, 36);
    let counters = h.app.world().resource::<SceneIcedCounters>();
    assert_eq!(counters.last_rects, vec![expected]);
    let after = h.totals();
    assert_eq!(after.bytes_queued - before.bytes_queued, 36 * 36 * 4);
    assert_eq!(after.rects_queued - before.rects_queued, 1);
    assert_eq!(after.allocations, 1);
    assert_eq!(after.own_modified, 0);
}

#[test]
fn focus_hands_off_in_and_out() {
    let mut h = Harness::new();
    h.run(3);
    // Press on the surface takes focus.
    h.pointer(
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Press(PointerButton::Primary),
    );
    h.run(1);
    assert_eq!(
        h.app.world().resource::<InputFocus>().get(),
        Some(h.surface)
    );
    h.run(1);
    assert_eq!(h.focus().owner, Some(h.surface));
    assert_eq!(h.focus().scene.as_deref(), Some("probe"));
    let ime = h.focus().ime.clone().expect("focused surface enables IME");
    // Caret at physical (12, 12) plus origin (10, 20), over 1.5.
    assert_eq!(ime.cursor.min, Vec2::new(22.0 / 1.5, 32.0 / 1.5));
    assert!(h.app.world().resource::<SceneIcedWake>().0.is_some());
    h.pointer(
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Release(PointerButton::Primary),
    );

    // Typing reaches only the owner and moves the caret.
    let before = h.totals();
    h.app.world_mut().write_message(KeyboardInput {
        key_code: KeyCode::KeyA,
        logical_key: Key::Character("a".into()),
        state: ButtonState::Pressed,
        text: Some("a".into()),
        repeat: false,
        window: Entity::PLACEHOLDER,
    });
    h.run(2);
    assert!(h.totals().bytes_queued > before.bytes_queued);

    // The caret blinks while focused, with no input at all.
    let before = h.totals();
    h.run(40);
    assert!(h.totals().draws > before.draws);

    // Focus moving elsewhere releases the surface and stops the blink.
    let other = h.app.world_mut().spawn_empty().id();
    h.app
        .world_mut()
        .resource_mut::<InputFocus>()
        .set(other, FocusCause::Navigated);
    h.run(2);
    assert_eq!(h.focus().owner, None);
    assert_eq!(h.focus().ime, None);
    h.run(2);
    assert_eq!(h.app.world().resource::<SceneIcedWake>().0, None);
    let before = h.totals();
    h.run(100);
    assert_eq!(h.totals().draws, before.draws);

    // Back in by press, then out by a press that hits no surface.
    h.pointer(
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Press(PointerButton::Primary),
    );
    h.run(2);
    assert_eq!(h.focus().owner, Some(h.surface));
    h.pointer(
        false,
        Vec2::new(500.0, 500.0),
        PointerAction::Press(PointerButton::Primary),
    );
    h.run(2);
    assert_eq!(h.app.world().resource::<InputFocus>().get(), None);
    assert_eq!(h.focus().owner, None);
}

#[test]
fn unload_and_adapter_switch_remove_the_surface() {
    let mut h = Harness::new();
    h.run(3);
    h.request(SceneVerb::Unload, "", json!({"scene": "probe"}));
    h.run(1);
    let mut query = h
        .app
        .world_mut()
        .query_filtered::<Entity, With<IcedSurface>>();
    assert_eq!(query.iter(h.app.world()).count(), 0);
    assert!(h.app.world().get_entity(h.surface).is_err());

    h.request(SceneVerb::Load, SCENE, json!({"adapter": ADAPTER}));
    h.run(1);
    assert_eq!(query.iter(h.app.world()).count(), 1);
    h.request(SceneVerb::Load, SCENE, json!({"adapter": "bevy"}));
    h.run(1);
    assert_eq!(query.iter(h.app.world()).count(), 0);
}

// Stands in for the CTK adapter: owns a page with the same id whenever the
// scene is not iced-backed, created and destroyed inside `SceneReconcile`.
#[derive(Resource, Default)]
struct FakeCtkPage(Option<Entity>);

fn fake_ctk(world: &mut World) {
    let iced = world
        .resource::<SceneStore>()
        .adapter_scenes(ADAPTER)
        .any(|(tree, _)| tree.name == "probe");
    let current = world.resource::<FakeCtkPage>().0;
    match (iced, current) {
        (true, Some(page)) => {
            cosmix_shell::chrome::unmount_page(
                world,
                cosmix_shell::core::Edge::Left,
                "scene-probe",
            );
            world.despawn(page);
            world.resource_mut::<FakeCtkPage>().0 = None;
        }
        (false, None) => {
            let page = world.spawn(Node::default()).id();
            assert!(cosmix_shell::chrome::mount_page(
                world,
                cosmix_shell::core::Edge::Left,
                "scene-probe",
                "probe",
                page
            ));
            world.resource_mut::<FakeCtkPage>().0 = Some(page);
        }
        _ => {}
    }
}

fn wrapper_of(world: &World, page: Entity) -> Option<Entity> {
    world.get::<ChildOf>(page).map(ChildOf::parent)
}

#[test]
fn adapter_hand_over_attaches_the_new_page_in_both_directions() {
    use cosmix_shell::chrome::{
        QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts, spawn_quoin_chrome,
    };
    use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
    use cosmix_shell::runtime::{ShellFrameState, ShellRuntimePlugin};
    let model = ShellModel::new(
        OutputKey::new("test").unwrap(),
        LogicalSize::new(800.0, 600.0).unwrap(),
        Duration::ZERO,
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .unwrap();
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Image>()
        .add_plugins(ShellRuntimePlugin::new(model))
        .add_plugins(SceneIcedPlugin)
        .init_resource::<FakeCtkPage>()
        .add_systems(Update, fake_ctk.in_set(cosmix_scene_bevy::SceneReconcile));
    let world = app.world_mut();
    let registry = QuoinPageRegistry::new(vec![], vec![], vec![], vec![]).unwrap();
    let props = registry
        .bind(
            &world.resource::<ShellFrameState>().0,
            QuoinContentBindings::default(),
        )
        .unwrap();
    let mounts = QuoinPanelMounts::new(
        world.spawn_empty().id(),
        world.spawn_empty().id(),
        world.spawn_empty().id(),
        world.spawn_empty().id(),
    );
    let mut queue = bevy::ecs::world::CommandQueue::default();
    spawn_quoin_chrome(&mut Commands::new(&mut queue, world), mounts, props);
    queue.apply(world);
    let (bridge, _peer) = ctk::bus::test_bridge("test");
    let load = |app: &mut App, adapter: &str| {
        let (rc, reply) = app.world_mut().resource_mut::<SceneStore>().dispatch(
            SceneVerb::Load,
            SCENE,
            &json!({ "adapter": adapter }),
            &bridge,
        );
        assert_eq!(rc, 0, "{reply}");
        app.update();
    };

    // CTK first, then iced: the CTK page is released before ours registers.
    load(&mut app, "bevy");
    let ctk_page = app.world().resource::<FakeCtkPage>().0.unwrap();
    assert!(wrapper_of(app.world(), ctk_page).is_some());
    load(&mut app, ADAPTER);
    let mut query = app
        .world_mut()
        .query_filtered::<Entity, With<IcedSurface>>();
    let surface = query.single(app.world()).unwrap();
    assert!(
        wrapper_of(app.world(), surface).is_some(),
        "iced page must be attached to a chrome wrapper"
    );

    // And back: ours is released before CTK registers its page.
    load(&mut app, "bevy");
    assert!(app.world().get_entity(surface).is_err());
    let ctk_page = app.world().resource::<FakeCtkPage>().0.unwrap();
    assert!(wrapper_of(app.world(), ctk_page).is_some());
}

#[test]
fn texture_buckets_absorb_a_resize_animation() {
    use crate::bridge::{BUCKET, texture_size};
    assert_eq!(
        texture_size(UVec2::new(1, 1), None),
        Some(UVec2::splat(BUCKET))
    );
    assert_eq!(
        texture_size(UVec2::new(129, 64), None),
        Some(UVec2::new(256, 128))
    );
    let tex = Some(UVec2::new(256, 128));
    assert_eq!(texture_size(UVec2::new(200, 100), tex), None);
    // Growing past the texture reallocates.
    assert_eq!(
        texture_size(UVec2::new(257, 100), tex),
        Some(UVec2::new(384, 128))
    );
    // Wobbling across a bucket edge keeps the larger texture ...
    assert_eq!(texture_size(UVec2::new(127, 100), tex), None);
    // ... but a much smaller need gives memory back.
    assert_eq!(
        texture_size(UVec2::new(60, 60), Some(UVec2::new(1024, 128))),
        Some(UVec2::new(128, 128))
    );

    // A reveal/resize animation through 20 widths inside one bucket.
    let mut h = Harness::new();
    h.run(3);
    assert_eq!(h.totals().allocations, 1);
    for width in 131..=150 {
        h.geometry(width, 100, 1.5);
        h.run(1);
    }
    h.run(1);
    let totals = h.totals();
    assert_eq!(totals.allocations, 1);
    assert_eq!(totals.resizes, 20);
    assert_eq!(totals.own_modified, 0);
    // Each size repaints only its visible part.
    assert_eq!(
        h.app.world().resource::<SceneIcedCounters>().last_rects,
        vec![Rect::new(0, 0, 150, 100)]
    );
    let mut views = h.app.world_mut().query::<&ImageNode>();
    let rects: Vec<_> = views.iter(h.app.world()).map(|node| node.rect).collect();
    assert_eq!(
        rects,
        vec![Some(bevy::math::Rect::new(0.0, 0.0, 150.0, 100.0))]
    );
}

#[test]
fn focused_surface_publishes_its_ime_target_and_receives_ime_input() {
    use cosmix_shell::runtime::{ExternalImeEvent, ExternalImeKind, ExternalImeTarget};
    let mut h = Harness::new();
    h.run(3);
    let target = |h: &Harness| {
        h.app
            .world()
            .get::<ExternalImeTarget>(h.surface)
            .unwrap()
            .clone()
    };
    assert!(!target(&h).enabled);
    h.app
        .world_mut()
        .resource_mut::<InputFocus>()
        .set(h.surface, FocusCause::Navigated);
    h.run(2);
    let enabled = target(&h);
    assert!(enabled.enabled);
    // Caret at physical (12, 12) + origin (10, 20), 3 x 24 px, at 1.5x.
    let min = Vec2::new(22.0, 32.0) / 1.5;
    assert_eq!(
        enabled.cursor,
        Some(bevy::math::Rect::from_corners(
            min,
            min + Vec2::new(3.0, 24.0) / 1.5
        ))
    );

    // A commit moves the caret: the target follows, and pixels change.
    let before = h.totals();
    let other = h.app.world_mut().spawn_empty().id();
    for event in [
        ExternalImeEvent {
            target: other,
            kind: ExternalImeKind::Commit("ignored".into()),
        },
        ExternalImeEvent {
            target: h.surface,
            kind: ExternalImeKind::DeleteSurrounding {
                before: 1,
                after: 0,
            },
        },
        ExternalImeEvent {
            target: h.surface,
            kind: ExternalImeKind::Commit("日本".into()),
        },
    ] {
        h.app.world_mut().write_message(event);
    }
    h.run(2);
    assert!(h.totals().bytes_queued > before.bytes_queued);
    // Two characters at 12 px each; a stray target's seven would be 84.
    let moved = target(&h).cursor.unwrap();
    assert_eq!(moved.min.x, (10.0 + 12.0 + 24.0) / 1.5);

    h.app.world_mut().resource_mut::<InputFocus>().clear();
    h.run(2);
    assert_eq!(target(&h), ExternalImeTarget::default());
}

#[test]
fn hovered_surface_drives_the_cursor_shape_and_leaving_resets_it() {
    use cosmix_shell::runtime::{CursorShape, CursorShapeRequest};
    let mut h = Harness::new();
    h.run(3);
    let shape = |h: &Harness| h.app.world().resource::<CursorShapeRequest>().0;
    assert_eq!(shape(&h), CursorShape::Default);
    h.pointer(
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Move { delta: Vec2::ZERO },
    );
    h.run(2);
    assert_eq!(shape(&h), CursorShape::Text);
    h.pointer(
        false,
        Vec2::new(500.0, 500.0),
        PointerAction::Move { delta: Vec2::ZERO },
    );
    h.run(2);
    assert_eq!(shape(&h), CursorShape::Default);
    // Another owner's request is not overwritten while no surface is hovered.
    h.app.world_mut().resource_mut::<CursorShapeRequest>().0 = CursorShape::Pointer;
    h.run(5);
    assert_eq!(shape(&h), CursorShape::Pointer);
}

#[test]
fn captured_pointer_ignores_positions_from_another_window() {
    let mut h = Harness::new();
    let home = h.app.world_mut().spawn_empty().id();
    let away = h.app.world_mut().spawn_empty().id();
    h.window = Some(home);
    h.geometry(200, 100, 1.5);
    h.run(3);
    h.pointer_in(
        Some(home),
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Press(PointerButton::Primary),
    );
    h.run(2);
    let before = h.totals();
    // Same logical position, other window: routed, this would redraw the
    // hover square elsewhere; instead the surface only loses hover.
    h.pointer_in(
        Some(away),
        false,
        Vec2::new(60.0, 30.0),
        PointerAction::Move { delta: Vec2::ZERO },
    );
    h.run(2);
    assert_eq!(h.totals().bytes_queued - before.bytes_queued, 36 * 36 * 4);
    assert_eq!(h.focus().cursor, None);
    // The release still ends the capture; later home-window input routes normally.
    h.pointer_in(
        Some(away),
        false,
        Vec2::new(60.0, 30.0),
        PointerAction::Release(PointerButton::Primary),
    );
    h.run(1);
    let before = h.totals();
    h.pointer_in(
        Some(home),
        true,
        Vec2::new(40.0, 60.0),
        PointerAction::Move { delta: Vec2::ZERO },
    );
    h.run(2);
    assert_eq!(h.totals().bytes_queued - before.bytes_queued, 36 * 36 * 4);
}
