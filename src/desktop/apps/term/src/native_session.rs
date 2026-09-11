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
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const RPC_BUDGET: Duration = Duration::from_secs(2);
const RENEW: Duration = Duration::from_secs(5);

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
pub struct NativeSession(
    UnboundedSender<Request>,
    Arc<std::sync::Mutex<HashMap<u64, std::sync::Weak<PaneState>>>>,
);

pub struct Supervisor {
    pub handle: NativeSession,
    worker: Option<std::thread::JoinHandle<()>>,
}

struct PaneState {
    id: u64,
    generation: AtomicU64,
    live: AtomicBool,
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
            let _ = rx.recv_timeout(Duration::from_secs(3));
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
    Prepare(
        Arc<PaneState>,
        mpsc::SyncSender<Result<Option<GrantResult>, String>>,
    ),
    Close(u64, Option<mpsc::SyncSender<()>>),
    Stop,
}

impl NativeSession {
    /// Only a short local-state lock; never acquires Terminal's PTY mutex or
    /// waits for ABP while the pane model is being mutated.
    pub fn revoke_pane(&self, id: u64) {
        if let Some(state) = self.1.lock().unwrap().remove(&id).and_then(|p| p.upgrade()) {
            state.live.store(false, Ordering::Release);
        }
        let _ = self.0.send(Request::Close(id, None));
    }

