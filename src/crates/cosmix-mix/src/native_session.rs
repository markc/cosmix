//! Private BROKER-019 bootstrap and BROKER-020 attachment owner.
//!
//! This binary-only module has a startup entry point, called before any threads,
//! startup hooks, evaluator or user source. The evaluator library cannot import
//! the binary. No Value, Environment, builtin, property, bus handler or context
//! object ever receives Bootstrap, its seed, or an owner handle. The resident
//! thread alone owns the Zeroizing seed; temporary SigningKeys zeroize on drop.
//! A separate exec-restart signal can only stop the owner; it exposes no state.
//! Ordinary shells without the marker return before config, allocation or I/O.

use cosmix_lib_bus::native_session::*;
use cosmix_lib_client::session::{ChallengeResult, ExpectedScope, Hello, SessionFailure};
use cosmix_lib_client::{
    BrokerAccount, ConnectError, NodedClient, UnixConnectOptions, UnixConnectOutcome,
    VerifiedConnection,
};
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const MARKER: &str = "COSMIX_SESSION_FD";
const SEALS: i32 = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
const RPC: Duration = Duration::from_secs(2);
// BROKER-020: renew every 5s, lease expires 15s after the last renewal.
const RENEW_CADENCE: Duration = Duration::from_secs(5);
const _: () = assert!(RENEW_CADENCE.as_secs() * 3 <= 15);
const CONNECT_BACKOFF_BASE: Duration = Duration::from_secs(5);
const PROOF_RETRY_FLOOR: Duration = Duration::from_secs(5);
const NOTICE_COALESCING_FLOOR: Duration = Duration::from_secs(5);
const PROOF_RETRY_CAP: u32 = 3;
const WAKE_RETRY: Duration = Duration::from_secs(60);
const CONNECT_CAP: u32 = 6;
type RestartAck = std::sync::mpsc::SyncSender<bool>;
// Control only: no key, descriptor, scope, record or evaluator value is shared.
static RESTART: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<RestartAck>> =
    std::sync::OnceLock::new();

/// The shell's two exec-restart paths call this before replacing the process.
/// This is intentionally not a builtin: it can only stop the private owner.
pub(super) fn before_exec_restart() {
    crate::session_state::commit(crate::session_state::Transition::ShellReplacement);
    let Some(sender) = RESTART.get() else {
        return;
    };
    let (ack, receiver) = std::sync::mpsc::sync_channel(1);
    let revoked = sender.send(ack).is_ok()
        && receiver
            .recv_timeout(Duration::from_secs(16))
            .unwrap_or(false);
    eprintln!(
        "mix native-session: exec restart leaves this pane unbound until pane restart; {}",
        if revoked {
            "record observed revoked"
        } else {
            "revocation unconfirmed; remaining records expire by lease/window"
        }
    );
}

// Deliberately no Debug, Clone, Serialize, accessor or shared/static storage.
struct Bootstrap {
    seed: Zeroizing<[u8; 32]>,
    scope: ExpectedScope,
    parent: (HexBytes<16>, HexBytes<16>),
    public_key: HexBytes<32>,
    #[cfg(test)]
    proof_delay_once: Duration,
    #[cfg(test)]
    proof_attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn marker_fd(value: &std::ffi::OsStr) -> Result<RawFd, &'static str> {
    value
        .to_str()
        .and_then(|v| v.parse().ok())
        .filter(|fd| *fd >= 3)
        .ok_or("marker: expected an open descriptor numbered at least 3")
}

/// Sole caller is main, before starting any threads. Removing the marker here
/// also scrubs malformed input; no later exec or environment import sees it.
fn consume() -> Result<Option<Bootstrap>, &'static str> {
    let result = consume_inner();
    if result.is_err() {
        quarantine_failed_bootstrap();
    }
    result
}

