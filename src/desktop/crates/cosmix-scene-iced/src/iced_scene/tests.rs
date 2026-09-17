use std::collections::HashSet;
use std::time::Duration;

use cosmix_iced_host::core::Rectangle;
use cosmix_scene::ResolvedScene;
use serde_json::{Value, json};

use super::*;
use crate::surface::{
    ImeEvent, ImeRequest, Key, Modifiers, NamedKey, PointerButton, Processed, Rect, SurfaceEvent,
    SurfaceRenderer,
};

const CONFORMANCE: &str = include_str!("../../../cosmix-scene/tests/fixtures/conformance.scene.md");
const CLIPPANEL: &str = include_str!("../../../cosmix-scene/tests/fixtures/clippanel.scene.md");
const STATIC: &str = include_str!("../../../cosmix-scene/tests/fixtures/static.scene.md");
static FONT: &[u8] = include_bytes!("../../../cosmix-comp/assets/fonts/DejaVuSans.ttf");

fn resolve(source: &str) -> ResolvedScene {
    cosmix_scene::resolve(&cosmix_scene::parse(source).unwrap()).unwrap()
}

fn test_design() -> SharedDesign {
    cosmix_iced_host::load_font(FONT);
    Arc::new(RwLock::new(DesignShare {
        revision: 0,
        look: Look {
            font: named_font("DejaVu Sans"),
            ..default_look()
        },
    }))
}

struct Rig {
    renderer: IcedSceneRenderer,
    outbox: Outbox,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    scale: f32,
    now: Duration,
}

impl Rig {
    fn new(source: &str, width: u32, height: u32, scale: f32) -> Self {
        let outbox = Outbox::default();
        let mut renderer = IcedSceneRenderer::new(test_design(), outbox.clone(), asset_root());
        renderer.resize(width, height, scale);
        renderer.set_scene(&resolve(source));
        Self {
            renderer,
            outbox,
            buffer: vec![0; width as usize * height as usize * 4],
            width,
            height,
            scale,
            now: Duration::from_secs(1),
        }
    }

    /// One host update, as `bridge::frame` runs it.
    fn frame(&mut self) -> (Processed, Option<Vec<Rect>>) {
        self.now += Duration::from_millis(16);
        let processed = self.renderer.process(self.now);
        let damage = processed.needs_redraw.then(|| {
            self.renderer
                .draw(&mut self.buffer, self.width, self.height, self.width * 4)
        });
        (processed, damage)
    }

    fn settle(&mut self) -> Processed {
        for _ in 0..8 {
            let (processed, damage) = self.frame();
            if damage.is_none() {
                return processed;
            }
        }
        panic!("surface never settled");
    }

    fn bounds(&mut self, key: &str) -> Rectangle {
        self.renderer
            .node_bounds(key)
            .unwrap_or_else(|| panic!("no bounds for {key}"))
    }

    fn physical(&mut self, key: &str, margin: f32) -> Rect {
        let b = self.bounds(key).expand(margin);
        let s = self.scale;
        let x = (b.x * s).floor().max(0.0);
        let y = (b.y * s).floor().max(0.0);
        Rect::new(
            x as u32,
            y as u32,
            ((b.x + b.width) * s).ceil() as u32 - x as u32,
            ((b.y + b.height) * s).ceil() as u32 - y as u32,
        )
    }

    fn point(&mut self, key: &str) {
        let c = self.bounds(key).center();
        self.renderer.queue(SurfaceEvent::PointerMoved {
            x: c.x * self.scale,
            y: c.y * self.scale,
        });
    }

    fn click(&mut self, key: &str) {
        self.point(key);
        for pressed in [true, false] {
            self.renderer.queue(SurfaceEvent::PointerButton {
                button: PointerButton::Primary,
                pressed,
            });
        }
        self.settle();
    }

