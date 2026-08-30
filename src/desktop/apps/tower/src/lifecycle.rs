//! Per-node daemon discovery and lifecycle control over bounded SSH workers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::{Message, MessageReader};
use bevy::prelude::{IntoScheduleConfigs, Res, ResMut, Resource};
use serde::Deserialize;

use crate::model::{now_unix_ms, AtlasState, DaemonUnit, DaemonUnitStatus};

const WORK_CAPACITY: usize = 16;
const COMMAND_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleVerb {
    Start,
    Stop,
    Restart,
}

impl LifecycleVerb {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Message, Clone, Debug)]
pub(crate) struct LifecycleCommand {
    pub node: String,
    pub unit: String,
    pub verb: LifecycleVerb,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeAliasConfig {
    #[serde(default)]
    ssh_aliases: BTreeMap<String, String>,
}

#[derive(Resource)]
struct LifecycleSettings {
    aliases: BTreeMap<String, String>,
    error: Option<String>,
}

impl LifecycleSettings {
    fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self {
                aliases: BTreeMap::new(),
                error: None,
            };
        };
        if !path.exists() {
            return Self {
                aliases: BTreeMap::new(),
                error: None,
            };
        }
        let parsed = std::fs::read_to_string(path)
            .map_err(|error| format!("reading {}: {error}", path.display()))
            .and_then(|source| {
                cosmix_config::from_conf_mix_str::<NodeAliasConfig>(&source)
                    .map_err(|error| format!("parsing {}: {error}", path.display()))
            });
        match parsed {
            Ok(config) => {
                let error = config.ssh_aliases.iter().find_map(|(node, alias)| {
                    (!valid_ssh_alias(alias))
                        .then(|| format!("invalid SSH alias {alias:?} for node {node}"))
                });
                Self {
                    aliases: config.ssh_aliases,
                    error,
                }
            }
            Err(error) => Self {
                aliases: BTreeMap::new(),
                error: Some(error),
            },
        }
    }

    fn alias_for(&self, node: &str) -> Result<String, String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let alias = self
            .aliases
            .get(node)
            .map_or_else(|| node.to_owned(), Clone::clone);
        valid_ssh_alias(&alias)
            .then_some(alias.clone())
            .ok_or_else(|| format!("invalid SSH alias {alias:?} for node {node}"))
    }
}

#[derive(Clone, Debug)]
enum WorkerKind {
    Discover,
    Control { verb: LifecycleVerb, unit: String },
}

#[derive(Clone, Debug)]
struct WorkerRequest {
    id: u64,
    node: String,
    alias: String,
    kind: WorkerKind,
    inventory_epoch: u64,
    queued_at: Instant,
}

#[derive(Clone, Debug)]
enum WorkerOutcome {
    Units(Result<Vec<DaemonUnit>, String>),
    Controlled {
        verb: LifecycleVerb,
        unit: String,
        result: Result<(), String>,
    },
}

#[derive(Clone, Debug)]
struct WorkerResult {
    id: u64,
    node: String,
    outcome: WorkerOutcome,
}

#[derive(Resource)]
struct LifecycleBridge {
    requests: flume::Sender<WorkerRequest>,
    results: flume::Receiver<WorkerResult>,
}

#[derive(Clone, Debug)]
enum PendingKind {
    Discover,
    Control,
}

#[derive(Resource, Default)]
struct LifecycleRuntime {
    next_id: u64,
    selected: Option<String>,
    was_refreshing: bool,
    pending: HashMap<u64, (String, PendingKind)>,
}

pub(crate) struct LifecyclePlugin {
    config_path: Option<PathBuf>,
}

impl LifecyclePlugin {
    pub(crate) fn new(config_path: Option<PathBuf>) -> Self {
        Self { config_path }
    }
}

impl Plugin for LifecyclePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AtlasState>();
        let inventory_epoch = app
            .world()
            .resource::<AtlasState>()
            .lifecycle_epoch_handle();
        let (request_tx, request_rx) = flume::bounded(WORK_CAPACITY);
        let (result_tx, result_rx) = flume::bounded(WORK_CAPACITY);
        let _ = thread::Builder::new()
            .name("cosmix-tower-lifecycle".into())
            .spawn(move || lifecycle_worker(request_rx, result_tx, inventory_epoch));
        app.insert_resource(LifecycleSettings::load(self.config_path.as_deref()))
            .insert_resource(LifecycleBridge {
                requests: request_tx,
                results: result_rx,
            })
            .init_resource::<LifecycleRuntime>()
            .add_message::<LifecycleCommand>()
            .add_systems(
                Update,
                (
                    request_selected_node,
                    handle_lifecycle_commands,
                    drain_lifecycle_results,
                )
                    .chain(),
            );
    }
}

