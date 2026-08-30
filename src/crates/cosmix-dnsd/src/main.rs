//! `cosmix-dnsd` — authoritative WG-mesh DNS daemon.
//!
//! ## P1/P2 feature seam (deliberate, plan-polarity-preserving)
//!
//! `Cargo.toml` keeps **`default = ["cosmix"]`** (plan §1/§9 polarity —
//! `--no-default-features` *is* goal-(c) isolated-node self-resolution).
//! In P1 the `cosmix` feature is **declared but empty-bodied** (no
//! `-bus`/`-client`/`-config` deps, no identity code), so the default
//! build and the `--no-default-features` build were **byte-identical —
//! both the standalone server**. P2 fills the `cosmix` body + pulls its
//! deps; the *default* build then becomes the citizen with **zero
//! polarity change**. This is a seam, NOT a half-finished citizen.
//!
//! ## The surviving `--no-default-features` invariant (reframed twice)
//!
//! Literal whole-binary byte-identity held through P1/P2-A/P2-B only
//! because none touched the shared `cosmix-lib-dns` core. It was then
//! forced to move twice, each time because a *binary*-level identity
//! claim turned out physically unattainable while the *source* property
//! it proxied stayed perfectly intact:
//!
//!   * **P2-C** adds the additive `serve_*_observed` siblings to the
//!     core (§7 rcode counting needs serve-path observation; plan §1
//!     forbids forking the loops into the binary). Adding *any* symbol
//!     to the shared rlib repacks its `.rodata` and relocates every
//!     `%rip`-relative constant — literal byte-identity dies here.
//!   * **P2-D**: the workspace `[profile.release]` pins `lto = true`.
//!     Cross-crate LTO redistributes how much of `serve_udp`/`serve_tcp`
//!     it inlines into the polled `async fn main` future from the
//!     *global* module composition. Building P1's exact `main.rs` (zero
//!     source change) against a core that merely gained the
//!     dead-stripped `_observed` siblings moves thousands of insns into
//!     `main`'s future with whole-`.text` size unchanged — so an
//!     instruction-multiset assertion is a false-failure generator under
//!     `lto = true` once the core legitimately grows, with zero logic
//!     change. (See `feedback_lto_inlining_redistribution`.)
//!
//! The load-bearing invariant — true, preserved, and verifiable — is
//! that the `--no-default-features` serve path is the committed P1
//! source **verbatim**: (a) the shared core diff vs P1 is purely
//! additive at the *code* level (zero P1 code lines removed/modified;
//! `//`-comment/doc reconciliation is exempt; the `_observed` siblings
//! are dead-stripped) AND the four standalone-reached `serve.rs` fns
//! (`negotiate_udp`/`serve_udp`/`serve_tcp`/`handle_tcp_conn`) are
//! P1-verbatim contiguous blocks with blank-line prefix intact (so
//! code appended *inside*, or an attribute added *above*, a P1 body
//! is caught — not just modify/delete), (b) the standalone
//! `main.rs` bind/serve arms are P1's bare statements under
//! statement-level `#[cfg]` (never block/tuple-wrapped — a block inside
//! the `async fn` mutates the future state machine: the P2-D
//! regression), and (c) the standalone binary's serve *behaviour* is
//! proven by the §8 functional prober. The `.text` instruction multiset
//! is **NOT** load-bearing and is not asserted (advisory disasm only).
//! See plan §1 "surviving `--no-default-features` invariant".
//!
//! ## Bind enforcement — only what P1 can actually do
//!
//! P1 has no citizen config, so it *cannot* verify an address truly is
//! the WG IP. It therefore: ALWAYS rejects unspecified/wildcard binds
//! (`0.0.0.0`/`::`) — hard error, no override; always allows loopback;
//! allows any other (non-loopback) address **only** with
//! `--allow-non-loopback-listen` **and** a WARN that WG-vs-LAN is not
//! validated until P2 (plan §6).
//!
//! The **citizen build** (default features) supersedes this: P2-B reads
//! `node.conf.mix` for this node's WG mesh IP and binds `wg_ip:53` or
//! loopback only — see `citizen.rs`. The block above is kept verbatim
//! as the `--no-default-features` standalone path and is never
//! loosened by the citizen layer.
//!
//! ## P2-D citizen-only additions (logging + WG-up bind-retry)
//!
//! Two more citizen-only layers, each cfg-split so the standalone arm
//! stays the committed P1 source **verbatim** (surviving invariant —
//! statement-level `#[cfg]`, never block/tuple-wrapped):
//!
//!   * **journald logging** — the citizen uses the shared
//!     `cosmix_log::init(...).daemon("cosmix-dnsd")` (journald-primary,
//!     stats off — this arc is logging-only).
//!     Standalone keeps the P1 stderr-only init (zero cosmix deps —
//!     goal-(c)). The crate-target arg is the UNDERSCORE form.
//!   * **WG-up-before-bind** (plan §6) — the citizen wraps the bind in
//!     a bounded retry on `AddrNotAvailable` (the WG tunnel still
//!     coming up); see `bind_retry`. Standalone binds plain (loopback
//!     / explicit `--listen`, never WG-dependent). The systemd unit
//!     (`src/_etc/systemd/cosmix-dnsd.service`, `User=cosmix-dnsd`
//!     UID/GID 506, `StateDirectory=cosmix/dnsd`) carries an `After=`
//!     ordering *hint* only — the bounded retry is the actual
//!     guarantee, keeping the unit portable across the mesh nodes'
//!     differing WG-bring-up mechanisms.

