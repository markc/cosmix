mod admin;
mod bus;
mod citizen;
mod controller;
mod lock;
mod mode;
mod reconcile;
mod reporter;
mod service;
mod store;
mod systemd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use cosmix_nspawnd::core::InstanceName;

use crate::lock::LockManager;
use crate::mode::Mode;
use crate::service::{MutationRequest, NspawnService, startup_maintenance};
use crate::store::StateStore;
use crate::systemd::{SystemdBackend, SystemdDbus};

const DAEMON_UID: u32 = 518;
const DAEMON_GID: u32 = 518;

#[derive(Parser)]
#[command(
    name = "cosmix-nspawnd",
    version,
    about = "Generation-fenced nspawn host executor"
)]
struct Cli {
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[arg(long)]
    node: Option<String>,
    #[arg(long, default_value = "/var/lib/cosmix/nspawnd")]
    state_dir: PathBuf,
    #[arg(long, default_value = "/var/lib/cosmix/c0/tombstones")]
    legacy_c0_tombstones: PathBuf,
    #[arg(long, default_value = "/run/lock/cosmix-nspawnd")]
    lock_dir: PathBuf,
    #[arg(long, value_delimiter = ',')]
    operator: Vec<String>,
    #[arg(long)]
    operation_token_file: Option<PathBuf>,
    #[arg(long)]
    operator_token_file: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    reporter: Vec<String>,
    /// Run one local smoke operation without connecting to the Bus.
    #[arg(long, value_enum)]
    once: Option<OnceVerb>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    generation: Option<u64>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnceVerb {
    List,
    Status,
    Start,
    Stop,
    Reconcile,
}

#[derive(Subcommand)]
enum Command {
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Strictly scan and import every JSON tombstone in the configured C0 directory.
    ImportC0Tombstone,
    /// Import one completed C0 placement record as local bootstrap authority.
    ImportC0Grant {
        record: PathBuf,
        /// Explicitly accept that the configured legacy tombstone directory is absent.
        #[arg(long)]
        legacy_absent_ok: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let _guard = match cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-nspawnd").with_stats(false),
    ) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("cosmix-nspawnd: logging init failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    citizen::report_spec10_identity();

    let mode = match Mode::resolve(
        cli.mode,
        std::env::var("COSMIX_NSPAWND_MODE").ok().as_deref(),
    ) {
        Ok(mode) => mode,
        Err(error) => return fail(error),
    };
    if !mode.needs_executor_stack() {
        return run_controller(cli).await;
    }
    let node = match resolve_node(cli.node.as_deref()) {
        Ok(node) => node,
        Err(error) => return fail(error),
    };
    let running_as_root = unsafe { libc::geteuid() } == 0;
    let mut store = StateStore::new(&cli.state_dir, &cli.legacy_c0_tombstones);
    let mut locks = LockManager::new(&cli.lock_dir);
    if running_as_root {
        store = store.with_owner(DAEMON_UID, DAEMON_GID);
        locks = locks.with_owner(DAEMON_UID, DAEMON_GID);
    }
    if let Some(Command::Admin { command }) = cli.command {
        let result = match command {
            AdminCommand::ImportC0Tombstone => admin::import_c0_tombstones(&store, &locks)
                .map(|count| serde_json::json!({"ok":true,"imported":count})),
            AdminCommand::ImportC0Grant {
                record,
                legacy_absent_ok,
            } => admin::import_c0_grant(&store, &locks, &record, &node, legacy_absent_ok)
                .map(|grant| serde_json::json!({"ok":true,"grant":grant})),
        };
        return match result {
            Ok(body) => {
                println!("{}", serde_json::to_string_pretty(&body).unwrap());
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        };
    }

    if let Err(error) = store.ensure_layout() {
        return fail(error);
    }
    if let Err(error) = locks.ensure_root() {
        return fail(error);
    }
    match startup_maintenance(&store, &locks) {
        Ok((interrupted, removed)) => {
            if interrupted > 0 || removed > 0 {
                tracing::warn!(
                    interrupted,
                    gc_removed = removed,
                    "startup operation maintenance completed"
                );
            }
        }
        Err(error) => return print_api_error(error, None),
    }
    let backend = match SystemdDbus::connect().await {
        Ok(backend) => Arc::new(backend),
        Err(error) => return fail(error),
    };
    let backend_trait: Arc<dyn SystemdBackend> = backend;
    let (report_tx, report_rx) = reporter::channel();
    let service = Arc::new(
        NspawnService::new(node, store, locks, backend_trait.clone()).with_reporter(report_tx),
    );
    if let Some(verb) = cli.once {
        return run_once(&cli, verb, &service).await;
    }

    let token_path = cli
        .operation_token_file
        .unwrap_or_else(|| default_credential_path("operation-token"));
    let token = match read_token(&token_path) {
        Ok(token) => token,
        Err(error) => return fail(error),
    };
    let mut operators = cli.operator;
    if let Ok(value) = std::env::var("COSMIX_NSPAWND_OPERATORS") {
        operators.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
    }
    operators.sort();
    operators.dedup();
    let authorizer = match bus::Authorizer::new(operators, token) {
        Ok(authorizer) => authorizer,
        Err(error) => return fail(error),
    };

    let controller_node = match std::env::var("COSMIX_NSPAWND_CONTROLLER_NODE") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return fail("COSMIX_NSPAWND_CONTROLLER_NODE is required in executor serve mode"),
    };
    let operation_token_text = match read_token(&token_path).and_then(|value| {
        String::from_utf8(value).map_err(|_| "operation credential must be UTF-8".into())
    }) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };

