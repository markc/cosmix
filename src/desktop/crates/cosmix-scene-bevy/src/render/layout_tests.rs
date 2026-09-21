//! P0a: document -> scheduled reconciliation -> Taffy -> logical geometry.
//! No window/render backend. Keep the platform setup in sync with bterm's
//! `layout_tests::layout_app`. These tests deliberately do not inspect styles.
use super::*;
use bevy::camera::{CameraPlugin, ComputedCameraValues, RenderTargetInfo, Viewport};
use bevy::reflect::ReflectRef;
use bevy::text::TextLayoutInfo;
use bevy::ui::{UiPlugin, widget::TextNodeFlags};
use cosmix_shell::runtime::SceneVerb;

const TOLERANCE: f32 = 0.5;
const SETTLE_FRAMES: usize = 12;

struct Harness {
    app: App,
    camera: Entity,
}

/// Raw computed components plus world-space, logical-pixel boxes. Text has
/// both a UI label node and an actual shaped label box; confusing those hides
/// the no-wrap/justification defect.
#[derive(Debug)]
struct Geometry {
    computed: ComputedNode,
    transform: UiGlobalTransform,
    border: Rect,
    content: Rect,
    label_node: Option<Rect>,
    label_box: Option<Rect>,
}

fn logical_rect(node: &ComputedNode, transform: &UiGlobalTransform, rect: Rect) -> Rect {
    // Fixtures are axis aligned. Convert positions AND sizes, exactly once.
    Rect::from_corners(
        transform.transform_point2(rect.min) * node.inverse_scale_factor,
        transform.transform_point2(rect.max) * node.inverse_scale_factor,
    )
}

fn close(context: &str, actual: f32, expected: f32) {
    assert!(
        actual.is_finite() && expected.is_finite() && (actual - expected).abs() <= TOLERANCE,
        "{context}: actual {actual:.3}, expected {expected:.3}, delta {:.3} logical px (tolerance {TOLERANCE})",
        actual - expected,
    );
}