    fn key(&mut self, key: Key, text: Option<&str>, modifiers: Modifiers) {
        let latin = match &key {
            Key::Character(c) => c.chars().next().filter(char::is_ascii_alphanumeric),
            _ => None,
        };
        for pressed in [true, false] {
            self.renderer.queue(SurfaceEvent::Key {
                key: key.clone(),
                latin,
                text: pressed.then(|| text.map(str::to_owned)).flatten(),
                pressed,
                repeat: false,
                modifiers,
            });
        }
        self.settle();
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.key(Key::Character(s.clone()), Some(&s), Modifiers::default());
        }
    }

    fn ctrl(&mut self, letter: &str, shift: bool) {
        self.key(
            Key::Character(letter.into()),
            None,
            Modifiers {
                control: true,
                shift,
                ..Modifiers::default()
            },
        );
    }

    fn actions(&self) -> Vec<SceneAction> {
        std::mem::take(&mut *self.outbox.lock().unwrap())
    }

    fn field(&self) -> String {
        self.renderer
            .program()
            .field_value("field")
            .unwrap_or_default()
            .to_owned()
    }

    fn colours(&self) -> usize {
        self.buffer
            .chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect::<HashSet<_>>()
            .len()
    }
}

fn action(kind: &'static str, handler: &str, node: &str, value: Option<Value>) -> SceneAction {
    SceneAction {
        scene: "conformance".into(),
        citizen: "conformance-citizen".into(),
        node: node.into(),
        kind,
        handler: handler.into(),
        value,
        item: None,
    }
}

#[test]
fn fixtures_render_then_idle() {
    for (source, width, height) in [
        (CLIPPANEL, 720, 520),
        (CONFORMANCE, 640, 480),
        (STATIC, 300, 200),
    ] {
        for scale in [1.0, 1.5] {
            let (w, h) = (
                (width as f32 * scale) as u32,
                (height as f32 * scale) as u32,
            );
            let mut rig = Rig::new(source, w, h, scale);
            let (processed, damage) = rig.frame();
            assert!(processed.needs_redraw);
            assert_eq!(damage.unwrap(), vec![Rect::new(0, 0, w, h)]);
            assert!(rig.colours() > 8, "only {} colours", rig.colours());
            let settled = rig.settle();
            assert!(!settled.needs_redraw);
            assert_eq!(settled.wake_at, None, "an unfocused scene must not wake");
            assert_eq!(settled.ime, ImeRequest::Disabled);
            // A forced redraw of an unchanged scene paints nothing.
            let buffer = rig.buffer.clone();
            assert!(rig.renderer.draw(&mut rig.buffer, w, h, w * 4).is_empty());
            assert!(rig.buffer == buffer);
        }
    }
}

#[test]
fn text_is_drawn_from_cells() {
    let mut rig = Rig::new(CLIPPANEL, 720, 520, 1.0);
    rig.settle();
    let row = rig.physical("r_prev@e1", 0.0);
    let background = rig.buffer[(row.y * 720 + row.x) as usize * 4..][..4].to_vec();
    let mut ink = 0;
    for y in row.y..row.bottom() {
        for x in row.x..row.right() {
            let i = (y * 720 + x) as usize * 4;
            if rig.buffer[i..i + 4] != background[..] {
                ink += 1;
            }
        }
    }
    assert!(ink > 50, "preview cell has {ink} ink pixels");
}

#[test]
fn hover_damage_stays_on_the_row() {
    let mut rig = Rig::new(CLIPPANEL, 720, 520, 1.5);
    rig.settle();
    let row = rig.physical("entry_row@e1", 2.0);
    let probe = (row.x + 4, row.y + row.h / 2);
    let at = |rig: &Rig| {
        let i = (probe.1 * rig.width + probe.0) as usize * 4;
        rig.buffer[i..i + 4].to_vec()
    };
    let before = at(&rig);
    rig.point("entry_row@e1");
    let (processed, damage) = rig.frame();
    assert!(processed.needs_redraw);
    let damage = damage.unwrap();
    assert!(!damage.is_empty());
    for rect in &damage {
        assert!(
            rect.x >= row.x
                && rect.y >= row.y
                && rect.right() <= row.right()
                && rect.bottom() <= row.bottom(),
            "{rect:?} outside {row:?}"
        );
    }
    assert_ne!(at(&rig), before, "hover colour not drawn");
    assert!(rig.settle().wake_at.is_none());
}

