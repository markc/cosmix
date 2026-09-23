//! Quoin's data-only `cosmix_path(Etc)/quoin/conf.mix` schema.
//!
//! All fields are optional; omitted fields reset to the defaults below on each
//! ingestion. Unknown keys, wrong types and duplicate names/chords are errors.
//! `panels.{left,bottom,right,top}` are ordered string lists; position zero is
//! primary. Right must start with `settings.appearance` (content comes separately).
//! `menu_items.{edge}` contains `{label, target, verb, args}` Bus actions, with
//! `args` an optional list of strings. These are additions to the mode menu.
//! `bindings.{edge}.{pin,dock,hide}` and `bindings.cycle_focus` are optional
//! chords (`Super+Shift+Left`), or nil to disable. Defaults are unbound, avoiding
//! unsolicited global grabs. Modifier tokens are Ctrl, Alt, Shift and Super;
//! keys are ASCII letters/digits, F1–F35, arrows, Tab, Return, space, Escape,
//! Home, End, Page_Up, Page_Down, Insert, Delete and BackSpace. Super+Escape is
//! reserved for comp. `carousel_motion` is `slide` (default) or `fade`; renderer
//! support/fallback belongs to the motion consumer, not the config reader.
//!
//! Example: `{panels: {left: ["places", "nav"]}, bindings: {left:
//! {pin: "Super+Shift+Left"}}, carousel_motion: "slide"}`.
//! Compositor thresholds, geometry and modifiers deliberately are not keys here.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use cosmix_config::{CosmixDir, Value, cosmix_path, parse_mix_data};
use cosmix_shell::core::Edge;
use cosmix_shell::runtime::{ShellRuntimeSet, redeclare_shell_pages};
use cosmix_shell_host::file_watch::{LayerHostFileWatch, LayerHostFileWatches};

pub const SETTINGS_APPEARANCE: &str = "settings.appearance";

/// The one resolver for the config file every in-process reader and writer
/// uses: the standalone watcher (`install`) and the settings verbs' motion
/// writes (`settings::dispatch_verb`). Never restate the path inline.
pub(crate) fn conf_mix_path() -> PathBuf {
    cosmix_path(CosmixDir::Etc).join("quoin/conf.mix")
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CarouselMotion {
    #[default]
    Slide,
    Fade,
}

/// A declarative Bus invocation; ingestion never executes menu actions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub target: String,
    pub verb: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EdgeBindings {
    pub pin: Option<String>,
    pub dock: Option<String>,
    pub hide: Option<String>,
}

/// Last accepted configuration, available to menu, keyboard and motion consumers.
/// Arrays use `Edge::index()`. Reading declarations never registers content.
#[derive(Resource, Clone, Debug, Eq, PartialEq)]
pub struct ShellConfig {
    pub panels: [Vec<String>; 4],
    pub menu_items: [Vec<MenuItem>; 4],
    pub bindings: [EdgeBindings; 4],
    pub cycle_focus: Option<String>,
    pub carousel_motion: CarouselMotion,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            panels: [
                vec!["nav".into(), "places".into(), "info".into()],
                vec!["launcher".into(), "power".into(), "tasks".into()],
                vec![
                    SETTINGS_APPEARANCE.into(),
                    "monitor".into(),
                    "demos".into(),
                    "agents".into(),
                ],
                vec!["status".into(), "spaces".into()],
            ],
            menu_items: std::array::from_fn(|_| Vec::new()),
            bindings: std::array::from_fn(|_| EdgeBindings::default()),
            cycle_focus: None,
            carousel_motion: CarouselMotion::Slide,
        }
    }
}

type Fields<'a> = BTreeMap<&'a str, &'a Value>;

fn fields<'a>(value: &'a Value, allowed: &[&str], path: &str) -> Result<Fields<'a>, String> {
    let Value::Map(map) = value else {
        return Err(format!("{path}: expected a map"));
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path}.{key}: unknown key"));
        }
    }
    Ok(map.iter().map(|(k, v)| (k.as_str(), v)).collect())
}

fn string(value: &Value, path: &str) -> Result<String, String> {
    match value {
        Value::String(text) if !text.trim().is_empty() && !text.chars().any(char::is_control) => {
            Ok(text.clone())
        }
        _ => Err(format!(
            "{path}: expected a non-empty string without control characters"
        )),
    }
}