    let client = match bus::connect().await {
        Ok(client) => client,
        Err(error) => return fail(error),
    };
    let bus_task = tokio::spawn(bus::run_executor(
        client.clone(),
        service.clone(),
        authorizer,
    ));
    let reporter_task = tokio::spawn(reporter::run(
        report_rx,
        service.clone(),
        client.clone(),
        controller_node,
        operation_token_text,
    ));
    let reconcile_task = tokio::spawn(reconcile::run(service, backend_trait));
    cosmix_daemon::shutdown_signal().await;
    bus_task.abort();
    reconcile_task.abort();
    reporter_task.abort();
    client.shutdown().await;
    ExitCode::SUCCESS
}

async fn run_controller(cli: Cli) -> ExitCode {
    if cli.once.is_some() || cli.command.is_some() {
        return fail("--once and admin commands are executor-mode only");
    }
    if let Err(error) = std::fs::create_dir_all(&cli.state_dir) {
        return fail(format!(
            "creating controller state directory {}: {error}",
            cli.state_dir.display()
        ));
    }
    let operation_token_path = cli
        .operation_token_file
        .unwrap_or_else(|| default_credential_path("operation-token"));
    let operator_token_path = cli
        .operator_token_file
        .unwrap_or_else(|| default_credential_path("operator-token"));
    let operation_token = match read_token(&operation_token_path) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let operator_token = match read_token(&operator_token_path) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let operation_token_text = match String::from_utf8(operation_token.clone()) {
        Ok(value) => value,
        Err(_) => return fail("operation credential must be UTF-8 for the v2 request envelope"),
    };
    let mut operators = cli.operator;
    extend_allowlist(&mut operators, "COSMIX_NSPAWND_OPERATORS");
    let mut reporters = cli.reporter;
    extend_allowlist(&mut reporters, "COSMIX_NSPAWND_REPORTERS");
    let executor_roster = reporter_executor_roster(&reporters);
    let operator_auth = match bus::Authorizer::new(operators, operator_token) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let reporter_auth = match bus::Authorizer::new(reporters, operation_token) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let store = match controller::ControllerStore::open(&cli.state_dir.join("controller.db")) {
        Ok(value) => Arc::new(value),
        Err(error) => return fail(error),
    };
    let client = match bus::connect().await {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let executor = Arc::new(bus::BusExecutorClient::new(client.clone()));
    let service = Arc::new(controller::ControllerService::new(
        store,
        executor,
        operation_token_text,
        executor_roster,
    ));
    if let Err(error) = service.recover().await {
        return print_api_error(error, None);
    }
    let recovery_service = service.clone();
    let recovery_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = recovery_service.recover().await {
                tracing::warn!(error = %error.message, code = error.code, "controller recovery backstop failed");
            }
        }
    });
    let bus_task = tokio::spawn(bus::run_controller(
        client.clone(),
        service,
        operator_auth,
        reporter_auth,
    ));
    cosmix_daemon::shutdown_signal().await;
    bus_task.abort();
    recovery_task.abort();
    client.shutdown().await;
    ExitCode::SUCCESS
}