#[test]
fn clicks_toggles_and_rows_reach_their_handlers() {
    let mut rig = Rig::new(CONFORMANCE, 640, 480, 1.0);
    rig.settle();
    rig.click("button");
    assert_eq!(rig.actions(), vec![action("click", "go", "button", None)]);
    rig.click("toggle");
    assert_eq!(
        rig.actions(),
        vec![action("change", "toggle", "toggle", Some(json!(true)))]
    );
    rig.click("row");
    assert_eq!(rig.actions(), vec![action("click", "pick", "row", None)]);
    rig.click("template@1");
    assert_eq!(
        rig.actions(),
        vec![SceneAction {
            item: Some(json!({"id": "1", "cells": ["one"]})),
            ..action("click", "select", "list", None)
        }]
    );
    // A patched toggle value is adopted: the port goes false -> true, so the
    // next click turns it off.
    let mut tree = resolve(CONFORMANCE);
    tree.nodes["toggle"]
        .ports
        .insert("value".into(), json!(true));
    rig.renderer.set_scene(&tree);
    rig.settle();
    rig.click("toggle");
    assert_eq!(
        rig.actions(),
        vec![action("change", "toggle", "toggle", Some(json!(false)))]
    );
}

#[test]
fn field_edits_submit_undo_and_survive_reloads() {
    let mut rig = Rig::new(CONFORMANCE, 640, 480, 1.25);
    rig.settle();
    rig.click("field");
    let processed = rig.settle();
    let ImeRequest::Enabled { cursor, purpose } = processed.ime else {
        panic!("focused field requested no IME: {processed:?}");
    };
    let field = rig.physical("field", 0.0);
    assert!(field.contains(cursor.x as f32 + 0.5, cursor.y as f32 + 0.5));
    assert_eq!(purpose, crate::surface::ImePurpose::Normal);
    assert!(processed.wake_at.is_some(), "caret blink not scheduled");

    rig.type_text("ab");
    assert_eq!(rig.field(), "ab");
    assert_eq!(
        rig.actions(),
        vec![
            action("change", "change", "field", Some(json!("a"))),
            action("change", "change", "field", Some(json!("ab"))),
        ]
    );
    rig.key(
        Key::Named(NamedKey::Enter),
        Some("\r"),
        Modifiers::default(),
    );
    assert_eq!(
        rig.actions(),
        vec![action("submit", "submit", "field", Some(json!("ab")))]
    );

    // An unrelated patch plus an external value while focused: text, focus
    // and history are retained.
    let mut patched = resolve(CONFORMANCE);
    patched.nodes["text"]
        .ports
        .insert("text".into(), json!("new status"));
    patched.nodes["field"]
        .ports
        .insert("value".into(), json!("external"));
    rig.renderer.set_scene(&patched);
    rig.settle();
    assert_eq!(rig.field(), "ab");
    assert!(rig.renderer.focused().contains("field"));
    rig.ctrl("z", false);
    let undone = rig.field();
    assert_ne!(undone, "ab", "undo history lost across reload");
    rig.ctrl("y", false);
    assert_eq!(rig.field(), "ab");
    rig.actions();

    // Preedit survives a reload and commits into the retained text.
    rig.renderer.queue(SurfaceEvent::Ime(ImeEvent::Preedit {
        text: "zz".into(),
        cursor: Some((2, 2)),
    }));
    rig.settle();
    let preedit = |rig: &Rig| match &rig.renderer.host_requests().ime {
        cosmix_iced_host::ImeRequest::Enabled { preedit, .. } => {
            preedit.as_ref().map(|p| p.content.clone())
        }
        cosmix_iced_host::ImeRequest::Disabled => None,
    };
    assert_eq!(preedit(&rig).as_deref(), Some("zz"));
    patched.nodes["text"]
        .ports
        .insert("text".into(), json!("again"));
    rig.renderer.set_scene(&patched);
    rig.settle();
    assert_eq!(preedit(&rig).as_deref(), Some("zz"));
    rig.renderer
        .queue(SurfaceEvent::Ime(ImeEvent::Commit("zz".into())));
    rig.renderer.queue(SurfaceEvent::Ime(ImeEvent::Disabled));
    rig.settle();
    assert_eq!(rig.field(), "abzz");

    // Once unfocused, an external value replaces the text.
    rig.click("text");
    assert!(!rig.renderer.focused().contains("field"));
    assert_eq!(rig.settle().ime, ImeRequest::Disabled);
    patched.nodes["field"]
        .ports
        .insert("value".into(), json!("external 2"));
    rig.renderer.set_scene(&patched);
    rig.settle();
    assert_eq!(rig.field(), "external 2");
}