// SPEC-10 daemon-identity legibility surface — citizen build only.
// Excluded entirely under `--no-default-features`; its serve path
// stays the committed P1 source verbatim (surviving invariant, see
// the module doc above).
#[cfg(feature = "cosmix")]
mod citizen;

// P2-C read-only Bus surface + the rcode counters it reads — citizen
// build only. `--no-default-features` compiles neither and calls the
// unchanged core `serve_udp`/`serve_tcp` (NOT the `_observed`
// siblings), so its serve path stays the committed P1 source verbatim
// (plan §1 surviving invariant; the P2-B cfg-split precedent).
#[cfg(feature = "cosmix")]
mod bus;
#[cfg(feature = "cosmix")]
mod stats;

// P2-D WG-up-before-bind (plan §6) — citizen build only. Excluded
// under `--no-default-features`; the standalone arm of the bind below
// is the committed P1 source verbatim (plain `bind`, no retry), so its
// serve path stays the committed P1 source verbatim (surviving
// invariant; §6 "Standalone P1 stays clear of this entirely").
#[cfg(feature = "cosmix")]
mod bind_retry {
    //! Bounded retry of the `WG-IP:53` bind on `AddrNotAvailable`.
    //!
    //! Plan §6 names two acceptable WG-up-before-bind mechanisms:
    //! systemd `After=`/`BindsTo=` the WG unit, **or** a bounded
    //! bind-retry on `EADDRNOTAVAIL`. We implement the retry — it is
    //! the deployment-unit-name-agnostic one: the 4 mesh nodes do not
    //! all bring WireGuard up the same way (`wg-quick@wg0` vs
    //! systemd-networkd vs NetworkManager), so coupling the canonical
    //! unit to one WG unit name would be fragile. The citizen owning
    //! the retry keeps the unit portable; the unit still carries an
    //! `After=` ordering *hint* (non-load-bearing — see the unit).
    //!
    //! `AddrNotAvailable` (Linux `EADDRNOTAVAIL`) is precisely "no
    //! interface carries this address yet" — exactly the still-coming-
    //! up WG tunnel. Loopback and an already-present address bind on
    //! the first attempt (never `AddrNotAvailable`), so `dig
    //! @127.0.0.1` debugging and a WG IP that *is* already up are
    //! unaffected — the retry only spins while the tunnel is settling.
    //! The wait is **bounded**: a genuinely wrong address (one that
    //! will never appear) still fails the unit fast rather than
    //! hanging forever, and systemd `Restart=on-failure` then re-tries
    //! at the unit level (a longer outer backoff than this inner one).
    //! Any non-`AddrNotAvailable` error (e.g. `AddrInUse`) fails
    //! immediately — those are not "WG not up yet".