// Startup only, before any other threads or descriptor-owning application
// objects exist. A malformed marker may name the wrong fd: close every named
// launch memfd, including duplicates. Never touch stdio or unrelated memfds.
// Without procfs the sweep is best-effort; consume_inner still closes the
// descriptor actually named by a valid marker, but cannot find mislabelled copies.
fn quarantine_failed_bootstrap() {
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        let descriptors: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let fd = entry.file_name().to_str()?.parse::<RawFd>().ok()?;
                let target = std::fs::read_link(entry.path()).ok()?;
                (fd >= 3
                    && target
                        .to_string_lossy()
                        .trim_start_matches('/')
                        .starts_with("memfd:cosmix-session"))
                .then_some(fd)
            })
            .collect();
        for fd in descriptors {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn consume_inner() -> Result<Option<Bootstrap>, &'static str> {
    let Some(value) = std::env::var_os(MARKER) else {
        return Ok(None);
    };
    // SAFETY: main is still single-threaded; no concurrent environment readers.
    unsafe { std::env::remove_var(MARKER) };
    let raw = marker_fd(&value)?;
    if unsafe { libc::fcntl(raw, libc::F_GETFD) } < 0 {
        return Err("descriptor: marker names a closed descriptor");
    }
    // SAFETY: the inherited descriptor is open, uniquely consumed by this
    // bootstrap before any threads. File closes it on EVERY success/error path.
    let file = unsafe { File::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err("descriptor: could not quarantine inherited descriptor");
    }
    parse(&file).map(Some)
}

fn parse(file: &File) -> Result<Bootstrap, &'static str> {
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & SEALS != SEALS {
        return Err("seals: bootstrap requires SHRINK, GROW, WRITE and SEAL");
    }
    let stat = file
        .metadata()
        .map_err(|_| "layout: cannot stat bootstrap")?;
    if !stat.is_file() || stat.nlink() != 0 || !(38..=16421).contains(&stat.len()) {
        return Err("layout: expected a bounded anonymous memfd");
    }
    let mut header = [0u8; 5];
    file.read_exact_at(&mut header, 0)
        .map_err(|_| "layout: truncated header")?;
    let size = u32::from_be_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if header[0] != 1 || size == 0 || size > 16384 || stat.len() != 37 + size as u64 {
        return Err("layout: invalid version, length or trailing bytes");
    }
    // pread never depends on the offset shared with Term. Public JSON and the
    // seed are read separately; no ordinary Vec/String ever contains the seed.
    let mut public = vec![0; size];
    file.read_exact_at(&mut public, 5)
        .map_err(|_| "layout: truncated descriptor")?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Descriptor {
        grant: SessionGrant,
        record: SessionRecord,
    }
    let descriptor: Descriptor =
        serde_json::from_slice(&public).map_err(|_| "layout: invalid public descriptor")?;
    let mut seed = Zeroizing::new([0u8; 32]);
    file.read_exact_at(seed.as_mut(), 5 + size as u64)
        .map_err(|_| "layout: truncated seed")?;
    let key = SigningKey::from_bytes(&seed);
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let Descriptor { grant, record } = descriptor;
    if grant.public_key != public_key
        || grant.record_id != record.record_id
        || grant.incarnation != record.incarnation
        || grant.state != GrantState::Pending
        || grant.expires_ms.0 == 0
        || record.state != BindingState::Pending
        || record.binding_generation.0 != 0
        || record.role != Role::PaneShell
        || record.owner_uid != unsafe { libc::geteuid() }
        || record.pane_id.is_none_or(|id| id.0 == 0)
        || record
            .pane_generation
            .is_none_or(|generation| generation.0 == 0)
    {
        return Err("scope: inconsistent launch descriptor or seed");
    }
    let parent = record
        .parent_instance
        .zip(record.parent_incarnation)
        .ok_or("scope: missing parent identity")?;
    let capabilities =
        encode_capabilities(&record.capabilities).map_err(|_| "scope: invalid capabilities")?;
    Ok(Bootstrap {
        seed,
        parent,
        public_key,
        #[cfg(test)]
        proof_delay_once: Duration::ZERO,
        #[cfg(test)]
        proof_attempts: Default::default(),
        scope: ExpectedScope {
            broker_epoch: record.broker_epoch,
            purpose: Purpose::Enrol,
            unix_uid: record.owner_uid,
            parent_key_hash: Some(grant.parent_key_hash),
            pane_id: record.pane_id,
            pane_high_water: record.pane_generation,
            role: record.role,
            public_key_hash: HexBytes(Sha256::digest(public_key.0).into()),
            capabilities_hash: HexBytes(Sha256::digest(capabilities).into()),
        },
    })
    // record.lease_remaining_ms is deliberately unused: stale by construction.
}

