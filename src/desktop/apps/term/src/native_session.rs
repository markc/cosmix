//! Term-owned identity actor. No diagnostic verbs are installed on this lane.
use crate::session_fd::{LaunchFd, fresh_key};
use cosmix_bus::native_session::*;
use cosmix_client::session::{ExpectedScope, GrantResult, SessionFailure};
use cosmix_client::{
    BrokerAccount, NodedClient, UnixConnectOptions, UnixConnectOutcome, VerifiedConnection,
};
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const RPC_BUDGET: Duration = Duration::from_secs(2);
const RENEW: Duration = Duration::from_secs(5);
const GRANT_LIFETIME: Duration = Duration::from_secs(30);
const RETRY_FLOOR: Duration = Duration::from_secs(120);
const RETRY_CAP: u32 = 3;
const STOP_BUDGET: Duration = Duration::from_secs(8);

fn clock_ms() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec. Unlike Instant, BOOTTIME includes suspend.
    (unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0).then(|| {
        (time.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add(time.tv_nsec as u64 / 1_000_000)
    })
}

async fn close_connection(connection: &VerifiedConnection) {
    let _ = tokio::time::timeout(RPC_BUDGET, connection.client().close()).await;
}

fn capabilities() -> Vec<Capability> {
    vec![
        Capability::ReadState,
        Capability::ReadContents,
        Capability::Input,
        Capability::Execute,
        Capability::ManageLayout,
        Capability::Terminate,
    ]
}

#[derive(Clone)]
pub struct NativeSession(UnboundedSender<Request>, Arc<std::sync::Mutex<Shared>>);

struct Shared {
    next_id: u64,
    ready: Option<Ready>,
    panes: HashMap<u64, std::sync::Weak<PaneState>>,
    status: HashMap<u64, String>,
    diagnostic: String,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            next_id: 1,
            ready: None,
            panes: HashMap::new(),
            status: HashMap::new(),
            diagnostic: "native session starting; no ready launch grant".into(),
        }
    }
}

struct Ready {
    pane: Arc<PaneState>,
    fd: LaunchFd,
    // Only an unconsumed bundle retains its seed for descriptor replacement.
    // prepare() drops this key before handing the memfd to the one spawn.
    key: SigningKey,
    deadline: u64,
}

pub struct Supervisor {
    pub handle: NativeSession,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: mpsc::Receiver<()>,
}

struct PaneState {
    id: u64,
    generation: AtomicU64,
    live: AtomicBool,
    launched: AtomicBool,
    public_key: HexBytes<32>,
}

pub struct PaneSession {
    state: Arc<PaneState>,
    handle: NativeSession,
}

impl PaneSession {
    /// Called under the pane model's mutation lock, before cleanup is queued.
    pub fn revoke(&self) {
        self.state.live.store(false, Ordering::Release);
        self.handle.revoke_pane(self.state.id);
    }

    pub fn revoke_before_cleanup(&self) {
        self.state.live.store(false, Ordering::Release);
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .handle
            .0
            .send(Request::Close(self.state.id, Some(tx)))
            .is_ok()
        {
            // Cleanup worker only. Three RPCs take up to six seconds, plus
            // queued work: this is a best-effort three-second wait, NOT an
            // acknowledgement guarantee. Local invalidation already happened.
            if rx.recv_timeout(Duration::from_secs(3)).is_err() {
                eprintln!(
                    "term pane {}: revoke still pending at cleanup deadline",
                    self.state.id
                );
            }
        }
    }

    pub fn exit_notifier(&self) -> impl Fn() + Send + Sync + 'static {
        let state = self.state.clone();
        let handle = self.handle.clone();
        move || {
            state.live.store(false, Ordering::Release);
            handle.revoke_pane(state.id);
        }
    }
}

impl Drop for PaneSession {
    fn drop(&mut self) {
        self.revoke();
    }
}

enum Request {
    Provision,
    Close(u64, Option<mpsc::SyncSender<()>>),
}

impl NativeSession {
    /// Invalidates locally under a short lock, then queues remote revocation.
    /// Remote completion is asynchronous; this does not acquire the PTY mutex.
    pub fn revoke_pane(&self, id: u64) {
        let mut shared = self.1.lock().unwrap();
        if let Some(state) = shared.panes.remove(&id).and_then(|p| p.upgrade()) {
            state.live.store(false, Ordering::Release);
        }
        shared.status.remove(&id);
        drop(shared);
        let _ = self.0.send(Request::Close(id, None));
    }

    /// Consume a pre-provisioned bundle. No random generation, memfd I/O, RPC
    /// or channel wait runs on the GUI / synchronous diagnostic Bus thread.
    pub fn prepare(&self, id: u64) -> Option<(PaneSession, LaunchFd)> {
        let mut shared = self.1.lock().unwrap();
        shared.next_id = id.saturating_add(1);
        let ready = shared.ready.take();
        let ready = ready.filter(|ready| {
            let usable = ready.pane.id == id
                && ready.pane.live.load(Ordering::Acquire)
                && clock_ms().is_some_and(|now| now < ready.deadline);
            if !usable {
                ready.pane.live.store(false, Ordering::Release);
            }
            usable
        });
        let result = if let Some(ready) = ready {
            ready.pane.launched.store(true, Ordering::Release);
            shared
                .status
                .insert(id, "grant delivered; awaiting child enrolment".into());
            shared.diagnostic = "preparing next pane launch grant".into();
            drop(ready.key); // zeroize-on-drop; no actor copy survives handoff
            Some((
                PaneSession {
                    state: ready.pane,
                    handle: self.clone(),
                },
                ready.fd,
            ))
        } else {
            let reason = format!(
                "unbound (graphics-only): no usable ready grant; {}",
                shared.diagnostic
            );
            eprintln!("term pane {id}: {reason}");
            shared.status.insert(id, reason);
            None
        };
        drop(shared);
        let _ = self.0.send(Request::Provision);
        result
    }

    pub fn status(&self) -> serde_json::Value {
        let shared = self.1.lock().unwrap();
        serde_json::json!({"diagnostic": shared.diagnostic, "panes": shared.status})
    }
}

