//! Settings/Appearance is a Mix Scene loaded only where configuration declares
//! `settings.appearance`. It will move to the scene editor.
//!
//! Three control groups, per the design: the theme colour palette (applied
//! live through the existing [`QuoinSchemeSelected`] → `ApplyTheme` path —
//! no restart), the carousel motion toggle (panel doc §8: one setting for the
//! whole shell; **slide is the only implementable mode** until the scene
//! renderer can stack sibling documents in one rectangle — fade renders as a
//! disabled option carrying [`FADE_UNAVAILABLE_REASON`], never a silent
//! omission and never a workaround; the Slide/Fade marks mirror the INGESTED
//! `carousel_motion`, so a hand-configured fade still shows Fade selected —
//! it renders as slide, and config's ingest logs a `QUOIN_CONFIG` note
//! saying so), and per-edge size steppers whose writes
//! go through `ResizeCommit` — the exact command an edge-drag completion
//! materialises, so the steppers edit the same remembered per-`(output,
//! edge)` thickness the drag writes (one source of truth, panel doc §9).
//! Steppers clamp to the resize range and the output budget, exactly as a
//! drag stops at the effective limit.
//!
//! The document uses this host's Bus service name, so its `on_click` handlers come
//! back over the Bus as `shell.settings.*` verbs on this process (the scene
//! event contract: directed Bus requests to the document's citizen). The
//! mount envelope names the declared sub-panel (`window.panel`), which is how
//! scene content attaches to a declared slot whose dotted name the scene-name
//! grammar cannot express.
//!
//! Registration retries state-drivenly until the host has chrome and a real
//! output; a deliberate removal (`sub.remove` / `scene.unload` of the panel)
//! sticks — the loader retires instead of fighting the caller.
//!
//! This built-in content is the FALLBACK for a host with no scenes loader.
//! The layout is the shipped template `share/scenes/settings/scene.mix`;
//! when the loader loads that template (`quoin-settings`, page
//! `settings.appearance`), [`yield_to_external`] unloads the built-in so the
//! external load takes the page, and [`maintain`] stands aside for as long as
//! another owner holds it. When that owner goes (disable, remove, loader
//! disconnect) the built-in returns, so the page never silently disappears.
//! Either way the effects stay here: `shell.settings.{scheme,motion,size}`
//! plus the `shell.settings.get` snapshot and `shell.settings.changed`
//! notice ([`snapshot`]) the template's behaviour builds its model from.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use cosmix_scene_bevy::{SceneMount, SceneStore};
use cosmix_shell::chrome::QuoinSchemeSelected;
use cosmix_shell::core::{Edge, RESIZE_THICKNESS_RANGE};
use cosmix_shell::runtime::{
    SceneVerb, ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
    SubPanelRegistryState,
};
use ctk::app_control::verify_caller_provenance;
use ctk::bus::{BusBridge, InboundRequest};
use ctk::theme::{Mode, Scheme, ThemeSpec};
use serde_json::json;

use crate::bus_service::ShellBusDispatch;
use crate::config::SETTINGS_APPEARANCE;
use crate::config::{CarouselMotion, ShellConfig};

/// The scene document's own name; its dotted panel address is in the envelope.
const SCENE_NAME: &str = "quoin-settings";

/// Seat owner for host-loaded content. Qualified with `@` on purpose: the
/// broker's citizen-disconnect sweep only tracks unqualified local owners,
/// and this content's lifetime is the Quoin process itself, not a Bus
/// connection (see `bus_service::reconcile_citizens`).
const OWNER: &str = "quoin@host";

/// One stepper press adjusts the remembered thickness by this many logical
/// pixels, clamped to the resize range and the output budget.
pub(crate) const STEP_PX: f32 = 10.0;

/// Why the fade motion option is disabled (panel doc §8's renderer gap, the
/// same one that defers the carousel's fade). Carried in the scene UI and in
/// the `shell.settings.motion` refusal — no workaround until the renderer
/// gains the primitive.
pub(crate) const FADE_UNAVAILABLE_REASON: &str =
    "Fade needs renderer sibling\nstacking in one rectangle.\nOnly slide is available.";

/// What the rendered document last showed; a change reloads the scene.
#[derive(Clone, PartialEq)]
struct Rendered {
    scheme: String,
    motion: CarouselMotion,
    thickness: [f32; 4],
}

#[derive(Resource)]
pub(crate) struct SettingsScene {
    rendered: Option<Rendered>,
    edge: Option<Edge>,
    /// Last applied scheme name (shadow of the `QuoinSchemeSelected` stream;
    /// seeded from the persisted state at install).
    scheme: String,
    receipt: u64,
    retired: bool,
    /// Stood aside for another owner of the page; the first update after it
    /// goes only schedules the reload (the edge-move pattern), so the owner's
    /// teardown reconciles before the fallback mounts again.
    yielded: bool,
    last_refused: Option<Instant>,
}

impl Default for SettingsScene {
    fn default() -> Self {
        Self {
            rendered: None,
            edge: None,
            scheme: Scheme::Ocean.name().to_owned(),
            receipt: 0,
            retired: false,
            yielded: false,
            last_refused: None,
        }
    }
}

pub(crate) fn install(app: &mut App, smoke: bool) {
    if smoke {
        return;
    }
    // Hosts that have not installed the config reader still get the motion
    // setting's default (the embedded host opts into the schema separately).
    app.init_resource::<ShellConfig>();
    app.add_systems(Startup, declare_initial_pages.after(crate::setup));
    let settings = SettingsScene {
        scheme: initial_scheme(app.world().get_resource::<crate::state::StateStore>()),
        ..default()
    };
    app.insert_resource(settings).add_systems(
        Update,
        maintain
            .in_set(ShellRuntimeSet::Input)
            .after(crate::config::ConfigIngest)
            .after(ShellBusDispatch),
    );
}

/// Embedded hosts do not install the standalone conf.mix watcher. Initialise
/// their declarations from ShellConfig, after chrome binds its initial frame.
/// Standalone ingestion replaces these before loading
/// Settings; output replacement already carries declarations forward.
fn declare_initial_pages(world: &mut World) {
    let declarations = world.resource::<ShellConfig>().panels.clone();
    if let Err(error) = cosmix_shell::runtime::redeclare_shell_pages(world, &declarations) {
        warn!("Settings/Appearance declarations refused: {error}");
    }
}