pub(super) fn start() {
    let bootstrap = match consume() {
        Ok(None) => return,
        Ok(Some(bootstrap)) => bootstrap,
        Err(stage) => {
            loud(stage);
            return;
        }
    };
    // Also remove duplicate launch descriptors before threads start. This
    // makes later runtime/worker failures incapable of leaving inherited fds.
    quarantine_failed_bootstrap();
    crate::session_state::enable();
    // Snapshot every env-derived input while main is still single-threaded.
    // The evaluator may later mutate environ; the resident must never read it.
    let account = std::env::var("COSMIX_BROKER_ACCOUNT").unwrap_or_else(|_| "cosmix-noded".into());
    let environment = crate::node_config::NativeEnvironment::capture();
    let (sender, restart) = tokio::sync::mpsc::unbounded_channel();
    let _ = RESTART.set(sender);
    if std::thread::Builder::new()
        .name("mix-native-session".into())
        .spawn(move || {
            let mut reporter = Reporter::default();
            // NSS, filesystem discovery and config reads may stall. They are
            // off the prompt path and consume only the captured env strings.
            let (endpoint, url) = match environment.resolve() {
                Ok(configuration) => configuration,
                Err(stage) => {
                    reporter.report(stage);
                    return;
                }
            };
            let options = match options(account, endpoint) {
                Ok(options) => options,
                Err(stage) => {
                    reporter.report(stage);
                    return;
                }
            };
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    reporter.report("runtime: attachment worker unavailable");
                    return;
                }
            };
            runtime.block_on(own(bootstrap, options, url, &mut reporter, restart));
            runtime.shutdown_timeout(Duration::from_millis(100));
        })
        .is_err()
    {
        loud("runtime: could not start attachment worker");
    }
    // Detached, process-owned lifetime. Shell exit never joins a broker task.
    // Term's real child-exit path owns revocation; abrupt exit also closes UDS.
}

fn options(
    account: String,
    endpoint: Option<std::path::PathBuf>,
) -> Result<UnixConnectOptions, &'static str> {
    let name =
        std::ffi::CString::new(account).map_err(|_| "configuration: invalid broker account")?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 65536];
    // Trusted launch configuration selects an account, never a numeric UID
    // inferred from the socket or the calling user. No cos-side dependency.
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
        return Err("configuration: broker account unavailable");
    }
    let entry = unsafe { entry.assume_init() };
    let mut options = UnixConnectOptions::new(BrokerAccount {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    });
    options.configured_endpoint = endpoint;
    options.require_native_session = true;
    options.incoming_capacity = Some(64);
    Ok(options)
}

fn loud(stage: &str) {
    eprintln!("mix native-session FAILED at {stage}; shell remains usable without binding");
}
#[derive(Default)]
struct Reporter {
    reported: bool,
    wake_reported: bool,
}
impl Reporter {
    fn report(&mut self, stage: &str) {
        if !self.reported {
            loud(stage);
            self.reported = true;
        }
    }
    fn wake(&mut self) {
        // BROKER-019 explicitly requires surfacing lost wake registration,
        // even if an earlier transport outage already produced a diagnostic.
        // Once only; neither reconnects nor the one fallback retry spam stderr.
        if !self.wake_reported {
            loud("wake registration: unavailable; if binding fails, one retry after 60s then stop");
            self.wake_reported = true;
            self.reported = true;
        }
    }
}