impl Harness {
    fn load(body: &str) -> Self {
        let mut app = App::new();
        // Plugin list and camera setup copied verbatim from bterm, including
        // fractional scale: physical 1000x700 means logical 800x560.
        app.add_plugins((
            MinimalPlugins,
            bevy::asset::AssetPlugin::default(),
            bevy::transform::TransformPlugin,
            CameraPlugin,
            ImagePlugin::default(),
            bevy::image::TextureAtlasPlugin,
            bevy::mesh::MeshPlugin,
            bevy::input::InputPlugin,
            bevy::picking::PickingPlugin,
            bevy::picking::InteractionPlugin,
            bevy::text::TextPlugin,
            UiPlugin,
        ));
        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    computed: ComputedCameraValues {
                        target_info: Some(RenderTargetInfo {
                            physical_size: UVec2::new(1000, 700),
                            scale_factor: 1.25,
                        }),
                        ..default()
                    },
                    viewport: Some(Viewport {
                        physical_size: UVec2::new(1000, 700),
                        ..default()
                    }),
                    ..default()
                },
            ))
            .id();
        // ScenePlugin installs CtkTextAreaPlugin and EditableTextInputPlugin.
        // Their scheduled readers need these queues even with no fields.
        // No WindowPlugin/window is needed.
        app.add_message::<bevy::window::WindowFocused>()
            .add_message::<bevy::window::Ime>();
        app.add_plugins(crate::ScenePlugin);
        app.finish();
        app.cleanup();
        assert!(app.world().contains_resource::<Assets<Image>>());
        let document =
            format!("---\nscene: 1\nname: layout\ncitizen: test\n---\n```mix\n{body}\n```\n");
        // Same parse/lint/resolve ingress as dispatch, without a live Bus or
        // publication side effect. ScenePlugin performs the real reconcile.
        app.world_mut()
            .resource_mut::<SceneStore>()
            .request(SceneVerb::Load, &document, &Value::Null)
            .expect("layout fixture must be a valid scene document");
        let mut harness = Self { app, camera };
        harness.settle();
        harness
    }

    fn settle(&mut self) {
        for _ in 0..SETTLE_FRAMES {
            self.app.update();
        }
        let world = self.app.world_mut();
        let camera = world.get::<Camera>(self.camera).unwrap();
        assert_eq!(camera.physical_target_size(), Some(UVec2::new(1000, 700)));
        assert_eq!(
            camera.logical_viewport_size(),
            Some(Vec2::new(800.0, 560.0))
        );
        let store = world.resource::<SceneStore>();
        let mounted = store.scenes["layout"]
            .mounted
            .as_ref()
            .expect("scene reconciled");
        let page = world.get::<ComputedNode>(mounted.page).unwrap();
        close(
            "page width / nonzero camera",
            page.size.x * page.inverse_scale_factor,
            800.0,
        );
        close(
            "page height / nonzero camera",
            page.size.y * page.inverse_scale_factor,
            560.0,
        );

        let mut count = 0;
        for (entity, flags, layout) in world
            .query::<(Entity, &TextNodeFlags, &TextLayoutInfo)>()
            .iter(world)
        {
            count += 1;
            // Bevy exposes this private flag through Reflect. Fail closed if
            // the field disappears; never replace this with a style assertion.
            let ReflectRef::Struct(fields) = flags.reflect_ref() else {
                panic!("TextNodeFlags must expose its measurement state");
            };
            for name in ["needs_measure_fn", "needs_recompute"] {
                let pending = fields
                    .field(name)
                    .and_then(|field| field.try_downcast_ref::<bool>())
                    .expect("Bevy text readiness flag must be reflected");
                assert!(
                    !pending,
                    "{entity:?}: {name} still true after {SETTLE_FRAMES} updates; check default_font feature / font availability"
                );
            }
            assert!(
                !layout.glyphs.is_empty(),
                "{entity:?}: fixture text must produce glyphs"
            );
            assert!(
                !layout.run_geometry.is_empty(),
                "{entity:?}: fixture text must produce run geometry"
            );
            assert!(layout.size.x > 0.0 && layout.size.y > 0.0);
        }
        assert!(
            count > 0,
            "every fixture includes a font-measurement sentinel"
        );
    }

    fn entity(&self, id: &str) -> Entity {
        self.app.world().resource::<SceneStore>().scenes["layout"]
            .mounted
            .as_ref()
            .unwrap()
            .nodes[id]
            .root
    }

    fn geometry(&self) -> BTreeMap<String, Geometry> {
        let world = self.app.world();
        let store = world.resource::<SceneStore>();
        let entry = &store.scenes["layout"];
        let mounted = entry.mounted.as_ref().unwrap();
        // These fixtures have no list templates: every document id must map.
        assert_eq!(mounted.nodes.len(), entry.tree.nodes.len());
        entry
            .tree
            .nodes
            .keys()
            .map(|id| {
                let view = &mounted.nodes[id];
                let computed = *world.get::<ComputedNode>(view.root).expect("ComputedNode");
                let transform = *world
                    .get::<UiGlobalTransform>(view.root)
                    .expect("UiGlobalTransform");
                let (label_node, label_box) = view.label.map_or((None, None), |label| {
                    let node = world.get::<ComputedNode>(label).unwrap();
                    let transform = world.get::<UiGlobalTransform>(label).unwrap();
                    let text = world.get::<TextLayoutInfo>(label).unwrap();
                    let mut runs = text.run_geometry.iter();
                    let mut bounds = runs.next().expect("measured text run").bounds;
                    for run in runs {
                        bounds.min = bounds.min.min(run.bounds.min);
                        bounds.max = bounds.max.max(run.bounds.max);
                    }
                    // The 0.19.1 pipeline builds Parley at the target scale and
                    // copies run coordinates directly. Like bevy_ui_render's
                    // decoration extraction, offset them from the content origin
                    // without scaling again (despite the "unscaled" field docs).
                    let origin = node.content_box().min;
                    let physical = Rect::from_corners(origin + bounds.min, origin + bounds.max);
                    (
                        Some(logical_rect(node, transform, node.border_box())),
                        Some(logical_rect(node, transform, physical)),
                    )
                });
                (
                    id.clone(),
                    Geometry {
                        computed,
                        transform,
                        border: logical_rect(&computed, &transform, computed.border_box()),
                        content: logical_rect(&computed, &transform, computed.content_box()),
                        label_node,
                        label_box,
                    },
                )
            })
            .collect()
    }
}

// Equal flexible spacers express main-axis centring using existing ports.
// `align: center` on row/column only controls the cross axis.
fn centred_document(family: &str, displaced: bool) -> String {
    let (bias_child, bias_node) = if displaced {
        ("\"bias\", ", "bias: {widget: \"spacer\", size: 6}\n")
    } else {
        ("", "")
    };
    format!(
        r#"
root: {{widget: "{family}", fill: true, padding: 16, align: "center", children: [{bias_child}"before", "child", "after"]}}
{bias_node}before: {{widget: "spacer"}}
child: {{widget: "text", text: "Clock"}}
after: {{widget: "spacer"}}
"#
    )
}

#[test]
fn row_main_axis_centres_child_box() {
    let geometry = Harness::load(&centred_document("row", false)).geometry();
    close(
        "row child centre x",
        geometry["child"].border.center().x,
        geometry["root"].content.center().x,
    );
    close(
        "intrinsic label box centre / text coordinate conversion",
        geometry["child"].label_box.unwrap().center().x,
        geometry["child"].content.center().x,
    );
}