/// Load or reload the scene whenever what it shows has changed. Retries are
/// state-driven (no timer): chrome arriving late, the placeholder output
/// being replaced, and a first connect all simply trigger the next attempt.
fn maintain(
    mut settings: ResMut<SettingsScene>,
    mut selections: MessageReader<QuoinSchemeSelected>,
    frame: Res<ShellFrameState>,
    config: Res<ShellConfig>,
    (mut registry, mut scenes): (ResMut<SubPanelRegistryState>, ResMut<SceneStore>),
    bridge: Res<BusBridge>,
    (time, mut redraw): (Res<Time>, MessageWriter<bevy::window::RequestRedraw>),
) {
    // Settings verbs publish scheme changes through the shared message path.
    for selection in selections.read() {
        settings.scheme = selection.0.name().to_owned();
    }
    if settings.retired {
        return;
    }
    // Do not reserve a seat against the embedded host's placeholder output
    // (state.rs owns the namespace rule).
    if frame
        .0
        .geometry
        .output
        .as_str()
        .starts_with(crate::state::EPHEMERAL_OUTPUT_PREFIX)
    {
        return;
    }
    // Another owner holds the page (the loader-managed template, admitted by
    // `yield_to_external`): stand aside without retiring, and come back when
    // it goes. The registry is state, so no timer is involved either way.
    if registry
        .0
        .seat(SETTINGS_APPEARANCE)
        .is_some_and(|seat| seat.owner != OWNER)
    {
        settings.rendered = None;
        settings.edge = None;
        settings.yielded = true;
        return;
    }
    if settings.yielded {
        settings.yielded = false;
        redraw.write(bevy::window::RequestRedraw);
        return;
    }
    let loaded = scenes
        .scenes_owned_by(OWNER)
        .iter()
        .any(|name| name == SCENE_NAME);
    if settings.rendered.is_some() && !loaded {
        // We loaded it and it is gone: a caller removed the panel through the
        // lifecycle verbs. That is deliberate; do not resurrect it.
        settings.retired = true;
        return;
    }
    let edge = declared_edge(&config);
    if loaded && settings.edge != edge {
        // Release the old mount before moving or withdrawing the declaration.
        // Reconcile tears down its carousel entry before a later update reloads.
        let mut mount = SceneMount {
            registry: &mut registry.0,
            output: &frame.0.geometry.output,
            owner: OWNER,
            accepted_at: settings.receipt,
        };
        let (rc, body) = scenes.dispatch(
            SceneVerb::Unload,
            "",
            &json!({"scene": SCENE_NAME}),
            &bridge,
            &mut mount,
        );
        if rc == 0 {
            settings.rendered = None;
            settings.edge = None;
            if edge.is_some() {
                // The standalone host can become idle after teardown. Arrange
                // the next update that mounts the newly declared edge.
                redraw.write(bevy::window::RequestRedraw);
            }
        } else {
            report_refusal(&mut settings, &body);
        }
        return;
    }
    let Some(edge) = edge else {
        return;
    };
    let desired = Rendered {
        scheme: settings.scheme.clone(),
        motion: config.carousel_motion,
        thickness: std::array::from_fn(|index| {
            frame.0.panel(Edge::ALL[index]).settled_thickness_px
        }),
    };
    if loaded && settings.rendered.as_ref() == Some(&desired) {
        return;
    }
    // Content refreshes keep the registration receipt: otherwise a refresh
    // could invalidate a sub.remove already accepted in this same update.
    if !loaded {
        settings.receipt = settings.receipt.saturating_add(1);
    }
    let registration = if !loaded {
        let (rc, body, command) = crate::bus_service::register_sub_panel(
            &frame.0,
            &mut registry.0,
            SETTINGS_APPEARANCE.to_owned(),
            edge,
            OWNER.to_owned(),
            settings.receipt,
            time.elapsed(),
        );
        if rc != 0 {
            report_refusal(&mut settings, &body);
            return;
        }
        command
    } else {
        None
    };
    let source = document(&desired, bridge.service_name(), edge);
    let mut mount = SceneMount {
        registry: &mut registry.0,
        output: &frame.0.geometry.output,
        owner: OWNER,
        accepted_at: settings.receipt,
    };
    let (rc, body) = scenes.dispatch(
        SceneVerb::Load,
        &source,
        &json!({"scene": SCENE_NAME}),
        &bridge,
        &mut mount,
    );
    if rc == 0 {
        // The register-only scene mount fills the carousel from this shared
        // sub.register reservation. Enqueuing its command as well would
        // register the same page twice (reconcile runs before Model).
        settings.rendered = Some(desired);
        settings.edge = Some(edge);
        settings.last_refused = None;
    } else {
        // A rejected first load must not strand its registration reservation.
        if registration.is_some() {
            registry.0.forget(SETTINGS_APPEARANCE);
        }
        report_refusal(&mut settings, &body);
    }
}

fn report_refusal(settings: &mut SettingsScene, body: &str) {
    let now = Instant::now();
    if settings
        .last_refused
        .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60))
    {
        warn!("Settings/Appearance scene refused: {body}");
        settings.last_refused = Some(now);
    }
}

/// The persisted scheme, or Ocean when none was ever chosen (the `builtin`
/// sentinel) — what the page marks as selected before any selection.
pub(crate) fn initial_scheme(store: Option<&crate::state::StateStore>) -> String {
    store
        .and_then(|store| store.scheme())
        .and_then(|name| Scheme::from_name(&name))
        .unwrap_or(Scheme::Ocean)
        .name()
        .to_owned()
}

/// The edge whose configuration declares `settings.appearance`, if any.
fn declared_edge(config: &ShellConfig) -> Option<Edge> {
    Edge::ALL.into_iter().find(|edge| {
        config.panels[edge.index()]
            .iter()
            .any(|name| name == SETTINGS_APPEARANCE)
    })
}

fn motion_name(motion: CarouselMotion) -> &'static str {
    match motion {
        CarouselMotion::Slide => "slide",
        CarouselMotion::Fade => "fade",
    }
}