enum Recovery {
    Reconnect,
    FreshChallenge,
    Wait,
    Stop,
}
struct Failure {
    stage: &'static str,
    recovery: Recovery,
    wake: bool,
}
impl Failure {
    fn scope() -> Self {
        Self {
            stage: "scope: broker scope does not match retained launch",
            recovery: Recovery::Wait,
            wake: false,
        }
    }
}
async fn rpc<T>(
    stage: &'static str,
    future: impl std::future::Future<Output = Result<T, SessionFailure>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(RPC, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(SessionFailure::Refused { error, wake_error })) => Err(Failure {
            stage,
            recovery: refusal_recovery(stage, &error),
            wake: wake_error.is_some(),
        }),
        Ok(Err(SessionFailure::Transport(_))) | Err(_) => Err(Failure {
            stage,
            recovery: Recovery::Reconnect,
            wake: false,
        }),
        _ => Err(Failure {
            stage,
            recovery: Recovery::Stop,
            wake: false,
        }),
    }
}

fn refusal_recovery(stage: &str, error: &SessionError) -> Recovery {
    let reason = error
        .details
        .get("reason")
        .and_then(serde_json::Value::as_str);
    if stage.starts_with("prove:")
        && matches!(
            (error.error_code, reason),
            (ErrorCode::Expired, Some("challenge_expired"))
                | (ErrorCode::Conflict, Some("challenge_consumed"))
        )
    {
        Recovery::FreshChallenge
    } else {
        Recovery::Wait
    }
}

#[derive(Default)]
struct ProofRetries(u32);
impl ProofRetries {
    fn next(&mut self, now: Instant) -> Option<Instant> {
        if self.0 >= PROOF_RETRY_CAP {
            return None;
        }
        self.0 += 1;
        Some(now + PROOF_RETRY_FLOOR)
    }
}

impl Bootstrap {
    fn expected(&self, hello: &Hello, record: &SessionRecord) -> Result<ExpectedScope, Failure> {
        let capabilities =
            encode_capabilities(&record.capabilities).map_err(|_| Failure::scope())?;
        if record.broker_epoch != hello.broker_epoch
            || record.owner_uid != self.scope.unix_uid
            || record.role != self.scope.role
            || record.pane_id != self.scope.pane_id
            || HexBytes(Sha256::digest(capabilities).into()) != self.scope.capabilities_hash
            || record.parent_instance.is_none()
            || record.parent_incarnation.is_none()
            || record
                .pane_generation
                .is_none_or(|generation| generation.0 == 0)
        {
            return Err(Failure::scope());
        }
        let mut expected = self.scope.clone();
        // Independently authenticated discovery supplies state/parent identity.
        // Never copy expected purpose, epoch or immutable scope from challenge.
        expected.broker_epoch = hello.broker_epoch;
        expected.purpose = match record.state {
            BindingState::Pending => Purpose::Enrol,
            BindingState::Attached | BindingState::Suspended => Purpose::Resume,
            BindingState::Revoked => {
                return Err(Failure {
                    stage: "discovery: record revoked",
                    recovery: Recovery::Wait,
                    wake: false,
                });
            }
        };
        if hello.broker_epoch != self.scope.broker_epoch
            || record.parent_instance.zip(record.parent_incarnation) != Some(self.parent)
        {
            // Random IDs do not establish continuity. The retained parent KEY
            // hash still must match in sign(), before this reset is committed.
            expected.pane_high_water = Some(DecimalU64(1));
        }
        Ok(expected)
    }