#[test]
fn column_main_axis_centres_child_box() {
    let geometry = Harness::load(&centred_document("column", false)).geometry();
    close(
        "column child centre y",
        geometry["child"].border.center().y,
        geometry["root"].content.center().y,
    );
}

fn gaps_document(family: &str) -> String {
    format!(
        r#"
root: {{widget: "{family}", fill: true, padding: 16, gap: 12, align: "start", children: ["a", "b", "c", "sentinel"]}}
a: {{widget: "spacer", size: 40}}
b: {{widget: "spacer", size: 40}}
c: {{widget: "spacer", size: 40}}
sentinel: {{widget: "text", text: "Measured"}}
"#
    )
}

#[test]
fn consecutive_gaps_and_padding_are_logical_pixels() {
    for (family, axis) in [("row", 0), ("column", 1)] {
        let geometry = Harness::load(&gaps_document(family)).geometry();
        let root = &geometry["root"];
        // Raw readback remains available to more specialised assertions.
        close("fractional scale", root.computed.inverse_scale_factor, 0.8);
        assert!(root.transform.translation.is_finite());
        for coordinate in 0..2 {
            close(
                "leading padding",
                root.content.min[coordinate] - root.border.min[coordinate],
                16.0,
            );
            close(
                "trailing padding",
                root.border.max[coordinate] - root.content.max[coordinate],
                16.0,
            );
            close(
                "first child at content origin",
                geometry["a"].border.min[coordinate],
                root.content.min[coordinate],
            );
        }
        let mut previous_gap = None;
        for [left, right] in [["a", "b"], ["b", "c"], ["c", "sentinel"]] {
            let gap = geometry[right].border.min[axis] - geometry[left].border.max[axis];
            close("authored gap", gap, 12.0);
            if let Some(previous) = previous_gap {
                close("equal consecutive gaps", gap, previous);
            }
            previous_gap = Some(gap);
        }
    }
}

#[test]
fn taffy_respects_minimum_and_maximum_constraints() {
    let mut harness = Harness::load(&gaps_document("row"));
    // Calibration only: the scene schema has no general min/max ports.
    // Constrain real document-created entities, without adding renderer ports
    // or substituting a hand-built tree. Their authored sizes are both 40.
    let a = harness.entity("a");
    let b = harness.entity("b");
    {
        let mut node = harness.app.world_mut().get_mut::<Node>(a).unwrap();
        node.min_width = px(80);
        node.min_height = px(60);
    }
    {
        let mut node = harness.app.world_mut().get_mut::<Node>(b).unwrap();
        node.max_width = px(20);
        node.max_height = px(24);
    }
    harness.settle();
    let geometry = harness.geometry();
    close("minimum width", geometry["a"].border.width(), 80.0);
    close("minimum height", geometry["a"].border.height(), 60.0);
    close("maximum width", geometry["b"].border.width(), 20.0);
    close("maximum height", geometry["b"].border.height(), 24.0);
}

#[test]
#[should_panic(expected = "displaced.child centre x: actual")]
fn harness_rejects_a_three_pixel_displacement() {
    let geometry = Harness::load(&centred_document("row", true)).geometry();
    let actual = geometry["child"].border.center().x;
    let expected = geometry["root"].content.center().x;
    // The extra 6px leading spacer steals 6px from the two equal flexible
    // spacers, moving the child by 3px. This is a document/layout fault, not
    // a fabricated measurement. Verify its magnitude before provoking the
    // exact assertion used by the positive control. Setup panics cannot pass.
    close("deliberate displacement magnitude", actual - expected, 3.0);
    close("displaced.child centre x", actual, expected);
}

#[test]
// P1: fix authored-width text centring (NoWrap uses unbounded text bounds),
// then remove this ignore. Do not accept the full-width UI node as proof.
#[ignore = "P1: centre the measured label box within the authored text width"]
fn authored_width_centres_the_measured_label_box() {
    let geometry = Harness::load(
        r#"
root: {widget: "column", fill: true, padding: 16, align: "start", children: ["label"]}
label: {widget: "text", text: "Clock", width: 240, align: "center"}
"#,
    )
    .geometry();
    let label = &geometry["label"];
    close("authored text width", label.border.width(), 240.0);
    close(
        "UI label node centre",
        label.label_node.unwrap().center().x,
        label.content.center().x,
    );
    let measured = label.label_box.unwrap();
    assert!(measured.width() > 0.0 && measured.width() < 240.0);
    close(
        "measured label box centre",
        measured.center().x,
        label.content.center().x,
    );
}