/// What `shell.settings.get` answers and `shell.settings.changed` carries:
/// every choice the Appearance page shows, as data rather than presentation,
/// so an external behaviour builds the same model the fallback builds here.
/// `sizes` are settled thicknesses (a drag in progress does not change them);
/// `motion` is the INGESTED value, fade included; `page_owner` names who
/// currently serves the page (`quoin@host` = this built-in fallback).
pub(crate) fn snapshot(
    scheme: &str,
    config: &ShellConfig,
    frame: &ShellFrame,
    page_owner: Option<&str>,
) -> serde_json::Value {
    let mut sizes = serde_json::Map::new();
    for edge in Edge::ALL {
        sizes.insert(
            edge_name(edge).to_owned(),
            json!(frame.panel(edge).settled_thickness_px),
        );
    }
    // Constant per build; computed once because the notice compares a fresh
    // snapshot every update.
    static SCHEMES: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let schemes = SCHEMES.get_or_init(|| {
        Scheme::ALL
            .into_iter()
            .map(|scheme| json!({"name": scheme.name(), "accent": scheme_hex(scheme)}))
            .collect()
    });
    let range = [
        *RESIZE_THICKNESS_RANGE.start(),
        *RESIZE_THICKNESS_RANGE.end(),
    ];
    json!({
        "scheme": scheme,
        "schemes": schemes,
        "motion": motion_name(config.carousel_motion),
        "motions": [
            {"name": "slide", "available": true},
            {"name": "fade", "available": false, "reason": FADE_UNAVAILABLE_REASON},
        ],
        "fade_reason": FADE_UNAVAILABLE_REASON,
        "sizes": sizes,
        "step_px": STEP_PX,
        "range_px": range,
        "edge": declared_edge(config).map(edge_name),
        "page_owner": page_owner,
    })
}

/// A Bus `shell.scene.load` whose envelope names the `settings.appearance`
/// page, while this built-in fallback holds it: unload the built-in first so
/// the external load (the loader's `quoin-settings` template) takes the page
/// in the same dispatch. Nothing to do for any other load, or when the
/// built-in is not loaded — ordinary ownership rules then apply. A template
/// authored for another edge than the declaration is refused (the page stays
/// with the fallback) rather than mounted where the configuration does not
/// put it. Returns the refusal, or `None` to proceed with the load.
///
/// The built-in is reset, not retired, so a load that is refused after this
/// point simply lets `maintain` load the fallback again.
pub(crate) fn yield_to_external(
    body: &str,
    args: &serde_json::Value,
    settings: Option<&mut SettingsScene>,
    scenes: &mut SceneStore,
    registry: &mut cosmix_shell::core::SubPanelRegistry,
    output: &cosmix_shell::core::OutputKey,
    bridge: &BusBridge,
) -> Option<(u8, String)> {
    let source = args["source"].as_str().unwrap_or(body);
    // Unparseable documents are the load's own refusal to report.
    let document = cosmix_scene::parse(source).ok()?;
    let window = document.window.as_ref()?;
    if window["panel"].as_str() != Some(SETTINGS_APPEARANCE) {
        return None;
    }
    if !scenes
        .scenes_owned_by(OWNER)
        .iter()
        .any(|name| name == SCENE_NAME)
    {
        return None;
    }
    let held = registry.seat(SETTINGS_APPEARANCE).map(|seat| seat.edge);
    // The scene renderer mounts an unknown or absent edge on the right.
    let requested = window["edge"]
        .as_str()
        .and_then(|edge| crate::bus_service::parse_edge(edge.to_owned()))
        .unwrap_or(Edge::Right);
    if let Some(held) = held
        && held != requested
    {
        return Some((
            10,
            json!({
                "error_code": "SETTINGS_EDGE_MISMATCH",
                "message": format!(
                    "settings.appearance is declared on the {} edge; the document mounts on {}",
                    edge_name(held),
                    edge_name(requested)
                ),
                "declared": edge_name(held),
                "requested": edge_name(requested),
            })
            .to_string(),
        ));
    }
    let accepted_at = settings.as_deref().map_or(0, |settings| settings.receipt);
    let mut mount = SceneMount {
        registry,
        output,
        owner: OWNER,
        accepted_at,
    };
    let (rc, reply) = scenes.dispatch(
        SceneVerb::Unload,
        "",
        &json!({"scene": SCENE_NAME}),
        bridge,
        &mut mount,
    );
    if rc != 0 {
        return Some((rc, reply));
    }
    if let Some(settings) = settings {
        settings.rendered = None;
        settings.edge = None;
        settings.yielded = true;
    }
    info!("Settings/Appearance built-in yields settings.appearance to an external load");
    None
}

/// Author the content in Mix Scene data; only live values and the host's
/// Bus address come from Rust. The list gives the content its own scrolling.
fn document(desired: &Rendered, citizen: &str, edge: Edge) -> String {
    let mut schemes = serde_json::Map::new();
    for scheme in Scheme::ALL {
        let selected = scheme.name() == desired.scheme;
        schemes.insert(
            scheme.name().to_owned(),
            json!({
                "accent": scheme_hex(scheme),
                "background": if selected { "#2a2e32" } else { "#00000000" },
                "mark": if selected { "●" } else { "○" },
            }),
        );
    }
    let mut sizes = serde_json::Map::new();
    for edge in Edge::ALL {
        sizes.insert(
            edge_name(edge).to_owned(),
            json!(format!(
                "{} px",
                desired.thickness[edge.index()].round() as i32
            )),
        );
    }
    let model = json!({
        "schemes": schemes, "sizes": sizes, "fade_reason": FADE_UNAVAILABLE_REASON,
        // The marks mirror the INGESTED motion, fade included: a hand-edited
        // conf.mix `carousel_motion: "fade"` selects Fade here (it still
        // renders as slide; the QUOIN_CONFIG ingest note and the reason text
        // below carry that), so the panel never claims Slide is configured
        // when it is not.
        "motion": {
            "slide": if desired.motion == CarouselMotion::Slide {
                "● Slide"
            } else {
                "○ Slide"
            },
            "fade": if desired.motion == CarouselMotion::Fade {
                "● Fade (unavailable)"
            } else {
                "○ Fade (unavailable)"
            },
        },
    });
    let window = json!({
        "kind": "edge", "edge": edge.as_str(), "title": "Settings", "panel": SETTINGS_APPEARANCE,
    });
    format!(
        "---\nscene: 1\nname: {SCENE_NAME}\ncitizen: {citizen}\nwindow: {window}\nmodel: {model}\n---\n{}",
        template_body(),
    )
}

/// The shipped template (`share/scenes/settings/scene.mix`) is the one
/// authored layout. The fallback reuses its node block verbatim under its own
/// envelope: this host as citizen, the declared edge and a live model.
const TEMPLATE: &str = include_str!("../../../../../share/scenes/settings/scene.mix");

/// Everything after the template's AMP header (the fenced node block).
fn template_body() -> &'static str {
    TEMPLATE
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body)
        .expect("the shipped settings template has an AMP header")
}

