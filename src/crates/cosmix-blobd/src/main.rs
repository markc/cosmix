use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use cosmix_blobd::citizen::Citizen;
use cosmix_blobd::core::config::Config;
use cosmix_blobd::core::store::{Store, StoreError, StoreOptions};
use cosmix_blobd::lane::LaneBindError;

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

    // Default root: the unit's StateDirectory as systemd hands it over
    // ($STATE_DIRECTORY), else /var/lib/cosmix/blobd. Not the XDG
    // resolver: under the unit's HOME redirect a non-root uid resolves
    // that inside the state dir, not to it.
    let root = cfg.root.clone().unwrap_or_else(|| {
        cosmix_blobd::core::config::default_root(std::env::var("STATE_DIRECTORY").ok().as_deref())
    });

    // `origin` is the node name from node.conf.mix — props-legible,
    // never an IP. `wg_ip` (the same source noded uses) is what the
    // lane's bind is proved against.
    let node = cosmix_config::node::load_node_config();
    let (origin, wg_ip) = match &node {
        Ok(Some(node)) => (node.node.clone(), node.wg_ip.clone()),
        Ok(None) | Err(_) => {
            eprintln!("cosmix-blobd: node.conf.mix not found; origin falls back to \"localhost\"");
            ("localhost".to_string(), String::new())
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

    // The byte lane. The bind proof runs before any socket is opened —
    // fail closed: the bind IP must be this node's own wg_ip, never
    // unspecified, never loopback, never another interface (noded's
    // bind_is_wg, copied). The listener is bound before the citizen is
    // constructed, so lane.bind/lane.port props only exist once the
    // socket is actually listening.
    let lane = match cfg.lane_bind {
        Some(bind) => {
            let listener = match cosmix_blobd::lane::WgProvenBind::bind(bind, &wg_ip).await {
                Ok(listener) => listener,
                Err(LaneBindError::NotWg) => {
                    eprintln!(
                        "cosmix-blobd: lane_bind {bind} is not this node's WG address (wg_ip {:?}) — the lane serves only the mesh; refusing to start",
                        if wg_ip.is_empty() { "<absent>" } else { &wg_ip }
                    );
                    std::process::exit(2);
                }
                Err(LaneBindError::Io(e)) => {
                    return Err(anyhow::anyhow!("bind byte lane {bind}: {e}"));
                }
            };
            let addr = listener
                .local_addr()
                .map_err(|e| anyhow::anyhow!("byte lane local_addr: {e}"))?;
            let lane_store = Arc::clone(&store);
            let max_uploads = cfg.lane_max_uploads;
            let upload_deadline = std::time::Duration::from_secs(cfg.lane_upload_deadline_secs);
            tokio::spawn(async move {
                if let Err(error) = cosmix_blobd::lane::serve_lane(
                    listener,
                    lane_store,
                    max_uploads,
                    upload_deadline,
                )
                .await
                {
                    eprintln!("cosmix-blobd: byte lane stopped: {error}");
                }
            });
            Some(addr)
        }
        None => None,
    };

    let instance = cfg.name.clone().unwrap_or_else(|| "default".to_string());
    // The fetch machinery resolves remote lanes and publishes its
    // completion events through blobd's own broker connection (the
    // citizen keeps the slot current across reconnects).
    let fetcher = Arc::new(cosmix_blobd::fetch::Fetcher::production(
        Arc::clone(&store),
        &cfg,
        lane,
        &instance,
    ));
    let citizen = Arc::new(Citizen::new(
        store,
        cfg.service_name(),
        instance,
        lane,
        fetcher,
    ));
    cosmix_blobd::citizen::serve(citizen, cfg.verb_max_concurrent).await
}