impl Supervisor {
    pub fn start() -> Result<Self, String> {
        // Account lookup is trusted system configuration, not the current UID
        // or the ownership of an attacker-selected socket. No numeric default.
        let account_name =
            std::env::var("COSMIX_BROKER_ACCOUNT").unwrap_or_else(|_| "cosmix-noded".into());
        let name = std::ffi::CString::new(account_name).map_err(|_| "invalid broker account")?;
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 65536];
        // SAFETY: getpwnam_r writes only the supplied storage; copied UID/GID
        // outlive the scratch buffer, and no libc static storage is retained.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc != 0 || result.is_null() {
            return Err("configured broker account unavailable".into());
        }
        let entry = unsafe { entry.assume_init() };
        let mut options = cosmix_config::client_helpers::unix_connect_options(BrokerAccount {
            uid: entry.pw_uid,
            gid: entry.pw_gid,
        })
        .map_err(|e| e.to_string())?;
        options.require_native_session = true;
        Self::with_options(options, cosmix_config::client_helpers::resolve_noded_url())
    }

    pub fn with_options(options: UnixConnectOptions, url: String) -> Result<Self, String> {
        let key = fresh_key().map_err(|e| e.to_string())?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(std::sync::Mutex::new(Shared::default()));
        let actor_shared = shared.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (done_tx, done) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("term-native-session".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("session runtime");
                runtime.block_on(async move {
                    let mut actor = Actor {
                        options,
                        url,
                        key,
                        connection: None,
                        parent: None,
                        children: HashMap::new(),
                        shared: actor_shared,
                        provisioned: None,
                        #[cfg(test)]
                        grant_creates: 0,
                    };
                    // Cancellation interrupts even a long reconciliation batch.
                    tokio::select! {
                        _ = stopped => {},
                        _ = async { actor.connect().await; actor.run(rx).await; } => {},
                    }
                    actor.shutdown().await;
                });
                runtime.shutdown_timeout(Duration::from_millis(100));
                let _ = done_tx.send(());
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            handle: NativeSession(tx, shared),
            worker: Some(worker),
            stop: Some(stop),
            done,
        })
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            if self.done.recv_timeout(STOP_BUDGET).is_ok() {
                if worker.is_finished() {
                    let _ = worker.join();
                }
            } else {
                // Dropping a JoinHandle detaches it. A stuck OS/runtime thread
                // cannot hold Term exit beyond this deadline.
                eprintln!("term native-session shutdown deadline exceeded");
            }
        }
    }
}

struct Child {
    pane: Arc<PaneState>,
    record: Option<SessionRecord>,
    retry: Retry,
    pending: Option<PendingGrant>,
}

#[derive(Default)]
struct Retry {
    enrolled: bool,
    external: bool,
    attempts: u32,
    due: Option<Instant>,
}
impl Retry {
    fn rearm(&mut self) {
        self.external = true;
        self.attempts = 0;
        self.due = None;
    }
    fn expired(&mut self, now: Instant) {
        if self.enrolled && self.attempts < RETRY_CAP {
            self.due
                .get_or_insert(now + RETRY_FLOOR * (1 << self.attempts));
        }
    }
    fn take(&mut self, now: Instant) -> bool {
        if self.external {
            self.external = false;
            self.due = None;
            return true;
        }
        if self.enrolled && self.attempts < RETRY_CAP && self.due.is_some_and(|due| now >= due) {
            self.attempts += 1;
            self.due = None;
            return true;
        }
        false
    }
}

struct PendingGrant {
    deadline: u64,
    expires_ms: Option<DecimalU64>,
}
impl PendingGrant {
    fn usable(&self, grant: &SessionGrant, now: u64) -> bool {
        // Broker timestamps are opaque across time namespaces (BUS-016).
        // Check the grant's own state and exact expiry value, with a conservative
        // local 30s deadline starting BEFORE create, including lost-ACK recovery.
        grant.state == GrantState::Pending
            && grant.expires_ms.0 != 0
            && self
                .expires_ms
                .is_none_or(|expires| expires == grant.expires_ms)
            && now < self.deadline
    }
}

struct Actor {
    #[cfg(test)]
    grant_creates: usize,
    options: UnixConnectOptions,
    url: String,
    key: SigningKey,
    connection: Option<VerifiedConnection>,
    parent: Option<SessionRecord>,
    children: HashMap<u64, Child>,
    shared: Arc<std::sync::Mutex<Shared>>,
    provisioned: Option<u64>,
}

type ResultSession<T> = Result<T, SessionFailure>;
async fn bounded<T>(
    future: impl std::future::Future<Output = ResultSession<T>>,
) -> ResultSession<T> {
    tokio::time::timeout(RPC_BUDGET, future)
        .await
        .unwrap_or(Err(SessionFailure::InvalidResponse))
}
fn forbidden(error: &SessionFailure) -> bool {
    matches!(error, SessionFailure::Refused { error, .. } if error.error_code == ErrorCode::Forbidden)
}

impl Actor {
    fn diagnostic(&self, id: Option<u64>, message: impl Into<String>) {
        let message = message.into();
        eprintln!("term native session: {message}");
        let mut shared = self.shared.lock().unwrap();
        if let Some(id) = id {
            if let Some(status) = shared.status.get_mut(&id) {
                *status = message;
            } else if shared
                .ready
                .as_ref()
                .is_some_and(|ready| ready.pane.id == id)
            {
                shared.diagnostic = message;
            }
        } else {
            shared.diagnostic = message;
        }
    }