    use std::io;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tokio::net::{TcpListener, UdpSocket};

    /// Total wall-clock budget for the inner retry. Kept short: this
    /// only covers the WG tunnel *settling* after the unit's ordering
    /// hint already fired; longer outages are systemd's job via
    /// `Restart=on-failure`.
    const WG_BIND_TOTAL_WAIT: Duration = Duration::from_secs(30);
    /// Fixed inter-attempt pause (no backoff needed for a ≤30 s budget;
    /// WG comes up within a second or two once its unit runs).
    const WG_BIND_RETRY_INTERVAL: Duration = Duration::from_secs(1);

    /// Bind the UDP socket + TCP listener for one `--listen` address,
    /// retrying the pair on `AddrNotAvailable` until the bounded
    /// deadline. The UDP+TCP pair is bound together so a half-open
    /// state (UDP up, TCP still `AddrNotAvailable`) also retries.
    pub async fn bind_pair(sa: &SocketAddr) -> io::Result<(UdpSocket, TcpListener)> {
        let deadline = Instant::now() + WG_BIND_TOTAL_WAIT;
        loop {
            match try_bind_pair(sa).await {
                Ok(pair) => return Ok(pair),
                Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => {
                    // Budget exhausted → fail the unit (systemd
                    // `Restart=on-failure` retries at the outer level).
                    // The check is *after* an attempt, so the bind is
                    // always tried at least once and again at/just past
                    // the deadline — the full WG_BIND_TOTAL_WAIT window
                    // is used, not WAIT-minus-one-interval (no early
                    // exit, no skipped final attempt).
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(e);
                    }
                    // Clamp the last nap so the loop cannot overshoot
                    // the deadline by an interval, while still pausing
                    // (never a busy-wait).
                    let nap = WG_BIND_RETRY_INTERVAL.min(deadline - now);
                    tracing::warn!(
                        %sa, %e,
                        "bind address not yet available (WG interface up?) — retrying within the bounded WG-up window (plan §6 citizen bounded bind-retry)"
                    );
                    tokio::time::sleep(nap).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn try_bind_pair(sa: &SocketAddr) -> io::Result<(UdpSocket, TcpListener)> {
        let udp = UdpSocket::bind(sa).await?;
        let tcp = TcpListener::bind(sa).await?;
        Ok((udp, tcp))
    }
}

use cosmix_dns::{FilePersistence, StateLoad, StaticZoneStore, ZoneStore, serve};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
// Only the standalone arm names these types directly (its verbatim P1
// bind `match` blocks). The citizen build binds via `bind_retry`,
// which imports them itself — so this top-level `use` is
// standalone-only (an unconditional `use` would be an unused-import
// warning under the default/citizen build). Source-irrelevant to the
// invariant: a `use` declaration is not in the serve path, and the
// standalone bind/serve statements it supports are P1-verbatim.
#[cfg(not(feature = "cosmix"))]
use tokio::net::{TcpListener, UdpSocket};

struct Args {
    zones: PathBuf,
    state: PathBuf,
    listen: Vec<SocketAddr>,
    allow_non_loopback: bool,
}

fn usage() -> &'static str {
    "usage: cosmix-dnsd --zones <zones.mix> --state <state-file> \
     --listen <ip:port> [--listen <ip:port> ...] \
     [--allow-non-loopback-listen]"
}

fn parse_args() -> Result<Args, String> {
    let mut zones = None;
    let mut state = None;
    let mut listen = Vec::new();
    let mut allow_non_loopback = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--zones" => {
                zones = Some(PathBuf::from(it.next().ok_or("--zones needs a value")?));
            }
            "--state" => {
                state = Some(PathBuf::from(it.next().ok_or("--state needs a value")?));
            }
            "--listen" => {
                let v = it.next().ok_or("--listen needs a value")?;
                let sa: SocketAddr = v
                    .parse()
                    .map_err(|_| format!("--listen {v:?} is not ip:port"))?;
                listen.push(sa);
            }
            "--allow-non-loopback-listen" => allow_non_loopback = true,
            "-h" | "--help" => return Err(usage().to_string()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }

    Ok(Args {
        zones: zones.ok_or("--zones is required")?,
        state: state.ok_or("--state is required")?,
        listen: if listen.is_empty() {
            return Err("at least one --listen is required".to_string());
        } else {
            listen
        },
        allow_non_loopback,
    })
}

/// Bind enforcement (prompt §11 rule 10). Wildcard is always a hard
/// error; non-loopback needs the explicit flag + a WARN.
fn check_bind(sa: &SocketAddr, allow_non_loopback: bool) -> Result<(), String> {
    let ip = sa.ip();
    let unspecified = match ip {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified(),
    };
    if unspecified {
        return Err(format!(
            "refusing wildcard/unspecified bind {sa} (0.0.0.0/:: is never allowed; no flag overrides this)"
        ));
    }
    if ip.is_loopback() {
        return Ok(());
    }
    if !allow_non_loopback {
        return Err(format!(
            "refusing non-loopback bind {sa} without --allow-non-loopback-listen"
        ));
    }
    tracing::warn!(
        %sa,
        "binding non-loopback address; WG-vs-LAN is NOT validated in P1 (P2 citizen-config concern, plan §6)"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    // `--version`/`-V` before anything else (mirrors clap's early
    // intercept in the sibling daemons): print to stdout, exit 0, ahead
    // of logging init so there's no log noise and the hand-rolled
    // `parse_args` never sees the flag as an "unknown argument".
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!(concat!("cosmix-dnsd ", env!("CARGO_PKG_VERSION")));
        return ExitCode::SUCCESS;
    }

    // Logging. The standalone (`--no-default-features`) arm is the P1
    // source VERBATIM — stderr-only, no cosmix-family dep
    // (goal-(c): the isolated-node server pulls ZERO cosmix crates;
    // journald/the controlling terminal captures stderr). The citizen
    // uses the shared `cosmix_log::init(...)` core (journald-primary,
    // stats off). The returned `LogHandle` MUST outlive the process
    // (drop = final flush), so it is bound for the whole of `main` (it
    // drops at each early `return`, which still flushes). The identity
    // argument is the **hyphenated package name `cosmix-dnsd`** —
    // `cosmix_log` derives the default EnvFilter target by replacing
    // `-` with `_` (→ `cosmix_dnsd`), so `RUST_LOG=cosmix_dnsd=info`
    // still matches (the RUST_LOG/target-name trap recorded as
    // feedback_tracing_envfilter_crate_target). Mirrors the other
    // daemons' `cosmix_log::init(...).daemon("cosmix-<name>")`.
    #[cfg(not(feature = "cosmix"))]
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    #[cfg(feature = "cosmix")]
    let _log_guard = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-dnsd").with_stats(false),
    )
    .expect("logging init failed");

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    // Bind enforcement. The standalone (`--no-default-features`) build
    // runs the P1 mechanical check VERBATIM — the block below is the
    // exact P1 source (source-verbatim; see the module-doc surviving
    // invariant) so the standalone server never regresses behaviourally
    // (proven by the §8 functional prober). The citizen build layers
    // the plan §6 WG-vs-LAN posture
    // on top, reading `node.conf.mix` for this node's WG mesh IP (see
    // `citizen.rs`); it never loosens the P1 rules.
    #[cfg(not(feature = "cosmix"))]
    for sa in &args.listen {
        if let Err(e) = check_bind(sa, args.allow_non_loopback) {
            tracing::error!("{e}");
            return ExitCode::from(2);
        }
    }
    #[cfg(feature = "cosmix")]
    {
        let wg_ip = citizen::configured_wg_ip();
        for sa in &args.listen {
            if let Err(e) = citizen::citizen_check_bind(sa, args.allow_non_loopback, wg_ip) {
                tracing::error!("{e}");
                return ExitCode::from(2);
            }
        }
    }