#[test]
fn scene_get_returns_the_p1_resolved_scene() {
    use cosmix_scene_bevy::SceneStore;
    use cosmix_shell::runtime::SceneVerb;
    let (bridge, _peer) = ctk::bus::test_bridge("test");
    for source in [CONFORMANCE, CLIPPANEL, STATIC] {
        let expected = resolve(source);
        let mut store = SceneStore::default();
        store.register_adapter(crate::ADAPTER);
        let (rc, reply) = store.dispatch(
            SceneVerb::Load,
            source,
            &json!({"adapter": crate::ADAPTER}),
            &bridge,
        );
        assert_eq!(rc, 0, "{reply}");
        let (rc, reply) = store.dispatch(
            SceneVerb::Get,
            "",
            &json!({"scene": expected.name}),
            &bridge,
        );
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!(expected)
        );
    }
}

#[test]
fn plugin_mounts_iced_and_sends_handlers_on_the_scene_bus_path() {
    use bevy::asset::AssetPlugin;
    use cosmix_scene_bevy::SceneStore;
    use cosmix_shell::runtime::SceneVerb;

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Image>()
        .add_plugins((crate::SceneIcedPlugin, IcedRendererPlugin));
    let (bridge, peer) = ctk::bus::test_bridge("test");
    let (rc, reply) = app.world_mut().resource_mut::<SceneStore>().dispatch(
        SceneVerb::Load,
        CONFORMANCE,
        &json!({"adapter": crate::ADAPTER}),
        &bridge,
    );
    assert_eq!(rc, 0, "{reply}");
    app.insert_resource(bridge);
    app.update();
    let surface = app
        .world_mut()
        .query_filtered::<Entity, With<crate::IcedSurface>>()
        .single(app.world())
        .unwrap();
    app.world_mut()
        .entity_mut(surface)
        .insert(crate::IcedSurfaceGeometry {
            // Not a texture bucket multiple: draw gets a padded stride.
            size: UVec2::new(600, 450),
            scale: 1.0,
            origin: Vec2::ZERO,
            window: None,
            pointer_scale: 1.0,
        });
    for _ in 0..4 {
        app.update();
    }
    let counters = app.world().resource::<crate::SceneIcedCounters>();
    assert_eq!(counters.totals.allocations, 1);
    assert!(counters.totals.draws >= 1);
    assert!(
        counters.totals.bytes_queued >= 600 * 450 * 4,
        "{:?}",
        counters.totals
    );

    let outbox = app.world().resource::<IcedOutbox>().0.clone();
    outbox
        .lock()
        .unwrap()
        .push(action("click", "go", "button", None));
    app.update();
    let calls = peer.drain_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].to, "conformance-citizen");
    assert_eq!(calls[0].command, "go");
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body).unwrap(),
        json!({"scene": "conformance", "node": "button", "kind": "click"})
    );
}

