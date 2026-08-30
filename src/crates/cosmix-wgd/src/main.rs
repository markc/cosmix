//! cosmix-wgd — the WireGuard mesh control plane (SPEC-13 D0), phase P2.
//!
//! **Derive + dry-run only.** wgd reads this node's signed inventory (the one
//! authoritative membership source — SPEC-13 §7.1 INV-1/§12a), derives the WG
//! peer set this node's kernel *should* hold, and reconciles it against the
//! live kernel in **dry-run**: it reports drift and never mutates the kernel or
//! the inventory. Apply-mode, the ownership state machine, and every write verb
//! are P3 (plan §8).
//!
//! Membership is never authored here — that would be the parallel-registry
//! drift SPEC-13 §12a exists to eliminate. The property substrate is a
//! distribution/derived layer, so P2 registers no SPEC-12 namespace; its Bus
//! surface is read-only derived views ([`bus`]).
//!
//! Structure: [`trust`] verifies the inventory, [`derive`] builds the intended
//! peer set (pure), [`live`] reads the kernel (`wg show dump`), [`reconcile`]
//! diffs (pure, no netlink SET), [`runner`] loops, [`bus`] serves reads.

mod bus;
mod citizen;
mod derive;
mod live;
mod reconcile;
mod runner;
mod state;
mod trust;

use std::process::ExitCode;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};

use trust::TrustPaths;

const DEFAULT_INTERVAL_SECS: u64 = 30;

/// Parsed CLI options. wgd needs almost no config: the interface derives from
/// the mesh identity in the verified inventory, self-identity comes from the
/// node config, and the trust/inventory paths default to noded's canonical
/// locations. Everything here is an override, mostly for lab smoke tests.
struct Opts {
    /// Override the derived kernel interface name (default: first DNS label of
    /// the mesh identity).
    iface: Option<String>,
    /// Override this node's mesh name (default: node config `node`).
    self_name: Option<String>,
    /// Reconcile cadence.
    interval: Duration,
    /// Run exactly one reconcile pass, print the status body, and exit (no
    /// broker needed) — the lab smoke path.
    once: bool,
    /// Trust/inventory path overrides (default: noded canonical paths).
    paths: TrustPaths,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut iface = None;
    let mut self_name = None;
    let mut interval = Duration::from_secs(DEFAULT_INTERVAL_SECS);
    let mut once = false;
    let mut paths = TrustPaths::default();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--iface" => iface = Some(validate_iface(next_val(&mut it, "--iface")?)?),
            "--self" => self_name = Some(next_val(&mut it, "--self")?),
            "--interval" => {
                let s = next_val(&mut it, "--interval")?;
                let secs: u64 = s
                    .parse()
                    .map_err(|_| format!("--interval must be seconds, got {s:?}"))?;
                interval = Duration::from_secs(secs.max(1));
            }
            "--once" => once = true,
            "--genesis" => paths.genesis_pub = next_val(&mut it, "--genesis")?.into(),
            "--signed" => paths.signed = next_val(&mut it, "--signed")?.into(),
            "--baseline" => paths.baseline = next_val(&mut it, "--baseline")?.into(),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Opts {
        iface,
        self_name,
        interval,
        once,
        paths,
    })
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

