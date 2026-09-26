use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use cosmix_blobd::citizen::Citizen;
use cosmix_blobd::core::config::Config;
use cosmix_blobd::core::store::{Store, StoreError, StoreOptions};

#[derive(Parser)]
#[command(
    name = "cosmix-blobd",
    version,
    about = "Node blob store: content-addressed bytes, owner pins, quotas, GC"
)]
struct Cli {
    /// Flat `key: value` configuration file (see docs/cos/cosmix-blobd).
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    // --version/-V first, before the tokio runtime exists: a thread- or
    // fd-starved host must still get an answer, not a runtime-build panic.
    cosmix_buildinfo::exit_on_version!();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = match &cli.config {
        Some(path) => Config::parse(
            &std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?,
        )
        .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?,
        None => Config::default(),
    };

    // Default root: the unit's StateDirectory via the shared FHS/XDG
    // resolver (/var/lib/cosmix/blobd on a system install).
    let root = cfg.root.clone().unwrap_or_else(|| {
        cosmix_config::paths::cosmix_path(cosmix_config::paths::CosmixDir::Var).join("blobd")
    });

    // `origin` is the node name from node.conf.mix — props-legible,
    // never an IP.
    let origin = match cosmix_config::node::load_node_config() {
        Ok(Some(node)) => node.node,
        Ok(None) | Err(_) => {
            eprintln!("cosmix-blobd: node.conf.mix not found; origin falls back to \"localhost\"");
            "localhost".to_string()
        }
    };

    let store = match Store::open(&root, StoreOptions::from_config(&cfg, origin)) {
        Ok(store) => Arc::new(store),
        Err(StoreError::Locked(path)) => {
            eprintln!(
                "cosmix-blobd: another instance holds {}; one GC owner per root — exiting",
                path.display()
            );
            std::process::exit(2);
        }
        Err(error) => return Err(error.into()),
    };
    let report = store.startup_report();
    if !report.orphans.is_empty() {
        eprintln!(
            "cosmix-blobd: startup reconcile found {} orphan CAS file(s); run blob.gc to sweep",
            report.orphans.len()
        );
    }

    let instance = cfg.name.clone().unwrap_or_else(|| "default".to_string());
    let citizen = Arc::new(Citizen::new(
        store,
        cfg.service_name(),
        instance,
        cfg.lane_bind,
    ));
    cosmix_blobd::citizen::serve(citizen).await
}
