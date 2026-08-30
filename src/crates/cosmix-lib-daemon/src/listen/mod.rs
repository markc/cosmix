//! Generic per-interface listener-set manager.
//!
//! A [`ListenerSet`] binds N logical listeners — each to one or more
//! interface addresses — and runs an accept loop per bind. It owns the
//! cross-cutting concerns every cosmix daemon shares: binding specific
//! interfaces (the WG-only-by-construction / explicit-public-opt-in
//! posture), a TCP-level [`GuardPolicy`], optional TLS termination via
//! the shared [`SniCertResolver`], connection tagging, and a live
//! enable/disable kill switch ([`ListenerSetControl`]).
//!
//! Each daemon supplies a [`ConnHandler`] for what happens *after*
//! accept (hyper for webd, an SMTP/IMAP session for maild). The set
//! itself is protocol-agnostic.
//!
//! # Lifetime
//!
//! Keep the [`ListenerSet`] (or a [`ListenerSetControl`] clone) alive
//! for the process lifetime — dropping the last handle drops the
//! cancellation senders, which stops every accept loop (a clean
//! shutdown, but not what you want mid-run).
//!
//! [`SniCertResolver`]: crate::tls::sni::SniCertResolver

mod guard;
mod handler;
mod spec;
#[cfg(feature = "tls")]
mod tls;

pub use guard::{Cidr, ConnPermit, GuardPolicy, Guards, IpAcl, RateLimit};
pub use handler::{AcceptedStream, ConnCtx, ConnHandler};
pub use spec::{BindPolicy, ListenerSpec, TlsMode};
#[cfg(feature = "tls")]
pub use tls::ListenerTls;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[cfg(feature = "tls")]
use crate::tls::sni::SniCertResolver;

/// Wall-clock cap on a single TLS handshake. A client that connects
/// then stalls (never completing the handshake) would otherwise pin a
/// `ConnPermit` forever — exhausting the guard caps and making a
/// `disable` drain hang. Past this, the connection is dropped and its
/// permit released.
#[cfg(feature = "tls")]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// One configured listener plus its runtime guard state and handler.
struct Entry {
    spec: ListenerSpec,
    handler: Arc<dyn ConnHandler>,
    guards: Arc<Guards>,
    #[cfg(feature = "tls")]
    tls: Option<ListenerTls>,
}

/// The live state of a started listener: a cancellation sender, the
/// accept-loop task handles (one per bound address), and the actual
/// kernel-assigned bind addresses (port 0 resolves to a real port).
struct Running {
    cancel: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    bound: Vec<SocketAddr>,
}

struct Inner {
    entries: HashMap<String, Entry>,
    running: Mutex<HashMap<String, Running>>,
    /// Serializes enable/disable so two concurrent control calls (e.g.
    /// from Bus verbs) cannot both pass the `is_running` check and then
    /// clobber each other's `Running` handle. Async-aware: held across
    /// the bind `.await`s.
    control: tokio::sync::Mutex<()>,
}

/// A set of configured listeners. Build with [`ListenerSet::builder`],
/// start with [`start_all`](ListenerSet::start_all), drive at runtime
/// with [`control`](ListenerSet::control).
pub struct ListenerSet {
    inner: Arc<Inner>,
}

/// Builder for a [`ListenerSet`].
pub struct ListenerSetBuilder {
    entries: HashMap<String, Entry>,
}

impl ListenerSetBuilder {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Add a plain-TCP listener.
    #[cfg(not(feature = "tls"))]
    pub fn add(&mut self, spec: ListenerSpec, handler: Arc<dyn ConnHandler>) -> &mut Self {
        self.insert(Entry {
            guards: Arc::new(Guards::new(spec.guard.clone())),
            handler,
            spec,
        });
        self
    }