fn extend_allowlist(values: &mut Vec<String>, variable: &str) {
    if let Ok(value) = std::env::var(variable) {
        values.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
    }
    values.sort();
    values.dedup();
}

fn reporter_executor_roster(reporters: &[String]) -> Vec<String> {
    reporters
        .iter()
        .filter_map(|reporter| reporter.strip_prefix("bridge-"))
        .filter(|node| !node.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn run_once(cli: &Cli, verb: OnceVerb, service: &NspawnService) -> ExitCode {
    let result = match verb {
        OnceVerb::List => service.list().await.map(service::ApiReply::ok),
        OnceVerb::Status => match required_name(cli) {
            Ok(name) => service.status(&name).await.map(service::ApiReply::ok),
            Err(error) => Err(error),
        },
        OnceVerb::Start | OnceVerb::Stop => {
            if let Err(error) = admin::require_root() {
                return fail(error);
            }
            let name = match required_name(cli) {
                Ok(name) => name,
                Err(error) => return print_api_error(error, None),
            };
            let generation = match cli.generation.filter(|value| *value > 0) {
                Some(generation) => generation,
                None => return fail("--generation >= 1 is required for --once start/stop"),
            };
            let request_id = format!("once-{}", ulid::Ulid::new());
            let request = MutationRequest {
                name,
                generation,
                request_id,
                grant: None,
            };
            if matches!(verb, OnceVerb::Start) {
                service.start("local:root", request).await
            } else {
                service.stop("local:root", request).await
            }
        }
        OnceVerb::Reconcile => {
            if let Err(error) = admin::require_root() {
                return fail(error);
            }
            service
                .reconcile_all("local:root")
                .await
                .map(|()| service::ApiReply::ok(serde_json::json!({"ok":true,"reconciled":true})))
        }
    };
    match result {
        Ok(reply) => {
            println!("{}", serde_json::to_string_pretty(&reply.body).unwrap());
            if reply.rc == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => print_api_error(error, None),
    }
}

fn required_name(cli: &Cli) -> Result<InstanceName, service::ApiError> {
    let raw = cli
        .name
        .as_deref()
        .ok_or_else(|| service::ApiError::caller("invalid_request", "--name is required"))?;
    InstanceName::parse(raw)
        .map_err(|error| service::ApiError::caller("invalid_request", error.to_string()))
}

fn print_api_error(error: service::ApiError, request_id: Option<&str>) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&error.body(request_id)).unwrap()
    );
    ExitCode::FAILURE
}

fn resolve_node(override_node: Option<&str>) -> Result<String, String> {
    if let Some(node) = override_node.filter(|value| !value.is_empty()) {
        return Ok(node.to_owned());
    }
    match cosmix_config::node::load_node_config() {
        Ok(Some(config)) if !config.node.is_empty() => Ok(config.node),
        Ok(_) => Err("no node identity found; pass --node".into()),
        Err(error) => Err(format!("loading node identity: {error}")),
    }
}

fn default_credential_path(name: &str) -> PathBuf {
    std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/credentials/cosmix-nspawnd.service"))
        .join(name)
}

fn read_token(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("reading operation credential {}: {error}", path.display()))?;
    let token = bytes.strip_suffix(b"\n").unwrap_or(&bytes).to_vec();
    if token.is_empty() {
        return Err(format!("operation credential {} is empty", path.display()));
    }
    Ok(token)
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("cosmix-nspawnd: {error}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporter_roster_contains_only_bridge_nodes() {
        assert_eq!(
            reporter_executor_roster(&[
                "bridge-alpha".into(),
                "operator".into(),
                "bridge-".into(),
                "bridge-beta".into(),
            ]),
            ["alpha", "beta"]
        );
    }
}