/// The scheme's accent as `#rrggbbaa` — the same colour the chrome scheme
/// dots show for it.
fn scheme_hex(scheme: Scheme) -> String {
    let colour = ThemeSpec::from_scheme(scheme, Mode::Dark)
        .colors
        .control_active
        .to_srgba();
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        (colour.red * 255.0).round() as u8,
        (colour.green * 255.0).round() as u8,
        (colour.blue * 255.0).round() as u8,
        (colour.alpha * 255.0).round() as u8
    )
}

fn edge_name(edge: Edge) -> &'static str {
    edge.as_str()
}

// ── the scene's Bus verbs ────────────────────────────────────────────────

/// A live preference the verb changes beside its reply/command.
#[derive(Debug, PartialEq)]
enum SettingsWrite {
    Scheme(Scheme),
    Motion(CarouselMotion),
}

/// Dispatch a `shell.settings.*` verb (the scene's `on_click` handlers, and
/// the same surface for any script): the scheme selection rides the existing
/// live-apply message; a motion write updates the setting chunk 6 ingests;
/// thickness writes return the drag-completion command for the caller to
/// enqueue through the normal receipt path. `applied_scheme` is the host's
/// shadow of the selection (the snapshot's `scheme`), updated with the write
/// so a `shell.settings.get` later in the same drain already reports it.
pub(crate) fn dispatch_verb(
    request: &InboundRequest,
    frame: &ShellFrame,
    config: &mut ShellConfig,
    config_path: &std::path::Path,
    schemes: &mut MessageWriter<'_, QuoinSchemeSelected>,
    applied_scheme: &mut String,
    at: Duration,
) -> (u8, String, Option<ShellCommand>) {
    let (rc, body, command, write) = plan_verb(request, frame, at);
    match write {
        Some(SettingsWrite::Scheme(scheme)) => {
            schemes.write(QuoinSchemeSelected(scheme));
            *applied_scheme = scheme.name().to_owned();
        }
        Some(SettingsWrite::Motion(motion)) => {
            if let Err(error) = crate::config::write_carousel_motion(config_path, motion) {
                return (10, json!({"error": error}).to_string(), None);
            }
            config.carousel_motion = motion;
        }
        None => {}
    }
    (rc, body, command)
}

/// Pure decision half of the settings verbs: reply, optional shell command,
/// optional live preference write.
fn plan_verb(
    request: &InboundRequest,
    frame: &ShellFrame,
    at: Duration,
) -> (u8, String, Option<ShellCommand>, Option<SettingsWrite>) {
    if let Err(error) = verify_caller_provenance(request) {
        return (
            10,
            json!({"error": format!("settings caller provenance: {error:?}")}).to_string(),
            None,
            None,
        );
    }
    match request.command.as_str() {
        "shell.settings.scheme" => {
            // Explicit name for scripts; the scene's node id otherwise
            // (`scheme_ocean` — the quoin-panel node-addressing pattern).
            let name = crate::bus_service::argument(request, "name").or_else(|| {
                event_node(request).and_then(|node| node.strip_prefix("scheme_").map(str::to_owned))
            });
            let Some(scheme) = name.as_deref().and_then(Scheme::from_name) else {
                return (
                    10,
                    json!({"error": "settings.scheme requires a known scheme name"}).to_string(),
                    None,
                    None,
                );
            };
            (
                0,
                json!({"accepted": true, "scheme": scheme.name()}).to_string(),
                None,
                Some(SettingsWrite::Scheme(scheme)),
            )
        }
        "shell.settings.motion" => {
            let token = crate::bus_service::argument(request, "motion").or_else(|| {
                event_node(request).and_then(|node| node.strip_prefix("motion_").map(str::to_owned))
            });
            match token.as_deref() {
                Some("slide") => (
                    0,
                    json!({"accepted": true, "motion": "slide"}).to_string(),
                    None,
                    Some(SettingsWrite::Motion(CarouselMotion::Slide)),
                ),
                // The single renderer gap that also defers the carousel's
                // fade: refuse the write, carrying the reason.
                Some("fade") => (
                    10,
                    json!({
                        "error_code": "MOTION_FADE_UNAVAILABLE",
                        "error": FADE_UNAVAILABLE_REASON,
                    })
                    .to_string(),
                    None,
                    None,
                ),
                _ => (
                    10,
                    json!({"error": "settings.motion requires motion slide or fade"}).to_string(),
                    None,
                    None,
                ),
            }
        }
        "shell.settings.size" => {
            let edge = crate::bus_service::argument(request, "edge")
                .and_then(crate::bus_service::parse_edge)
                .or_else(|| {
                    event_node(request).and_then(|node| {
                        let rest = node.strip_prefix("size_")?;
                        let (edge, _) = rest.split_once('_')?;
                        crate::bus_service::parse_edge(edge.to_owned())
                    })
                });
            let Some(edge) = edge else {
                return (
                    10,
                    json!({"error": "settings.size requires an edge argument"}).to_string(),
                    None,
                    None,
                );
            };
            // The scene's steppers carry no arguments on the wire: the node id
            // names the direction (`size_left_plus`).
            let delta = crate::bus_service::number_argument(request, "delta_px")
                .filter(|value| value.is_finite() && value.abs() <= f32::MAX as f64)
                .map(|value| value as f32)
                .or_else(|| match event_node(request).as_deref() {
                    Some(node) => node
                        .strip_prefix("size_")
                        .and_then(|rest| rest.rsplit_once('_').map(|(_, dir)| dir.to_owned()))
                        .filter(|dir| dir == "minus" || dir == "plus")
                        .map(|dir| if dir == "minus" { -STEP_PX } else { STEP_PX }),
                    None => None,
                });
            let Some(delta) = delta else {
                return (
                    10,
                    json!({"error": "settings.size requires a delta_px argument"}).to_string(),
                    None,
                    None,
                );
            };
            let panel = frame.panel(edge);
            // Step from the remembered value (settled), then clamp like a
            // drag: the resize range first, the output budget last — a drag
            // stops at the effective limit, and so does a stepper.
            let target = (panel.settled_thickness_px + delta)
                .clamp(
                    *RESIZE_THICKNESS_RANGE.start(),
                    *RESIZE_THICKNESS_RANGE.end(),
                )
                .min(panel.max_thickness_px);
            if (target - panel.settled_thickness_px).abs() < f32::EPSILON {
                return (
                    0,
                    json!({"accepted": true, "edge": edge_name(edge), "thickness_px": target, "unchanged": true})
                        .to_string(),
                    None,
                    None,
                );
            }
            (
                0,
                json!({"accepted": true, "edge": edge_name(edge), "thickness_px": target})
                    .to_string(),
                Some(ShellCommand {
                    output: frame.geometry.output.clone(),
                    at,
                    kind: ShellCommandKind::ResizeCommit {
                        edge,
                        thickness_px: target,
                    },
                }),
                None,
            )
        }
        _ => (
            10,
            json!({"error": "unknown settings verb"}).to_string(),
            None,
            None,
        ),
    }
}