    async fn attach(
        &mut self,
        connection: &VerifiedConnection,
        hello: &Hello,
        reporter: &mut Reporter,
    ) -> Result<SessionRecord, Failure> {
        #[cfg(test)]
        self.proof_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let challenge = rpc(
            "challenge: key lookup refused or unavailable",
            connection.session_challenge_key(self.public_key),
        )
        .await?;
        if challenge.wake_error.is_some() {
            reporter.wake();
        }
        let wake_failed = challenge.wake_error.is_some();
        #[cfg(test)]
        tokio::time::sleep(std::mem::take(&mut self.proof_delay_once)).await;
        let result = rpc(
            "discovery: record lookup failed",
            connection.session_self(challenge.transcript.record_id),
        )
        .await
        .map_err(|mut error| {
            error.wake |= wake_failed;
            error
        })?;
        // The challenge supplies only a selector. Expected immutable scope is
        // retained from launch and checked against this authenticated UID read.
        let record = &result.record;
        if record.record_id != challenge.transcript.record_id
            || record.incarnation != challenge.transcript.incarnation
        {
            return Err(Failure::scope());
        }
        let expected = self.expected(hello, record)?;
        self.check_challenge(&challenge, hello, record)?;
        let key = SigningKey::from_bytes(&self.seed);
        let proof = challenge
            .sign(&key, &expected)
            .map_err(|_| Failure::scope())?;
        drop(key);
        let result = rpc(
            "prove: broker refused attachment",
            connection.session_prove(&proof),
        )
        .await
        .map_err(|mut error| {
            error.wake |= wake_failed;
            error
        })?;
        let bound = result.record;
        if bound.record_id != record.record_id
            || bound.incarnation != record.incarnation
            || bound.instance_id != record.instance_id
            || bound.parent_instance != record.parent_instance
            || bound.parent_incarnation != record.parent_incarnation
            || bound.pane_generation != record.pane_generation
            || bound.binding_generation != challenge.transcript.binding_generation
            || bound.state != BindingState::Attached
        {
            return Err(Failure::scope());
        }
        self.expected(hello, &bound)?;
        self.scope = expected;
        self.scope.pane_high_water = bound.pane_generation;
        self.parent = bound
            .parent_instance
            .zip(bound.parent_incarnation)
            .ok_or_else(Failure::scope)?;
        Ok(bound)
    }

    fn check_challenge(
        &self,
        challenge: &ChallengeResult,
        hello: &Hello,
        record: &SessionRecord,
    ) -> Result<(), Failure> {
        let p = &challenge.transcript;
        if p.connection_id != hello.connection_id
            || p.instance_id != record.instance_id
            || p.parent_instance != record.parent_instance
            || p.parent_incarnation != record.parent_incarnation
            || p.pane_generation != record.pane_generation
            || record.binding_generation.0.checked_add(1) != Some(p.binding_generation.0)
        {
            return Err(Failure::scope());
        }
        Ok(())
    }
}