    // SPEC-10 daemon-identity legibility surface (citizen build only).
    // Pure logging + an advisory euid/egid self-report; never refuses,
    // changes no serving behaviour. The `--no-default-features`
    // standalone build skips this entirely; its serve path stays the
    // committed P1 source verbatim (surviving invariant).
    #[cfg(feature = "cosmix")]
    citizen::report_spec10_identity();

    // State file: a corrupt/unreadable file at boot is availability-first
    // (log loudly, treat both floors absent, keep serving — NOT a hard
    // stop; safe in v0: no replay channel, no zone-transfer secondaries).
    let persistence = match FilePersistence::open(&args.state) {
        StateLoad::Ok(p) => p,
        StateLoad::Corrupt { store, detail } => {
            tracing::error!(
                path = %args.state.display(),
                %detail,
                "state file is corrupt/unreadable — treating both floors as ABSENT and continuing to serve (v0 availability-first; NOT a hard stop)"
            );
            store
        }
    };

    // First boot with a bad zones.mix and no last-good is the ONLY v0
    // hard stop.
    let store = match StaticZoneStore::load_initial(args.zones.clone(), Box::new(persistence)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("fatal: {e}");
            return ExitCode::from(1);
        }
    };

    let store_dyn: Arc<dyn ZoneStore> = store.clone();
    let mut tasks = Vec::new();

    // P2-C citizen: one shared per-rcode counter, observed by every
    // serve task and read by the `dnsd.stats` Bus action. Excluded
    // entirely under `--no-default-features` — the standalone P1 server
    // holds no stats state (plan §1; see `stats.rs`).
    #[cfg(feature = "cosmix")]
    let stats = Arc::new(stats::DnsStats::new());

    for sa in &args.listen {
        // Bind. The `--no-default-features` arm is the committed P1
        // source VERBATIM — plain `UdpSocket::bind`/`TcpListener::bind`,
        // NO WG-up retry: §6 "Standalone P1 stays clear of this
        // entirely; its tests and smoke bind loopback / an explicit
        // --listen, so P1 acceptance never depends on a live WG
        // interface". Statement-level `#[cfg]` is stripped before
        // codegen, so in the `--no-default-features` build these two
        // `let … = match … .await { … }` statements are the P1 token
        // stream verbatim (surviving invariant). They are deliberately
        // NOT wrapped in a `let (udp, tcp) = { … }` block: a
        // tuple-returning block around `.await` points inside an
        // `async fn` adds a future state / temporary and changes the
        // standalone serve path (it cost +147 standalone `.text` insns
        // at P2-D and was reverted). `dnsd_p2_invariant.mix` GATE B
        // asserts this wrapper-free bare-statement shape directly in
        // source. The citizen arm adds the
        // plan §6 bounded WG-up bind-retry (`bind_retry`); a
        // non-`AddrNotAvailable` error still fails fast exactly as P1.
        #[cfg(not(feature = "cosmix"))]
        let udp = match UdpSocket::bind(sa).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%sa, %e, "UDP bind failed");
                return ExitCode::from(1);
            }
        };
        #[cfg(not(feature = "cosmix"))]
        let tcp = match TcpListener::bind(sa).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%sa, %e, "TCP bind failed");
                return ExitCode::from(1);
            }
        };
        #[cfg(feature = "cosmix")]
        let (udp, tcp) = match bind_retry::bind_pair(sa).await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!(
                    %sa, %e,
                    "bind failed (after the bounded WG-up retry window; plan §6)"
                );
                return ExitCode::from(1);
            }
        };
        tracing::info!(%sa, "listening (UDP+TCP)");

        // Serve-task spawn. The `--no-default-features` arm is the P1
        // source VERBATIM — unchanged `serve_udp`/`serve_tcp`, no
        // observer — emitted as bare statement-level `#[cfg]` lines
        // (NOT a `#[cfg] { … }` block: a block introduced inside this
        // `async fn` is the same future-state hazard the bind arm
        // documents, so the standalone arm is P1's bare `let s1` /
        // `let s2` / two `tasks.push(…)` statements verbatim — the
        // committed P1 source verbatim, see the module-doc surviving
        // invariant). The
        // citizen arm calls the additive `*_observed` siblings
        // (cosmix-lib-dns, pinned wire-identical by
        // `observed_matches_unobserved_wire`) with a pure closure that
        // bumps the shared rcode counter — zero core *logic* change,
        // plan §1/§7.
        #[cfg(not(feature = "cosmix"))]
        let s1 = Arc::clone(&store_dyn);
        #[cfg(not(feature = "cosmix"))]
        let s2 = Arc::clone(&store_dyn);
        #[cfg(not(feature = "cosmix"))]
        tasks.push(tokio::spawn(async move {
            if let Err(e) = serve::serve_udp(udp, s1).await {
                tracing::error!(%e, "serve_udp exited (fatal socket error)");
            }
        }));
        #[cfg(not(feature = "cosmix"))]
        tasks.push(tokio::spawn(async move {
            if let Err(e) = serve::serve_tcp(tcp, s2).await {
                tracing::error!(%e, "serve_tcp exited (fatal listener error)");
            }
        }));
        #[cfg(feature = "cosmix")]
        {
            let s1 = Arc::clone(&store_dyn);
            let s2 = Arc::clone(&store_dyn);
            let o1: cosmix_dns::ResponseObserver = {
                let st = Arc::clone(&stats);
                Arc::new(move |resp| st.record(resp))
            };
            let o2 = Arc::clone(&o1);
            tasks.push(tokio::spawn(async move {
                if let Err(e) = serve::serve_udp_observed(udp, s1, o1).await {
                    tracing::error!(%e, "serve_udp exited (fatal socket error)");
                }
            }));
            tasks.push(tokio::spawn(async move {
                if let Err(e) = serve::serve_tcp_observed(tcp, s2, o2).await {
                    tracing::error!(%e, "serve_tcp exited (fatal listener error)");
                }
            }));
        }
    }

    // Citizen code-default loopback leg (generic mesh resolved drop-in).
    // The mesh ships ONE byte-identical systemd-resolved drop-in to all
    // four nodes — `DNS=127.0.0.1` + `Domains=~bus ~example.com` — so no
    // node has to hardwire its own WG IP into resolved just to bootstrap
    // mesh DNS. For that to hold the citizen also answers on
    // 127.0.0.1:53, in addition to the `<wg-ip>:53` peer leg that
    // `service.env`'s `--listen` carries. 127.0.0.1:53 never collides
    // with systemd-resolved's 127.0.0.53:53 stub (see `citizen.rs`).
    //
    // BEST-EFFORT, by design: this is a code-injected default the
    // operator did not request, so it MUST NOT be able to turn an
    // otherwise-healthy daemon (every explicit `--listen` bound) into an
    // `exit 1`. Production runs under the systemd unit with
    // CAP_NET_BIND_SERVICE, where :53 binds fine; an unprivileged run
    // (the standalone prober against the *default* binary, ad-hoc `dig`
    // debugging) instead logs one WARN and serves the explicit
    // address(es) only. If the operator already passed `127.0.0.1:53`
    // explicitly it was bound by the loop above with the normal
    // hard-fail semantics, so this leg is skipped (no double-bind).
    //
    // CITIZEN-ONLY and OUTSIDE the verbatim bind/serve loop above: the
    // standalone (`--no-default-features`) build is `#[cfg]`-stripped
    // entirely here, so its P1 source-verbatim serve path and the
    // GATE-B bare-statement loop shape are untouched (the surviving
    // `--no-default-features` invariant; dnsd_p2_invariant.mix GATE
    // A/B/C). `bind_retry::bind_pair` returns fast on EACCES/AddrInUse
    // (those are not the `AddrNotAvailable` WG-up case it retries), so
    // the unprivileged path WARNs promptly rather than spinning the
    // bounded retry window.
    #[cfg(feature = "cosmix")]
    {
        let loopback = SocketAddr::from(([127, 0, 0, 1], 53));
        if args.listen.contains(&loopback) {
            tracing::info!(
                %loopback,
                "code-default loopback was requested explicitly via --listen; already bound by the main loop"
            );
        } else {
            match bind_retry::bind_pair(&loopback).await {
                Ok((udp, tcp)) => {
                    tracing::info!(
                        %loopback,
                        "listening (UDP+TCP) — code-default loopback for the generic mesh resolved drop-in"
                    );
                    let s1 = Arc::clone(&store_dyn);
                    let s2 = Arc::clone(&store_dyn);
                    let o1: cosmix_dns::ResponseObserver = {
                        let st = Arc::clone(&stats);
                        Arc::new(move |resp| st.record(resp))
                    };
                    let o2 = Arc::clone(&o1);
                    tasks.push(tokio::spawn(async move {
                        if let Err(e) = serve::serve_udp_observed(udp, s1, o1).await {
                            tracing::error!(%e, "serve_udp exited (fatal socket error)");
                        }
                    }));
                    tasks.push(tokio::spawn(async move {
                        if let Err(e) = serve::serve_tcp_observed(tcp, s2, o2).await {
                            tracing::error!(%e, "serve_tcp exited (fatal listener error)");
                        }
                    }));
                }
                Err(e) => {
                    tracing::warn!(
                        %loopback, %e,
                        "code-default loopback bind failed (need CAP_NET_BIND_SERVICE, or :53 already in use) — \
                         mesh DNS is still served on the explicit --listen address(es); the generic resolved \
                         drop-in's 127.0.0.1 leg will NOT resolve on THIS node until cosmix-dnsd can bind it"
                    );
                }
            }
        }
    }

    // P2-C read-only Bus surface (citizen build only). Spawned after the
    // serve tasks so `dnsd.stats` reads the counter they bump;
    // non-blocking — `bus::run` owns its own retry/backoff loop, so a
    // broker that is down at startup (or drops mid-life) only delays
    // the Bus surface; DNS serving is unaffected (goal-(c)). The task
    // is aborted on Ctrl-C via the `t.abort()` sweep below. Excluded
    // under `--no-default-features` (committed P1 source-verbatim
    // standalone serve path; surviving invariant).
    #[cfg(feature = "cosmix")]
    tasks.push(tokio::spawn(bus::run(
        Arc::clone(&store_dyn),
        Arc::clone(&stats),
    )));

    // SIGHUP → reload (adopt-or-keep-last-good); Ctrl-C → graceful exit.
    let reload_store = store.clone();
    let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "cannot install SIGHUP handler");
            return ExitCode::from(1);
        }
    };

    loop {
        tokio::select! {
            _ = sighup.recv() => {
                match reload_store.reload() {
                    Ok(()) => tracing::info!("SIGHUP: zones.mix reloaded"),
                    Err(r) => tracing::error!(reject = %r, "SIGHUP: reload rejected; keeping last-known-good"),
                }
            }
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    tracing::error!(%e, "ctrl_c handler error");
                }
                tracing::info!("shutting down");
                for t in tasks {
                    t.abort();
                }
                return ExitCode::SUCCESS;
            }
        }
    }
}