#[test]
fn font_faces_resolve_to_the_same_family_in_both_stacks() {
    use cosmix_iced_host::tiny_skia_renderer::graphics::text::font_system;

    cosmix_iced_host::load_font(FONT);
    let mut bevy_fonts = bevy::text::FontCx::default();
    let registered = bevy_fonts
        .collection
        .register_fonts(parley::fontique::Blob::from(FONT.to_vec()), None);
    let bevy_names: Vec<String> = registered
        .iter()
        .filter_map(|(id, _)| bevy_fonts.collection.family_name(*id).map(str::to_owned))
        .collect();
    assert_eq!(bevy_names, ["DejaVu Sans"]);

    let iced_family = |name: &str| {
        let mut system = font_system().write().unwrap();
        system
            .raw()
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(family, _)| family.clone()))
            .find(|family| family.eq_ignore_ascii_case(name))
    };
    assert_eq!(iced_family("DejaVu Sans").as_deref(), Some("DejaVu Sans"));

    // CTK's configured family comes from the system in both stacks; report
    // whether this machine has it rather than assume.
    let ctk_family = ctk::theme::CtkTypography::default().requested_family;
    let bevy_has = bevy_fonts
        .collection
        .family_by_name(&ctk_family)
        .map(|f| f.name().to_owned());
    let iced_has = iced_family(&ctk_family);
    println!("FONT_CHECK family={ctk_family:?} bevy={bevy_has:?} iced={iced_has:?}");
    assert_eq!(
        bevy_has.is_some(),
        iced_has.is_some(),
        "{ctk_family:?} resolves in one stack only: bevy={bevy_has:?} iced={iced_has:?}"
    );
    if let (Some(bevy), Some(iced)) = (&bevy_has, &iced_has) {
        assert_eq!(bevy, iced);
    }
    // The face both stacks were given resolves in both, by the same name.
    assert_eq!(
        bevy_fonts
            .collection
            .family_by_name("DejaVu Sans")
            .map(|family| family.name().to_owned())
            .as_deref(),
        iced_family("DejaVu Sans").as_deref()
    );
}

#[test]
fn password_fields_ask_for_a_secure_input_method() {
    let source = "---\nscene: 1\nname: secret\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", padding: 8, children: [\"field\"]}\nfield: {widget: \"field\", value: \"\", password: true, width: 120}\n```\n";
    let mut rig = Rig::new(source, 300, 100, 2.0);
    rig.settle();
    rig.click("field");
    match rig.settle().ime {
        ImeRequest::Enabled { purpose, .. } => {
            assert_eq!(purpose, crate::surface::ImePurpose::Secure)
        }
        other => panic!("no IME request: {other:?}"),
    }
}

#[test]
fn bucketed_texture_stride_is_drawn() {
    let (width, height) = (600, 450);
    let texture = crate::bridge::texture_size(UVec2::new(width, height), None).unwrap();
    assert!(texture.x > width, "size must not be a bucket multiple");
    let mut rig = Rig::new(CONFORMANCE, width, height, 1.0);
    let stride = texture.x * 4;
    let mut buffer = vec![0u8; (stride * texture.y) as usize];
    rig.renderer.process(Duration::from_secs(1));
    let damage = rig.renderer.draw(&mut buffer, width, height, stride);
    assert_eq!(damage, vec![Rect::new(0, 0, width, height)]);
    let row = |y: u32| &buffer[(y * stride) as usize..((y + 1) * stride) as usize];
    // Visible pixels are painted (opaque surface), padding is untouched.
    assert!(
        row(10)[..(width * 4) as usize]
            .chunks_exact(4)
            .all(|p| p[3] == 255)
    );
    assert!(row(10)[(width * 4) as usize..].iter().all(|b| *b == 0));
}

/// CTK gives a clickable template row its own binding and stops propagation;
/// the list's row click is only for rows that have none.
#[test]
fn a_clickable_template_row_reports_itself() {
    let source = "---\nscene: 1\nname: rows\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", padding: 4, children: [\"list\"]}\nlist: {widget: \"list\", rows: [{id: \"r1\", cells: [\"one\"]}], row: \"entry\", row_height: 30, on_click: \"pick\"}\nentry: {widget: \"row\", height: 30, children: [\"cell\"], on_click: \"open\"}\ncell: {widget: \"text\", text: \"{cells[0]}\"}\n```\n";
    let mut rig = Rig::new(source, 300, 200, 1.0);
    rig.settle();
    rig.click("entry@r1");
    let actions = rig.actions();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0].node, "entry@r1");
    assert_eq!(actions[0].handler, "open");
    assert_eq!(actions[0].kind, "click");
    assert_eq!(actions[0].item, None);
}

#[test]
fn rows_and_list_rows_fire_on_release() {
    let mut rig = Rig::new(CONFORMANCE, 640, 480, 1.0);
    rig.settle();
    let row = rig.bounds("row").center();
    rig.renderer
        .queue(SurfaceEvent::PointerMoved { x: row.x, y: row.y });
    rig.renderer.queue(SurfaceEvent::PointerButton {
        button: PointerButton::Primary,
        pressed: true,
    });
    rig.settle();
    assert!(rig.actions().is_empty(), "a press alone must not click");
    rig.renderer.queue(SurfaceEvent::PointerButton {
        button: PointerButton::Primary,
        pressed: false,
    });
    rig.settle();
    assert_eq!(rig.actions(), vec![action("click", "pick", "row", None)]);
}

