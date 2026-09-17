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
                target: NormalizedRenderTarget::None {
                    width: 800,
                    height: 600,
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
    h.geometry(300, 100, 2.0);
    h.run(3);
    assert_eq!(h.totals().allocations, 3);
    assert_eq!(h.totals().own_added, 3);
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