fn strings(value: &Value, path: &str) -> Result<Vec<String>, String> {
    let Value::List(items) = value else {
        return Err(format!("{path}: expected a list of strings"));
    };
    items.iter().map(|item| string(item, path)).collect()
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// Parse and canonicalise chords so modifier order cannot disguise duplicates.
fn chord(value: &Value, path: &str) -> Result<Option<String>, String> {
    if matches!(value, Value::Nil) {
        return Ok(None);
    }
    let text = string(value, path)?;
    let mut parts: Vec<_> = text.split('+').collect();
    let key = parts.pop().unwrap_or_default();
    let modifiers = ["Ctrl", "Alt", "Shift", "Super"];
    let mut seen = HashSet::new();
    if parts
        .iter()
        .any(|part| !modifiers.contains(part) || !seen.insert(*part))
    {
        return Err(format!("{path}: invalid or repeated modifier in {text}"));
    }
    let valid_key = (key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric())
        || [
            "Left",
            "Right",
            "Up",
            "Down",
            "Tab",
            "Return",
            "space",
            "Escape",
            "Home",
            "End",
            "Page_Up",
            "Page_Down",
            "Insert",
            "Delete",
            "BackSpace",
        ]
        .contains(&key)
        || key
            .strip_prefix('F')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=35).contains(&n) && key == format!("F{n}"));
    if !valid_key || (key == "Escape" && seen.len() == 1 && seen.contains("Super")) {
        return Err(format!("{path}: invalid or reserved key chord {text}"));
    }
    let mut canonical: Vec<String> = modifiers
        .into_iter()
        .filter(|m| seen.contains(m))
        .map(str::to_owned)
        .collect();
    canonical.push(if key.len() == 1 {
        key.to_ascii_uppercase()
    } else {
        key.into()
    });
    Ok(Some(canonical.join("+")))
}

impl ShellConfig {
    pub fn parse(source: &str) -> Result<Self, String> {
        let value = parse_mix_data(source).map_err(|e| e.to_string())?;
        let root = fields(
            &value,
            &["panels", "menu_items", "bindings", "carousel_motion"],
            "config",
        )?;
        let mut config = Self::default();
        let edges = ["left", "bottom", "right", "top"];
        if let Some(value) = root.get("panels") {
            let panels = fields(value, &edges, "panels")?;
            for edge in Edge::ALL {
                let name = crate::edge_name(edge);
                if let Some(value) = panels.get(name) {
                    config.panels[edge.index()] = strings(value, &format!("panels.{name}"))?;
                }
            }
        }
        let mut names = HashSet::new();
        for name in config.panels.iter().flatten() {
            if !identifier(name) || !names.insert(name) {
                return Err(format!(
                    "panels: invalid or duplicate sub-panel name {name:?}"
                ));
            }
        }
        if config.panels[Edge::Right.index()]
            .first()
            .map(String::as_str)
            != Some(SETTINGS_APPEARANCE)
        {
            return Err(format!(
                "panels.right: primary must be {SETTINGS_APPEARANCE}"
            ));
        }
        if let Some(value) = root.get("menu_items") {
            let menus = fields(value, &edges, "menu_items")?;
            for edge in Edge::ALL {
                let name = crate::edge_name(edge);
                let Some(value) = menus.get(name) else {
                    continue;
                };
                let Value::List(items) = value else {
                    return Err(format!("menu_items.{name}: expected a list"));
                };
                for item in items.iter() {
                    let path = format!("menu_items.{name}");
                    let item = fields(item, &["label", "target", "verb", "args"], &path)?;
                    let required = |key| -> Result<String, String> {
                        string(
                            item.get(key)
                                .ok_or_else(|| format!("{path}.{key}: required"))?,
                            &format!("{path}.{key}"),
                        )
                    };
                    let label = required("label")?;
                    let target = required("target")?;
                    let verb = required("verb")?;
                    if !identifier(&target) || !identifier(&verb) {
                        return Err(format!("{path}: target and verb must be Bus identifiers"));
                    }
                    let args = item
                        .get("args")
                        .map(|value| strings(value, &format!("{path}.args")))
                        .transpose()?
                        .unwrap_or_default();
                    config.menu_items[edge.index()].push(MenuItem {
                        label,
                        target,
                        verb,
                        args,
                    });
                }
            }
        }
        if let Some(value) = root.get("bindings") {
            let bindings = fields(
                value,
                &["left", "bottom", "right", "top", "cycle_focus"],
                "bindings",
            )?;
            for edge in Edge::ALL {
                let name = crate::edge_name(edge);
                if let Some(value) = bindings.get(name) {
                    let values =
                        fields(value, &["pin", "dock", "hide"], &format!("bindings.{name}"))?;
                    let binding = |key| -> Result<Option<String>, String> {
                        values
                            .get(key)
                            .map(|v| chord(v, &format!("bindings.{name}.{key}")))
                            .transpose()
                            .map(Option::flatten)
                    };
                    config.bindings[edge.index()] = EdgeBindings {
                        pin: binding("pin")?,
                        dock: binding("dock")?,
                        hide: binding("hide")?,
                    };
                }
            }
            config.cycle_focus = bindings
                .get("cycle_focus")
                .map(|v| chord(v, "bindings.cycle_focus"))
                .transpose()?
                .flatten();
        }
        let mut keys = HashSet::new();
        for key in config
            .bindings
            .iter()
            .flat_map(|b| [&b.pin, &b.dock, &b.hide])
            .chain(std::iter::once(&config.cycle_focus))
            .flatten()
        {
            if !keys.insert(key) {
                return Err(format!("bindings: duplicate chord {key}"));
            }
        }
        if let Some(value) = root.get("carousel_motion") {
            config.carousel_motion = match string(value, "carousel_motion")?.as_str() {
                "slide" => CarouselMotion::Slide,
                "fade" => CarouselMotion::Fade,
                _ => return Err("carousel_motion: expected slide or fade".into()),
            };
        }
        Ok(config)
    }
}