fn request_selected_node(
    bridge: Res<LifecycleBridge>,
    settings: Res<LifecycleSettings>,
    mut runtime: ResMut<LifecycleRuntime>,
    mut atlas: ResMut<AtlasState>,
) {
    let selected_changed = runtime.selected != atlas.selected;
    let refresh_started = atlas.refreshing && !runtime.was_refreshing;
    runtime.selected = atlas.selected.clone();
    runtime.was_refreshing = atlas.refreshing;
    if !selected_changed && !refresh_started {
        return;
    }
    let Some(node) = atlas.selected.clone() else {
        return;
    };
    queue_discovery(&node, &bridge, &settings, &mut runtime, &mut atlas);
}

fn handle_lifecycle_commands(
    mut commands: MessageReader<LifecycleCommand>,
    bridge: Res<LifecycleBridge>,
    settings: Res<LifecycleSettings>,
    mut runtime: ResMut<LifecycleRuntime>,
    mut atlas: ResMut<AtlasState>,
) {
    for command in commands.read() {
        if !atlas.nodes.contains_key(&command.node) {
            record_lifecycle_error(
                &mut atlas,
                &command.node,
                "node is no longer in verified inventory".into(),
            );
            continue;
        }
        if !valid_unit_name(&command.unit) {
            record_lifecycle_error(
                &mut atlas,
                &command.node,
                format!("invalid daemon unit {}", command.unit),
            );
            continue;
        }
        let alias = match settings.alias_for(&command.node) {
            Ok(alias) => alias,
            Err(error) => {
                record_lifecycle_error(&mut atlas, &command.node, error);
                continue;
            }
        };
        let Some(inventory_epoch) = atlas.lifecycle_epoch() else {
            record_lifecycle_error(
                &mut atlas,
                &command.node,
                "not executed: stale or unverified inventory epoch".into(),
            );
            continue;
        };
        let id = next_id(&mut runtime);
        let request = WorkerRequest {
            id,
            node: command.node.clone(),
            alias,
            kind: WorkerKind::Control {
                verb: command.verb,
                unit: command.unit.clone(),
            },
            inventory_epoch,
            queued_at: Instant::now(),
        };
        match bridge.requests.try_send(request) {
            Ok(()) => {
                runtime
                    .pending
                    .insert(id, (command.node.clone(), PendingKind::Control));
                let state = atlas.daemons.entry(command.node.clone()).or_default();
                state.result = Some(format!(
                    "{} {} in progress",
                    command.verb.as_str(),
                    command.unit
                ));
                state.result_observed_at_ms = Some(now_unix_ms());
                atlas.bump();
            }
            Err(error) => record_lifecycle_error(
                &mut atlas,
                &command.node,
                format!("lifecycle worker queue unavailable: {error}"),
            ),
        }
    }
}

fn drain_lifecycle_results(
    bridge: Res<LifecycleBridge>,
    settings: Res<LifecycleSettings>,
    mut runtime: ResMut<LifecycleRuntime>,
    mut atlas: ResMut<AtlasState>,
) {
    for result in bridge.results.try_iter() {
        let Some((node, pending)) = runtime.pending.remove(&result.id) else {
            continue;
        };
        if node != result.node {
            continue;
        }
        if !atlas.nodes.contains_key(&node) {
            atlas.notice = Some(format!(
                "Lifecycle result for {node} ignored: node left verified inventory"
            ));
            atlas.bump();
            continue;
        }
        match (pending, result.outcome) {
            (PendingKind::Discover, WorkerOutcome::Units(units)) => {
                let state = atlas.daemons.entry(node).or_default();
                state.loading = false;
                state.observed_at_ms = Some(now_unix_ms());
                match units {
                    Ok(units) => {
                        state.units = units;
                        state.error = None;
                    }
                    Err(error) => state.error = Some(error),
                }
                atlas.bump();
            }
            (PendingKind::Control, WorkerOutcome::Controlled { verb, unit, result }) => {
                let state = atlas.daemons.entry(node.clone()).or_default();
                state.result_observed_at_ms = Some(now_unix_ms());
                state.result = Some(match result {
                    Ok(()) => format!("{} {} succeeded", verb.as_str(), unit),
                    Err(error) => format!("{} {} failed: {error}", verb.as_str(), unit),
                });
                atlas.bump();
                queue_discovery(&node, &bridge, &settings, &mut runtime, &mut atlas);
            }
            _ => {}
        }
    }
}