async fn own(
    mut bootstrap: Bootstrap,
    options: UnixConnectOptions,
    url: String,
    reporter: &mut Reporter,
    mut restart: tokio::sync::mpsc::UnboundedReceiver<RestartAck>,
) {
    let mut failures = 0;
    // Survives transport loss: exec restart during backoff still has a target.
    let mut last_record: Option<SessionRecord> = None;
    loop {
        if failures >= CONNECT_CAP {
            return;
        }
        if failures > 0
            && !reconnect_backoff(
                failures,
                &mut restart,
                &mut bootstrap,
                last_record.as_ref(),
                &url,
                &options,
            )
            .await
        {
            return;
        }
        failures += 1;
        let result =
            tokio::time::timeout(RPC, NodedClient::connect_unix("", &url, &options, None)).await;
        let connection = match result {
            Ok(Ok(UnixConnectOutcome::VerifiedUnix(connection))) => connection,
            Ok(Err(
                ConnectError::InvalidEndpoint
                | ConnectError::EndpointOwnership
                | ConnectError::EndpointChanged
                | ConnectError::PeerCredentials
                | ConnectError::UnsupportedVersion,
            )) => {
                reporter.report(
                    "connect verification: broker identity/profile rejected; no retry or downgrade",
                );
                return;
            }
            _ => {
                reporter.report(
                    "connect: verified broker unavailable; at most six attempts, then stop",
                );
                continue;
            }
        };
        let connection = std::sync::Arc::new(connection);
        let hello = match rpc(
            "hello: broker context unavailable",
            connection.session_hello(),
        )
        .await
        {
            Ok(hello) => hello,
            Err(error) => {
                reporter.report(error.stage);
                close(&connection).await;
                continue;
            }
        };
        let mut record: Option<SessionRecord> = None;
        let mut pending = Some(Instant::now());
        let mut next_attempt = Instant::now();
        let mut wake_retry_used = false;
        let mut proof_retries = ProofRetries::default();
        let mut tick = tokio::time::interval(RENEW_CADENCE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut requests = tokio::task::JoinSet::new();
        let mut refusal: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> =
            None;
        let reconnect = loop {
            tokio::select! {
                biased;
                Some(ack) = restart.recv() => {
                    requests.abort_all();
                    while requests.join_next().await.is_some() {}
                    let revoked = revoke_for_restart(&mut bootstrap, Some(&connection), record.as_ref().or(last_record.as_ref()), &url, &options).await;
                    let _ = ack.send(revoked);
                    break false;
                }
                _ = tick.tick() => {
                    if pending.is_some_and(|due| Instant::now() >= due) {
                        pending = None;
                        next_attempt = Instant::now() + NOTICE_COALESCING_FLOOR;
                        match bootstrap.attach(&connection, &hello, reporter).await {
                            Ok(bound) => {
                                crate::session_state::commit(crate::session_state::Transition::AttachmentChanged { source: Some((&bound).into()) });
                                last_record = Some(bound.clone()); record = Some(bound); failures = 0; proof_retries = ProofRetries::default();
                            }
                            Err(error) => {
                                record = None;
                                crate::session_state::commit(crate::session_state::Transition::AttachmentChanged { source: None });
                                if error.wake { reporter.wake(); } else { reporter.report(error.stage); }
                                match error.recovery {
                                    Recovery::Reconnect => break true,
                                    Recovery::Stop => break false,
                                    Recovery::FreshChallenge => {
                                        pending = proof_retries.next(Instant::now());
                                    }
                                    Recovery::Wait => {
                                        if error.wake && !wake_retry_used {
                                            wake_retry_used = true;
                                            pending = Some(Instant::now() + WAKE_RETRY);
                                        } else if error.wake {
                                            break false;
                                        }
                                        // Otherwise retain key interest and wait for a
                                        // notice/gap. No periodic grant-state polling.
                                    }
                                }
                            }
                        }
                    } else if let Some(bound) = &record {
                        match rpc("renew: attachment lost", connection.session_renew(bound.reference())).await {
                            Ok(result) if result.record.reference() == bound.reference() && result.record.state == BindingState::Attached => record = Some(result.record),
                            _ => { reporter.report("renew: attachment lost; bounded reconnect"); break true; }
                        }
                    }
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
                _ = async { if let Some(work) = &mut refusal { work.await } }, if refusal.is_some() => { refusal = None; }
                event = connection.recv_shared(), if refusal.is_none() => {
                    let Some(event) = event else { break true };
                    if !connection.client().is_connected() { break true; }
                    let command = event.command();
                    if command.command == "noded.session.lifecycle.gap" || command.command == "noded.session.lifecycle" {
                        // Authenticated notices are hints, never scope or authority.
                        // Coalesce them behind the floor. Attached notices at our
                        // own generation must not trigger a self-resume feedback loop.
                        let relevant = match relevant_notice(&command.command, &command.body, &hello, record.as_ref()) {
                            Ok(relevant) => relevant,
                            Err(_) => { reporter.report("notice decode: malformed lifecycle hint dropped"); false }
                        };
                        if relevant {
                            record = None;
                            crate::session_state::commit(crate::session_state::Transition::AttachmentChanged { source: None });
                            pending.get_or_insert(next_attempt);
                        }
                    } else if command.id.is_some() {
                        let connection = connection.clone();
                        if requests.len() < 4 && let Some(bound) = record.as_ref() {
                            let bound = bound.clone();
                            let hello = hello.clone();
                            requests.spawn(async move {
                                crate::session_status::dispatch(&connection, &hello, &bound, &event).await;
                            });
                        } else {
                            refusal = Some(Box::pin(async move {
                                crate::session_status::refuse(&connection, &event).await;
                            }));
                        }
                    }
                }
            }
        };
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        drop(refusal);
        crate::session_state::commit(crate::session_state::Transition::AttachmentChanged {
            source: None,
        });
        close(&connection).await;
        if !reconnect {
            return;
        }
        reporter.report("transport: attachment disconnected; bounded reconnects");
        failures = failures.max(1);
    }
}

async fn reconnect_backoff(
    failures: u32,
    restart: &mut tokio::sync::mpsc::UnboundedReceiver<RestartAck>,
    bootstrap: &mut Bootstrap,
    record: Option<&SessionRecord>,
    url: &str,
    options: &UnixConnectOptions,
) -> bool {
    tokio::select! {
        biased;
        Some(ack) = restart.recv() => {
            let confirmed = revoke_for_restart(bootstrap, None, record, url, options).await;
            let _ = ack.send(confirmed);
            false
        }
        _ = tokio::time::sleep(CONNECT_BACKOFF_BASE * (1 << (failures - 1).min(3))) => true
    }
}

async fn revoke_for_restart(
    bootstrap: &mut Bootstrap,
    connection: Option<&VerifiedConnection>,
    record: Option<&SessionRecord>,
    url: &str,
    options: &UnixConnectOptions,
) -> bool {
    // All phases share a total budget, including a fresh proof during backoff.
    // The caller's 16s wait also allows an in-flight ordinary proof to finish.
    tokio::time::timeout(
        RPC * 5,
        revoke_for_restart_inner(bootstrap, connection, record, url, options),
    )
    .await
    .unwrap_or(false)
}

async fn revoke_for_restart_inner(
    bootstrap: &mut Bootstrap,
    connection: Option<&VerifiedConnection>,
    record: Option<&SessionRecord>,
    url: &str,
    options: &UnixConnectOptions,
) -> bool {
    let Some(record) = record else {
        return false;
    };
    let mut record = record.clone();
    let independent;
    let connection = match connection {
        Some(connection) => connection,
        None => {
            let Ok(Ok(UnixConnectOutcome::VerifiedUnix(connection))) =
                tokio::time::timeout(RPC, NodedClient::connect_unix("", url, options, None)).await
            else {
                return false;
            };
            let Ok(Ok(hello)) = tokio::time::timeout(RPC, connection.session_hello()).await else {
                close(&connection).await;
                return false;
            };
            // UID authentication permits discovery, not child revocation.
            // Re-prove the retained key to recover self-revoke authority.
            match bootstrap
                .attach(&connection, &hello, &mut Reporter::default())
                .await
            {
                Ok(bound) => record = bound,
                Err(_) => {
                    close(&connection).await;
                    return false;
                }
            }
            independent = connection;
            &independent
        }
    };
    let _ = tokio::time::timeout(RPC, connection.session_revoke(record.reference())).await;
    close(connection).await;
    // Self-revoke closes its transport before the ACK is guaranteed. Confirm
    // committed state through an independent authenticated, targeted read.
    let Ok(Ok(UnixConnectOutcome::VerifiedUnix(observer))) =
        tokio::time::timeout(RPC, NodedClient::connect_unix("", url, options, None)).await
    else {
        return false;
    };
    // Confirms an observed state, not that our revoke RPC caused it. Term or
    // expiry may already have revoked the record before this read.
    let confirmed = matches!(tokio::time::timeout(RPC, observer.session_self(record.record_id)).await,
        Ok(Ok(result)) if result.record.state == BindingState::Revoked
            && result.record.broker_epoch == record.broker_epoch
            && result.record.incarnation == record.incarnation);
    close(&observer).await;
    confirmed
}

fn relevant_notice(
    command: &str,
    body: &str,
    hello: &Hello,
    current: Option<&SessionRecord>,
) -> Result<bool, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct Notice {
        broker_epoch: HexBytes<16>,
        target: RecordRef,
        state: BindingState,
    }
    #[derive(serde::Deserialize)]
    struct Gap {
        broker_epoch: HexBytes<16>,
    }
    if command == "noded.session.lifecycle.gap" {
        return serde_json::from_str::<Gap>(body).map(|gap| gap.broker_epoch == hello.broker_epoch);
    }
    serde_json::from_str::<Notice>(body).map(|notice| {
        notice.broker_epoch == hello.broker_epoch
            && current.is_none_or(|current| {
                notice.target.record_id != current.record_id
                    || notice.target.incarnation != current.incarnation
                    || notice.target.binding_generation.0 > current.binding_generation.0
                    || notice.target.binding_generation == current.binding_generation
                        && notice.state != BindingState::Attached
            })
    })
}

async fn close(connection: &VerifiedConnection) {
    let _ = tokio::time::timeout(RPC, connection.client().close()).await;
}

#[cfg(test)]
#[path = "native_session_tests.rs"]
mod tests;