type Pending = Arc<Mutex<ConfigInbox>>;
type ReportRefusal = Arc<dyn Fn(&Path, &str) + Send + Sync>;

/// Only application is coalesced. Parsing and refusal reporting occur for every
/// dispatched write event, before replacing this last-write-wins slot.
#[derive(Default)]
struct ConfigInbox {
    generation: u64,
    candidate: Option<Result<ShellConfig, String>>,
}

impl ConfigInbox {
    fn publish(&mut self, candidate: Result<ShellConfig, String>) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("config generation exhausted");
        self.candidate = Some(candidate);
    }

    fn restore_if_current(&mut self, generation: u64, config: ShellConfig) {
        if self.generation == generation && self.candidate.is_none() {
            self.candidate = Some(Ok(config));
        }
    }
}

#[derive(Resource)]
struct ConfigReader {
    pending: Pending,
    applied: bool,
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

/// Settings writes the same data-only field ingestion reads. Preserve all
/// other authored VALUES, refuse invalid input, and atomically replace the
/// file so the existing watcher observes a complete configuration. The
/// rewrite re-encodes the whole file: comments and formatting do not survive
/// a motion write — only the parsed values do.
pub(crate) fn write_carousel_motion(path: &Path, motion: CarouselMotion) -> Result<(), String> {
    use std::io::Write;
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => return Err(error.to_string()),
    };
    ShellConfig::parse(&source)?;
    let mut value = parse_mix_data(&source).map_err(|error| error.to_string())?;
    let Value::Map(root) = &mut value else {
        unreachable!("validated config map")
    };
    std::rc::Rc::make_mut(root).insert(
        "carousel_motion".into(),
        Value::String(
            match motion {
                CarouselMotion::Slide => "slide",
                CarouselMotion::Fade => "fade",
            }
            .into(),
        ),
    );
    let encoded = value
        .to_mix_data_string_pretty()
        .map_err(|error| error.to_string())?;
    ShellConfig::parse(&encoded)?;
    let parent = path.parent().ok_or("config path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    temp.write_all(encoded.as_bytes())
        .map_err(|error| error.to_string())?;
    temp.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temp.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConfigIngest;

impl ConfigReader {
    fn start(path: PathBuf, report: ReportRefusal) -> std::io::Result<(Self, LayerHostFileWatch)> {
        let pending = Arc::new(Mutex::new(ConfigInbox::default()));
        let inbox = Arc::clone(&pending);
        let file = path.clone();
        let event_report = Arc::clone(&report);
        // Watch before reading, so an edit during initial ingestion stays queued
        // for calloop. Watching the parent survives rename-replace and re-creation.
        let watch = LayerHostFileWatch::new(
            path.clone(),
            Arc::new(move || {
                let candidate = read(&file).and_then(|source| ShellConfig::parse(&source));
                if let Err(error) = &candidate {
                    event_report(&file, error);
                }
                inbox
                    .lock()
                    .expect("config inbox poisoned")
                    .publish(candidate);
            }),
        )?;
        // A missing file at startup means defaults. Subsequent deletion is a
        // refused edit; writing {} is the explicit reset-to-defaults operation.
        let initial = std::fs::read_to_string(&path);
        let candidate = match &initial {
            Ok(source) => ShellConfig::parse(source),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ShellConfig::default()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(error) = &candidate {
            report(&path, error);
        }
        pending
            .lock()
            .expect("config inbox poisoned")
            .publish(candidate);
        Ok((
            Self {
                pending,
                applied: false,
            },
            watch,
        ))
    }
}

/// Standalone only: no watcher, filesystem access or settings changes in smoke
/// hosts. The embedded host may opt into the same schema in its own lifecycle.
pub(crate) fn install(app: &mut App, smoke: bool) {
    if smoke {
        return;
    }
    let (reader, watch) = ConfigReader::start(
        conf_mix_path(),
        Arc::new(|path, error| {
            eprintln!(
                "QUOIN_CONFIG refused path={} reason={error}",
                path.display()
            )
        }),
    )
    .expect("install Quoin config directory watch");
    app.world_mut()
        .resource_mut::<LayerHostFileWatches>()
        .0
        .push(watch);
    app.init_resource::<ShellConfig>()
        .insert_resource(reader)
        .add_systems(
            Update,
            ingest
                .in_set(ShellRuntimeSet::Input)
                .in_set(ConfigIngest)
                .before(crate::bus_service::ShellBusDispatch),
        );
}

fn ingest(world: &mut World) {
    let (generation, pending) = {
        let mut inbox = world
            .resource::<ConfigReader>()
            .pending
            .lock()
            .expect("config inbox poisoned");
        (inbox.generation, inbox.candidate.take())
    };
    // Refusals were already reported at the read boundary, before coalescing.
    let candidate = pending.and_then(Result::ok);
    let initial = !world.resource::<ConfigReader>().applied;
    if candidate.is_none() && !initial {
        return;
    }
    let config = candidate.unwrap_or_else(|| world.resource::<ShellConfig>().clone());
    match redeclare_shell_pages(world, &config.panels) {
        Ok(true) => {
            // A fade configured by hand in conf.mix ingests (the schema is
            // renderer-neutral) but cannot render yet; say so once per
            // ingestion rather than letting the panel's marks imply it runs.
            if config.carousel_motion == CarouselMotion::Fade {
                eprintln!(
                    "QUOIN_CONFIG carousel_motion=fade note=renders as slide until the renderer gains sibling stacking"
                );
            }
            world.insert_resource(config);
            world.resource_mut::<ConfigReader>().applied = true;
        }
        Ok(false) => {
            world
                .resource::<ConfigReader>()
                .pending
                .lock()
                .expect("config inbox poisoned")
                .restore_if_current(generation, config);
        }
        Err(error) => eprintln!("QUOIN_CONFIG refused reason={error}"),
    }
}

/// Feed the real ingestion path without installing a filesystem watcher.
#[cfg(test)]
pub(crate) fn ingest_test_config(world: &mut World, source: &str) {
    let mut inbox = ConfigInbox::default();
    inbox.publish(ShellConfig::parse(source));
    world.init_resource::<ShellConfig>();
    world.insert_resource(ConfigReader {
        pending: Arc::new(Mutex::new(inbox)),
        applied: false,
    });
    ingest(world);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
    use cosmix_shell::runtime::{ShellFrameState, ShellRuntimePlugin};
    use std::time::Duration;

    fn model() -> ShellModel {
        ShellModel::new(
            OutputKey::new("test-output").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap()
    }

    fn app(model: ShellModel) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .init_resource::<ShellConfig>()
            .insert_resource(ConfigReader {
                pending: Arc::new(Mutex::new(ConfigInbox::default())),
                applied: false,
            })
            .add_systems(Update, ingest.in_set(ShellRuntimeSet::Input));
        app
    }

    fn edit(app: &mut App, source: &str) {
        app.world()
            .resource::<ConfigReader>()
            .pending
            .lock()
            .unwrap()
            .publish(ShellConfig::parse(source));
        app.update();
    }

    #[test]
    fn declared_order_ingests_per_edge() {
        let config = ShellConfig::parse(
            r#"{panels: {
            left: ["l2", "l1"], bottom: ["b2", "b1"],
            right: ["settings.appearance", "r2", "r1"], top: ["t2", "t1"]}}"#,
        )
        .unwrap();
        let mut model = model();
        for edge in Edge::ALL {
            model
                .declare_carousel(edge, config.panels[edge.index()].clone())
                .unwrap();
            for name in config.panels[edge.index()].iter().rev() {
                model.carousel_mut(edge).register(name).unwrap();
            }
        }
        let mut app = app(model);
        app.world()
            .resource::<ConfigReader>()
            .pending
            .lock()
            .unwrap()
            .publish(Ok(config.clone()));
        app.update();
        for edge in Edge::ALL {
            assert_eq!(
                app.world()
                    .resource::<ShellFrameState>()
                    .0
                    .panel(edge)
                    .page_ids
                    .as_ref(),
                config.panels[edge.index()].as_slice()
            );
        }
    }

    #[test]
    fn undeclared_names_stay_empty_slots() {
        // Declarations alone never fabricate live content; an undeclared live
        // name is a tail entry, not a new declaration.
        let mut app = app(model());
        edit(&mut app, r#"{panels: {left: ["future", "later"]}}"#);
        for edge in Edge::ALL {
            assert!(
                app.world()
                    .resource::<ShellFrameState>()
                    .0
                    .panel(edge)
                    .page_ids
                    .is_empty()
            );
        }
        let config = app.world().resource::<ShellConfig>();
        let mut model = model();
        model
            .declare_carousel(Edge::Left, config.panels[0].clone())
            .unwrap();
        model.carousel_mut(Edge::Left).register("tail").unwrap();
        model.carousel_mut(Edge::Left).register("later").unwrap();
        assert_eq!(model.carousel(Edge::Left).page_ids(), ["later", "tail"]);
        assert!(model.carousel_mut(Edge::Left).activate("future").is_err());
    }

    #[test]
    fn settings_appearance_is_declared_right_primary() {
        let config = ShellConfig::parse("{}").unwrap();
        assert_eq!(config.panels[Edge::Right.index()][0], SETTINGS_APPEARANCE);
        assert!(ShellConfig::parse(r#"{panels: {right: ["monitor"]}}"#).is_err());
        assert!(ShellConfig::parse(r#"{panels: {right: []}}"#).is_err());
        let mut model = model();
        model
            .declare_carousel(Edge::Right, config.panels[Edge::Right.index()].clone())
            .unwrap();
        model.carousel_mut(Edge::Right).register("monitor").unwrap();
        assert_eq!(model.carousel(Edge::Right).page_ids(), ["monitor"]);
        model
            .carousel_mut(Edge::Right)
            .register(SETTINGS_APPEARANCE)
            .unwrap();
        assert_eq!(
            model.carousel(Edge::Right).page_ids(),
            [SETTINGS_APPEARANCE, "monitor"]
        );
    }

    #[test]
    fn invalid_schema_is_refused_and_previous_config_retained() {
        let mut app = app(model());
        edit(
            &mut app,
            r#"{panels: {left: ["custom"]}, carousel_motion: "fade"}"#,
        );
        let previous = app.world().resource::<ShellConfig>().clone();
        for source in [
            "{",
            "[]",
            "{typo: true}",
            "{deadzone: 10}",
            "{panels: {lef: []}}",
            "{panels: {left: 1}}",
            r#"{panels: {left: ["dup", "dup"]}}"#,
            r#"{panels: {left: ["status"]}}"#,
            r#"{panels: {left: [""]}}"#,
            r#"{carousel_motion: "sldie"}"#,
            r#"{menu_items: {left: [{label: "Missing action"}]}}"#,
            r#"{bindings: {left: {pni: "Super+Left"}}}"#,
            r#"{bindings: {cycle_focus: "Super+Escape"}}"#,
            r#"{bindings: {cycle_focus: "Shfit+Left"}}"#,
            r#"{bindings: {cycle_focus: "Super+MadeUpKey"}}"#,
            r#"{bindings: {left: {pin: "Ctrl+Super+a"}, cycle_focus: "Super+Ctrl+A"}}"#,
        ] {
            assert!(ShellConfig::parse(source).is_err(), "accepted {source}");
            edit(&mut app, source);
            assert_eq!(app.world().resource::<ShellConfig>(), &previous);
        }
        edit(&mut app, "{}");
        assert_eq!(
            app.world().resource::<ShellConfig>(),
            &ShellConfig::default()
        );
    }

    #[test]
    fn live_change_reapplies_order() {
        let mut model = model();
        for name in ["nav", "places", "tail-one", "tail-two"] {
            model.carousel_mut(Edge::Left).register(name).unwrap();
        }
        model.carousel_mut(Edge::Left).activate("places").unwrap();
        let mut app = app(model);
        edit(&mut app, r#"{panels: {left: ["places", "future", "nav"]}}"#);
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left);
        assert_eq!(
            panel.page_ids.as_ref(),
            ["places", "nav", "tail-one", "tail-two"]
        );
        assert_eq!(panel.active_page_id.as_deref(), Some("places"));
        edit(&mut app, r#"{panels: {left: ["nav", "new-empty"]}}"#);
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left);
        assert_eq!(
            panel.page_ids.as_ref(),
            ["nav", "places", "tail-one", "tail-two"]
        );
        assert_eq!(panel.active_page_id.as_deref(), Some("places"));
        // A reveal restores remembered selection; reordering must not turn
        // explicit selection into an index or replace it with the primary.
        let output = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        app.world_mut()
            .write_message(cosmix_shell::runtime::ShellCommand {
                output,
                at: Duration::ZERO,
                kind: cosmix_shell::runtime::ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: cosmix_shell::core::PanelInput::Reveal,
                },
            });
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("places")
        );
    }

    #[test]
    fn menu_bindings_and_motion_are_typed_data_with_defaults() {
        let config = ShellConfig::parse(r#"{
            menu_items: {right: [{label: "Open tools", target: "tools", verb: "tools.open", args: ["main"]}]},
            bindings: {left: {pin: "Super+Shift+Left", dock: "Super+F2", hide: nil}, cycle_focus: "Super+Tab"},
            carousel_motion: "fade"
        }"#).unwrap();
        assert_eq!(config.bindings[0].pin.as_deref(), Some("Shift+Super+Left"));
        assert_eq!(config.bindings[0].dock.as_deref(), Some("Super+F2"));
        assert_eq!(config.bindings[0].hide, None);
        assert_eq!(config.cycle_focus.as_deref(), Some("Super+Tab"));
        assert_eq!(config.carousel_motion, CarouselMotion::Fade);
        assert_eq!(
            config.menu_items[Edge::Right.index()][0],
            MenuItem {
                label: "Open tools".into(),
                target: "tools".into(),
                verb: "tools.open".into(),
                args: vec!["main".into()],
            }
        );
        assert_eq!(ShellConfig::parse("{}").unwrap(), ShellConfig::default());
    }

    #[test]
    fn declaration_batch_refuses_invalid_later_edge_without_partial_application() {
        let mut model = model();
        for name in ["nav", "places"] {
            model.carousel_mut(Edge::Left).register(name).unwrap();
        }
        let mut app = app(model);
        app.update();
        let before = app.world().resource::<ShellFrameState>().0.clone();
        let mut declarations = ShellConfig::default().panels;
        declarations[Edge::Left.index()] = vec!["places".into(), "nav".into()];
        declarations[Edge::Top.index()] = vec!["duplicate".into(), "duplicate".into()];
        assert!(redeclare_shell_pages(app.world_mut(), &declarations).is_err());
        assert_eq!(app.world().resource::<ShellFrameState>().0, before);
        // Resampling also proves the underlying model did not partially change.
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .as_ref(),
            ["nav", "places"]
        );
    }

    #[test]
    fn file_change_wakes_idle_host_and_recovers_after_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        let (reader, watch) = ConfigReader::start(path.clone(), Arc::new(|_, _| {})).unwrap();
        let mut app = app(model());
        app.insert_resource(reader);
        app.update();
        assert_eq!(
            app.world().resource::<ShellConfig>(),
            &ShellConfig::default()
        );
        for (source, motion) in [
            (r#"{carousel_motion: "fade"}"#, CarouselMotion::Fade),
            (r#"{carousel_motion: "typo"}"#, CarouselMotion::Fade),
            (r#"{carousel_motion: "slide"}"#, CarouselMotion::Slide),
        ] {
            // Atomic replacement exercises the same path as editor saves.
            let replacement = directory.path().join("replacement.mix");
            std::fs::write(&replacement, source).unwrap();
            std::fs::rename(replacement, &path).unwrap();
            assert!(watch.dispatch_pending().unwrap() > 0);
            app.update();
            assert_eq!(
                app.world().resource::<ShellConfig>().carousel_motion,
                motion
            );
        }
        std::fs::remove_file(&path).unwrap();
        assert!(watch.dispatch_pending().unwrap() > 0);
        app.update();
        assert_eq!(
            app.world().resource::<ShellConfig>().carousel_motion,
            CarouselMotion::Slide
        );
        std::fs::write(path, r#"{carousel_motion: "fade"}"#).unwrap();
        assert!(watch.dispatch_pending().unwrap() > 0);
        app.update();
        assert_eq!(
            app.world().resource::<ShellConfig>().carousel_motion,
            CarouselMotion::Fade
        );
        assert_eq!(watch.dispatch_pending().unwrap(), 0);
        // The calloop source owns the descriptor: no reader thread to shut down.
    }

    #[test]
    fn refused_write_is_reported_before_valid_write_coalesces_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        let reports = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&reports);
        let (reader, watch) = ConfigReader::start(
            path.clone(),
            Arc::new(move |_, error| {
                recorded.lock().unwrap().push(error.to_owned());
            }),
        )
        .unwrap();
        let mut app = app(model());
        app.insert_resource(reader);
        app.update();

        // Dispatch the refused write immediately, then a valid write, without
        // giving Bevy an update between them. No timer or debounce window.
        // The kernel does not snapshot bytes overwritten before dispatch.
        std::fs::write(&path, r#"{carousel_motion: "typo"}"#).unwrap();
        let invalid_events = watch.dispatch_pending().unwrap();
        assert!(invalid_events > 0);
        assert_eq!(reports.lock().unwrap().len(), invalid_events);
        std::fs::write(&path, r#"{carousel_motion: "fade"}"#).unwrap();
        assert!(watch.dispatch_pending().unwrap() > 0);
        app.update();
        assert_eq!(
            app.world().resource::<ShellConfig>().carousel_motion,
            CarouselMotion::Fade
        );
        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), invalid_events);
        assert!(
            reports
                .iter()
                .all(|error| error.contains("expected slide or fade"))
        );
    }

    #[test]
    fn deferred_candidate_never_clobbers_a_newer_generation() {
        let mut inbox = ConfigInbox::default();
        let original = ShellConfig::default();
        inbox.publish(Ok(original.clone()));
        let generation = inbox.generation;
        inbox.candidate.take();
        let newer = ShellConfig::parse(r#"{carousel_motion: "fade"}"#).unwrap();
        inbox.publish(Ok(newer.clone()));
        inbox.restore_if_current(generation, original.clone());
        assert_eq!(inbox.candidate.as_ref().unwrap().as_ref().unwrap(), &newer);
        // Even if another consumer took the newer candidate, the old generation
        // must not be resurrected merely because the slot is empty again.
        inbox.candidate.take();
        inbox.restore_if_current(generation, original);
        assert!(inbox.candidate.is_none());
        inbox.restore_if_current(inbox.generation, newer.clone());
        assert_eq!(inbox.candidate.unwrap().unwrap(), newer);
    }
}