    pub fn prepare(&self, id: u64) -> Result<Option<(PaneSession, LaunchFd)>, String> {
        let key = fresh_key().map_err(|e| e.to_string())?;
        let state = Arc::new(PaneState {
            id,
            generation: AtomicU64::new(1),
            live: AtomicBool::new(true),
            public_key: HexBytes(key.verifying_key().to_bytes()),
        });
        let pane = PaneSession {
            state: state.clone(),
            handle: self.clone(),
        };
        self.1.lock().unwrap().insert(id, Arc::downgrade(&state));
        let (tx, rx) = mpsc::sync_channel(1);
        self.0
            .send(Request::Prepare(state, tx))
            .map_err(|_| "session task stopped")?;
        let grant = rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| "session launch timed out")??;
        let Some(grant) = grant else { return Ok(None) };
        let fd = LaunchFd::new(&grant, &key).map_err(|e| e.to_string())?;
        // SigningKey has zeroize-on-drop enabled; only its public key survives.
        drop(key);
        Ok(Some((pane, fd)))
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
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
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
                        enabled_once: false,
                    };
                    actor.connect().await;
                    let _ = ready_tx.send(());
                    actor.run(rx).await;
                });
            })
            .map_err(|e| e.to_string())?;
        let _ = ready_rx.recv_timeout(Duration::from_secs(5));
        Ok(Self {
            handle: NativeSession(tx, Arc::new(std::sync::Mutex::new(HashMap::new()))),
            worker: Some(worker),
        })
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.handle.0.send(Request::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Child {
    pane: Arc<PaneState>,
    record: Option<SessionRecord>,
}

struct Actor {
    options: UnixConnectOptions,
    url: String,
    key: SigningKey,
    connection: Option<VerifiedConnection>,
    parent: Option<SessionRecord>,
    children: HashMap<u64, Child>,
    enabled_once: bool,
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
    async fn connect(&mut self) {
        if let Some(old) = self.connection.take() {
            old.client().close().await;
        }
        let connection = tokio::time::timeout(
            RPC_BUDGET,
            NodedClient::connect_unix("", &self.url, &self.options, None),
        )
        .await;
        let Ok(Ok(UnixConnectOutcome::VerifiedUnix(connection))) = connection else {
            return;
        };
        let public_key = HexBytes(self.key.verifying_key().to_bytes());
        // Always reconcile by retained key first, including uncertain allocate
        // ACKs. Only an authenticated, definitive absence permits allocation.
        let result = match bounded(connection.session_challenge(&ChallengeArgs::Key(
            KeyChallenge {
                public_key,
                purpose: Purpose::Enrol,
            },
        )))
        .await
        {
            Ok(challenge) => {
                if challenge.wake_error.is_some() {
                    eprintln!("term session wake registration unavailable");
                }
                let expected = ExpectedScope {
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
                self.enabled_once = true;
                if replacement {
                    for child in self.children.values_mut() {
                        child.record = None;
                        child.pane.generation.store(1, Ordering::Release);
                    }
                }
                self.reconcile().await;
            }
            Err(error) => {
                eprintln!("term identity recovery: {error}");
                connection.client().close().await;
            }
        }
    }

    async fn grant(&mut self, id: u64) -> ResultSession<GrantResult> {
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
            Ok(found) if found.record.state != BindingState::Revoked => {
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
        let result = bounded(connection.session_grant_create(&args)).await?;
        child.record = Some(result.record.clone());
        Ok(result)
    }

    async fn reconcile(&mut self) {
        let ids: Vec<_> = self.children.keys().copied().collect();
        for id in ids {
            if !self.children[&id].pane.live.load(Ordering::Acquire) {
                self.close_child(id).await;
            } else if let Err(error) = self.grant(id).await {
                eprintln!("term child grant reconciliation: {error}");
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
                    // Retry only locally requested closes; never poll grant expiry.
                    let closing: Vec<_> = self.children.iter().filter(|(_, c)| !c.pane.live.load(Ordering::Acquire)).map(|(id, _)| *id).collect();
                    for id in closing { self.close_child(id).await; }
                }
                request = requests.recv() => match request {
                    Some(Request::Prepare(pane, reply)) => {
                        if self.connection.is_none() {
                            let _ = reply.send(if self.enabled_once { Err("native session temporarily unavailable".into()) } else { Ok(None) });
                            continue;
                        }
                        let id = pane.id;
                        self.children.insert(id, Child { pane, record: None });
                        let result = self.grant(id).await.map(Some).map_err(|e| e.to_string());
                        if reply.send(result).is_err() { self.close_child(id).await; }
                    }
                    Some(Request::Close(id, ack)) => {
                        self.close_child(id).await;
                        if let Some(ack) = ack { let _ = ack.send(()); }
                    }
                    Some(Request::Stop) | None => break,
                },
                event = async { self.connection.as_mut().expect("guarded connection").recv().await }, if self.connection.is_some() => {
                    match event {
                        Some(event) => {
                            let command = event.command();
                            if command.command == "noded.session.lifecycle.gap" {
                                self.reconcile().await;
                            } else if command.command == "noded.session.lifecycle" {
                                self.notice(&command.body).await;
                            }
                        }
                        None => { self.connect().await; }
                    }
                }
            }
        }
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
            connection.client().close().await;
        }
    }

    async fn notice(&mut self, body: &str) {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Notice {
            target: RecordRef,
            state: BindingState,
            broker_epoch: HexBytes<16>,
        }
        let Ok(notice) = serde_json::from_str::<Notice>(body) else {
            return;
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
                && self.children[&id].pane.live.load(Ordering::Acquire)
            {
                if let Err(error) = self.grant(id).await {
                    eprintln!("term child re-grant: {error}");
                }
            } else if let Some(record) = self.children.get_mut(&id).and_then(|c| c.record.as_mut())
            {
                record.binding_generation = notice.target.binding_generation;
                record.state = notice.state;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_native_test_broker::Broker;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
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
        let (pane, fd) = supervisor
            .handle
            .prepare(1)
            .unwrap()
            .expect("profile enabled");
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
            let (second, fd) = supervisor.handle.prepare(2).unwrap().unwrap();
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
        let (pane, fd) = supervisor.handle.prepare(9).unwrap().unwrap();
        drop(fd);
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let child = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(9))).await;
            pane.exit_notifier()();
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
        let mut tabs =
            crate::tabs::TabSet::with_session(settings, Some(supervisor.handle.clone())).unwrap();
        let tab_id = tabs.active_tab().id;
        let pane_id = tabs.active_tab().active_pane;
        let terminal = tabs.active_pane_terminal();
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let child = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(pane_id))).await;
            // Closing must not acquire the PTY mutex. Keep cleanup deliberately
            // unsubmitted until broker revocation has been observed.
            let removed = {
                let _held = terminal.lock().unwrap();
                tabs.close(tab_id).1
            };
            assert!(tabs.is_empty());
            wait_record(&observer, |r| {
                r.record_id == child.record_id && r.state == BindingState::Revoked
            })
            .await;
            drop(removed);
        });
    }

    #[test]
    fn real_uds_broker_bounce_regrants_retained_public_key() {
        let mut broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (pane, fd) = supervisor.handle.prepare(7).unwrap().unwrap();
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
                options: broker.options(),
                url: broker.url.clone(),
                key: fresh_key().unwrap(),
                connection: None,
                parent: None,
                children: HashMap::new(),
                enabled_once: false,
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
                        public_key: HexBytes(key.verifying_key().to_bytes()),
                    }),
                    record: None,
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
    fn real_uds_expiry_notice_regrants_and_renew_keeps_parent_alive() {
        let broker = Broker::start();
        let supervisor = Supervisor::with_options(broker.options(), broker.url.clone()).unwrap();
        let (pane, fd) = supervisor.handle.prepare(3).unwrap().unwrap();
        drop(fd);
        runtime().block_on(async {
            let observer = observer(&broker).await;
            let old = wait_record(&observer, |r| r.pane_id == Some(DecimalU64(3))).await;
            // Real broker CLOCK_BOOTTIME: no fake time or grant-state polling
            // in the actor. Also spans two complete 15-second parent leases.
            tokio::time::sleep(Duration::from_secs(31)).await;
            let new = wait_record(&observer, |r| {
                r.pane_id == Some(DecimalU64(3))
                    && r.state == BindingState::Pending
                    && r.record_id != old.record_id
            })
            .await;
            assert_eq!(new.parent_instance, old.parent_instance);
            assert_eq!(new.pane_generation, Some(DecimalU64(2)));
            assert_eq!(pane.state.generation.load(Ordering::Acquire), 2);
            assert_eq!(new.broker_epoch, old.broker_epoch);
            let challenge = observer
                .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                    public_key: pane.state.public_key,
                    purpose: Purpose::Enrol,
                }))
                .await
                .unwrap();
            assert_eq!(challenge.transcript.record_id, new.record_id);
        });
    }

    #[test]
    #[ignore = "S3 Mix slice: consume memfd before hooks and prove over real PTY"]
    fn mix_child_bootstrap_proves_end_to_end() {
        panic!("Implement with the child-side bootstrap slice; never count this seam as passing");
    }
}