/// Validate an `--iface` override against the kernel ifname grammar before it
/// reaches `wg show <iface> dump` (`live.rs` documents caller validation). It
/// is passed as a single argv element (no shell), so this is not injection
/// defence — it rejects a malformed interface name early with a clear message
/// instead of a confusing `wg` error. Kernel rule: 1..=IFNAMSIZ-1 (15) bytes,
/// no `/`, `:`, or whitespace, and not `.`/`..`.
fn validate_iface(name: String) -> Result<String, String> {
    if name.is_empty() || name.len() > 15 {
        return Err(format!(
            "--iface {name:?} must be 1..=15 chars (kernel IFNAMSIZ)"
        ));
    }
    if name == "." || name == ".." {
        return Err(format!("--iface {name:?} is not a valid interface name"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(format!(
            "--iface {name:?} has an invalid character (allowed: alphanumeric and .-_)"
        ));
    }
    Ok(name)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(concat!("cosmix-wgd ", env!("CARGO_PKG_VERSION")));
        return ExitCode::SUCCESS;
    }
    let opts = match parse_args(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("cosmix-wgd: {e}");
            return ExitCode::from(2);
        }
    };

    let _log_guard = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-wgd").with_stats(false),
    )
    .expect("logging init failed");

    citizen::report_spec10_identity();

    let self_name = match resolve_self_name(&opts) {
        Ok(name) => name,
        Err(e) => {
            tracing::error!(error = %e, "cannot determine this node's mesh name");
            return ExitCode::FAILURE;
        }
    };

    // One-shot lab smoke: a single reconcile, print the status, exit. No broker.
    if opts.once {
        let shared = state::new_shared();
        return match runner::tick(&opts.paths, opts.iface.as_deref(), &self_name, &shared) {
            Ok(()) => {
                print_once(&shared);
                ExitCode::SUCCESS
            }
            Err(e) => {
                tracing::error!(error = %e, "reconcile tick failed");
                ExitCode::FAILURE
            }
        };
    }

    let shared = state::new_shared();
    let tasks = vec![
        tokio::spawn(runner::run(
            opts.paths,
            opts.iface.clone(),
            self_name,
            opts.interval,
            shared.clone(),
        )),
        tokio::spawn(bus::run(shared)),
    ];

    // Graceful shutdown on SIGINT/SIGTERM; SIGHUP is an advisory "reload"
    // (the reconcile loop already re-reads the inventory every interval, so a
    // HUP is a no-op beyond the log line).
    let mut sighup = signal(SignalKind::hangup()).ok();
    let mut sigterm = signal(SignalKind::terminate()).ok();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received; shutting down");
                break;
            }
            _ = async { sigterm.as_mut().unwrap().recv().await }, if sigterm.is_some() => {
                tracing::info!("SIGTERM received; shutting down");
                break;
            }
            _ = async { sighup.as_mut().unwrap().recv().await }, if sighup.is_some() => {
                tracing::info!("SIGHUP received; reconcile loop re-reads the inventory each interval (no explicit reload needed)");
            }
        }
    }
    for t in tasks {
        t.abort();
    }
    ExitCode::SUCCESS
}

/// This node's mesh name: `--self` override, else the node config's `node`.
fn resolve_self_name(opts: &Opts) -> Result<String, String> {
    if let Some(name) = &opts.self_name {
        return Ok(name.clone());
    }
    match cosmix_config::node::load_node_config() {
        Ok(Some(nc)) if !nc.node.is_empty() => Ok(nc.node),
        Ok(Some(_)) => Err("node config has an empty `node` name; pass --self <name>".into()),
        Ok(None) => Err("no node config found; pass --self <name>".into()),
        Err(e) => Err(format!("loading node config: {e}")),
    }
}

/// Print the `wgd.iface.status` body for the `--once` smoke path.
fn print_once(shared: &state::Shared) {
    if let Ok(guard) = shared.lock()
        && let Some(snap) = guard.as_ref()
    {
        let live = match &snap.live {
            Ok(r) => format!(
                "live OK — {} synced, {} drift ({})",
                r.synced.len(),
                r.drift.len(),
                if r.is_clean() { "in sync" } else { "DRIFT" }
            ),
            Err(e) => format!("live UNAVAILABLE — {e}"),
        };
        println!(
            "cosmix-wgd --once: iface={} mesh={} epoch={} self={} intended_peers={} | {}",
            snap.iface,
            snap.intended.mesh,
            snap.intended.epoch,
            snap.intended.self_name,
            snap.intended.peers.len(),
            live,
        );
    }
}
