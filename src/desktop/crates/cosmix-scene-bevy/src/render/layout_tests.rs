//! P0a: document -> scheduled reconciliation -> Taffy -> logical geometry.
//! No window/render backend. Keep the platform setup in sync with bterm's
//! `layout_tests::layout_app`. These tests deliberately do not inspect styles.
use super::*;
use bevy::camera::{CameraPlugin, ComputedCameraValues, RenderTargetInfo, Viewport};
use bevy::reflect::ReflectRef;
use bevy::text::TextLayoutInfo;
use bevy::ui::{UiPlugin, widget::TextNodeFlags};
use cosmix_shell::runtime::SceneVerb;

mod legacy;

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
        Self::load_options(body, false, false)
    }

    fn load_options(body: &str, frozen: bool, theme: bool) -> Self {
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
        if frozen {
            app.add_systems(
                Update,
                frozen_layout_once
                    .after(reconcile)
                    .run_if(bevy::ecs::schedule::common_conditions::run_once),
            );
        }
        if theme {
            // Elision belongs to this production plugin, not ScenePlugin.
            // Pin the free fixture face and its authored size: host SF/default
            // role metrics change the fractional physical-pixel rounding slack
            // between the shaped run and its ceil-rounded UI measurement.
            let mut fonts = bevy::text::FontCx::default();
            fonts.collection = fontique::Collection::new(fontique::CollectionOptions {
                shared: false,
                system_fonts: false,
            });
            fonts.collection.register_fonts(
                Font::from_bytes(include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cosmix-comp/assets/fonts/DejaVuSans.ttf"
                )).to_vec()).data,
                None,
            );
            app.insert_resource(fonts)
                .insert_resource(ctk::theme::CtkTypography::without_environment())
                .add_plugins(ctk::theme::CtkThemePlugin::isolated());
            let mut spec = ctk::theme::ThemeSpec::builtin();
            spec.typography.family = "DejaVu Sans".into();
            spec.typography.body_px = 13.0;
            spec.typography.weight = 400;
            app.world_mut().write_message(ctk::theme::ApplyTheme(spec));
        }
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

    fn patch(&mut self, path: &str, value: Value) {
        self.app
            .world_mut()
            .resource_mut::<SceneStore>()
            .request(
                SceneVerb::Patch,
                "",
                &json!({"scene":"layout", "path":path, "value":value}),
            )
            .unwrap();
        self.settle();
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
// P1 regression: measure the shaped run, not just its UI wrapper.
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

#[test]
fn text_centring_follows_legacy_fill_and_explicit_flex_allocation() {
    for sizing in ["fill: true", "grow: 1, basis: 0"] {
        let body = format!(
            r#"
root: {{widget: "row", fill: true, align: "center", children: ["before", "label", "after"]}}
before: {{widget: "spacer", size: 20}}
label: {{widget: "text", text: "Clock", align: "center", {sizing}}}
after: {{widget: "spacer", size: 20}}
"#
        );
        let g = Harness::load(&body).geometry();
        close("allocated text width", g["label"].border.width(), 760.0);
        close(
            "allocated text centre",
            g["label"].label_box.unwrap().center().x,
            g["label"].content.center().x,
        );
    }
}

fn frozen_layout_once(world: &mut World) {
    world.resource_scope(|world, store: Mut<SceneStore>| {
        let entry = &store.scenes["layout"];
        let mounted = entry.mounted.as_ref().unwrap();
        for (id, view) in &mounted.nodes {
            // Start from the native spawn defaults, not P1's modified Node.
            let base = world.get::<SceneLayoutBase>(view.root).unwrap().0.clone();
            world.entity_mut(view.root).insert(base);
            if let Some(label) = view.label {
                world.entity_mut(label).insert(Node::default());
            }
            legacy::update(world, &entry.tree, id, &entry.tree.nodes[id], None, view);
        }
    });
}

fn same_rect(context: &str, actual: Rect, expected: Rect) {
    for axis in 0..2 {
        close(
            &format!("{context} min[{axis}]"),
            actual.min[axis],
            expected.min[axis],
        );
        close(
            &format!("{context} max[{axis}]"),
            actual.max[axis],
            expected.max[axis],
        );
    }
}

#[test]
fn legacy_fixture_geometry_is_frozen() {
    // Static reconstructions of programmatically generated Quoin shapes, not
    // a claim to have run the citizen or captured an operator's live state.
    // Independent ID manifests make deletion from BOTH mappings visible.
    // These freeze coverage, not geometry; update deliberately with fixtures.
    let mut differences = Vec::new();
    let mut coverage_errors = Vec::new();
    let mut summaries = Vec::new();
    let mut unchanged_focus = Vec::new();
    let mut total_compared = 0;
    let mut total_expected = 0;
    const FOCUS: &[&str] = &[
        "root",
        "fill",
        "clock",
        "clock_col",
        "clock_time",
        "clock_date",
        "peek",
    ];
    for (name, body, expected_ids) in [
        (
            "panel",
            include_str!("layout_tests/fixtures/panel.mix"),
            "root launcher launcher_icon gap1 pager ws1 ws1_t ws2 ws2_t gap2 tasks task task_icon task_label fill status status_icon status_count clock clock_col clock_time clock_date peek peek_icon",
        ),
        (
            "popup",
            include_str!("layout_tests/fixtures/popup.mix"),
            "root heading previous month next week mon tue wed days day1 one day2 two day3 three notice title body",
        ),
        ("row", &gaps_document("row"), "root a b c sentinel"),
        ("column", &gaps_document("column"), "root a b c sentinel"),
    ] {
        let before = Harness::load_options(body, true, false).geometry();
        let after = Harness::load(body).geometry();
        let expected: BTreeSet<_> = expected_ids.split_whitespace().map(str::to_owned).collect();
        total_expected += expected.len();
        for (side, geometry) in [("old", &before), ("new", &after)] {
            let ids: BTreeSet<_> = geometry.keys().cloned().collect();
            for id in expected.difference(&ids) {
                coverage_errors.push(format!("{name}/{id}: missing from {side} mapping"));
            }
            for id in ids.difference(&expected) {
                coverage_errors.push(format!(
                    "{name}/{id}: unexpected {side} node; update the coverage manifest deliberately"
                ));
            }
        }
        // Include unmanifested nodes too: a coverage error must not hide their
        // geometry or stop any later fixture from being compared.
        let ids: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
        let mut compared = 0;
        let first_difference = differences.len();
        for id in ids {
            let old = before.get(&id);
            let new = after.get(&id);
            if old.is_some() && new.is_some() {
                compared += 1;
            }
            let magnitude = match (old, new) {
                (Some(old), Some(new)) => [
                    freeze_rect_delta(Some(old.border), Some(new.border)),
                    freeze_rect_delta(Some(old.content), Some(new.content)),
                    freeze_rect_delta(old.label_box, new.label_box),
                ]
                .into_iter()
                .fold(0.0_f32, f32::max),
                _ => f32::INFINITY,
            };
            // Restoring the zero text-wrapper minimum after 7456b56e /
            // 1a1c2598 restores these legacy rectangles exactly. Do not accept
            // a sub-tolerance movement or re-freeze the containment faults.
            if magnitude > 0.0 {
                differences.push((
                    magnitude,
                    format!("{name}/{id}"),
                    freeze_node_table(old, new),
                ));
            } else if name == "panel" && FOCUS.contains(&id.as_str()) {
                // Include exact endpoints for the diagnostic chain even when
                // identical, so a fixed right edge need not be guessed.
                unchanged_focus.push(format!("{name}/{id}\n{}", freeze_node_table(old, new)));
            }
        }
        let differing = differences.len() - first_difference;
        total_compared += compared;
        summaries.push(format!(
            "{name}: {compared} nodes compared, {differing} differ; expected {}, old {}, new {}",
            expected.len(),
            before.len(),
            after.len()
        ));
    }
    differences.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut report = format!(
        "{total_compared} nodes compared, {} differ; {total_expected} expected; {} coverage errors\n{}\n\
         Logical px; exact equality required; delta = new - old.\n\
         Sorted by maximum absolute edge-coordinate delta across border/content/text-run boxes.\n\
         Missing/non-finite geometry ranks first. Width/height deltas are diagnostic.\n",
        differences.len(),
        coverage_errors.len(),
        summaries.join("\n"),
    );
    for error in &coverage_errors {
        report.push_str(&format!("COVERAGE ERROR: {error}\n"));
    }
    for (magnitude, id, table) in &differences {
        report.push_str(&format!("\n{id} | max edge delta {magnitude:.3}\n{table}"));
    }
    if !unchanged_focus.is_empty() {
        report.push_str("\nPanel diagnostic nodes exactly unchanged:\n");
        report.push_str(&unchanged_focus.join("\n"));
    }
    // Also emit the coverage summary on success (visible with --nocapture).
    // Font/camera/readback validity guards still fail immediately: invalid
    // measurements cannot be presented as a geometry comparison.
    if differences.is_empty() && coverage_errors.is_empty() {
        eprintln!("{report}");
    }
    assert!(
        differences.is_empty() && coverage_errors.is_empty(),
        "{report}"
    );
}

fn freeze_rect_delta(old: Option<Rect>, new: Option<Rect>) -> f32 {
    match (old, new) {
        (None, None) => 0.0,
        (Some(old), Some(new)) => [old.min.x, old.min.y, old.max.x, old.max.y]
            .into_iter()
            .zip([new.min.x, new.min.y, new.max.x, new.max.y])
            .map(|(old, new)| {
                if old.is_finite() && new.is_finite() {
                    (new - old).abs()
                } else {
                    f32::INFINITY
                }
            })
            .fold(0.0, f32::max),
        _ => f32::INFINITY,
    }
}

fn freeze_node_table(old: Option<&Geometry>, new: Option<&Geometry>) -> String {
    let mut table = String::from(
        "box       | value |       left        top      right     bottom      width     height\n",
    );
    for (name, old, new) in [
        ("border", old.map(|g| g.border), new.map(|g| g.border)),
        ("content", old.map(|g| g.content), new.map(|g| g.content)),
        (
            "text-run",
            old.and_then(|g| g.label_box),
            new.and_then(|g| g.label_box),
        ),
    ] {
        let values = |rect: Rect| {
            [
                rect.min.x,
                rect.min.y,
                rect.max.x,
                rect.max.y,
                rect.width(),
                rect.height(),
            ]
        };
        let old = old.map(values);
        let new = new.map(values);
        let delta = old
            .zip(new)
            .map(|(old, new)| std::array::from_fn::<_, 6, _>(|i| new[i] - old[i]));
        for (kind, values) in [("old", old), ("new", new), ("delta", delta)] {
            let cells = values.map_or_else(
                || "— (absent / not applicable)".to_owned(),
                |values| {
                    values
                        .into_iter()
                        .map(|value| format!("{value:>10.3}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                },
            );
            table.push_str(&format!("{name:<9} | {kind:<5} | {cells}\n"));
        }
    }
    table
}

#[test]
fn justify_distributes_real_boxes_on_both_main_axes() {
    for (family, axis) in [("row", 0), ("column", 1)] {
        for justify in ["start", "center", "end", "between", "around", "evenly"] {
            let body = format!(
                r#"
root: {{widget: "{family}", fill: true, padding: 16, gap: 12, justify: "{justify}", children: ["a", "b", "c"]}}
a: {{widget: "text", text: "a", min_width: 40, max_width: 40, min_height: 40, max_height: 40}}
b: {{widget: "spacer", size: 40}}
c: {{widget: "spacer", size: 40}}
"#
            );
            let g = Harness::load(&body).geometry();
            let free = g["root"].content.size()[axis] - 3.0 * 40.0 - 2.0 * 12.0;
            let (leading, extra_gap) = match justify {
                "center" => (free / 2.0, 0.0),
                "end" => (free, 0.0),
                "between" => (0.0, free / 2.0),
                "around" => (free / 6.0, free / 3.0),
                "evenly" => (free / 4.0, free / 4.0),
                _ => (0.0, 0.0),
            };
            for (index, id) in ["a", "b", "c"].iter().enumerate() {
                close(
                    &format!("{family}/{justify}/{id}"),
                    g[*id].border.min[axis],
                    g["root"].content.min[axis] + leading + index as f32 * (52.0 + extra_gap),
                );
            }
        }
    }
}

#[test]
fn align_self_overrides_only_the_child_cross_axis() {
    for (alignment, expected_top, expected_height) in [
        ("auto", 0.0, 24.0),
        ("start", 0.0, 24.0),
        ("center", 88.0, 24.0),
        ("end", 176.0, 24.0),
        ("stretch", 0.0, 200.0),
    ] {
        let body = format!(
            r#"
root: {{widget: "row", fill: true, max_height: 200, align: "start", children: ["label"]}}
label: {{widget: "text", text: "x", min_height: 24, align_self: "{alignment}"}}
"#
        );
        let g = Harness::load(&body).geometry();
        close(
            alignment,
            g["label"].border.min.y - g["root"].content.min.y,
            expected_top,
        );
        close(alignment, g["label"].border.height(), expected_height);
        close(
            "cross-axis override preserves main-axis start",
            g["label"].border.min.x,
            g["root"].content.min.x,
        );
    }
}

#[test]
fn explicit_flex_sizes_and_bounds_are_measured() {
    for (a, b, expected) in [
        ("grow: 1, basis: 0", "grow: 3, basis: 0", [100.0, 300.0]),
        (
            "width: 300, shrink: 1",
            "width: 300, shrink: 3",
            [250.0, 150.0],
        ),
        ("grow: 0, basis: 80", "grow: 0, basis: 120", [80.0, 120.0]),
    ] {
        let body = format!(
            r#"
root: {{widget: "row", fill: true, max_width: 400, children: ["a", "b"]}}
a: {{widget: "text", text: "a", min_width: 0, {a}}}
b: {{widget: "text", text: "b", min_width: 0, {b}}}
"#
        );
        let g = Harness::load(&body).geometry();
        for (id, width) in ["a", "b"].into_iter().zip(expected) {
            close(id, g[id].border.width(), width);
        }
    }
    let g = Harness::load(
        r#"
root: {widget: "row", fill: true, children: ["a", "b", "sentinel"]}
a: {widget: "spacer", size: 40, min_width: 80, min_height: 60}
b: {widget: "spacer", size: 40, max_width: 20, max_height: 24}
sentinel: {widget: "text", text: "Measured"}
"#,
    )
    .geometry();
    close("authored min width", g["a"].border.width(), 80.0);
    close("authored min height", g["a"].border.height(), 60.0);
    close("authored max width", g["b"].border.width(), 20.0);
    close("authored max height", g["b"].border.height(), 24.0);
}

#[test]
fn axis_gaps_and_each_padding_side_override_the_shorthand() {
    for (family, axis, expected_gap) in [("row", 0, 20.0), ("column", 1, 8.0)] {
        let body = gaps_document(family).replace("gap: 12", "gap: 12, row_gap: 8, column_gap: 20")
            .replace("padding: 16", "padding: 16, padding_left: 4, padding_right: 8, padding_top: 12, padding_bottom: 20");
        let mut harness = Harness::load(&body);
        let g = harness.geometry();
        close(
            "left",
            g["root"].content.min.x - g["root"].border.min.x,
            4.0,
        );
        close(
            "right",
            g["root"].border.max.x - g["root"].content.max.x,
            8.0,
        );
        close(
            "top",
            g["root"].content.min.y - g["root"].border.min.y,
            12.0,
        );
        close(
            "bottom",
            g["root"].border.max.y - g["root"].content.max.y,
            20.0,
        );
        close(
            "axis gap",
            g["b"].border.min[axis] - g["a"].border.max[axis],
            expected_gap,
        );
        harness.patch(
            if axis == 0 {
                "root.column_gap"
            } else {
                "root.row_gap"
            },
            Value::Null,
        );
        harness.patch("root.padding_left", Value::Null);
        let g = harness.geometry();
        close(
            "gap clear restores shorthand",
            g["b"].border.min[axis] - g["a"].border.max[axis],
            12.0,
        );
        close(
            "padding clear restores shorthand",
            g["root"].content.min.x - g["root"].border.min.x,
            16.0,
        );
    }
}

#[test]
fn natural_text_and_overflow_stay_single_line() {
    let natural =
        Harness::load(r#"root: {widget: "text", text: "one two three four five"}"#).geometry();
    let intrinsic = natural["root"].label_box.unwrap();
    for align in ["left", "center", "right"] {
        let body = format!(
            r#"root: {{widget: "text", text: "one two three four five", width: 40, min_width: 0, align: "{align}"}}"#
        );
        let mut harness = Harness::load(&body);
        let g = harness.geometry();
        close(
            "no soft wrap height",
            g["root"].label_box.unwrap().height(),
            intrinsic.height(),
        );
        close(
            "no soft wrap width",
            g["root"].label_box.unwrap().width(),
            intrinsic.width(),
        );
        close("allocated wrapper", g["root"].border.width(), 40.0);
        assert!(intrinsic.width() > 40.0);
        harness.patch("root.width", Value::Null);
        harness.patch("root.min_width", Value::Null);
        same_rect(
            "natural width restored",
            harness.geometry()["root"].border,
            natural["root"].border,
        );
    }
}

#[test]
fn elision_uses_the_wrapper_budget_and_recovers_after_resize() {
    let source = "a-very-long-document-name-for-elision.txt";
    for align in ["left", "center", "right"] {
        let body = format!(
            r#"root: {{widget: "text", text: "{source}", width: 120, elide: true, align: "{align}"}}"#
        );
        let mut harness = Harness::load_options(&body, false, true);
        assert_eq!(
            harness
                .app
                .world()
                .resource::<ctk::theme::CtkTypography>()
                .effective_family
                .as_deref(),
            Some("DejaVu Sans"),
            "geometry fixture must never inherit the host desktop font"
        );
        let label = harness.app.world().resource::<SceneStore>().scenes["layout"]
            .mounted
            .as_ref()
            .unwrap()
            .nodes["root"]
            .label
            .unwrap();
        let displayed = &harness.app.world().get::<Text>(label).unwrap().0;
        let font = harness.app.world().get::<TextFont>(label).unwrap();
        assert_eq!(font.font_size, bevy::text::FontSize::Px(13.0));
        assert_eq!(font.weight.0, 400);
        assert!(
            displayed.contains('…') && displayed.ends_with(".txt"),
            "{displayed}"
        );
        let g = harness.geometry();
        let shaped = g["root"].label_box.unwrap();
        let allocated = g["root"].label_node.unwrap();
        assert!(shaped.width() <= 120.0 + TOLERANCE);
        // Bevy ceil-rounds the intrinsic UI measurement in physical pixels;
        // Parley's run advance stays fractional. Taffy aligns that allocated
        // box, so the run can end short by the font-dependent rounding slack.
        // Test allocation alignment and the run's origin separately rather
        // than mistaking that slack for a layout error (or widening tolerance).
        close("elided run origin", shaped.min.x, allocated.min.x);
        match align {
            "center" => close(
                "elided centre",
                allocated.center().x,
                g["root"].content.center().x,
            ),
            "right" => close("elided right", allocated.max.x, g["root"].content.max.x),
            _ => close("elided left", allocated.min.x, g["root"].content.min.x),
        }
        let height = shaped.height();
        harness.patch("root.width", json!(700));
        assert_eq!(harness.app.world().get::<Text>(label).unwrap().0, source);
        close(
            "elision stays single line",
            harness.geometry()["root"].label_box.unwrap().height(),
            height,
        );
        harness.patch("root.elide", json!(false));
        harness.patch("root.width", json!(120));
        assert_eq!(harness.app.world().get::<Text>(label).unwrap().0, source);
        assert!(harness.geometry()["root"].label_box.unwrap().width() > 120.0);
    }
}

#[test]
fn explicit_newlines_are_preserved_without_soft_wrapping() {
    let one = Harness::load(r#"root: {widget: "text", text: "wide words on one line"}"#).geometry();
    let two = Harness::load(r#"root: {widget: "text", text: "wide words on one line\nsecond line", width: 40, min_width: 0, align: "center"}"#).geometry();
    let line_height = one["root"].label_box.unwrap().height();
    close(
        "exactly two explicit lines",
        two["root"].label_box.unwrap().height(),
        2.0 * line_height,
    );
    assert!(two["root"].label_box.unwrap().width() > 40.0);
}

#[test]
fn clearing_new_ports_restores_fresh_document_geometry() {
    let body = r#"
root: {widget: "row", children: ["a", "b"], grow: 1, shrink: 0, basis: 200, min_width: 400, max_width: 600, min_height: 200, max_height: 400, align_self: "center", justify: "between", gap: 12, row_gap: 8, column_gap: 20, padding: 16, padding_left: 4, padding_right: 8, padding_top: 12, padding_bottom: 20}
a: {widget: "text", text: "Label", width: 40, grow: 1, shrink: 0, basis: 60, min_width: 20, max_width: 100, min_height: 40, max_height: 80, align_self: "end"}
b: {widget: "text", text: "Sentinel"}
"#;
    for id in ["root", "a"] {
        for port in [
            "grow",
            "shrink",
            "basis",
            "min_width",
            "max_width",
            "min_height",
            "max_height",
            "align_self",
            "justify",
            "row_gap",
            "column_gap",
            "padding_left",
            "padding_right",
            "padding_top",
            "padding_bottom",
        ] {
            if id == "a"
                && [
                    "justify",
                    "row_gap",
                    "column_gap",
                    "padding_left",
                    "padding_right",
                    "padding_top",
                    "padding_bottom",
                ]
                .contains(&port)
            {
                continue;
            }
            let mut harness = Harness::load(body);
            harness.patch(&format!("{id}.{port}"), Value::Null);
            let source = crate::serialised_document(
                &harness.app.world().resource::<SceneStore>().scenes["layout"].document,
            );
            let fresh_body = source
                .split_once("```mix\n")
                .unwrap()
                .1
                .split_once("```")
                .unwrap()
                .0;
            let fresh = Harness::load(fresh_body).geometry();
            for (node, actual) in harness.geometry() {
                same_rect(
                    &format!("clear {id}.{port}: {node}"),
                    actual.border,
                    fresh[&node].border,
                );
            }
        }
    }
}

#[test]
fn conflicting_patch_preserves_the_mounted_last_good_scene() {
    let mut harness = Harness::load(r#"root: {widget: "text", text: "x", fill: true}"#);
    let before = harness.geometry()["root"].border;
    let revision = harness.app.world().resource::<SceneStore>().scenes["layout"].revision;
    let error = harness
        .app
        .world_mut()
        .resource_mut::<SceneStore>()
        .request(
            SceneVerb::Patch,
            "",
            &json!({"scene":"layout", "path":"root.grow", "value":2}),
        )
        .unwrap_err();
    assert!(
        error["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "layout-conflict")
    );
    harness.settle();
    assert_eq!(
        harness.app.world().resource::<SceneStore>().scenes["layout"].revision,
        revision
    );
    same_rect(
        "rejected patch leaves geometry",
        harness.geometry()["root"].border,
        before,
    );
}