    /// Add a listener. Pass `tls` for `Terminate` / `StartTlsPassthrough`
    /// modes; `None` for a plain listener.
    #[cfg(feature = "tls")]
    pub fn add(
        &mut self,
        spec: ListenerSpec,
        handler: Arc<dyn ConnHandler>,
        tls: Option<ListenerTls>,
    ) -> &mut Self {
        self.insert(Entry {
            guards: Arc::new(Guards::new(spec.guard.clone())),
            handler,
            tls,
            spec,
        });
        self
    }

    fn insert(&mut self, entry: Entry) {
        let id = entry.spec.id.clone();
        if self.entries.insert(id.clone(), entry).is_some() {
            tracing::warn!(listener = %id, "duplicate listener id — previous definition overwritten");
        }
    }

    /// Finalise the set.
    pub fn build(self) -> ListenerSet {
        ListenerSet {
            inner: Arc::new(Inner {
                entries: self.entries,
                running: Mutex::new(HashMap::new()),
                control: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

impl ListenerSet {
    /// Start a new builder.
    pub fn builder() -> ListenerSetBuilder {
        ListenerSetBuilder::new()
    }

    /// A cheap, cloneable control handle (drive enable/disable/status
    /// from an Bus verb or the daemon's runtime loop).
    pub fn control(&self) -> ListenerSetControl {
        ListenerSetControl {
            inner: self.inner.clone(),
        }
    }

    /// Bind and spawn every listener whose spec is `enabled`. Logs and
    /// continues past a listener that fails to bind; returns `Err` only
    /// if at least one was configured and **none** came up.
    pub async fn start_all(&self) -> anyhow::Result<()> {
        let ctl = self.control();
        let mut ids: Vec<String> = self
            .inner
            .entries
            .iter()
            .filter(|(_, e)| e.spec.enabled)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();

        let want = ids.len();
        let mut up = 0usize;
        for id in &ids {
            match ctl.enable(id).await {
                Ok(()) => up += 1,
                Err(e) => {
                    tracing::error!(listener = %id, error = format!("{e:#}"), "failed to start listener")
                }
            }
        }
        if want > 0 && up == 0 {
            anyhow::bail!("no listeners could be started ({want} enabled, all failed to bind)");
        }
        Ok(())
    }
}

/// A live status snapshot for one listener.
#[derive(Debug, Clone)]
pub struct ListenerStatus {
    pub id: String,
    pub running: bool,
    pub binds: Vec<SocketAddr>,
    pub external: bool,
    pub active_conns: u32,
}

/// Cheap, cloneable control surface for a [`ListenerSet`]. Shares the
/// set's state, so a clone driven from an Bus verb toggles the same
/// listeners the daemon's main task started.
#[derive(Clone)]
pub struct ListenerSetControl {
    inner: Arc<Inner>,
}

impl ListenerSetControl {
    /// Bind the listener's addresses and spawn its accept loops.
    /// No-op if already running. Fail-closed for a `Terminate`
    /// listener whose TLS resolver is empty.
    pub async fn enable(&self, id: &str) -> anyhow::Result<()> {
        // Serialize all enable/disable so a concurrent caller cannot
        // pass the is_running check and clobber our Running handle.
        let _ctl = self.inner.control.lock().await;

        let entry = self
            .inner
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown listener {id:?}"))?;

        if self.is_running(id) {
            return Ok(());
        }

        // A Terminate listener with no usable resolver would accept TCP
        // then fail every handshake — refuse to bind instead.
        #[cfg(feature = "tls")]
        if entry.spec.tls_mode == TlsMode::Terminate {
            let usable = entry.tls.as_ref().is_some_and(|t| t.is_enabled());
            if !usable {
                anyhow::bail!("listener {id:?}: Terminate mode requires a non-empty TLS resolver");
            }
        }

        // Bind every address up front, then apply the bind policy
        // before spawning anything.
        let mut bound: Vec<(SocketAddr, TcpListener)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for addr in &entry.spec.binds {
            match TcpListener::bind(addr).await {
                Ok(l) => bound.push((*addr, l)),
                Err(e) => errors.push(format!("{addr}: {e}")),
            }
        }
        if !errors.is_empty() {
            match entry.spec.bind_policy {
                BindPolicy::FailAll => {
                    // `bound` drops here, releasing any partial binds.
                    anyhow::bail!("listener {id:?} bind failed: {}", errors.join(", "));
                }
                BindPolicy::BestEffort => {
                    for e in &errors {
                        tracing::warn!(listener = %id, "bind failed (BestEffort): {e}");
                    }
                }
            }
        }
        if bound.is_empty() {
            anyhow::bail!("listener {id:?}: no addresses bound");
        }

        let args = Arc::new(AcceptArgs {
            listener_id: Arc::from(id),
            external: entry.spec.external,
            tls_mode: entry.spec.tls_mode,
            guards: entry.guards.clone(),
            handler: entry.handler.clone(),
            #[cfg(feature = "tls")]
            tls: entry.tls.clone(),
        });

        let (cancel, cancel_rx) = watch::channel(false);
        let mut tasks = Vec::with_capacity(bound.len());
        let mut bound_addrs = Vec::with_capacity(bound.len());
        for (addr, listener) in bound {
            // Resolve the real bound address (port 0 → an OS-assigned
            // port); reported by `status` as the daemon-owned truth.
            bound_addrs.push(listener.local_addr().unwrap_or(addr));
            tasks.push(tokio::spawn(run_accept(
                listener,
                addr,
                args.clone(),
                cancel_rx.clone(),
            )));
        }
        self.inner
            .running
            .lock()
            .expect("listener running map poisoned")
            .insert(
                id.to_string(),
                Running {
                    cancel,
                    tasks,
                    bound: bound_addrs,
                },
            );
        Ok(())
    }

    /// Stop accepting on the listener and drop its sockets (the kernel
    /// closes the ports). With `drain`, wait up to that long for
    /// in-flight connections to finish; past the deadline they are
    /// left to complete in the background (no forced abort). No-op if
    /// not running.
    pub async fn disable(&self, id: &str, drain: Option<Duration>) -> anyhow::Result<()> {
        // Same control lock as enable — a disable racing an enable of
        // the same id must not interleave bind/insert/remove.
        let _ctl = self.inner.control.lock().await;

        let running = self
            .inner
            .running
            .lock()
            .expect("listener running map poisoned")
            .remove(id);
        let Some(running) = running else {
            return Ok(());
        };
        // Signal, then await the accept loops so their TcpListeners are
        // dropped (ports freed) before we report done.
        let _ = running.cancel.send(true);
        for task in running.tasks {
            let _ = task.await;
        }
        if let Some(drain) = drain
            && let Some(entry) = self.inner.entries.get(id)
        {
            let deadline = Instant::now() + drain;
            while entry.guards.active() > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }

    /// Hot-swap a listener's TLS resolver (e.g. on ACME renewal). The
    /// new certs apply to subsequent handshakes; in-flight ones are
    /// unaffected.
    #[cfg(feature = "tls")]
    pub fn swap_tls(&self, id: &str, resolver: Option<Arc<SniCertResolver>>) -> anyhow::Result<()> {
        let entry = self
            .inner
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown listener {id:?}"))?;
        match entry.tls.as_ref() {
            Some(t) => {
                t.swap(resolver);
                Ok(())
            }
            None => anyhow::bail!("listener {id:?} has no TLS slot"),
        }
    }

    /// Hot-swap a listener's [`GuardPolicy`] (rate limit, conn caps,
    /// ACL). Takes effect on the next accepted connection; in-flight
    /// connections and the live counters are unaffected. Works whether
    /// or not the listener is currently running — the policy lives in
    /// the shared `Arc<Guards>` the accept loop reads, so a swap on a
    /// stopped listener applies when it is next enabled.
    pub fn swap_guard(&self, id: &str, policy: GuardPolicy) -> anyhow::Result<()> {
        let entry = self
            .inner
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown listener {id:?}"))?;
        entry.guards.swap_guard(policy);
        Ok(())
    }

    /// Whether a listener is currently bound and accepting.
    pub fn is_running(&self, id: &str) -> bool {
        self.inner
            .running
            .lock()
            .expect("listener running map poisoned")
            .contains_key(id)
    }

    /// A status snapshot of every configured listener, id-sorted.
    pub fn status(&self) -> Vec<ListenerStatus> {
        let running = self
            .inner
            .running
            .lock()
            .expect("listener running map poisoned");
        let mut out: Vec<ListenerStatus> = self
            .inner
            .entries
            .iter()
            .map(|(id, e)| {
                // When running, report the real kernel-assigned addrs;
                // otherwise the configured binds.
                let binds = match running.get(id) {
                    Some(r) => r.bound.clone(),
                    None => e.spec.binds.clone(),
                };
                ListenerStatus {
                    id: id.clone(),
                    running: running.contains_key(id),
                    binds,
                    external: e.spec.external,
                    active_conns: e.guards.active(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// Shared, immutable per-listener context handed to each accept loop
/// and each spawned connection.
struct AcceptArgs {
    listener_id: Arc<str>,
    external: bool,
    tls_mode: TlsMode,
    guards: Arc<Guards>,
    handler: Arc<dyn ConnHandler>,
    #[cfg(feature = "tls")]
    tls: Option<ListenerTls>,
}

/// Accept loop for one bound address. Runs until the cancellation
/// signal flips (or its sender drops), then returns — dropping the
/// `listener` and freeing the port.
async fn run_accept(
    listener: TcpListener,
    bind_addr: SocketAddr,
    args: Arc<AcceptArgs>,
    mut cancel: watch::Receiver<bool>,
) {
    tracing::info!(
        listener = %args.listener_id,
        addr = %bind_addr,
        external = args.external,
        "listener bound"
    );
    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
            }
            res = listener.accept() => {
                match res {
                    Ok((stream, peer)) => {
                        let Some(permit) = args.guards.admit(peer.ip()) else {
                            // Guard rejected — drop the stream now.
                            continue;
                        };
                        tokio::spawn(handle_conn(stream, peer, bind_addr, args.clone(), permit));
                    }
                    Err(e) => {
                        tracing::warn!(
                            listener = %args.listener_id,
                            addr = %bind_addr,
                            error = %e,
                            "accept error"
                        );
                    }
                }
            }
        }
    }
    tracing::info!(listener = %args.listener_id, addr = %bind_addr, "listener stopped");
}

/// Per-connection: optionally terminate TLS, then hand off to the
/// daemon's handler. The `ConnPermit` is held for the connection's
/// lifetime so the guard counters reflect it.
async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    bind_addr: SocketAddr,
    args: Arc<AcceptArgs>,
    permit: ConnPermit,
) {
    let _permit = permit;
    let local = stream.local_addr().unwrap_or(bind_addr);

    let (accepted, sni) = match args.tls_mode {
        TlsMode::Plain => (AcceptedStream::Tcp(stream), None),
        #[cfg(feature = "tls")]
        TlsMode::Terminate => {
            let Some(tls) = args.tls.as_ref() else {
                tracing::error!(listener = %args.listener_id, "Terminate listener missing TLS handle");
                return;
            };
            let Some((_resolver, cfg)) = tls.current() else {
                tracing::warn!(listener = %args.listener_id, "Terminate listener TLS disabled mid-run");
                return;
            };
            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(s)) => {
                    let sni = s.get_ref().1.server_name().map(|n| n.to_string());
                    (AcceptedStream::Tls(Box::new(s)), sni)
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        listener = %args.listener_id,
                        peer = %peer,
                        error = %e,
                        "TLS handshake failed"
                    );
                    return;
                }
                Err(_elapsed) => {
                    tracing::debug!(
                        listener = %args.listener_id,
                        peer = %peer,
                        timeout_secs = TLS_HANDSHAKE_TIMEOUT.as_secs(),
                        "TLS handshake timed out"
                    );
                    return;
                }
            }
        }
        #[cfg(feature = "tls")]
        TlsMode::StartTlsPassthrough => (AcceptedStream::Tcp(stream), None),
    };

    let ctx = ConnCtx {
        listener_id: args.listener_id.clone(),
        local_addr: local,
        peer_addr: peer,
        external: args.external,
        sni,
        #[cfg(feature = "tls")]
        tls: args.tls.clone(),
    };
    args.handler.handle(accepted, ctx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A handler that counts connections and echoes nothing.
    struct CountingHandler {
        seen: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl ConnHandler for CountingHandler {
        async fn handle(&self, _stream: AcceptedStream, _ctx: ConnCtx) {
            self.seen.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn plain_spec(id: &str) -> ListenerSpec {
        // Port 0 = let the OS pick a free port.
        ListenerSpec::new(id, vec!["127.0.0.1:0".parse().unwrap()])
    }

    #[cfg(not(feature = "tls"))]
    fn build_one(id: &str, seen: Arc<AtomicU32>) -> ListenerSet {
        let mut b = ListenerSet::builder();
        b.add(plain_spec(id), Arc::new(CountingHandler { seen }));
        b.build()
    }

    #[cfg(feature = "tls")]
    fn build_one(id: &str, seen: Arc<AtomicU32>) -> ListenerSet {
        let mut b = ListenerSet::builder();
        b.add(plain_spec(id), Arc::new(CountingHandler { seen }), None);
        b.build()
    }

    #[tokio::test]
    async fn enable_then_disable_toggles_running() {
        let seen = Arc::new(AtomicU32::new(0));
        let set = build_one("wg", seen);
        let ctl = set.control();
        assert!(!ctl.is_running("wg"));
        ctl.enable("wg").await.unwrap();
        assert!(ctl.is_running("wg"));
        // idempotent
        ctl.enable("wg").await.unwrap();
        assert!(ctl.is_running("wg"));
        ctl.disable("wg", None).await.unwrap();
        assert!(!ctl.is_running("wg"));
        // disable of a stopped listener is a no-op
        ctl.disable("wg", None).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_enable_is_serialized() {
        let seen = Arc::new(AtomicU32::new(0));
        let set = build_one("wg", seen);
        let ctl = set.control();
        let (c1, c2) = (ctl.clone(), ctl.clone());
        // Two enables racing the same listener must both succeed and
        // leave exactly one running listener (no clobbered handle).
        let (r1, r2) = tokio::join!(c1.enable("wg"), c2.enable("wg"));
        assert!(r1.is_ok() && r2.is_ok());
        assert!(ctl.is_running("wg"));
        ctl.disable("wg", None).await.unwrap();
        assert!(!ctl.is_running("wg"));
    }

    #[tokio::test]
    async fn enable_unknown_listener_errors() {
        let seen = Arc::new(AtomicU32::new(0));
        let set = build_one("wg", seen);
        assert!(set.control().enable("nope").await.is_err());
    }

    #[tokio::test]
    async fn status_reports_running_state() {
        let seen = Arc::new(AtomicU32::new(0));
        let set = build_one("wg", seen);
        let ctl = set.control();
        let before = ctl.status();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].id, "wg");
        assert!(!before[0].running);
        ctl.enable("wg").await.unwrap();
        assert!(ctl.status()[0].running);
    }

    #[tokio::test]
    async fn accepts_a_real_connection_and_drains_on_disable() {
        let seen = Arc::new(AtomicU32::new(0));
        let set = build_one("wg", seen.clone());
        let ctl = set.control();
        ctl.enable("wg").await.unwrap();

        // status reports the real OS-assigned port (spec used :0).
        let addr = ctl.status()[0].binds[0];
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();

        for _ in 0..100 {
            if seen.load(Ordering::Acquire) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            seen.load(Ordering::Acquire) >= 1,
            "handler should have seen the connection"
        );

        ctl.disable("wg", Some(Duration::from_millis(200)))
            .await
            .unwrap();
        assert!(!ctl.is_running("wg"));

        // Port is freed: a fresh bind on the same addr now succeeds.
        assert!(
            tokio::net::TcpListener::bind(addr).await.is_ok(),
            "disabled listener must release its port"
        );
    }
}
