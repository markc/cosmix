//! cosmix-noded — Consolidated node daemon.
//!
//! Single binary running the broker (WebSocket message broker), system
//! monitor, and Bus traffic logger as async tasks.

use anyhow::Result;
use clap::Parser;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod admission;
mod authority;
mod log_props;
mod logger;
mod mon_props;
mod monitor;
mod noded;
mod observe;
mod props;
mod props_reservation;
mod routing;
mod spec;
mod subscription;

/// `--version` line including git sha + build time (build-provenance
/// contract). `COSMIX_*` are set by `build.rs` → `cosmix_buildinfo::emit()`.
///
/// NB: this uses `env!` (not `option_env!`) deliberately — a daemon that
/// advertises a provenance version line MUST have the `build.rs`; if a
/// future daemon copies this `const` but forgets `build.rs`, it fails to
/// compile loudly rather than silently shipping `"unknown"`. (The
/// `build_info!()` macro, by contrast, uses `option_env!` so library
/// crates without a `build.rs` still build.)
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("COSMIX_GIT_SHA"),
    ", built ",
    env!("COSMIX_BUILD_TIME"),
    ")"
);

#[derive(Parser)]
#[command(name = "cosmix-noded", version = VERSION, about = "Consolidated cosmix node daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the node daemon (default)
    Serve {
        /// Listen address override (default: derived from node.conf.mix)
        #[arg(long)]
        listen: Option<String>,

        /// Node name override (default: from node.conf.mix)
        #[arg(long)]
        node: Option<String>,

        /// Path to mesh config file (default: from node.conf.mix)
        #[arg(long)]
        mesh_config: Option<String>,

        /// Disable the system monitor module
        #[arg(long)]
        no_monitor: bool,

        /// Disable the Bus traffic logger module
        #[arg(long)]
        no_log: bool,

        /// Path to the `_spec/` directory (default: discovered via env/cwd)
        #[arg(long)]
        spec_dir: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-noded").with_stats(false),
    )
    .expect("logging init failed");

    let cli = Cli::parse();

    // Extract CLI args (default to Serve if no subcommand)
    let (cli_listen, cli_node, cli_mesh_config, no_monitor, no_log, cli_spec_dir) =
        match cli.command {
            Some(Command::Serve {
                listen,
                node,
                mesh_config,
                no_monitor,
                no_log,
                spec_dir,
            }) => (listen, node, mesh_config, no_monitor, no_log, spec_dir),
            None => (None, None, None, false, false, None),
        };

    // Load node.conf.mix, then apply CLI overrides
    let node_cfg = cosmix_config::node::require_node_config()?;
    let listen = cli_listen.unwrap_or_else(|| node_cfg.noded_listen());
    let node = cli_node.unwrap_or_else(|| node_cfg.node.clone());
    let mesh_config = cli_mesh_config.or_else(|| node_cfg.noded.mesh_config.clone());

    let noded_url = format!("ws://{}/ws", listen);

    let spec_dir = spec::locate_spec_dir(cli_spec_dir.as_deref());
    tracing::info!(
        node = %node,
        listen = %listen,
        spec_dir = ?spec_dir,
        "Starting cosmix-noded",
    );

    // Start the broker with a readiness signal
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let noded_node = node.clone();
    let admission_mode = node_cfg.noded.admission;
    let observe_allowed_services = node_cfg.observe.allowed_services.clone();
    let wg_ip = node_cfg.wg_ip.clone();
    let mut noded_handle = tokio::spawn(async move {
        noded::run(
            noded::RunConfig {
                listen,
                node: noded_node,
                wg_ip,
                mesh_config_path: mesh_config,
                spec_dir,
                admission_mode,
                observe_allowed_services,
            },
            ready_tx,
        )
        .await
    });

    // Wait for the broker listener to be bound
    if ready_rx.await.is_err() {
        anyhow::bail!("Broker failed to start");
    }

    // Spawn client modules
    let monitor_handle = if !no_monitor {
        let mon_url = noded_url.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = monitor::run(&mon_url).await {
                tracing::error!("Monitor module failed: {e}");
            }
        }))
    } else {
        None
    };

    let logger_handle = if !no_log {
        let log_url = noded_url.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = logger::run(&log_url).await {
                tracing::error!("Logger module failed: {e}");
            }
        }))
    } else {
        None
    };

    // Wait for shutdown signal or broker failure
    let broker_exit = tokio::select! {
        _ = cosmix_daemon::shutdown_signal() => {
            tracing::info!("Shutdown signal received");
            None
        }
        result = &mut noded_handle => {
            Some(result)
        }
    };

    // Cleanup — abort remaining tasks
    if let Some(h) = monitor_handle {
        h.abort();
    }
    if let Some(h) = logger_handle {
        h.abort();
    }

    if let Some(result) = broker_exit {
        return match result {
            Ok(Ok(())) => anyhow::bail!("Broker exited unexpectedly"),
            Ok(Err(error)) => Err(error),
            Err(error) => anyhow::bail!("Broker task failed: {error}"),
        };
    }

    tracing::info!("cosmix-noded stopped");
    Ok(())
}