/// The scene event body's `node` id (UI handlers arrive with
/// `{scene,node,kind}` and no verb arguments).
fn event_node(request: &InboundRequest) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&request.body)
        .ok()?
        .get("node")?
        .as_str()
        .map(|node| node.split_once('@').map_or(node, |(id, _)| id).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, PanelInput, ShellModel};
    use cosmix_shell::runtime::{ShellRuntimePlugin, ShellRuntimeSet};
    use std::path::Path;

    fn model_for(output: &str) -> ShellModel {
        let mut model = ShellModel::new(
            OutputKey::new(output).unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let registry = crate::tests::fixture_registry();
        for edge in Edge::ALL {
            model.set_carousel(edge, registry.carousel(edge));
        }
        model
    }

    fn verb_request(command: &str, body: serde_json::Value) -> InboundRequest {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("broker_origin".to_owned(), "local".to_owned());
        InboundRequest {
            connection_generation: 1,
            from: "peer".to_owned(),
            command: command.to_owned(),
            headers,
            body: body.to_string(),
            reply_id: Some("1".to_owned()),
        }
    }

    /// A scene UI handler request: verb as command, node id in the body.
    fn event_request(command: &str, node: &str) -> InboundRequest {
        let mut request = verb_request(command, json!({}));
        request.body = json!({"scene": SCENE_NAME, "node": node, "kind": "click"}).to_string();
        request
    }

    fn frame_for(output: &str) -> ShellFrame {
        ShellFrame::from_model(&model_for(output))
    }

    /// What a restart restores from a persisted state file: the remembered
    /// thickness per edge, read through the real restore path.
    fn restored_thickness(path: &Path, edge: Edge) -> f32 {
        let mut model = model_for("DP-1");
        crate::state::StateStore::load(Some(path.to_owned())).restore(&mut model);
        model.panel(edge).thickness_px
    }

    fn settings_test_app(source: &str) -> App {
        use cosmix_shell::chrome::{
            QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts,
            spawn_quoin_chrome,
        };
        let mut app = App::new();
        let mut model = model_for("DP-1");
        for edge in Edge::ALL {
            model.set_carousel(
                edge,
                cosmix_shell::core::Carousel::new(std::iter::empty::<&str>()).unwrap(),
            );
        }
        model.suppress_empty_edges(true);
        app.add_plugins((
            MinimalPlugins,
            ShellRuntimePlugin::new(model),
            QuoinChromePlugin,
        ))
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<SceneStore>()
        .add_systems(
            Update,
            cosmix_scene_bevy::reconcile_scene_mounts
                .after(ShellRuntimeSet::Input)
                .before(ShellRuntimeSet::Model),
        );
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
        let (bridge, _peer) = ctk::bus::test_bridge("settings-test");
        world.insert_resource(bridge);
        world.insert_resource(crate::state::StateStore::load(None));
        crate::config::ingest_test_config(world, source);
        install(&mut app, false);
        app.update();
        app
    }

    #[test]
    fn settings_requires_its_named_declaration_on_any_edge() {
        for source in ["{}", r#"{panels: {right: ["scene-tools"]}}"#] {
            let app = settings_test_app(source);
            assert!(
                app.world()
                    .resource::<SceneStore>()
                    .scenes_owned_by(OWNER)
                    .is_empty()
            );
            assert!(
                app.world()
                    .resource::<SubPanelRegistryState>()
                    .0
                    .seat(SETTINGS_APPEARANCE)
                    .is_none()
            );
            for panel in &app.world().resource::<ShellFrameState>().0.panels {
                assert!(panel.page_ids.is_empty());
                assert!(!panel.mapped);
                assert_eq!(panel.exclusive_zone_px, 0.0);
            }
        }
        for edge in Edge::ALL {
            // It need not be the primary, and must not occupy the preceding slot.
            let app = settings_test_app(&format!(
                "{{panels: {{{}: [\"scene-tools\", \"settings.appearance\"]}}}}",
                edge.as_str()
            ));
            let registry = &app.world().resource::<SubPanelRegistryState>().0;
            assert_eq!(registry.seat(SETTINGS_APPEARANCE).unwrap().edge, edge);
            assert!(registry.seat("scene-tools").is_none());
            let frame = &app.world().resource::<ShellFrameState>().0;
            for other in Edge::ALL {
                let panel = frame.panel(other);
                if other == edge {
                    assert_eq!(panel.page_ids.as_ref(), [SETTINGS_APPEARANCE]);
                    assert_eq!(panel.active_page_id.as_deref(), Some(SETTINGS_APPEARANCE));
                } else {
                    assert!(panel.page_ids.is_empty());
                }
                assert!(!panel.mapped);
            }
        }
    }

    #[test]
    fn settings_follows_config_add_move_and_withdrawal() {
        let mut app = settings_test_app("{}");
        for source in [
            r#"{panels: {left: ["settings.appearance"]}}"#,
            r#"{panels: {bottom: ["settings.appearance"]}}"#,
            "{}",
        ] {
            crate::config::ingest_test_config(app.world_mut(), source);
            app.update();
            app.update();
            let registry = &app.world().resource::<SubPanelRegistryState>().0;
            let expected = if source.contains("left") {
                Some(Edge::Left)
            } else if source.contains("bottom") {
                Some(Edge::Bottom)
            } else {
                None
            };
            assert_eq!(
                registry.seat(SETTINGS_APPEARANCE).map(|seat| seat.edge),
                expected
            );
            for edge in Edge::ALL {
                let panel = app.world().resource::<ShellFrameState>().0.panel(edge);
                assert_eq!(
                    panel.page_ids.contains(&SETTINGS_APPEARANCE.to_owned()),
                    expected == Some(edge)
                );
            }
            assert!(!app.world().resource::<SettingsScene>().retired);
        }
    }

    #[test]
    fn settings_registers_under_declared_right_primary() {
        let mut app =
            settings_test_app(r#"{panels: {right: ["settings.appearance", "test-tools"]}}"#);
        let frame = app.world().resource::<ShellFrameState>().0.clone();
        assert_eq!(
            frame.panel(Edge::Right).page_ids.as_ref(),
            &["settings.appearance"],
            "the scene must fill the declared primary slot"
        );
        assert!(!frame.panel(Edge::Right).mapped, "mounting never reveals");
        let seat = app
            .world()
            .resource::<SubPanelRegistryState>()
            .0
            .seat(SETTINGS_APPEARANCE)
            .expect("the seat is reserved under the declared name");
        assert_eq!((seat.edge, seat.owner.as_str()), (Edge::Right, OWNER));
        assert_eq!(
            app.world().resource::<SceneStore>().scenes_owned_by(OWNER),
            vec![SCENE_NAME.to_owned()]
        );
        // A deliberate removal sticks: the Model stage lands the removal,
        // the scene reconcile unloads the content, and only the NEXT pass
        // observes the loss and retires the loader.
        let accepted_at = app
            .world()
            .resource::<SubPanelRegistryState>()
            .0
            .seat(SETTINGS_APPEARANCE)
            .unwrap()
            .accepted_at;
        app.world_mut().write_message(ShellCommand {
            output: frame.geometry.output.clone(),
            at: Duration::ZERO,
            kind: ShellCommandKind::SubPanelRemove {
                edge: Edge::Right,
                name: SETTINGS_APPEARANCE.to_owned(),
                owner: OWNER.to_owned(),
                accepted_at,
            },
        });
        app.update();
        app.update();
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat(SETTINGS_APPEARANCE)
                .is_none()
        );
        assert!(app.world().resource::<SettingsScene>().retired);
    }

    /// The persistence fixture state.rs uses for the drag path.
    fn persist_app(path: &Path) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model_for("DP-1"))))
            .add_message::<QuoinSchemeSelected>()
            .add_message::<ctk::theme::ApplyTheme>()
            .add_message::<bevy::window::RequestRedraw>()
            .insert_resource(crate::state::StateStore::load(Some(path.to_owned())))
            .add_systems(
                Update,
                crate::state::persist_transitions.in_set(ShellRuntimeSet::Host),
            );
        app
    }

    fn apply_command(app: &mut App, kind: ShellCommandKind) {
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind,
        });
        app.update();
    }

    #[test]
    fn size_controls_write_same_remembered_thickness_as_edge_drag() {
        let directory = tempfile::tempdir().unwrap();
        let frame = frame_for("DP-1");
        for edge in Edge::ALL {
            let starting = frame.panel(edge).settled_thickness_px;
            let target = (starting + STEP_PX)
                .clamp(
                    *RESIZE_THICKNESS_RANGE.start(),
                    *RESIZE_THICKNESS_RANGE.end(),
                )
                .min(frame.panel(edge).max_thickness_px);
            let drag_path = directory
                .path()
                .join(format!("drag-{}.mix", edge_name(edge)));
            let step_path = directory
                .path()
                .join(format!("step-{}.mix", edge_name(edge)));
            let mut drag_app = persist_app(&drag_path);
            apply_command(
                &mut drag_app,
                ShellCommandKind::Panel {
                    edge,
                    input: PanelInput::ResizeStarted,
                },
            );
            apply_command(
                &mut drag_app,
                ShellCommandKind::Resize {
                    edge,
                    thickness_px: target,
                },
            );
            apply_command(
                &mut drag_app,
                ShellCommandKind::Panel {
                    edge,
                    input: PanelInput::ResizeCompleted,
                },
            );

            let mut step_app = persist_app(&step_path);
            let (bridge, peer) = ctk::bus::test_bridge("settings-test");
            step_app
                .insert_resource(bridge)
                .add_plugins(crate::bus_service::ShellBusPlugin);
            peer.send(event_request(
                "shell.settings.size",
                &format!("size_{}_plus@appearance", edge_name(edge)),
            ));
            step_app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
            assert_eq!(restored_thickness(&step_path, edge), target);
            assert_eq!(
                std::fs::read_to_string(&step_path).unwrap(),
                std::fs::read_to_string(&drag_path).unwrap(),
                "stepper and drag must save identical per-output state"
            );
            let mut other = model_for("DP-2");
            let unchanged = other.panel(edge).thickness_px;
            crate::state::StateStore::load(Some(step_path)).restore(&mut other);
            assert_eq!(
                other.panel(edge).thickness_px,
                unchanged,
                "a step on DP-1 must not change DP-2"
            );
        }

        // At the range floor a further minus step is an unchanged no-op, and
        // the scene's node form maps each direction and edge.
        let mut at_floor = frame_for("DP-1");
        at_floor.panels[Edge::Bottom.index()].settled_thickness_px =
            *RESIZE_THICKNESS_RANGE.start();
        let (rc, body, command, _) = plan_verb(
            &event_request("shell.settings.size", "size_bottom_minus"),
            &at_floor,
            Duration::ZERO,
        );
        assert_eq!(rc, 0, "{body}");
        assert!(command.is_none(), "a clamped no-op enqueues nothing");
        assert!(body.contains("\"unchanged\":true"), "{body}");
        let (_, _, command, _) = plan_verb(
            &event_request("shell.settings.size", "size_right_minus"),
            &frame,
            Duration::ZERO,
        );
        assert_eq!(
            command.unwrap().kind,
            ShellCommandKind::ResizeCommit {
                edge: Edge::Right,
                thickness_px: frame.panel(Edge::Right).settled_thickness_px - STEP_PX,
            }
        );
    }

    #[test]
    fn fade_is_disabled_with_reason_until_renderer_sibling_stacking() {
        assert!(
            FADE_UNAVAILABLE_REASON.contains("sibling"),
            "the reason must name the renderer sibling-stacking gap"
        );
        // The verb refuses a fade write with that reason — one truth for the
        // scene UI and any script caller.
        for request in [
            verb_request("shell.settings.motion", json!({"motion": "fade"})),
            event_request("shell.settings.motion", "motion_fade"),
        ] {
            let (rc, body, command, write) =
                plan_verb(&request, &frame_for("DP-1"), Duration::ZERO);
            assert_eq!(rc, 10, "{body}");
            assert!(command.is_none() && write.is_none());
            let error = serde_json::from_str::<serde_json::Value>(&body).unwrap();
            assert_eq!(error["error_code"], json!("MOTION_FADE_UNAVAILABLE"));
            assert_eq!(error["error"], json!(FADE_UNAVAILABLE_REASON));
        }
        // Slide stays selectable.
        for request in [
            verb_request("shell.settings.motion", json!({"motion": "slide"})),
            event_request("shell.settings.motion", "motion_slide"),
        ] {
            let (rc, body, command, write) =
                plan_verb(&request, &frame_for("DP-1"), Duration::ZERO);
            assert_eq!(rc, 0, "{body}");
            assert!(command.is_none() && matches!(write, Some(SettingsWrite::Motion(_))));
        }
        // The same dispatcher writes the field the config reader ingests,
        // preserving other values and leaving invalid files untouched.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        let source = r#"{panels: {left: ["custom"]}, carousel_motion: "fade"}"#;
        std::fs::write(&path, source).unwrap();
        let mut config = ShellConfig::parse(source).unwrap();
        let mut app = App::new();
        app.add_message::<QuoinSchemeSelected>();
        let mut writer = bevy::ecs::system::SystemState::<MessageWriter<QuoinSchemeSelected>>::new(
            app.world_mut(),
        );
        let mut applied = "ocean".to_owned();
        let (rc, body, _) = dispatch_verb(
            &event_request("shell.settings.motion", "motion_fade@appearance"),
            &frame_for("DP-1"),
            &mut config,
            &path,
            &mut writer.get_mut(app.world_mut()).unwrap(),
            &mut applied,
            Duration::ZERO,
        );
        assert_eq!(rc, 10, "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let (rc, body, _) = dispatch_verb(
            &event_request("shell.settings.motion", "motion_slide@appearance"),
            &frame_for("DP-1"),
            &mut config,
            &path,
            &mut writer.get_mut(app.world_mut()).unwrap(),
            &mut applied,
            Duration::ZERO,
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(config.carousel_motion, CarouselMotion::Slide);
        let saved = ShellConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved, config);
        std::fs::write(&path, "{invalid").unwrap();
        assert!(crate::config::write_carousel_motion(&path, CarouselMotion::Slide).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{invalid");

        // The rendered document carries the disabled option and its reason,
        // not a silent omission: fade has no handler, slide does, and the
        // reason text is a visible node. Every scheme selection lints clean.
        let base = Rendered {
            scheme: "ocean".into(),
            motion: CarouselMotion::Slide,
            thickness: [260.0, 52.0, 200.0, 240.0],
        };
        for desired in
            std::iter::once(base.clone()).chain(Scheme::ALL.iter().map(|scheme| Rendered {
                scheme: scheme.name().into(),
                ..base.clone()
            }))
        {
            let parsed = cosmix_scene::parse(&document(&desired, "settings-test", Edge::Right))
                .expect("the document parses");
            assert!(
                cosmix_scene::lint(&parsed).is_empty(),
                "{:?}",
                cosmix_scene::lint(&parsed)
            );
            let tree = cosmix_scene::resolve(&parsed).unwrap();
            assert!(!tree.nodes["motion_fade"].ports.contains_key("on_click"));
            assert_eq!(
                tree.nodes["motion_slide"].ports["on_click"],
                json!("shell.settings.motion")
            );
            assert_eq!(
                tree.nodes["fade_reason"].ports["text"],
                json!(FADE_UNAVAILABLE_REASON)
            );
            assert_eq!(
                tree.nodes[&format!("scheme_{}", desired.scheme)].ports["on_click"],
                json!("shell.settings.scheme")
            );
        }

        // The Slide/Fade marks mirror the INGESTED motion, fade included: a
        // hand-edited conf.mix `carousel_motion: "fade"` selects Fade here —
        // still disabled, still carrying the reason — instead of the panel
        // claiming Slide is configured when it is not.
        for (motion, slide_mark, fade_mark) in [
            (CarouselMotion::Slide, "● Slide", "○ Fade (unavailable)"),
            (CarouselMotion::Fade, "○ Slide", "● Fade (unavailable)"),
        ] {
            let desired = Rendered {
                motion,
                ..base.clone()
            };
            let parsed = cosmix_scene::parse(&document(&desired, "settings-test", Edge::Right))
                .expect("the document parses");
            assert!(cosmix_scene::lint(&parsed).is_empty());
            let tree = cosmix_scene::resolve(&parsed).unwrap();
            assert_eq!(
                tree.nodes["motion_slide_t"].ports["text"],
                json!(slide_mark)
            );
            assert_eq!(tree.nodes["motion_fade_t"].ports["text"], json!(fade_mark));
            assert!(
                !tree.nodes["motion_fade"].ports.contains_key("on_click"),
                "fade stays unselectable whatever the ingested motion"
            );
        }
    }

    #[test]
    fn theme_change_applies_without_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = persist_app(&path);
        let frame = app.world().resource::<ShellFrameState>().0.clone();
        // The scene's forest dot, exactly as the document encodes it.
        let (rc, body, command, write) = plan_verb(
            &event_request("shell.settings.scheme", "scheme_forest@appearance"),
            &frame,
            Duration::ZERO,
        );
        assert_eq!(rc, 0, "{body}");
        assert!(command.is_none());
        assert_eq!(write, Some(SettingsWrite::Scheme(Scheme::Forest)));
        // Exercise the actual inbound dispatcher, not a manually emitted
        // selection: a scene click must reach ApplyTheme in this update.
        let (bridge, peer) = ctk::bus::test_bridge("settings-test");
        app.insert_resource(bridge)
            .add_plugins(crate::bus_service::ShellBusPlugin);
        peer.send(event_request(
            "shell.settings.scheme",
            "scheme_forest@appearance",
        ));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0);
        let applied = app
            .world_mut()
            .resource_mut::<Messages<ctk::theme::ApplyTheme>>()
            .drain()
            .last()
            .expect("the theme applies in the same update, with no restart");
        assert_eq!(applied.0.scheme, Scheme::Forest);
        assert_eq!(applied.0.mode, Mode::Dark);
        assert_eq!(
            crate::state::StateStore::load(Some(path.clone())).scheme(),
            Some("forest".to_owned()),
            "and it persists for the next launch"
        );
    }

    /// The same fixtures `scene-template-test.mix` feeds the template's Mix
    /// model builder: one snapshot, one expected model, shared by both sides.
    const SNAPSHOT_FIXTURE: &str =
        include_str!("../../../scripts/tests/fixtures/scenes/settings-forest-fade.snapshot.json");
    const MODEL_FIXTURE: &str =
        include_str!("../../../scripts/tests/fixtures/scenes/settings-forest-fade.model.json");

    /// The fixtures carry placeholder accents (`#00000001`…): the palette is
    /// ctk's to change; the shape, selection and marks are the contract.
    fn fixtures_with_build_accents() -> (serde_json::Value, serde_json::Value) {
        let mut snapshot: serde_json::Value = serde_json::from_str(SNAPSHOT_FIXTURE).unwrap();
        let mut model: serde_json::Value = serde_json::from_str(MODEL_FIXTURE).unwrap();
        for (index, scheme) in Scheme::ALL.into_iter().enumerate() {
            snapshot["schemes"][index]["accent"] = json!(scheme_hex(scheme));
            model["schemes"][scheme.name()]["accent"] = json!(scheme_hex(scheme));
        }
        (snapshot, model)
    }

    /// Leaves replaced by null: two models bind the same paths.
    fn shape(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect::<serde_json::Map<_, _>>()
                .into(),
            _ => serde_json::Value::Null,
        }
    }

    #[test]
    fn snapshot_and_fallback_model_match_the_shared_fixtures() {
        let config = ShellConfig::parse(
            r#"{panels: {right: ["settings.appearance"]}, carousel_motion: "fade"}"#,
        )
        .unwrap();
        let mut frame = frame_for("DP-1");
        // Exact binary fractions, so the f32 thickness equals the fixture's
        // number, and the halves pin both roundings half away from zero.
        for (edge, px) in [
            (Edge::Top, 260.0),
            (Edge::Bottom, 52.5),
            (Edge::Left, 200.0),
            (Edge::Right, 239.5),
        ] {
            frame.panels[edge.index()].settled_thickness_px = px;
        }
        let (snapshot_fixture, model_fixture) = fixtures_with_build_accents();
        assert_eq!(
            snapshot("forest", &config, &frame, Some(OWNER)),
            snapshot_fixture
        );
        let desired = Rendered {
            scheme: "forest".into(),
            motion: CarouselMotion::Fade,
            thickness: std::array::from_fn(|index| {
                frame.panel(Edge::ALL[index]).settled_thickness_px
            }),
        };
        let parsed = cosmix_scene::parse(&document(&desired, "settings-test", Edge::Right))
            .expect("the fallback document parses");
        assert_eq!(parsed.model, Some(model_fixture));
        // No declaration: the snapshot says so rather than guessing an edge.
        let bare = snapshot("ocean", &ShellConfig::parse("{}").unwrap(), &frame, None);
        assert_eq!(bare["edge"], serde_json::Value::Null);
        assert_eq!(bare["page_owner"], serde_json::Value::Null);
        assert_eq!(bare["motion"], json!("slide"));
    }

    #[test]
    fn shipped_template_is_the_fallback_layout() {
        let template = cosmix_scene::parse(TEMPLATE).expect("the shipped template parses");
        assert_eq!(template.name, SCENE_NAME);
        assert_eq!(template.citizen, "scene-quoin-settings");
        let window = template.window.as_ref().expect("the template declares its mount");
        assert_eq!(window["panel"], json!(SETTINGS_APPEARANCE));
        assert_eq!(window["edge"], json!("right"));
        assert!(
            cosmix_scene::lint(&template).is_empty(),
            "{:?}",
            cosmix_scene::lint(&template)
        );
        cosmix_scene::resolve(&template).expect("the template resolves on its default model");
        let desired = Rendered {
            scheme: "ocean".into(),
            motion: CarouselMotion::Slide,
            thickness: [260.0, 52.0, 200.0, 240.0],
        };
        let fallback = document(&desired, "settings-test", Edge::Left);
        assert!(
            fallback.ends_with(template_body()),
            "the fallback reuses the template's node block verbatim"
        );
        let fallback = cosmix_scene::parse(&fallback).unwrap();
        assert_eq!(
            fallback.nodes.keys().collect::<Vec<_>>(),
            template.nodes.keys().collect::<Vec<_>>()
        );
        assert_eq!(fallback.window.as_ref().unwrap()["edge"], json!("left"));
        assert_eq!(
            shape(template.model.as_ref().unwrap()),
            shape(fallback.model.as_ref().unwrap()),
            "the template's default model binds every path the live one does"
        );
    }

    /// The loader-managed template takes `settings.appearance` from the
    /// built-in through the real Bus dispatch; a template for the wrong edge
    /// is refused; and the fallback returns the moment the owner unloads —
    /// the handover never retires it.
    #[test]
    fn external_template_takes_the_page_and_the_fallback_returns() {
        let mut app = settings_test_app(r#"{panels: {right: ["settings.appearance"]}}"#);
        let (bridge, peer) = ctk::bus::test_bridge("settings-test");
        app.insert_resource(bridge)
            .add_plugins(crate::bus_service::ShellBusPlugin);
        app.update();
        let seat = |app: &App| {
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat(SETTINGS_APPEARANCE)
                .map(|seat| (seat.edge, seat.owner.clone()))
        };
        let owned_by = |app: &App, owner: &str| {
            app.world().resource::<SceneStore>().scenes_owned_by(owner)
        };
        let load = |source: String| {
            let mut request = verb_request(
                "shell.scene.load",
                json!({"source": source, "model_generation": 1}),
            );
            request.from = "scenes".to_owned();
            request
        };
        assert_eq!(seat(&app), Some((Edge::Right, OWNER.to_owned())));
        assert_eq!(owned_by(&app, OWNER), vec![SCENE_NAME.to_owned()]);

        peer.send(load(TEMPLATE.replace("\"edge\":\"right\"", "\"edge\":\"left\"")));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 10, "{}", replies[0].body);
        assert!(
            replies[0].body.contains("SETTINGS_EDGE_MISMATCH"),
            "{}",
            replies[0].body
        );
        assert_eq!(seat(&app), Some((Edge::Right, OWNER.to_owned())));
        assert_eq!(owned_by(&app, OWNER), vec![SCENE_NAME.to_owned()]);

        peer.send(load(TEMPLATE.to_owned()));
        app.update();
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        assert_eq!(seat(&app), Some((Edge::Right, "scenes".to_owned())));
        assert!(owned_by(&app, OWNER).is_empty());
        assert_eq!(owned_by(&app, "scenes"), vec![SCENE_NAME.to_owned()]);
        let settings = app.world().resource::<SettingsScene>();
        assert!(!settings.retired && settings.rendered.is_none());
        let frame = &app.world().resource::<ShellFrameState>().0;
        assert_eq!(frame.panel(Edge::Right).page_ids.as_ref(), [SETTINGS_APPEARANCE]);

        let mut unload = verb_request("shell.scene.unload", json!({"scene": SCENE_NAME}));
        unload.from = "scenes".to_owned();
        peer.send(unload);
        // The unload update only schedules the fallback (the edge-move
        // pattern); the next one reloads it and reconciles its mount.
        app.update();
        app.update();
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        assert_eq!(seat(&app), Some((Edge::Right, OWNER.to_owned())));
        assert_eq!(owned_by(&app, OWNER), vec![SCENE_NAME.to_owned()]);
        assert!(!app.world().resource::<SettingsScene>().retired);
    }
}