fn queue_discovery(
    node: &str,
    bridge: &LifecycleBridge,
    settings: &LifecycleSettings,
    runtime: &mut LifecycleRuntime,
    atlas: &mut AtlasState,
) {
    if !atlas.nodes.contains_key(node) {
        return;
    }
    if runtime
        .pending
        .values()
        .any(|(pending_node, kind)| pending_node == node && matches!(kind, PendingKind::Discover))
    {
        return;
    }
    let alias = match settings.alias_for(node) {
        Ok(alias) => alias,
        Err(error) => {
            record_lifecycle_error(atlas, node, error);
            return;
        }
    };
    let Some(inventory_epoch) = atlas.lifecycle_epoch() else {
        record_lifecycle_error(
            atlas,
            node,
            "not executed: stale or unverified inventory epoch".into(),
        );
        return;
    };
    let id = next_id(runtime);
    let request = WorkerRequest {
        id,
        node: node.to_owned(),
        alias,
        kind: WorkerKind::Discover,
        inventory_epoch,
        queued_at: Instant::now(),
    };
    match bridge.requests.try_send(request) {
        Ok(()) => {
            runtime
                .pending
                .insert(id, (node.to_owned(), PendingKind::Discover));
            let state = atlas.daemons.entry(node.to_owned()).or_default();
            state.loading = true;
            atlas.bump();
        }
        Err(error) => record_lifecycle_error(
            atlas,
            node,
            format!("lifecycle worker queue unavailable: {error}"),
        ),
    }
}

fn next_id(runtime: &mut LifecycleRuntime) -> u64 {
    runtime.next_id = runtime.next_id.wrapping_add(1);
    runtime.next_id
}

fn record_lifecycle_error(atlas: &mut AtlasState, node: &str, error: String) {
    let state = atlas.daemons.entry(node.to_owned()).or_default();
    state.loading = false;
    state.error = Some(error);
    state.observed_at_ms = Some(now_unix_ms());
    atlas.bump();
}

fn lifecycle_worker(
    requests: flume::Receiver<WorkerRequest>,
    results: flume::Sender<WorkerResult>,
    inventory_epoch: Arc<RwLock<Option<u64>>>,
) {
    while let Ok(request) = requests.recv() {
        let fence = validate_worker_request(&request, &inventory_epoch);
        let outcome = match (&request.kind, fence) {
            (WorkerKind::Discover, Err(error)) => WorkerOutcome::Units(Err(error)),
            (WorkerKind::Control { verb, unit }, Err(error)) => WorkerOutcome::Controlled {
                verb: *verb,
                unit: unit.clone(),
                result: Err(error),
            },
            (WorkerKind::Discover, Ok(())) => WorkerOutcome::Units(
                run_ssh(&discovery_argv(&request.alias))
                    .and_then(|output| parse_systemctl_units(&output.stdout)),
            ),
            (WorkerKind::Control { verb, unit }, Ok(())) => WorkerOutcome::Controlled {
                verb: *verb,
                unit: unit.clone(),
                result: run_ssh(&control_argv(&request.alias, *verb, unit)).map(|_| ()),
            },
        };
        if results
            .send(WorkerResult {
                id: request.id,
                node: request.node,
                outcome,
            })
            .is_err()
        {
            break;
        }
    }
}

fn validate_worker_request(
    request: &WorkerRequest,
    inventory_epoch: &RwLock<Option<u64>>,
) -> Result<(), String> {
    if request.queued_at.elapsed() > COMMAND_TTL {
        return Err("not executed: stale command exceeded 30-second TTL".into());
    }
    let current = *inventory_epoch
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if current != Some(request.inventory_epoch) {
        return Err(format!(
            "not executed: stale inventory epoch {} (current {})",
            request.inventory_epoch,
            current.map_or_else(|| "unverified".into(), |epoch| epoch.to_string())
        ));
    }
    Ok(())
}

fn discovery_argv(alias: &str) -> Vec<String> {
    vec![
        "timeout".into(),
        "--signal=KILL".into(),
        "15s".into(),
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "--".into(),
        alias.into(),
        "systemctl".into(),
        "list-units".into(),
        "--type=service".into(),
        "--all".into(),
        "--output=json".into(),
    ]
}

fn control_argv(alias: &str, verb: LifecycleVerb, unit: &str) -> Vec<String> {
    vec![
        "timeout".into(),
        "--signal=KILL".into(),
        "15s".into(),
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "--".into(),
        alias.into(),
        "systemctl".into(),
        verb.as_str().into(),
        unit.into(),
    ]
}

fn run_ssh(argv: &[String]) -> Result<Output, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("empty lifecycle argv".into());
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error("ssh/systemctl", &output))
    }
}

#[derive(Deserialize)]
struct RawUnit {
    unit: String,
    active: String,
}