    async fn provision(&mut self) {
        let id = self.shared.lock().unwrap().next_id;
        if self.connection.is_none() || self.provisioned == Some(id) {
            return;
        }
        self.provisioned = Some(id);
        let stale = {
            let mut shared = self.shared.lock().unwrap();
            if shared
                .ready
                .as_ref()
                .is_some_and(|ready| ready.pane.id != id)
            {
                shared.ready.take()
            } else {
                None
            }
        };
        if let Some(stale) = stale {
            let old_id = stale.pane.id;
            stale.pane.live.store(false, Ordering::Release);
            drop(stale);
            self.close_child(old_id).await;
        }
        // Failed closes retain public-key tombstones for retry. Bound that
        // accumulation: new panes remain usable, unbound, until recovery.
        if self.children.len() >= 128 {
            self.diagnostic(None, "revocation backlog full; opening panes unbound");
            return;
        }
        // MAX_TABS is 32, coincidentally the broker's default per-parent
        // pending quota. One look-ahead grant also spends quota; an operator
        // may lower it. RESOURCE_LIMIT must degrade to an unbound pane.
        let key = match fresh_key() {
            Ok(key) => key,
            Err(error) => {
                self.diagnostic(None, format!("launch key unavailable: {error}"));
                return;
            }
        };
        let pane = Arc::new(PaneState {
            id,
            generation: AtomicU64::new(1),
            live: AtomicBool::new(true),
            launched: AtomicBool::new(false),
            public_key: HexBytes(key.verifying_key().to_bytes()),
        });
        let mut retry = Retry::default();
        retry.rearm();
        self.children.insert(
            id,
            Child {
                pane: pane.clone(),
                record: None,
                retry,
                pending: None,
            },
        );
        self.shared
            .lock()
            .unwrap()
            .panes
            .insert(id, Arc::downgrade(&pane));
        let result = self.grant(id).await;
        match result {
            Ok(grant) => match LaunchFd::new(&grant, &key) {
                Ok(fd) => {
                    let deadline = self.children[&id]
                        .pending
                        .as_ref()
                        .expect("created grant")
                        .deadline;
                    let mut shared = self.shared.lock().unwrap();
                    if shared.next_id == id && pane.live.load(Ordering::Acquire) {
                        shared.ready = Some(Ready {
                            pane,
                            fd,
                            key,
                            deadline,
                        });
                        shared.diagnostic = "next pane launch grant ready".into();
                        return;
                    }
                }
                Err(error) => self.diagnostic(None, format!("launch memfd unavailable: {error}")),
            },
            Err(SessionFailure::Refused { error, .. })
                if error.error_code == ErrorCode::ResourceLimit =>
            {
                let kind = if error.details.get("reason").and_then(|v| v.as_str())
                    == Some("grant_limit")
                {
                    "pending-grant quota"
                } else {
                    "resource quota"
                };
                self.diagnostic(
                    None,
                    format!("broker {kind} exhausted; opening pane unbound (graphics-only)"),
                );
            }
            Err(error) => self.diagnostic(None, format!("launch grant unavailable: {error}")),
        }
        // The consumer may already have opened this ID unbound. Never publish
        // a late bundle or retain its private key for that running child.
        self.close_child(id).await;
    }

    fn refresh_ready(&mut self, id: u64, grant: &GrantResult) {
        if self.children[&id].pane.launched.load(Ordering::Acquire) {
            return;
        }
        let ready = {
            let mut shared = self.shared.lock().unwrap();
            if shared.ready.as_ref().is_none_or(|r| r.pane.id != id) {
                return;
            }
            shared.ready.take().unwrap()
        };
        let Some(pending) = self.children[&id].pending.as_ref() else {
            ready.pane.live.store(false, Ordering::Release);
            return;
        };
        if !clock_ms().is_some_and(|now| pending.usable(&grant.grant, now)) {
            ready.pane.live.store(false, Ordering::Release);
            return;
        }
        match LaunchFd::new(grant, &ready.key) {
            Ok(fd) => {
                let mut shared = self.shared.lock().unwrap();
                if shared.next_id == id && ready.pane.live.load(Ordering::Acquire) {
                    shared.ready = Some(Ready {
                        fd,
                        deadline: pending.deadline,
                        ..ready
                    });
                }
            }
            Err(error) => {
                ready.pane.live.store(false, Ordering::Release);
                self.diagnostic(Some(id), format!("launch memfd refresh failed: {error}"));
            }
        }
    }

