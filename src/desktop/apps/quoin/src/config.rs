//! Quoin's data-only `cosmix_path(Etc)/quoin/conf.mix` schema.
//!
//! All fields are optional; omitted fields reset to the defaults below on each
//! ingestion. Unknown keys, wrong types and duplicate names/chords are errors.
//! `panels.{left,bottom,right,top}` are ordered string lists; position zero is
//! primary. Content is supplied only by registered scenes.
//! `menu_items.{edge}` contains `{label, target, verb, args}` Bus actions, with
//! `args` an optional list of strings. These are additions to the mode menu.
//! `bindings.{edge}.{pin,dock,hide}` and `bindings.cycle_focus` are optional
//! chords (`Super+Shift+Left`), or nil to disable. Defaults are unbound, avoiding
//! unsolicited global grabs. A chord needs Ctrl, Alt or Super: a bare or
//! Shift-only key would take typing from the panel's own controls. Modifier
//! tokens are Ctrl, Alt, Shift and Super;
//! keys are ASCII letters/digits, F1–F35, arrows, Tab, Return, space,
//! Home, End, Page_Up, Page_Down, Insert, Delete and BackSpace. Escape is
//! refused with any modifiers: it is the panel's own key, and Super+Escape is
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

/// Seed both hosts before constructing the model. Standalone also watches edits.
pub(crate) fn startup_config(smoke: bool) -> ShellConfig {
    if smoke {
        return ShellConfig::default();
    }
    let path = conf_mix_path();
    let candidate = match std::fs::read_to_string(&path) {
        Ok(source) => ShellConfig::parse(&source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ShellConfig::default();
        }
        Err(error) => Err(error.to_string()),
    };
    candidate.unwrap_or_else(|error| {
        eprintln!(
            "QUOIN_CONFIG refused path={} reason={error}",
            path.display()
        );
        ShellConfig::default()
    })
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
            panels: std::array::from_fn(|_| Vec::new()),
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
    if !valid_key {
        return Err(format!("{path}: invalid or reserved key chord {text}"));
    }
    // Any Escape reaching a focused panel is the shell's Escape (hide a
    // transient, hand focus back), so no chord may also claim it; this also
    // covers comp's reserved Super+Escape.
    if key == "Escape" {
        return Err(format!(
            "{path}: {text}: Escape is the panel's own key (§4.3)"
        ));
    }
    // A bare or Shift-only key would steal typing from the panel's own
    // controls (a Tab, letter or capital from the launcher's search field).
    if !["Ctrl", "Alt", "Super"].iter().any(|m| seen.contains(m)) {
        return Err(format!("{path}: key chord {text} needs Ctrl, Alt or Super"));
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
    replace_atomically(path, &encoded)
}

/// Write `encoded` beside `path` and rename it over, so the watcher only ever
/// observes a complete file.
fn replace_atomically(path: &Path, encoded: &str) -> Result<(), String> {
    use std::io::Write;
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

/// Most pages `shell.panel.order` accepts on one edge.
pub(crate) const MAX_ORDER_PAGES: usize = 32;

/// A `shell.panel.order` refusal: rc 10 `{error_code, message, edges?}`.
#[derive(Debug, PartialEq)]
pub(crate) struct OrderRefusal {
    pub code: &'static str,
    pub message: String,
    /// The edges the conflict involves, when there is one.
    pub edges: Vec<&'static str>,
}

impl OrderRefusal {
    fn invalid(message: String, edges: Vec<&'static str>) -> Self {
        Self {
            code: "INVALID_ARGUMENT",
            message,
            edges,
        }
    }

    fn write(message: String) -> Self {
        Self {
            code: "CONFIG_WRITE",
            message,
            edges: Vec::new(),
        }
    }

    pub(crate) fn body(&self) -> serde_json::Value {
        let mut body = serde_json::json!({"error_code": self.code, "message": self.message});
        if !self.edges.is_empty() {
            body["edges"] = serde_json::json!(self.edges);
        }
        body
    }
}

/// Parse a `shell.panel.order` body, `{edges:{<edge>:[string], …}}` with one
/// to four edges. Every page name must be a sub-panel identifier, at most
/// [`MAX_ORDER_PAGES`] per edge, and no name may repeat within or across the
/// named edges. The result is in [`Edge::ALL`] order.
pub(crate) fn parse_panel_order(
    body: &serde_json::Value,
) -> Result<Vec<(Edge, Vec<String>)>, OrderRefusal> {
    let invalid = |message: &str| OrderRefusal::invalid(message.to_owned(), Vec::new());
    let edges = body
        .get("edges")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| invalid("edges must be a map of edge to page list"))?;
    if edges.is_empty() || edges.len() > 4 {
        return Err(invalid("edges must name one to four edges"));
    }
    if let Some(name) = edges
        .keys()
        .find(|name| crate::bus_service::parse_edge((*name).to_owned()).is_none())
    {
        return Err(OrderRefusal::invalid(
            format!("{name} is not an edge (left, bottom, right or top)"),
            Vec::new(),
        ));
    }
    let mut order = Vec::new();
    for edge in Edge::ALL {
        let name = crate::edge_name(edge);
        let Some(pages) = edges.get(name) else {
            continue;
        };
        let pages = pages
            .as_array()
            .ok_or_else(|| invalid("each edge takes a list of page names"))?;
        if pages.len() > MAX_ORDER_PAGES {
            return Err(OrderRefusal::invalid(
                format!("{name} names {} pages; at most {MAX_ORDER_PAGES}", pages.len()),
                vec![name],
            ));
        }
        let pages = pages
            .iter()
            .map(|page| {
                page.as_str()
                    .filter(|page| identifier(page))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        OrderRefusal::invalid(
                            format!("{name}: page names must be non-empty sub-panel identifiers"),
                            vec![name],
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        order.push((edge, pages));
    }
    check_unique(&order)?;
    Ok(order)
}

/// A page may be declared once in the whole file.
fn check_unique(order: &[(Edge, Vec<String>)]) -> Result<(), OrderRefusal> {
    let mut seen: BTreeMap<&str, Edge> = BTreeMap::new();
    for (edge, pages) in order {
        for page in pages {
            if let Some(first) = seen.insert(page.as_str(), *edge) {
                let name = crate::edge_name(*edge);
                return Err(if first == *edge {
                    OrderRefusal::invalid(format!("page {page} is named twice on {name}"), vec![name])
                } else {
                    OrderRefusal::invalid(
                        format!("page {page} is named on two edges"),
                        vec![crate::edge_name(first), name],
                    )
                });
            }
        }
    }
    Ok(())
}

/// `shell.panel.order`'s one write: replace `panels.<edge>` for every edge in
/// `order` in a single atomic file replacement, so moving a page between
/// edges is never half-applied. Edges not named keep their declarations, and
/// a page they declare may not also appear in `order`. Other keys' values are
/// preserved; comments and formatting are not (as for the motion write). The
/// watcher then ingests the file exactly like a hand edit.
pub(crate) fn write_panel_order(
    path: &Path,
    order: &[(Edge, Vec<String>)],
) -> Result<(), OrderRefusal> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => return Err(OrderRefusal::write(format!("could not read conf.mix: {error}"))),
    };
    let current = ShellConfig::parse(&source)
        .map_err(|error| OrderRefusal::write(format!("current conf.mix is invalid: {error}")))?;
    let merged: Vec<(Edge, Vec<String>)> = Edge::ALL
        .into_iter()
        .map(|edge| {
            let pages = order
                .iter()
                .find(|(named, _)| *named == edge)
                .map_or_else(|| current.panels[edge.index()].clone(), |(_, pages)| pages.clone());
            (edge, pages)
        })
        .collect();
    check_unique(&merged)?;
    let mut value = parse_mix_data(&source).map_err(|error| OrderRefusal::write(error.to_string()))?;
    let Value::Map(root) = &mut value else {
        unreachable!("validated config map")
    };
    let root = std::rc::Rc::make_mut(root);
    let panels = root
        .entry("panels".to_owned())
        .or_insert_with(|| Value::Map(Default::default()));
    let Value::Map(panels) = panels else {
        unreachable!("validated panels map")
    };
    let panels = std::rc::Rc::make_mut(panels);
    for (edge, pages) in order {
        panels.insert(
            crate::edge_name(*edge).to_owned(),
            Value::List(std::rc::Rc::new(
                pages.iter().cloned().map(Value::String).collect(),
            )),
        );
    }
    let encoded = value
        .to_mix_data_string_pretty()
        .map_err(|error| OrderRefusal::write(error.to_string()))?;
    ShellConfig::parse(&encoded)
        .map_err(|error| OrderRefusal::write(format!("re-encoded conf.mix is invalid: {error}")))?;
    replace_atomically(path, &encoded)
        .map_err(|error| OrderRefusal::write(format!("could not replace conf.mix: {error}")))
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
    fn defaults_are_empty_and_right_primary_is_unrestricted() {
        let config = ShellConfig::parse("{}").unwrap();
        assert!(config.panels.iter().all(Vec::is_empty));
        for source in [
            r#"{panels: {right: ["scene-tools"]}}"#,
            r#"{panels: {right: []}}"#,
        ] {
            assert!(ShellConfig::parse(source).is_ok());
        }
        let config = ShellConfig::parse(r#"{panels: {bottom: ["scene-panel"]}}"#).unwrap();
        assert_eq!(
            config
                .panels
                .iter()
                .filter(|pages| !pages.is_empty())
                .count(),
            1
        );
        assert_eq!(config.panels[Edge::Bottom.index()], ["scene-panel"]);
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
            r#"{panels: {left: ["status"], top: ["status"]}}"#,
            r#"{panels: {left: [""]}}"#,
            r#"{carousel_motion: "sldie"}"#,
            r#"{menu_items: {left: [{label: "Missing action"}]}}"#,
            r#"{bindings: {left: {pni: "Super+Left"}}}"#,
            r#"{bindings: {cycle_focus: "Super+Escape"}}"#,
            r#"{bindings: {cycle_focus: "Shfit+Left"}}"#,
            r#"{bindings: {cycle_focus: "Super+MadeUpKey"}}"#,
            r#"{bindings: {left: {pin: "Ctrl+Super+a"}, cycle_focus: "Super+Ctrl+A"}}"#,
            // Bare keys: Escape would become a mode change, Tab and letters
            // would steal typing from the panel's own controls.
            r#"{bindings: {left: {hide: "Escape"}}}"#,
            r#"{bindings: {left: {hide: "Ctrl+Escape"}}}"#,
            r#"{bindings: {cycle_focus: "Tab"}}"#,
            r#"{bindings: {right: {pin: "a"}}}"#,
            r#"{bindings: {top: {dock: "F2"}}}"#,
            r#"{bindings: {top: {dock: "Shift+A"}}}"#,
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

    const ORDER_SOURCE: &str = r#"{
        panels: {left: ["scene-launcher", "scene-calendar"], right: ["settings.appearance"], top: ["scene-top"]},
        menu_items: {right: [{label: "Open tools", target: "tools", verb: "tools.open", args: ["main"]}]},
        bindings: {left: {pin: "Super+Shift+Left"}, cycle_focus: "Super+Tab"},
        carousel_motion: "fade"
    }"#;

    fn order(body: serde_json::Value) -> Result<Vec<(Edge, Vec<String>)>, OrderRefusal> {
        parse_panel_order(&body)
    }

    #[test]
    fn panel_order_moves_a_page_across_edges_in_one_replacement() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        std::fs::write(&path, ORDER_SOURCE).unwrap();
        let (reader, watch) = ConfigReader::start(path.clone(), Arc::new(|_, _| {})).unwrap();
        let mut app = app(model());
        app.insert_resource(reader);
        app.update();
        let before = app.world().resource::<ShellConfig>().clone();
        let inode = std::fs::metadata(&path).unwrap().ino();

        // scene-calendar leaves left and joins right in the same call.
        let request = order(serde_json::json!({"edges": {
            "right": ["scene-calendar", "settings.appearance"],
            "left": ["scene-launcher"]
        }}))
        .unwrap();
        write_panel_order(&path, &request).unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "the file is replaced by rename, never rewritten in place"
        );
        let entries = std::fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(entries, 1, "no temporary file is left behind");

        // Ingested on the watcher path, like a hand edit, in one step.
        assert!(watch.dispatch_pending().unwrap() > 0);
        app.update();
        let after = app.world().resource::<ShellConfig>().clone();
        assert_eq!(after.panels[Edge::Left.index()], ["scene-launcher"]);
        assert_eq!(
            after.panels[Edge::Right.index()],
            ["scene-calendar", "settings.appearance"]
        );
        // Unnamed edges and every other key keep their values.
        assert_eq!(after.panels[Edge::Top.index()], ["scene-top"]);
        assert_eq!(after.menu_items, before.menu_items);
        assert_eq!(after.bindings, before.bindings);
        assert_eq!(after.cycle_focus, before.cycle_focus);
        assert_eq!(after.carousel_motion, CarouselMotion::Fade);

        // A later hand edit still wins over the written order.
        let replacement = directory.path().join("hand.mix");
        std::fs::write(
            &replacement,
            r#"{panels: {left: ["scene-calendar", "scene-launcher"], right: ["settings.appearance"]}}"#,
        )
        .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(watch.dispatch_pending().unwrap() > 0);
        app.update();
        let hand = app.world().resource::<ShellConfig>();
        assert_eq!(
            hand.panels[Edge::Left.index()],
            ["scene-calendar", "scene-launcher"]
        );
        assert_eq!(hand.panels[Edge::Right.index()], ["settings.appearance"]);
        assert_eq!(hand.carousel_motion, CarouselMotion::Slide);
    }

    #[test]
    fn panel_order_creates_a_missing_file_and_clears_an_edge() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin/conf.mix");
        let request = order(serde_json::json!({"edges": {"bottom": ["scene-panel"], "top": []}})).unwrap();
        write_panel_order(&path, &request).unwrap();
        let written = ShellConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.panels[Edge::Bottom.index()], ["scene-panel"]);
        assert!(written.panels[Edge::Top.index()].is_empty());
    }

    #[test]
    fn panel_order_refuses_bad_requests_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        std::fs::write(&path, ORDER_SOURCE).unwrap();
        let many: Vec<String> = (0..=MAX_ORDER_PAGES).map(|n| format!("p{n}")).collect();
        for (body, edges) in [
            (serde_json::json!({}), vec![]),
            (serde_json::json!({"edges": {}}), vec![]),
            (serde_json::json!({"edges": []}), vec![]),
            (serde_json::json!({"edges": {"middle": ["a"]}}), vec![]),
            (serde_json::json!({"edges": {"left": "a"}}), vec![]),
            (serde_json::json!({"edges": {"left": [""]}}), vec!["left"]),
            (serde_json::json!({"edges": {"left": ["has space"]}}), vec!["left"]),
            (serde_json::json!({"edges": {"left": [7]}}), vec!["left"]),
            (serde_json::json!({"edges": {"left": many.clone()}}), vec!["left"]),
            // Within one edge, and across two named edges.
            (serde_json::json!({"edges": {"left": ["a", "a"]}}), vec!["left"]),
            (
                serde_json::json!({"edges": {"left": ["a"], "right": ["a"]}}),
                vec!["left", "right"],
            ),
        ] {
            let refusal = order(body.clone()).unwrap_err();
            assert_eq!(refusal.code, "INVALID_ARGUMENT", "{body}");
            assert_eq!(refusal.edges, edges, "{body}");
            assert_eq!(refusal.body()["error_code"], "INVALID_ARGUMENT");
        }
        let most = serde_json::json!({"edges": {"left": &many[..MAX_ORDER_PAGES]}});
        assert_eq!(order(most).unwrap()[0].1.len(), MAX_ORDER_PAGES);

        // A page an unnamed edge still declares cannot also be ordered
        // elsewhere: the "declared on two edges" state is never written.
        let request = order(serde_json::json!({"edges": {"bottom": ["scene-top"]}})).unwrap();
        let refusal = write_panel_order(&path, &request).unwrap_err();
        assert_eq!(
            refusal,
            OrderRefusal::invalid(
                "page scene-top is named on two edges".into(),
                vec!["bottom", "top"]
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORDER_SOURCE);

        // An invalid current file is not rewritten.
        std::fs::write(&path, "{typo: true}").unwrap();
        let request = order(serde_json::json!({"edges": {"left": ["a"]}})).unwrap();
        let refusal = write_panel_order(&path, &request).unwrap_err();
        assert_eq!(refusal.code, "CONFIG_WRITE");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{typo: true}");
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