#[test]
fn stretch_rows_fill_their_children() {
    let source = "---\nscene: 1\nname: stretchy\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", padding: 0, children: [\"bar\"]}\nbar: {widget: \"row\", height: 60, align: \"stretch\", children: [\"tall\"]}\ntall: {widget: \"row\", children: [\"label\"], background: \"#345\"}\nlabel: {widget: \"text\", text: \"x\"}\n```\n";
    let mut rig = Rig::new(source, 300, 200, 1.0);
    rig.settle();
    let bar = rig.bounds("bar");
    let tall = rig.bounds("tall");
    assert!(
        (tall.height - bar.height).abs() < 0.5,
        "stretch: {} vs {}",
        tall.height,
        bar.height
    );
}

#[test]
fn hover_state_does_not_outlive_its_rows() {
    let mut rig = Rig::new(CLIPPANEL, 720, 520, 1.0);
    rig.settle();
    rig.point("entry_row@e1");
    rig.settle();
    assert_eq!(rig.renderer.program().hovered_keys(), ["entry_row@e1"]);
    let mut without = resolve(CLIPPANEL);
    without.nodes["table"]
        .ports
        .insert("rows".into(), json!([]));
    rig.renderer.set_scene(&without);
    rig.settle();
    assert!(
        rig.renderer.program().hovered_keys().is_empty(),
        "stale hover: {:?}",
        rig.renderer.program().hovered_keys()
    );
}

#[test]
fn a_hidden_field_keeps_its_text_and_focus() {
    let source = "---\nscene: 1\nname: hider\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", padding: 6, children: [\"pad\", \"field\"]}\npad: {widget: \"text\", text: \"pad\"}\nfield: {widget: \"field\", value: \"\", width: 150, on_change: \"change\"}\n```\n";
    let mut rig = Rig::new(source, 300, 200, 1.0);
    rig.settle();
    rig.click("field");
    rig.type_text("hi");
    assert_eq!(rig.renderer.program().field_value("field"), Some("hi"));
    rig.actions();

    // The scene hides the column that holds it: CTK keeps the entity, so the
    // iced widget must keep its state too.
    let mut hidden = resolve(source);
    hidden.nodes["pad"]
        .ports
        .insert("hidden".into(), json!(true));
    rig.renderer.set_scene(&hidden);
    rig.settle();
    assert!(rig.renderer.focused().contains("field"));
    rig.type_text("!");
    assert_eq!(rig.renderer.program().field_value("field"), Some("hi!"));
}

#[test]
fn images_are_reported_not_silently_blank() {
    let mut rig = Rig::new(CONFORMANCE, 640, 480, 1.0);
    rig.settle();
    assert_eq!(rig.renderer.program().undrawn_nodes(), 1);
}

#[test]
fn buttons_follow_the_design_tokens_not_iceds_palette() {
    // A light look: the button must not be painted from iced's dark palette.
    let light = Tokens {
        surface: cosmix_iced_host::core::Color::WHITE,
        selection: cosmix_iced_host::core::Color::from_rgb8(0x2f, 0x81, 0xf7),
        selection_text: cosmix_iced_host::core::Color::WHITE,
        ..Tokens::default()
    };
    let design = Arc::new(RwLock::new(DesignShare {
        revision: 0,
        look: Look {
            tokens: light,
            dark: false,
            font: named_font("DejaVu Sans"),
            text_px: 15.0,
        },
    }));
    cosmix_iced_host::load_font(FONT);
    let outbox = Outbox::default();
    let mut renderer = IcedSceneRenderer::new(design, outbox, asset_root());
    renderer.resize(300, 200, 1.0);
    let source = "---\nscene: 1\nname: tones\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", padding: 10, children: [\"go\"]}\ngo: {widget: \"button\", label: \"Go\", tone: \"primary\", width: 80, on_click: \"go\"}\n```\n";
    renderer.set_scene(&resolve(source));
    renderer.process(Duration::from_secs(1));
    let mut buffer = vec![0u8; 300 * 200 * 4];
    renderer.draw(&mut buffer, 300, 200, 300 * 4);
    let button = renderer.node_bounds("go").expect("button bounds");
    let x = (button.x + button.width / 2.0) as usize;
    let y = (button.y + button.height / 2.0) as usize;
    let pixel = &buffer[(y * 300 + x) * 4..][..4];
    let want: [u8; 3] = [0x2f, 0x81, 0xf7];
    for (channel, expected) in pixel.iter().zip(want) {
        assert!(
            (*channel as i32 - expected as i32).abs() <= 2,
            "button pixel {pixel:?} is not the accent {want:?}"
        );
    }
}

#[test]
fn handler_calls_wait_for_the_bus() {
    use bevy::asset::AssetPlugin;
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Image>()
        .add_plugins((crate::SceneIcedPlugin, IcedRendererPlugin));
    let outbox = app.world().resource::<IcedOutbox>().0.clone();
    outbox
        .lock()
        .unwrap()
        .push(action("click", "go", "button", None));
    app.update();
    assert_eq!(
        outbox.lock().unwrap().len(),
        1,
        "a click before the Bus is up must stay queued"
    );
    let (bridge, peer) = ctk::bus::test_bridge("test");
    app.insert_resource(bridge);
    app.update();
    assert!(outbox.lock().unwrap().is_empty());
    assert_eq!(peer.drain_calls().len(), 1);
}

#[test]
fn pointer_positions_use_the_pointer_scale() {
    // Bevy's UiScale separates the render scale from the scale pointer
    // positions arrive in; the bridge passes both.
    let mut rig = Rig::new(CONFORMANCE, 640, 480, 3.75);
    rig.renderer.set_pointer_scale(2.5);
    rig.settle();
    let button = rig.bounds("button").center();
    rig.renderer.queue(SurfaceEvent::PointerMoved {
        x: button.x * 2.5,
        y: button.y * 2.5,
    });
    for pressed in [true, false] {
        rig.renderer.queue(SurfaceEvent::PointerButton {
            button: PointerButton::Primary,
            pressed,
        });
    }
    rig.settle();
    assert_eq!(rig.actions(), vec![action("click", "go", "button", None)]);

    // The same logical point, now arriving unscaled, still hits: the divisor
    // is the pointer scale, not the render scale.
    rig.renderer.set_pointer_scale(1.0);
    rig.renderer.queue(SurfaceEvent::PointerMoved {
        x: button.x,
        y: button.y,
    });
    for pressed in [true, false] {
        rig.renderer.queue(SurfaceEvent::PointerButton {
            button: PointerButton::Primary,
            pressed,
        });
    }
    rig.settle();
    assert_eq!(rig.actions(), vec![action("click", "go", "button", None)]);
}

#[test]
fn the_renderer_plugin_mounts_the_bridge_itself() {
    use bevy::asset::AssetPlugin;
    use cosmix_scene_bevy::SceneStore;
    use cosmix_shell::runtime::SceneVerb;
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Image>()
        .add_plugins(IcedRendererPlugin);
    let (bridge, _peer) = ctk::bus::test_bridge("test");
    let (rc, reply) = app.world_mut().resource_mut::<SceneStore>().dispatch(
        SceneVerb::Load,
        CONFORMANCE,
        &json!({"adapter": crate::ADAPTER}),
        &bridge,
    );
    assert_eq!(rc, 0, "{reply}");
    app.update();
    let surface = app
        .world_mut()
        .query_filtered::<Entity, With<crate::IcedSurface>>()
        .single(app.world())
        .unwrap();
    app.world_mut()
        .entity_mut(surface)
        .insert(crate::IcedSurfaceGeometry {
            size: UVec2::new(600, 450),
            scale: 1.0,
            origin: Vec2::ZERO,
            window: None,
            pointer_scale: 1.0,
        });
    for _ in 0..3 {
        app.update();
    }
    let counters = app.world().resource::<crate::SceneIcedCounters>();
    assert!(counters.totals.draws >= 1, "the iced renderer never drew");
    assert!(counters.totals.bytes_queued >= 600 * 450 * 4);
}