    async fn connect(&mut self) {
        if let Some(old) = self.connection.take() {
            close_connection(&old).await;
        }
        let connection = tokio::time::timeout(
            RPC_BUDGET,
            NodedClient::connect_unix("", &self.url, &self.options, None),
        )
        .await;
        let Ok(Ok(UnixConnectOutcome::VerifiedUnix(connection))) = connection else {
            self.diagnostic(
                None,
                "broker/profile unavailable; opening panes unbound (graphics-only)",
            );
            return;
        };
        let public_key = HexBytes(self.key.verifying_key().to_bytes());
        let hello = match bounded(connection.session_hello()).await {
            Ok(hello) => hello,
            Err(_) => {
                close_connection(&connection).await;
                return;
            }
        };
        // Always reconcile by retained key first, including uncertain allocate
        // ACKs. Only an authenticated, definitive absence permits allocation.
        let result = match bounded(connection.session_challenge_key(public_key)).await {
            Ok(challenge) => {
                if challenge.wake_error.is_some() {
                    eprintln!("term session wake registration unavailable");
                }
                let expected = ExpectedScope {
                    broker_epoch: hello.broker_epoch,
                    purpose: Purpose::Resume,
                    unix_uid: unsafe { libc::geteuid() },
                    parent_key_hash: None,
                    pane_id: None,
                    pane_high_water: None,
                    role: Role::Term,
                    public_key_hash: HexBytes(Sha256::digest(public_key.0).into()),
                    capabilities_hash: HexBytes(
                        Sha256::digest(
                            encode_capabilities(&capabilities()).expect("fixed capabilities"),
                        )
                        .into(),
                    ),
                };
                match challenge.sign(&self.key, &expected) {
                    Ok(proof) => bounded(connection.session_prove(&proof)).await,
                    Err(error) => Err(error),
                }
            }
            Err(error) if forbidden(&error) => {
                bounded(connection.session_allocate(&self.key, Policy::DefaultOpen)).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => {
                let replacement = self.parent.as_ref().is_none_or(|old| {
                    old.record_id != result.record.record_id
                        || old.broker_epoch != result.record.broker_epoch
                });
                self.parent = Some(result.record);
                self.connection = Some(connection);
                if replacement {
                    self.children
                        .retain(|_, child| child.pane.live.load(Ordering::Acquire));
                    for child in self.children.values_mut() {
                        child.record = None;
                        child.pending = None;
                        child.pane.generation.store(1, Ordering::Release);
                    }
                }
                self.reconcile().await;
            }
            Err(error) => {
                eprintln!("term identity recovery: {error}");
                close_connection(&connection).await;
            }
        }
    }

    async fn grant(&mut self, id: u64) -> ResultSession<GrantResult> {
        self.grant_at(id, Instant::now(), false).await
    }

    async fn grant_at(
        &mut self,
        id: u64,
        retry_now: Instant,
        prepaid: bool,
    ) -> ResultSession<GrantResult> {
        let connection = self
            .connection
            .as_ref()
            .ok_or(SessionFailure::InvalidResponse)?;
        let child = self
            .children
            .get_mut(&id)
            .ok_or(SessionFailure::InvalidResponse)?;
        if !child.pane.live.load(Ordering::Acquire) {
            return Err(SessionFailure::ScopeMismatch);
        }
        // Fetch precedes every (re-)mint: a lost create ACK must never lead to
        // a second mutation before the original outcome has been reconciled.
        match bounded(connection.session_grant_fetch(child.pane.public_key)).await {
            Ok(found)
                if found.grant.state == GrantState::Consumed
                    && matches!(
                        found.record.state,
                        BindingState::Attached | BindingState::Suspended
                    )
                    || found.record.state == BindingState::Pending
                        && child.pending.as_ref().is_some_and(|p| {
                            clock_ms().is_some_and(|now| p.usable(&found.grant, now))
                        }) =>
            {
                child.retry.enrolled |= found.grant.state == GrantState::Consumed;
                child.retry.due = None;
                if let Some(pending) = &mut child.pending {
                    pending.expires_ms = Some(found.grant.expires_ms);
                }
                child.pane.generation.store(
                    found
                        .record
                        .pane_generation
                        .ok_or(SessionFailure::InvalidResponse)?
                        .0,
                    Ordering::Release,
                );
                child.record = Some(found.record.clone());
                return Ok(found);
            }
            Ok(found) => {
                child.retry.enrolled |= found.grant.state == GrantState::Consumed;
                // A locally exhausted conservative window must not cause a
                // duplicate mint while the broker still owns a pending grant.
                if found.grant.state == GrantState::Pending {
                    return Err(SessionFailure::LeaseExpired);
                }
                let generation = found
                    .record
                    .pane_generation
                    .ok_or(SessionFailure::InvalidResponse)?
                    .0
                    .checked_add(1)
                    .ok_or(SessionFailure::InvalidResponse)?;
                child
                    .pane
                    .generation
                    .fetch_max(generation, Ordering::AcqRel);
            }
            Err(error) if forbidden(&error) => {
                if let Some(record) = &child.record {
                    let next = record
                        .pane_generation
                        .ok_or(SessionFailure::InvalidResponse)?
                        .0
                        .checked_add(1)
                        .ok_or(SessionFailure::InvalidResponse)?;
                    child.pane.generation.fetch_max(next, Ordering::AcqRel);
                }
            }
            Err(error) => return Err(error),
        }
        if !child.pane.live.load(Ordering::Acquire) {
            return Err(SessionFailure::ScopeMismatch);
        }
        // Charge BEFORE sending: uncertain ACKs and quota failures spend an
        // attempt too. A fetch of an existing grant never spends a name.
        if !prepaid && !child.retry.take(retry_now) {
            return Err(SessionFailure::ScopeMismatch);
        }
        let args = GrantCreateArgs {
            parent: self
                .parent
                .as_ref()
                .ok_or(SessionFailure::InvalidResponse)?
                .reference(),
            pane_id: DecimalU64(id),
            pane_generation: DecimalU64(child.pane.generation.load(Ordering::Acquire)),
            public_key: child.pane.public_key,
            role: Role::PaneShell,
            capabilities: capabilities(),
        };
        child.pending = Some(PendingGrant {
            deadline: clock_ms()
                .ok_or(SessionFailure::InvalidResponse)?
                .saturating_add(GRANT_LIFETIME.as_millis() as u64),
            expires_ms: None,
        });
        #[cfg(test)]
        {
            self.grant_creates += 1;
        }
        let result = bounded(connection.session_grant_create(&args)).await?;
        if result.record.state != BindingState::Pending
            || !clock_ms()
                .is_some_and(|now| child.pending.as_ref().unwrap().usable(&result.grant, now))
        {
            return Err(SessionFailure::InvalidResponse);
        }
        child.pending.as_mut().unwrap().expires_ms = Some(result.grant.expires_ms);
        child.record = Some(result.record.clone());
        Ok(result)
    }

    async fn reconcile(&mut self) {
        let next = self.shared.lock().unwrap().next_id;
        if !self.children.contains_key(&next) {
            self.provisioned = None;
        }
        let ids: Vec<_> = self.children.keys().copied().collect();
        for id in ids {
            if !self.children[&id].pane.live.load(Ordering::Acquire) {
                self.close_child(id).await;
            } else {
                // Exactly one mint opportunity per successful reconnect/gap.
                self.children.get_mut(&id).unwrap().retry.rearm();
                match self.grant(id).await {
                    Ok(grant) => self.refresh_ready(id, &grant),
                    Err(error) => {
                        self.diagnostic(Some(id), format!("child grant reconciliation: {error}"))
                    }
                }
            }
            // A slow reconciliation batch cannot starve the parent's lease.
            if let (Some(connection), Some(parent)) = (&self.connection, &self.parent)
                && let Ok(result) = bounded(connection.session_renew(parent.reference())).await
            {
                self.parent = Some(result.record);
            }
        }
    }

    async fn close_child(&mut self, id: u64) {
        let Some(child) = self.children.get_mut(&id) else {
            return;
        };
        child.pane.live.store(false, Ordering::Release);
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        // A forbidden fetch can also mean a suspended parent. Confirm this
        // attachment is live before treating absence as successful cleanup.
        // This also keeps large close batches from starving the parent lease.
        let Some(parent) = self.parent.as_ref() else {
            return;
        };
        match bounded(connection.session_renew(parent.reference())).await {
            Ok(result) => self.parent = Some(result.record),
            Err(_) => return,
        }
        // Attachment generation may have advanced since the initial grant.
        // Fetch the current owned key's reference before parent-initiated revoke.
        match bounded(connection.session_grant_fetch(child.pane.public_key)).await {
            Ok(found) => {
                child.record = Some(found.record.clone());
                if bounded(connection.session_revoke(found.record.reference()))
                    .await
                    .is_ok()
                {
                    self.children.remove(&id);
                }
            }
            Err(error) if forbidden(&error) => {
                self.children.remove(&id);
            }
            Err(_) => {}
        }
    }

    async fn retry_due(&mut self, now: Instant) {
        let ids: Vec<_> = self
            .children
            .iter()
            .filter(|(_, c)| {
                c.pane.live.load(Ordering::Acquire) && c.retry.due.is_some_and(|due| now >= due)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            // Scheduled retries spend their budget before even fetching, so a
            // transport failure cannot become a five-second fetch/re-mint loop.
            if !self.children.get_mut(&id).unwrap().retry.take(now) {
                continue;
            }
            match self.grant_at(id, now, true).await {
                Ok(grant) => self.refresh_ready(id, &grant),
                Err(error) => {
                    self.children.get_mut(&id).unwrap().retry.expired(now);
                    let held = self.children[&id].retry.due.is_none();
                    self.diagnostic(
                        Some(id),
                        format!(
                            "bounded child re-grant failed: {error}; {}",
                            if held {
                                "held until reconnect/gap (attempt cap reached)"
                            } else {
                                "next backoff scheduled"
                            }
                        ),
                    );
                }
            }
            // As in reconciliation, a slow batch must not starve the parent.
            if let (Some(connection), Some(parent)) = (&self.connection, &self.parent)
                && let Ok(result) = bounded(connection.session_renew(parent.reference())).await
            {
                self.parent = Some(result.record);
            }
        }
    }

    async fn run(&mut self, mut requests: UnboundedReceiver<Request>) {
        let mut renew = tokio::time::interval(RENEW);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = renew.tick() => {
                    let healthy = if let (Some(connection), Some(parent)) = (&self.connection, &self.parent) {
                        match bounded(connection.session_renew(parent.reference())).await {
                            Ok(result) => { self.parent = Some(result.record); true }
                            Err(_) => false,
                        }
                    } else { false };
                    if !healthy { self.connect().await; }
                    // Notices schedule retries; this tick does not poll grants.
                    self.retry_due(Instant::now()).await;
                    let closing: Vec<_> = self.children.iter().filter(|(_, c)| !c.pane.live.load(Ordering::Acquire)).map(|(id, _)| *id).collect();
                    for id in closing { self.close_child(id).await; }
                    self.provision().await;
                }
                request = requests.recv() => match request {
                    Some(Request::Provision) => self.provision().await,
                    Some(Request::Close(id, ack)) => {
                        self.close_child(id).await;
                        if let Some(ack) = ack { let _ = ack.send(()); }
                    }
                    None => break,
                },
                event = async { self.connection.as_mut().expect("guarded connection").recv().await }, if self.connection.is_some() => {
                    match event {
                        Some(event) => {
                            let command = event.command();
                            if command.command == "noded.session.lifecycle.gap" {
                                self.reconcile().await;
                                self.provision().await;
                            } else if command.command == "noded.session.lifecycle" {
                                self.notice(&command.body).await;
                            }
                        }
                        None => { self.connect().await; }
                    }
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        // No unconsumed launch key or descriptor survives shutdown.
        self.shared.lock().unwrap().ready = None;
        // Children first, then the parent (which recursively revokes anything
        // whose individual revoke raced a child resumption). Bound total shutdown.
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            let ids: Vec<_> = self.children.keys().copied().collect();
            for id in ids {
                self.close_child(id).await;
            }
        })
        .await;
        if let (Some(connection), Some(parent)) = (&self.connection, &self.parent) {
            let _ = bounded(connection.session_revoke(parent.reference())).await;
            close_connection(connection).await;
        }
    }

    async fn notice(&mut self, body: &str) {
        self.notice_at(body, Instant::now()).await;
    }

    async fn notice_at(&mut self, body: &str, now: Instant) {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Notice {
            target: RecordRef,
            state: BindingState,
            broker_epoch: HexBytes<16>,
        }
        let notice = match serde_json::from_str::<Notice>(body) {
            Ok(notice) => notice,
            Err(error) => {
                // Do not log the payload; exact schema failures are observable.
                eprintln!("term dropped native-session lifecycle notice: {error}");
                return;
            }
        };
        if self
            .parent
            .as_ref()
            .is_none_or(|p| p.broker_epoch != notice.broker_epoch)
        {
            return;
        }
        let id = self.children.iter().find_map(|(id, child)| {
            child
                .record
                .as_ref()
                .filter(|record| {
                    record.record_id == notice.target.record_id
                        && record.incarnation == notice.target.incarnation
                        && notice.target.binding_generation.0 >= record.binding_generation.0
                })
                .map(|_| *id)
        });
        if let Some(id) = id {
            if notice.state == BindingState::Revoked
                && !self.children[&id].retry.enrolled
                && self.children[&id].pane.live.load(Ordering::Acquire)
                && let Some(connection) = &self.connection
                && let Ok(found) =
                    bounded(connection.session_grant_fetch(self.children[&id].pane.public_key))
                        .await
            {
                // A coalesced lifecycle stream may skip Attached. A consumed
                // grant records enrolment even after attachment revocation.
                self.children.get_mut(&id).unwrap().retry.enrolled =
                    found.grant.state == GrantState::Consumed;
            }
            let child = self.children.get_mut(&id).unwrap();
            child.retry.enrolled |= notice.state == BindingState::Attached;
            if let Some(record) = &mut child.record {
                record.binding_generation = notice.target.binding_generation;
                record.state = notice.state;
            }
            if notice.state == BindingState::Revoked && child.pane.live.load(Ordering::Acquire) {
                // Never enrol => hold until an external event. Enrolled once
                // => at most three attempts, at 120/240/480s after expiry.
                child.retry.expired(now);
                let message = if child.retry.due.is_some() {
                    "grant expired/revoked; bounded re-grant backoff scheduled"
                } else {
                    "grant expired/revoked; held until reconnect, lifecycle gap or pane restart"
                };
                self.diagnostic(Some(id), message);
            } else {
                self.diagnostic(Some(id), format!("child binding {:?}", notice.state));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use term_native_test_broker::Broker;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    // Tests may wait for background work; production pane-open never does.
    pub(crate) fn wait_ready(handle: &NativeSession, id: u64) {
        handle.1.lock().unwrap().next_id = id;
        let _ = handle.0.send(Request::Provision);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if handle
                .1
                .lock()
                .unwrap()
                .ready
                .as_ref()
                .is_some_and(|r| r.pane.id == id)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "ready bundle: {}",
                handle.status()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn prepare_ready(handle: &NativeSession, id: u64) -> (PaneSession, LaunchFd) {
        wait_ready(handle, id);
        handle.prepare(id).expect("ready launch")
    }

    fn test_actor(broker: &Broker) -> Actor {
        Actor {
            grant_creates: 0,
            options: broker.options(),
            url: broker.url.clone(),
            key: fresh_key().unwrap(),
            connection: None,
            parent: None,
            children: HashMap::new(),
            shared: Arc::new(std::sync::Mutex::new(Shared::default())),
            provisioned: None,
        }
    }

    #[test]
    fn simulated_expiry_cycles_bound_real_grant_creates() {
        let broker = Broker::start();
        runtime().block_on(async {
            let mut actor = test_actor(&broker);
            actor.connect().await;
            let key = fresh_key().unwrap();
            actor.children.insert(
                1,
                Child {
                    pane: Arc::new(PaneState {
                        id: 1,
                        generation: AtomicU64::new(1),
                        live: AtomicBool::new(true),
                        launched: AtomicBool::new(true),
                        public_key: HexBytes(key.verifying_key().to_bytes()),
                    }),
                    record: None,
                    retry: Retry {
                        external: true,
                        ..Retry::default()
                    },
                    pending: None,
                },
            );
            let before = clock_ms().unwrap();
            let granted = actor.grant(1).await.unwrap();
            let after = clock_ms().unwrap();
            // Verify the simulation's 30s cadence against an ACTUAL mint by
            // noded (this fixture shares our clock namespace). The separate
            // wall-clock expiry test covers its deadline scheduler as well.
            assert!(granted.grant.expires_ms.0 >= before + GRANT_LIFETIME.as_millis() as u64);
            assert!(granted.grant.expires_ms.0 <= after + GRANT_LIFETIME.as_millis() as u64);
            let pending = actor.children[&1].pending.as_ref().unwrap();
            assert!(pending.usable(&granted.grant, after));
            assert!(!pending.usable(&granted.grant, pending.deadline));
            let mut inconsistent = granted.grant.clone();
            inconsistent.expires_ms.0 += 1;
            assert!(!pending.usable(&inconsistent, after));
            inconsistent = granted.grant.clone();
            inconsistent.state = GrantState::Expired;
            assert!(!pending.usable(&inconsistent, after)); // record still Pending

            // At 32 idle panes, the old loop spends 65,535 remaining names in
            // about 17h (noded MAX_ISSUED=65,536, including the parent). Run
            // beyond that horizon; count actual grant.create RPCs, not notices.
            let cycles = 65_536 / 32 + 2;
            let seconds_to_exhaust = ((65_536 - 1) / 32) * GRANT_LIFETIME.as_secs();
            assert!((17 * 3600..18 * 3600).contains(&seconds_to_exhaust));
            async fn cycles_for(actor: &mut Actor, cycles: u32) -> Vec<u64> {
                let start = Instant::now();
                let mut minted_at = Vec::new();
                for cycle in 1..=cycles {
                    let now = start + GRANT_LIFETIME * cycle;
                    let record = actor.children[&1].record.clone().unwrap();
                    if record.state != BindingState::Revoked {
                        // Expire at the simulated broker deadline via its real
                        // revocation engine; never fabricate grant RPC replies.
                        actor
                            .connection
                            .as_ref()
                            .unwrap()
                            .session_revoke(record.reference())
                            .await
                            .unwrap();
                        actor
                            .notice_at(
                                &serde_json::json!({"target":record.reference(),
                            "state":"revoked", "broker_epoch":record.broker_epoch})
                                .to_string(),
                                now,
                            )
                            .await;
                    }
                    let before = actor.grant_creates;
                    actor.retry_due(now).await;
                    if actor.grant_creates != before {
                        minted_at.push((now - start).as_secs());
                    }
                }
                minted_at
            }
            assert!(cycles_for(&mut actor, cycles).await.is_empty());
            assert_eq!(actor.grant_creates, 1, "never-enrolled pane must hold");
            actor.reconcile().await; // one external gap/reconnect opportunity
            assert_eq!(actor.grant_creates, 2);
            assert!(cycles_for(&mut actor, cycles).await.is_empty());
            assert_eq!(actor.grant_creates, 2);

            actor.reconcile().await;
            let granted = actor.grant(1).await.unwrap();
            let child = observer(&broker).await;
            let scope = ExpectedScope {
                broker_epoch: child.session_hello().await.unwrap().broker_epoch,
                purpose: Purpose::Enrol,
                unix_uid: granted.record.owner_uid,
                parent_key_hash: Some(granted.grant.parent_key_hash),
                pane_id: granted.record.pane_id,
                pane_high_water: granted.record.pane_generation,
                role: granted.record.role,
                public_key_hash: HexBytes(Sha256::digest(key.verifying_key().to_bytes()).into()),
                capabilities_hash: HexBytes(
                    Sha256::digest(encode_capabilities(&granted.record.capabilities).unwrap())
                        .into(),
                ),
            };
            let challenge = child
                .session_challenge_key(granted.grant.public_key)
                .await
                .unwrap();
            child
                .session_prove(&challenge.sign(&key, &scope).unwrap())
                .await
                .unwrap();
            // Fetch reconciles actual enrolment even without an Attached notice.
            actor.grant(1).await.unwrap();
            assert!(actor.children[&1].retry.enrolled);
            let before = actor.grant_creates;
            assert_eq!(cycles_for(&mut actor, cycles).await, [150, 420, 930]);
            assert_eq!(actor.grant_creates - before, RETRY_CAP as usize);
            assert!(actor.children[&1].retry.due.is_none());
            actor.reconcile().await; // external recovery re-arms a capped pane
            assert_eq!(actor.grant_creates, before + RETRY_CAP as usize + 1);
            // Transport/fetch failures must also spend scheduled attempts and
            // respect the floor. No connection means these cannot mint at all.
            let connection = actor.connection.take().unwrap();
            close_connection(&connection).await;
            let count = actor.grant_creates;
            let mut now = Instant::now();
            actor.children.get_mut(&1).unwrap().retry.expired(now);
            for attempt in 0..RETRY_CAP {
                let due = now + RETRY_FLOOR * (1 << attempt);
                actor.retry_due(due - Duration::from_millis(1)).await;
                assert_eq!(actor.children[&1].retry.attempts, attempt);
                actor.retry_due(due).await;
                assert_eq!(actor.children[&1].retry.attempts, attempt + 1);
                now = due;
            }
            assert!(actor.children[&1].retry.due.is_none());
            assert_eq!(actor.grant_creates, count);
            actor.shutdown().await;
        });
    }

    #[test]
    fn ready_consumption_and_missing_bundle_never_wait_for_broker() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        wait_ready(&supervisor.handle, 1);
        let paused = broker.pause();
        let start = Instant::now();
        let (pane, fd) = supervisor.handle.prepare(1).unwrap();
        assert!(supervisor.handle.prepare(2).is_none());
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(
            supervisor.handle.status()["panes"]["2"]
                .as_str()
                .unwrap()
                .contains("graphics-only")
        );
        drop(fd);
        drop(paused);
        drop(pane);
    }

    #[test]
    fn lowered_pending_quota_opens_pane_with_queryable_diagnostic() {
        let broker = Broker::with_grant_limit(1);
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (_pane, fd) = prepare_ready(&supervisor.handle, 1);
        drop(fd); // leave the first grant pending: no deployed/enrolling child
        let deadline = Instant::now() + Duration::from_secs(5);
        while !supervisor.handle.status()["diagnostic"]
            .as_str()
            .unwrap()
            .contains("quota exhausted")
        {
            assert!(Instant::now() < deadline, "{}", supervisor.handle.status());
            std::thread::sleep(Duration::from_millis(10));
        }
        let terminal = crate::terminal::Terminal::start_session(
            crate::config::Settings {
                config: crate::config::Config::default(),
                term: "xterm-256color",
            },
            Some(&supervisor.handle),
            2,
        )
        .unwrap();
        assert!(terminal.pid > 0);
        assert!(
            supervisor.handle.status()["panes"]["2"]
                .as_str()
                .unwrap()
                .contains("quota exhausted")
        );
    }

    #[test]
    fn saturated_uds_close_and_supervisor_shutdown_are_bounded() {
        let broker = Broker::start();
        runtime().block_on(async {
            let connection = observer(&broker).await;
            let paused = broker.pause();
            let flood = async {
                for _ in 0..1024 {
                    connection
                        .client()
                        .send(
                            "noded",
                            "noded.help",
                            serde_json::json!({"padding":"x".repeat(65536)}),
                        )
                        .await
                        .unwrap();
                }
            };
            assert!(
                tokio::time::timeout(Duration::from_millis(200), flood)
                    .await
                    .is_err(),
                "UDS must be backpressured"
            );
            let start = Instant::now();
            close_connection(&connection).await;
            assert!(start.elapsed() < RPC_BUDGET + Duration::from_secs(1));
            drop(paused);
        });
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        wait_ready(&supervisor.handle, 1);
        let paused = broker.pause();
        let start = Instant::now();
        drop(supervisor);
        assert!(start.elapsed() < STOP_BUDGET + Duration::from_secs(1));
        drop(paused);
    }

    async fn observer(broker: &Broker) -> VerifiedConnection {
        let UnixConnectOutcome::VerifiedUnix(connection) =
            NodedClient::connect_unix("", &broker.url, &broker.options(), None)
                .await
                .unwrap()
        else {
            panic!("verified Unix required")
        };
        connection
    }

    async fn wait_record(
        connection: &VerifiedConnection,
        predicate: impl Fn(&SessionRecord) -> bool,
    ) -> SessionRecord {
        // Test observation only. The production actor never polls grant state.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(record) = connection
                    .session_list()
                    .await
                    .unwrap()
                    .records
                    .into_iter()
                    .find(&predicate)
                {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("broker state transition")
    }

    #[test]
    fn real_uds_pane_close_and_term_shutdown_revoke() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (pane, fd) = prepare_ready(&supervisor.handle, 1);
        drop(fd);
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let child = wait_record(&observer, |r| {
                r.pane_id == Some(DecimalU64(1)) && r.state == BindingState::Pending
            })
            .await;
            assert_eq!(child.policy, Policy::DefaultOpen);
            assert_eq!(child.capabilities.len(), 6);
            pane.revoke();
            assert!(!pane.state.live.load(Ordering::Acquire));
            pane.revoke_before_cleanup();
            wait_record(&observer, |r| {
                r.record_id == child.record_id && r.state == BindingState::Revoked
            })
            .await;
            // Repeated close is harmless and cannot create a replacement.
            pane.revoke();
            let (second, fd) = prepare_ready(&supervisor.handle, 2);
            drop(fd);
            let child = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(2))).await;
            drop(supervisor);
            wait_record(&observer, |r| {
                r.record_id == child.record_id && r.state == BindingState::Revoked
            })
            .await;
            assert!(
                observer
                    .session_list()
                    .await
                    .unwrap()
                    .records
                    .iter()
                    .all(|r| r.state == BindingState::Revoked)
            );
            drop(second);
        });
    }