fn parse_systemctl_units(input: &[u8]) -> Result<Vec<DaemonUnit>, String> {
    let raw: Vec<RawUnit> = serde_json::from_slice(input)
        .map_err(|error| format!("invalid systemctl JSON: {error}"))?;
    let mut units = raw
        .into_iter()
        .filter(|unit| valid_unit_name(&unit.unit))
        .map(|unit| DaemonUnit {
            unit: unit.unit,
            status: match unit.active.as_str() {
                "active" => DaemonUnitStatus::Active,
                "failed" => DaemonUnitStatus::Failed,
                _ => DaemonUnitStatus::Inactive,
            },
        })
        .collect::<Vec<_>>();
    units.sort_by(|left, right| left.unit.cmp(&right.unit));
    Ok(units)
}

pub(crate) fn valid_unit_name(unit: &str) -> bool {
    unit.strip_prefix("cosmix-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .is_some_and(|stem| {
            !stem.is_empty()
                && stem.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'@' | b'.' | b'-')
                })
        })
}

fn valid_ssh_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 253
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn command_error(label: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("{label} exited with {}", output.status)
    } else {
        format!("{label} exited with {}: {}", output.status, concise(detail))
    }
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 240;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push_str("...");
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_exact_cosmix_service_unit_grammar() {
        for valid in [
            "cosmix-noded.service",
            "cosmix-maild@tenant.example.service",
            "cosmix-web.d-1.service",
        ] {
            assert!(valid_unit_name(valid), "{valid}");
        }
        for invalid in [
            "noded.service",
            "cosmix-.service",
            "cosmix-Noded.service",
            "cosmix-noded.timer",
            "cosmix-noded.service;reboot",
            "-cosmix-noded.service",
        ] {
            assert!(!valid_unit_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn constructs_ssh_argv_without_shell_interpolation() {
        assert_eq!(
            control_argv(
                "alpha-admin",
                LifecycleVerb::Restart,
                "cosmix-noded.service"
            ),
            [
                "timeout",
                "--signal=KILL",
                "15s",
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "--",
                "alpha-admin",
                "systemctl",
                "restart",
                "cosmix-noded.service"
            ]
        );
        let discovery = discovery_argv("alpha");
        assert_eq!(discovery[0], "timeout");
        assert_eq!(discovery[9], "alpha");
        assert_eq!(discovery[10], "systemctl");
        assert_eq!(discovery.last().map(String::as_str), Some("--output=json"));
        assert!(
            !discovery.iter().any(|arg| arg.contains('*')),
            "remote argv must contain no shell-expandable glob"
        );
    }

    #[test]
    fn ssh_alias_config_maps_explicit_nodes_and_defaults_to_node_name() {
        let config: NodeAliasConfig = cosmix_config::from_conf_mix_str(
            r#"
                ssh_aliases: {
                    alpha: "alpha-admin",
                    storage: "storage-ops"
                }
            "#,
        )
        .unwrap();
        let settings = LifecycleSettings {
            aliases: config.ssh_aliases,
            error: None,
        };
        assert_eq!(settings.alias_for("alpha").unwrap(), "alpha-admin");
        assert_eq!(settings.alias_for("delta").unwrap(), "delta");
        assert!(LifecycleSettings {
            aliases: BTreeMap::from([("alpha".into(), "-oProxyCommand=bad".into())]),
            error: None,
        }
        .alias_for("alpha")
        .is_err());
    }

    #[test]
    fn parses_only_valid_cosmix_units() {
        let units = parse_systemctl_units(
            br#"[
                {"unit":"cosmix-noded.service","active":"active"},
                {"unit":"cosmix-webd.service","active":"failed"},
                {"unit":"other.service","active":"active"}
            ]"#,
        )
        .unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].status, DaemonUnitStatus::Active);
        assert_eq!(units[1].status, DaemonUnitStatus::Failed);
    }

    #[test]
    fn worker_rejects_expired_and_epoch_stale_commands_before_execution() {
        let epoch = RwLock::new(Some(42));
        let request = WorkerRequest {
            id: 1,
            node: "alpha".into(),
            alias: "alpha".into(),
            kind: WorkerKind::Control {
                verb: LifecycleVerb::Stop,
                unit: "cosmix-noded.service".into(),
            },
            inventory_epoch: 42,
            queued_at: Instant::now(),
        };
        assert!(validate_worker_request(&request, &epoch).is_ok());

        *epoch.write().unwrap() = Some(43);
        assert!(validate_worker_request(&request, &epoch)
            .unwrap_err()
            .contains("stale inventory epoch"));

        let expired = WorkerRequest {
            queued_at: Instant::now() - COMMAND_TTL - Duration::from_secs(1),
            ..request
        };
        assert!(validate_worker_request(&expired, &epoch)
            .unwrap_err()
            .contains("30-second TTL"));
    }
}