    #[test]
    fn real_uds_child_exit_revokes_without_ui_reap() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        wait_ready(&supervisor.handle, 1);
        let terminal = crate::terminal::Terminal::start_session(
            crate::config::Settings {
                config: crate::config::Config::default(),
                term: "xterm-256color",
            },
            Some(&supervisor.handle),
            1,
        )
        .unwrap();
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let child = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(1))).await;
            // Actual OS child exit reaches MeteredPty::next_child_event. No
            // callback invocation or TabSet/UI reap substitutes for that path.
            assert_eq!(unsafe { libc::kill(terminal.pid, libc::SIGKILL) }, 0);
            wait_record(&observer, |r| {
                r.record_id == child.record_id && r.state == BindingState::Revoked
            })
            .await;
        });
    }

    #[test]
    fn real_uds_tab_close_revokes_before_pty_cleanup() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        wait_ready(&supervisor.handle, 1);
        let mut tabs =
            crate::tabs::TabSet::with_session(settings, Some(supervisor.handle.clone())).unwrap();
        let tab_id = tabs.active_tab().id;
        let pane_id = tabs.active_tab().active_pane;
        let terminal = tabs.active_pane_terminal();
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let child = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(pane_id))).await;
            let state = supervisor.handle.1.lock().unwrap().panes[&pane_id]
                .upgrade()
                .unwrap();
            let (metadata_tx, metadata_rx) = mpsc::sync_channel(1);
            tabs.probe_metadata_removal(Box::new(move |id| {
                assert_eq!(id, pane_id);
                assert!(
                    !state.live.load(Ordering::Acquire),
                    "metadata removed before local revoke"
                );
                metadata_tx.send(()).unwrap();
            }));
            let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(1);
            let options = broker.options();
            let url = broker.url.clone();
            let record_id = child.record_id;
            terminal.lock().unwrap().before_pty_cleanup = Some(Box::new(move || {
                // This probe runs after the revoke wait, immediately before
                // PTY cleanup. Query the real broker at that boundary.
                runtime().block_on(async {
                    let UnixConnectOutcome::VerifiedUnix(connection) =
                        NodedClient::connect_unix("", &url, &options, None)
                            .await
                            .unwrap()
                    else {
                        panic!("verified Unix");
                    };
                    let record = connection
                        .session_list()
                        .await
                        .unwrap()
                        .records
                        .into_iter()
                        .find(|r| r.record_id == record_id)
                        .unwrap();
                    assert_eq!(
                        record.state,
                        BindingState::Revoked,
                        "PTY cleanup preceded broker revoke"
                    );
                });
                cleanup_tx.send(()).unwrap();
            }));
            let paused = broker.pause();
            // The mutation must not acquire the PTY mutex. Reordering revoke
            // below metadata.remove fails the first boundary probe above.
            let removed = {
                let _held = terminal.lock().unwrap();
                tabs.close(tab_id).1
            };
            assert!(tabs.is_empty());
            metadata_rx.try_recv().unwrap();
            let cleanup = std::thread::spawn(move || drop(removed));
            assert!(cleanup_rx.recv_timeout(Duration::from_millis(100)).is_err());
            drop(paused);
            cleanup.join().unwrap();
            cleanup_rx.try_recv().unwrap();
            wait_record(&observer, |r| {
                r.record_id == child.record_id && r.state == BindingState::Revoked
            })
            .await;
        });
    }

    #[test]
    fn real_uds_broker_bounce_regrants_retained_public_key() {
        let mut broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (pane, fd) = prepare_ready(&supervisor.handle, 7);
        drop(fd);
        let key = pane.state.public_key;
        runtime().block_on(async {
            let old_observer = observer(&broker).await;
            let old = wait_record(&old_observer, |r| r.pane_id == Some(DecimalU64(7))).await;
            let challenge = old_observer
                .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                    public_key: key,
                    purpose: Purpose::Enrol,
                }))
                .await
                .unwrap();
            let parent_hash = challenge.transcript.parent_key_hash;
            broker.bounce();
            let observer = observer(&broker).await;
            let new = wait_record(&observer, |r| {
                r.pane_id == Some(DecimalU64(7)) && r.state == BindingState::Pending
            })
            .await;
            assert_ne!(old.broker_epoch, new.broker_epoch);
            assert_ne!(old.parent_instance, new.parent_instance);
            assert_ne!(old.incarnation, new.incarnation);
            let challenge = observer
                .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                    public_key: key,
                    purpose: Purpose::Enrol,
                }))
                .await
                .unwrap();
            assert_eq!(challenge.transcript.parent_key_hash, parent_hash);
            assert_eq!(
                challenge.transcript.public_key_hash,
                HexBytes(Sha256::digest(key.0).into())
            );
            assert_eq!(challenge.transcript.pane_id, Some(DecimalU64(7)));
            assert_eq!(challenge.transcript.role, Role::PaneShell);
            pane.revoke_before_cleanup();
            wait_record(&observer, |r| {
                r.record_id == new.record_id && r.state == BindingState::Revoked
            })
            .await;
        });
    }

    #[test]
    fn real_uds_resume_retains_parent_and_reconciles_lost_grant_ack() {
        let broker = Broker::start();
        runtime().block_on(async {
            let mut actor = Actor {
                grant_creates: 0,
                options: broker.options(),
                url: broker.url.clone(),
                key: fresh_key().unwrap(),
                connection: None,
                parent: None,
                children: HashMap::new(),
                shared: Arc::new(std::sync::Mutex::new(Shared::default())),
                provisioned: None,
            };
            actor.connect().await;
            let parent = actor.parent.clone().unwrap();
            let key = fresh_key().unwrap();
            actor.children.insert(
                1,
                Child {
                    pane: Arc::new(PaneState {
                        id: 1,
                        generation: AtomicU64::new(1),
                        live: AtomicBool::new(true),
                        launched: AtomicBool::new(true),
                        public_key: HexBytes(key.verifying_key().to_bytes()),
                    }),
                    record: None,
                    retry: Retry {
                        external: true,
                        ..Retry::default()
                    },
                    pending: None,
                },
            );
            let grant = actor.grant(1).await.unwrap();
            // Model a committed create whose reply did not reach the supervisor.
            actor.children.get_mut(&1).unwrap().record = None;
            actor.connection.as_ref().unwrap().client().close().await;
            actor.connect().await;
            let resumed = actor.parent.as_ref().unwrap();
            assert_eq!(resumed.record_id, parent.record_id);
            assert_eq!(resumed.incarnation, parent.incarnation);
            assert!(resumed.binding_generation.0 > parent.binding_generation.0);
            let reconciled = actor.grant(1).await.unwrap();
            assert_eq!(reconciled.grant.grant_id, grant.grant.grant_id);
            assert_eq!(reconciled.record.record_id, grant.record.record_id);
            actor.close_child(1).await;
            actor.connection.as_ref().unwrap().client().close().await;
        });
    }

    #[test]
    fn real_uds_never_enrolled_expiry_holds_and_renew_keeps_parent_alive() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (pane, fd) = prepare_ready(&supervisor.handle, 3);
        drop(fd);
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let old = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(3))).await;
            // Real broker CLOCK_BOOTTIME: no fake time or grant-state polling
            // in the actor. Also spans two complete 15-second parent leases.
            tokio::time::sleep(Duration::from_secs(31)).await;
            wait_record(&observer, |r| {
                r.record_id == old.record_id && r.state == BindingState::Revoked
            })
            .await;
            let records = observer.session_list().await.unwrap().records;
            assert_eq!(
                records
                    .iter()
                    .filter(|r| r.pane_id == Some(DecimalU64(3)))
                    .count(),
                1
            );
            assert!(
                records
                    .iter()
                    .any(|r| r.role == Role::Term && r.state == BindingState::Attached)
            );
            assert_eq!(pane.state.generation.load(Ordering::Acquire), 1);
            tokio::time::timeout(Duration::from_secs(3), async {
                while !supervisor.handle.status()["panes"]["3"]
                    .as_str()
                    .unwrap()
                    .contains("held until")
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                supervisor.handle.status()["panes"]["3"]
                    .as_str()
                    .unwrap()
                    .contains("held until")
            );
        });
    }

    #[test]
    #[ignore = "S3 Mix slice: consume memfd before hooks and prove over real PTY"]
    fn mix_child_bootstrap_proves_end_to_end() {
        panic!("Implement with the child-side bootstrap slice; never count this seam as passing");
    }
}
