//! Event-driven SSH host/key catalogue and bounded reachability probes.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use inotify::{Inotify, WatchMask};

const HOST_LIMIT: usize = 256;
const HOST_FILE_LIMIT: usize = 64 * 1024;
const KEY_LIMIT: usize = 64;
const KEY_FILE_LIMIT: usize = 16 * 1024;
const CONFIG_FILE_LIMIT: usize = 64 * 1024;
const CONFIG_INCLUDE_LIMIT: usize = 16;
const HOST_FILE_MODE: u32 = 0o600;
const PROBE_ERROR_LIMIT: usize = 16 * 1024;
const PROBE_WORKERS: usize = 4;
const PROBE_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "AddKeysToAgent=no",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "UpdateHostKeys=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "RequestTTY=no",
    "-o",
    "RemoteCommand=none",
    "-o",
    "LogLevel=ERROR",
];
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;

pub(crate) type WireSshHost = (
    String,
    String,
    String,
    String,
    u16,
    String,
    String,
    bool,
    String,
    String,
    u64,
    u64,
);
pub(crate) type WireSshKey = (String, String, String);

#[derive(Clone, Debug, serde::Serialize, zbus::zvariant::Type)]
pub(crate) struct WireSshSnapshot {
    revision: u64,
    state: String,
    error: String,
    hosts: Vec<WireSshHost>,
    keys: Vec<WireSshKey>,
    active_probes: u32,
}

#[derive(Debug, zbus::DBusError, PartialEq, Eq)]
#[zbus(prefix = "dev.cosmix.trayd.Error", impl_display = true)]
pub(crate) enum SshError {
    InvalidSshField(String),
    UnknownSshHost(String),
    SshHostNotActionable(String),
    SshHostExists(String),
    SshTrashCollision(String),
    SshHostTrashed(String),
    SshConfigNotIncluded(String),
    SshStoreFailure(String),
    SshProbeLimit(String),
    SshLaunchFailure(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HostEntry {
    id: String,
    host_error: String,
    host_warning: String,
    hostname: String,
    port: u16,
    user: String,
    identity: String,
    trashed: bool,
    probe_status: String,
    probe_error: String,
    probe_ms: u64,
    probe_checked_at: u64,
    content_tag: u64,
    source_tag: u64,
}

impl HostEntry {
    fn actionable(&self) -> bool {
        !self.trashed && self.host_error.is_empty()
    }

    fn wire(&self) -> WireSshHost {
        (
            self.id.clone(),
            self.host_error.clone(),
            self.host_warning.clone(),
            self.hostname.clone(),
            self.port,
            self.user.clone(),
            self.identity.clone(),
            self.trashed,
            self.probe_status.clone(),
            self.probe_error.clone(),
            self.probe_ms,
            self.probe_checked_at,
        )
    }

    fn reset_probe(&mut self) {
        self.probe_status = "unknown".into();
        self.probe_error.clear();
        self.probe_ms = 0;
        self.probe_checked_at = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyEntry {
    id: String,
    fingerprint: String,
    key_error: String,
}

impl KeyEntry {
    fn wire(&self) -> WireSshKey {
        (
            self.id.clone(),
            self.fingerprint.clone(),
            self.key_error.clone(),
        )
    }
}

#[derive(Debug)]
struct SshState {
    revision: u64,
    state: String,
    error: String,
    config_included: bool,
    hosts: Vec<HostEntry>,
    keys: Vec<KeyEntry>,
    active_probes: u32,
    publications: VecDeque<u64>,
}

impl Default for SshState {
    fn default() -> Self {
        Self {
            revision: 0,
            state: "absent".into(),
            error: String::new(),
            config_included: false,
            hosts: Vec::new(),
            keys: Vec::new(),
            active_probes: 0,
            publications: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct FingerprintCacheKey {
    dev: u64,
    ino: u64,
    mtime_ns: i128,
    size: u64,
}

#[derive(Clone, Debug)]
struct ProbeJob {
    id: String,
    source_tag: u64,
}

#[derive(Clone, Debug)]
struct ProbeResult {
    ok: bool,
    error: String,
    elapsed_ms: u64,
    checked_at: u64,
}

trait SshRunner: Send + Sync {
    fn fingerprint(&self, public_key: &[u8]) -> Result<String, String>;
    fn probe(&self, ssh: &Path, id: &str) -> ProbeResult;
    fn retry_resolution(&self) {}
}

struct SystemSshRunner {
    timeout: Mutex<Result<PathBuf, String>>,
    ssh_keygen: Mutex<Result<PathBuf, String>>,
}

impl SystemSshRunner {
    fn new() -> Self {
        Self {
            timeout: Mutex::new(resolve_trusted_executable("timeout")),
            ssh_keygen: Mutex::new(resolve_trusted_executable("ssh-keygen")),
        }
    }
}

impl SshRunner for SystemSshRunner {
    fn fingerprint(&self, public_key: &[u8]) -> Result<String, String> {
        let timeout = cached_resolution(&self.timeout)?;
        let ssh_keygen = cached_resolution(&self.ssh_keygen)?;
        let mut child = Command::new(&timeout)
            .args(["--signal=KILL", "5s"])
            .arg(&ssh_keygen)
            .args(["-lf", "/dev/stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start ssh-keygen: {error}"))?;
        let write_result = child
            .stdin
            .take()
            .ok_or_else(|| "ssh-keygen did not provide stdin".to_owned())?
            .write_all(public_key);
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("cannot supply public key to ssh-keygen: {error}"));
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("cannot wait for ssh-keygen: {error}"))?;
        if output.status.success() {
            let fingerprint = String::from_utf8_lossy(&output.stdout);
            let fingerprint = concise(fingerprint.trim());
            if fingerprint.is_empty() {
                Err("ssh-keygen returned an empty fingerprint".into())
            } else {
                Ok(fingerprint)
            }
        } else {
            Err(command_failure("ssh-keygen", &output))
        }
    }

    fn probe(&self, ssh: &Path, id: &str) -> ProbeResult {
        let started = std::time::Instant::now();
        let checked_at = now_ms();
        let timeout = match cached_resolution(&self.timeout) {
            Ok(timeout) => timeout,
            Err(error) => {
                return ProbeResult {
                    ok: false,
                    error,
                    elapsed_ms: 0,
                    checked_at,
                };
            }
        };
        let child = Command::new(&timeout)
            .args(["--signal=KILL", "10s"])
            .arg(ssh)
            .args(PROBE_OPTIONS)
            .args([id, "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let (status, error_bytes) = match child {
            Ok(mut child) => {
                let mut error_pipe = child.stderr.take();
                let status = child.wait();
                let error_bytes = error_pipe.as_mut().map_or_else(
                    || Err("SSH probe did not provide stderr".into()),
                    |pipe| {
                        drain_probe_stderr(pipe)
                            .map_err(|error| format!("cannot read SSH probe error: {error}"))
                    },
                );
                (status, error_bytes)
            }
            Err(error) => (Err(error), Ok(Vec::new())),
        };
        let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        let checked_at = now_ms();
        match status {
            Ok(status) if probe_exit_is_ok(&status, error_bytes.as_deref().unwrap_or_default()) => {
                ProbeResult {
                    ok: true,
                    error: String::new(),
                    elapsed_ms,
                    checked_at,
                }
            }
            Ok(status) => ProbeResult {
                ok: false,
                error: error_bytes.map_or_else(
                    |error| concise(&error),
                    |bytes| probe_failure(&status, &bytes),
                ),
                elapsed_ms,
                checked_at,
            },
            Err(error) => ProbeResult {
                ok: false,
                error: concise(&format!("cannot start SSH probe: {error}")),
                elapsed_ms,
                checked_at,
            },
        }
    }

    fn retry_resolution(&self) {
        retry_cached_resolution(&self.timeout, "timeout");
        retry_cached_resolution(&self.ssh_keygen, "ssh-keygen");
    }
}

fn cached_resolution(slot: &Mutex<Result<PathBuf, String>>) -> Result<PathBuf, String> {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn retry_cached_resolution(slot: &Mutex<Result<PathBuf, String>>, program: &str) {
    let mut resolved = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if resolved.is_err() {
        *resolved = resolve_trusted_executable(program);
    }
}

fn retain_probe_error_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= PROBE_ERROR_LIMIT {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - PROBE_ERROR_LIMIT..]);
        return;
    }
    let excess = tail
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(PROBE_ERROR_LIMIT);
    if excess != 0 {
        tail.drain(..excess);
    }
    tail.extend_from_slice(bytes);
}

fn drain_probe_stderr(pipe: &mut std::process::ChildStderr) -> std::io::Result<Vec<u8>> {
    let descriptor = pipe.as_raw_fd();
    // SAFETY: fcntl only reads or changes flags on this live pipe descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the descriptor remains owned by ChildStderr and the flags preserve
    // every existing bit while adding non-blocking reads.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_GETPIPE_SZ is a read-only query on the live pipe descriptor.
    let pipe_capacity = unsafe { libc::fcntl(descriptor, libc::F_GETPIPE_SZ) };
    if pipe_capacity <= 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut remaining = pipe_capacity as usize;
    let mut tail = Vec::with_capacity(PROBE_ERROR_LIMIT.min(remaining));
    let mut buffer = [0_u8; 4096];
    while remaining != 0 {
        let wanted = buffer.len().min(remaining);
        match pipe.read(&mut buffer[..wanted]) {
            Ok(0) => break,
            Ok(read) => {
                retain_probe_error_tail(&mut tail, &buffer[..read]);
                remaining -= read;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(tail)
}

#[derive(Default)]
struct ProbeQueueState {
    jobs: VecDeque<ProbeJob>,
    closed: bool,
}

struct ProbeQueue {
    state: Mutex<ProbeQueueState>,
    ready: Condvar,
}

impl ProbeQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProbeQueueState::default()),
            ready: Condvar::new(),
        }
    }
}

#[derive(Default)]
struct WatcherLifecycle {
    generation: u64,
    running: bool,
    failing: bool,
    restart_requested: bool,
}

impl WatcherLifecycle {
    fn request_start(&mut self) -> Option<u64> {
        if self.running || self.failing {
            self.restart_requested = true;
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        self.running = true;
        self.restart_requested = false;
        Some(self.generation)
    }

    fn begin_failure(&mut self, generation: u64) -> bool {
        if !self.running || self.generation != generation {
            return false;
        }
        self.running = false;
        self.failing = true;
        true
    }

    fn complete_failure(&mut self, generation: u64) -> Option<u64> {
        if !self.failing || self.generation != generation {
            return None;
        }
        self.failing = false;
        if !std::mem::take(&mut self.restart_requested) {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        self.running = true;
        Some(self.generation)
    }

    fn became_absent(&mut self, generation: u64) {
        if self.running && self.generation == generation {
            self.running = false;
            self.failing = false;
        }
    }
}

pub(crate) struct SshPublication {
    pub(crate) revision: u64,
}

pub(crate) struct SshController {
    store: SshStore,
    state: Mutex<SshState>,
    operations: Mutex<()>,
    runner: Arc<dyn SshRunner>,
    ssh_path: Mutex<Result<PathBuf, String>>,
    fingerprint_cache: Mutex<HashMap<FingerprintCacheKey, Result<String, String>>>,
    queue: Arc<ProbeQueue>,
    publish_tx: SyncSender<()>,
    publish_rx: Mutex<Option<Receiver<()>>>,
    watcher_lifecycle: Mutex<WatcherLifecycle>,
    watcher_ready: AtomicBool,
    watch_error: Mutex<String>,
}

impl SshController {
    pub(crate) fn new_default() -> Arc<Self> {
        let root = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|home| home.join(".ssh"))
            .unwrap_or_else(|| PathBuf::from("/nonexistent/cosmix-trayd-home/.ssh"));
        Self::new_with(
            root,
            Arc::new(SystemSshRunner::new()),
            resolve_trusted_executable("ssh"),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_test(root: PathBuf) -> Arc<Self> {
        Self::new_with(
            root,
            Arc::new(FakeProber::default()),
            Ok(PathBuf::from("/usr/bin/ssh")),
        )
    }

    #[cfg(test)]
    fn new_test_with(root: PathBuf, runner: Arc<dyn SshRunner>) -> Arc<Self> {
        Self::new_with(root, runner, Ok(PathBuf::from("/usr/bin/ssh")))
    }

    fn new_with(
        root: PathBuf,
        runner: Arc<dyn SshRunner>,
        ssh_path: Result<PathBuf, String>,
    ) -> Arc<Self> {
        let (publish_tx, publish_rx) = mpsc::sync_channel(1);
        let controller = Arc::new(Self {
            store: SshStore::new(root),
            state: Mutex::new(SshState::default()),
            operations: Mutex::new(()),
            runner,
            ssh_path: Mutex::new(ssh_path),
            fingerprint_cache: Mutex::new(HashMap::new()),
            queue: Arc::new(ProbeQueue::new()),
            publish_tx,
            publish_rx: Mutex::new(Some(publish_rx)),
            watcher_lifecycle: Mutex::new(WatcherLifecycle::default()),
            watcher_ready: AtomicBool::new(false),
            watch_error: Mutex::new(String::new()),
        });
        Self::start_probe_workers(&controller);
        controller.rescan();
        controller.start_watcher();
        controller
    }

    fn state(&self) -> MutexGuard<'_, SshState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn operation(&self) -> MutexGuard<'_, ()> {
        self.operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state().revision
    }

    pub(crate) fn status(&self) -> String {
        self.state().state.clone()
    }

    pub(crate) fn error(&self) -> String {
        self.state().error.clone()
    }

    pub(crate) fn active_probes(&self) -> u32 {
        self.state().active_probes
    }

    pub(crate) fn snapshot(&self) -> WireSshSnapshot {
        let state = self.state();
        WireSshSnapshot {
            revision: state.revision,
            state: state.state.clone(),
            error: state.error.clone(),
            hosts: state.hosts.iter().map(HostEntry::wire).collect(),
            keys: state.keys.iter().map(KeyEntry::wire).collect(),
            active_probes: state.active_probes,
        }
    }

    pub(crate) fn refresh(self: &Arc<Self>) {
        self.retry_executable_resolution();
        self.rescan();
        self.start_watcher();
    }

    fn retry_executable_resolution(&self) {
        let mut ssh_path = self
            .ssh_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ssh_path.is_err() {
            *ssh_path = resolve_trusted_executable("ssh");
        }
        drop(ssh_path);
        self.runner.retry_resolution();
    }

    fn resolved_ssh(&self) -> Result<PathBuf, SshError> {
        self.ssh_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .map_err(SshError::SshLaunchFailure)
    }

    pub(crate) fn create(
        self: &Arc<Self>,
        name: &str,
        hostname: &str,
        port: u32,
        user: &str,
        key_id: &str,
    ) -> Result<(), SshError> {
        let name = validate_identity("name", name)?;
        let hostname = validate_hostname(hostname)?;
        let port = validate_port(port)?;
        let user = validate_user(user)?;
        let key_id = validate_identity("key_id", key_id)?;
        let _operation = self.operation();
        {
            let state = self.state();
            require_config(&state)?;
            if state.hosts.len() >= HOST_LIMIT {
                return Err(SshError::SshStoreFailure(format!(
                    "SSH host catalogue is at the {HOST_LIMIT} entry cap"
                )));
            }
            let key = state
                .keys
                .iter()
                .find(|key| key.id == key_id)
                .ok_or_else(|| {
                    SshError::InvalidSshField(format!(
                        "key_id {key_id:?} is not in the SSH key catalogue"
                    ))
                })?;
            if !key.key_error.is_empty() {
                return Err(SshError::InvalidSshField(format!(
                    "key_id {key_id:?} is not usable: {}",
                    key.key_error
                )));
            }
        }
        if !self
            .store
            .config_included()
            .map_err(SshError::SshStoreFailure)?
        {
            return Err(SshError::SshConfigNotIncluded(
                "~/.ssh/config does not Include ~/.ssh/hosts/*".into(),
            ));
        }
        if !self
            .store
            .key_exists(&key_id)
            .map_err(SshError::SshStoreFailure)?
        {
            return Err(SshError::InvalidSshField(format!(
                "key_id {key_id:?} is no longer in the SSH key catalogue"
            )));
        }
        let content = format!(
            "Host {name}\n  Hostname {hostname}\n  Port {port}\n  User {user}\n  IdentityFile ~/.ssh/keys/{key_id}\n"
        );
        self.store
            .create_host(&name, content.as_bytes())
            .map_err(|error| match error {
                CreateHostError::Exists => {
                    SshError::SshHostExists(format!("SSH host {name} already exists"))
                }
                CreateHostError::Failure(error) => SshError::SshStoreFailure(error),
            })?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn edit_path(self: &Arc<Self>, id: &str) -> Result<PathBuf, SshError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_id(id).map_err(SshError::InvalidSshField)?;
        {
            let state = self.state();
            require_live_host(&state, &id)?;
        }
        self.store.edit_path(&id).map_err(SshError::SshStoreFailure)
    }

    pub(crate) fn trash(self: &Arc<Self>, id: &str) -> Result<(), SshError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_id(id).map_err(SshError::InvalidSshField)?;
        {
            let state = self.state();
            require_live_host(&state, &id)?;
        }
        self.store
            .move_host(&id, false)
            .map_err(|error| map_move_error(error, &id, false))?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn restore(self: &Arc<Self>, id: &str) -> Result<(), SshError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_id(id).map_err(SshError::InvalidSshField)?;
        {
            let state = self.state();
            require_trashed_host(&state, &id, true)?;
        }
        self.store
            .move_host(&id, true)
            .map_err(|error| map_move_error(error, &id, true))?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn purge(self: &Arc<Self>, id: &str) -> Result<(), SshError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_id(id).map_err(SshError::InvalidSshField)?;
        {
            let state = self.state();
            require_trashed_host(&state, &id, false)?;
        }
        self.store
            .purge_host(&id)
            .map_err(SshError::SshStoreFailure)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    /// Admission freshness is bounded: probes retain the apply-side source-tag fence, while Connect lets ssh reread live configuration at execution inside the same-user trust boundary.
    pub(crate) fn connect_argv(&self, id: &str) -> Result<Vec<String>, SshError> {
        let id = canonical_id(id).map_err(SshError::InvalidSshField)?;
        let _operation = self.operation();
        let cached_source_tag = {
            let state = self.state();
            require_config(&state)?;
            let host = state
                .hosts
                .iter()
                .find(|host| host.id == id)
                .ok_or_else(|| SshError::UnknownSshHost(format!("unknown SSH host: {id}")))?;
            require_actionable(host)?;
            host.source_tag
        };
        self.require_fresh_config_locked()?;
        let fresh = self.fresh_actionable_host_locked(&id)?;
        if fresh.source_tag != cached_source_tag {
            self.rescan_locked();
        }
        let ssh = self.resolved_ssh()?;
        Ok(vec![
            "konsole".into(),
            "-e".into(),
            ssh.to_string_lossy().into_owned(),
            id,
        ])
    }

    pub(crate) fn probe(self: &Arc<Self>, ids: Vec<String>) -> Result<(), SshError> {
        let _operation = self.operation();
        let mut requested = Vec::new();
        let requested_ids = {
            let state = self.state();
            require_config(&state)?;
            self.resolved_ssh()?;
            if ids.is_empty() {
                state
                    .hosts
                    .iter()
                    .filter(|host| host.actionable())
                    .map(|host| host.id.clone())
                    .collect::<Vec<_>>()
            } else {
                let mut seen = BTreeSet::new();
                ids.into_iter()
                    .map(|id| canonical_id(&id).map_err(SshError::InvalidSshField))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|id| seen.insert(id.clone()))
                    .collect()
            }
        };
        self.require_fresh_config_locked()?;
        let mut source_changed = false;
        for id in requested_ids {
            let cached = {
                let state = self.state();
                let host = state
                    .hosts
                    .iter()
                    .find(|host| host.id == id)
                    .ok_or_else(|| SshError::UnknownSshHost(format!("unknown SSH host: {id}")))?;
                require_actionable(host)?;
                host.clone()
            };
            let fresh = self.fresh_actionable_host_locked(&id)?;
            source_changed |= fresh.source_tag != cached.source_tag;
            if cached.probe_status != "probing" {
                requested.push(ProbeJob {
                    id,
                    source_tag: fresh.source_tag,
                });
            }
        }
        if source_changed {
            self.rescan_locked();
            let state = self.state();
            requested.retain_mut(|job| {
                let Some(host) = state.hosts.iter().find(|host| host.id == job.id) else {
                    return false;
                };
                if !host.actionable() || host.probe_status == "probing" {
                    return false;
                }
                job.source_tag = host.source_tag;
                true
            });
        }
        if requested.is_empty() {
            return Ok(());
        }

        let mut queue = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.closed || queue.jobs.len().saturating_add(requested.len()) > HOST_LIMIT {
            return Err(SshError::SshProbeLimit(format!(
                "at most {HOST_LIMIT} SSH probes may be queued"
            )));
        }
        {
            let mut state = self.state();
            require_config(&state)?;
            let mut queued = 0_u32;
            for job in &requested {
                let Some(host) = state.hosts.iter_mut().find(|host| {
                    host.id == job.id
                        && host.source_tag == job.source_tag
                        && host.actionable()
                        && host.probe_status != "probing"
                }) else {
                    continue;
                };
                host.probe_status = "probing".into();
                host.probe_error.clear();
                queue.jobs.push_back(job.clone());
                queued = queued.saturating_add(1);
            }
            if queued != 0 {
                state.active_probes = state.active_probes.saturating_add(queued);
                mark_changed(&mut state);
            }
        }
        drop(queue);
        self.queue.ready.notify_all();
        self.wake_publisher();
        Ok(())
    }

    fn require_fresh_config_locked(&self) -> Result<(), SshError> {
        match self.store.config_included() {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.rescan_locked();
                Err(SshError::SshConfigNotIncluded(
                    "~/.ssh/config does not Include ~/.ssh/hosts/*".into(),
                ))
            }
            Err(error) => {
                self.rescan_locked();
                Err(SshError::SshStoreFailure(error))
            }
        }
    }

    fn fresh_actionable_host_locked(&self, id: &str) -> Result<HostEntry, SshError> {
        let fresh = match self.store.read_live_host(id) {
            Ok(fresh) => fresh,
            Err(error) => {
                self.rescan_locked();
                return Err(SshError::SshHostNotActionable(concise(&format!(
                    "SSH host {id} is not actionable: {error}"
                ))));
            }
        };
        if let Err(error) = require_actionable(&fresh) {
            self.rescan_locked();
            return Err(error);
        }
        Ok(fresh)
    }

    pub(crate) fn take_publish_receiver(&self) -> Receiver<()> {
        self.publish_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("SSH publisher receiver may only be taken once")
    }

    pub(crate) fn take_publication(&self) -> Option<SshPublication> {
        let mut state = self.state();
        state
            .publications
            .pop_front()
            .map(|revision| SshPublication { revision })
    }

    fn rescan(&self) {
        self.retry_executable_resolution();
        let _operation = self.operation();
        self.rescan_locked();
    }

    fn rescan_locked(&self) {
        let scan = self.store.scan(&self.runner, &self.fingerprint_cache);
        let watch_error = self
            .watch_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut state = self.state();
        match scan {
            ScanResult::Absent => {
                let changed = state.state != "absent"
                    || !state.error.is_empty()
                    || !state.hosts.is_empty()
                    || !state.keys.is_empty()
                    || state.config_included;
                if changed {
                    state.state = "absent".into();
                    state.error.clear();
                    state.config_included = false;
                    state.hosts.clear();
                    state.keys.clear();
                    mark_changed(&mut state);
                }
            }
            ScanResult::Present(mut scanned) => {
                preserve_probe_results(&state.hosts, &mut scanned.hosts);
                let mut errors = scanned.errors;
                if let Err(error) = &*self
                    .ssh_path
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                {
                    errors.push(error.clone());
                }
                if !watch_error.is_empty() {
                    errors.push(watch_error);
                }
                let error = concise(&errors.join("; "));
                let status = if error.is_empty() {
                    "watching"
                } else {
                    "degraded"
                };
                let changed = state.state != status
                    || state.error != error
                    || state.config_included != scanned.config_included
                    || state.hosts != scanned.hosts
                    || state.keys != scanned.keys;
                if changed {
                    state.state = status.into();
                    state.error = error;
                    state.config_included = scanned.config_included;
                    state.hosts = scanned.hosts;
                    state.keys = scanned.keys;
                    mark_changed(&mut state);
                }
            }
        }
        let publish = !state.publications.is_empty();
        drop(state);
        if publish {
            self.wake_publisher();
        }
    }

    fn start_probe_workers(controller: &Arc<Self>) {
        for index in 0..PROBE_WORKERS {
            let weak = Arc::downgrade(controller);
            let queue = Arc::clone(&controller.queue);
            thread::Builder::new()
                .name(format!("cosmix-trayd-ssh-probe-{index}"))
                .spawn(move || probe_worker(weak, queue))
                .expect("cannot start SSH probe worker");
        }
    }

    fn apply_probe_result(&self, job: ProbeJob, result: ProbeResult) {
        let mut state = self.state();
        state.active_probes = state.active_probes.saturating_sub(1);
        if let Some(host) = state.hosts.iter_mut().find(|host| {
            host.id == job.id
                && host.source_tag == job.source_tag
                && host.actionable()
                && host.probe_status == "probing"
        }) {
            host.probe_status = if result.ok { "ok" } else { "failed" }.into();
            host.probe_error = concise(&result.error);
            host.probe_ms = result.elapsed_ms;
            host.probe_checked_at = result.checked_at;
        }
        mark_changed(&mut state);
        drop(state);
        self.wake_publisher();
    }

    fn start_watcher(self: &Arc<Self>) {
        let generation = self
            .watcher_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .request_start();
        if let Some(generation) = generation {
            self.spawn_watcher(generation);
        }
    }

    fn spawn_watcher(self: &Arc<Self>, generation: u64) {
        let weak = Arc::downgrade(self);
        if let Err(error) = thread::Builder::new()
            .name("cosmix-trayd-ssh-inotify".into())
            .spawn(move || watch_catalogue(weak, generation))
        {
            self.watcher_failed(
                generation,
                format!("cannot start SSH catalogue watcher: {error}"),
            );
        }
    }

    fn set_watcher_ready(&self, generation: u64, ready: bool) {
        let lifecycle = self
            .watcher_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lifecycle.running && !lifecycle.failing && lifecycle.generation == generation {
            self.watcher_ready.store(ready, Ordering::Release);
        }
    }

    fn watcher_absent(&self, generation: u64) {
        self.watcher_ready.store(false, Ordering::Release);
        self.watcher_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .became_absent(generation);
        self.rescan();
    }

    fn watcher_failed(self: &Arc<Self>, generation: u64, error: String) {
        let publish = self
            .watcher_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .begin_failure(generation);
        if !publish {
            return;
        }
        self.watcher_ready.store(false, Ordering::Release);
        *self
            .watch_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = concise(&error);
        self.rescan();
        let restart = self
            .watcher_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .complete_failure(generation);
        if let Some(generation) = restart {
            self.spawn_watcher(generation);
        }
    }

    fn clear_watch_error(&self) {
        self.watch_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn wake_publisher(&self) {
        let _ = self.publish_tx.try_send(());
    }
}

impl Drop for SshController {
    fn drop(&mut self) {
        let mut queue = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.closed = true;
        drop(queue);
        self.queue.ready.notify_all();
    }
}

fn require_config(state: &SshState) -> Result<(), SshError> {
    if state.config_included {
        Ok(())
    } else {
        Err(SshError::SshConfigNotIncluded(
            "~/.ssh/config does not Include ~/.ssh/hosts/*".into(),
        ))
    }
}

fn require_actionable(host: &HostEntry) -> Result<(), SshError> {
    if host.trashed {
        Err(SshError::SshHostTrashed(format!(
            "SSH host {} is trashed",
            host.id
        )))
    } else if !host.host_error.is_empty() {
        Err(SshError::SshHostNotActionable(format!(
            "SSH host {} is not actionable: {}",
            host.id, host.host_error
        )))
    } else {
        Ok(())
    }
}

fn require_live_host<'a>(state: &'a SshState, id: &str) -> Result<&'a HostEntry, SshError> {
    if let Some(host) = state
        .hosts
        .iter()
        .find(|host| host.id == id && !host.trashed)
    {
        return Ok(host);
    }
    if state.hosts.iter().any(|host| host.id == id && host.trashed) {
        Err(SshError::SshHostTrashed(format!(
            "SSH host {id} is trashed"
        )))
    } else {
        Err(SshError::UnknownSshHost(format!("unknown SSH host: {id}")))
    }
}

fn require_trashed_host<'a>(
    state: &'a SshState,
    id: &str,
    restoring: bool,
) -> Result<&'a HostEntry, SshError> {
    if let Some(host) = state
        .hosts
        .iter()
        .find(|host| host.id == id && host.trashed)
    {
        return Ok(host);
    }
    if state
        .hosts
        .iter()
        .any(|host| host.id == id && !host.trashed)
    {
        if restoring {
            Err(SshError::SshHostExists(format!(
                "SSH host {id} already exists in the live catalogue"
            )))
        } else {
            Err(SshError::SshHostNotActionable(format!(
                "SSH host {id} is not trashed"
            )))
        }
    } else {
        Err(SshError::UnknownSshHost(format!(
            "unknown trashed SSH host: {id}"
        )))
    }
}

fn reject_ssh_metacharacters(label: &str, value: &str) -> Result<(), SshError> {
    if value.starts_with('-') {
        return Err(SshError::InvalidSshField(format!(
            "{label} must not start with '-'"
        )));
    }
    if let Some(character) = value
        .chars()
        .find(|character| matches!(character, '#' | '"' | '='))
    {
        return Err(SshError::InvalidSshField(format!(
            "{label} must not contain {character:?}"
        )));
    }
    Ok(())
}

fn validate_identity(label: &str, value: &str) -> Result<String, SshError> {
    reject_ssh_metacharacters(label, value)?;
    canonical_id(value)
        .map_err(|error| SshError::InvalidSshField(format!("invalid {label}: {error}")))
}

fn validate_hostname(value: &str) -> Result<String, SshError> {
    reject_ssh_metacharacters("hostname", value)?;
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-'))
    {
        return Err(SshError::InvalidSshField(
            "hostname must be non-empty and contain only A-Z/a-z/0-9/./:/-".into(),
        ));
    }
    Ok(value.to_owned())
}

fn validate_port(value: u32) -> Result<u16, SshError> {
    if value == 0 {
        return Ok(22);
    }
    u16::try_from(value).map_err(|_| {
        SshError::InvalidSshField("port must be between 1 and 65535, or 0 for default 22".into())
    })
}

fn validate_user(value: &str) -> Result<String, SshError> {
    let value = if value.is_empty() { "root" } else { value };
    reject_ssh_metacharacters("user", value)?;
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        unreachable!("empty user was defaulted")
    };
    if (!first.is_ascii_alphabetic() && first != b'_')
        || bytes.any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(SshError::InvalidSshField(
            "user must be a conservative POSIX token beginning with a letter or '_'".into(),
        ));
    }
    Ok(value.to_owned())
}

fn map_move_error(error: MoveHostError, id: &str, restoring: bool) -> SshError {
    match error {
        MoveHostError::Exists if restoring => {
            SshError::SshHostExists(format!("SSH host {id} already exists"))
        }
        MoveHostError::Exists => {
            SshError::SshTrashCollision(format!("SSH trash entry for {id} already exists"))
        }
        MoveHostError::Missing => {
            SshError::UnknownSshHost(format!("SSH host {id} disappeared before the move"))
        }
        MoveHostError::Failure(error) => SshError::SshStoreFailure(error),
    }
}

fn preserve_probe_results(old: &[HostEntry], new: &mut [HostEntry]) {
    for host in new {
        let Some(previous) = old
            .iter()
            .find(|old| old.id == host.id && old.trashed == host.trashed)
        else {
            continue;
        };
        if previous.content_tag == host.content_tag && previous.host_error == host.host_error {
            // Atomic editors can replace an inode with identical bytes. A
            // completed result remains truthful for that content, but an
            // in-flight result must be fenced when its source was replaced.
            if previous.probe_status != "probing" || previous.source_tag == host.source_tag {
                host.probe_status.clone_from(&previous.probe_status);
                host.probe_error.clone_from(&previous.probe_error);
                host.probe_ms = previous.probe_ms;
                host.probe_checked_at = previous.probe_checked_at;
            }
        } else {
            host.reset_probe();
        }
    }
}

fn mark_changed(state: &mut SshState) {
    state.revision = state.revision.saturating_add(1);
    // At most 256 probes plus catalogue/watch edges can be outstanding. Keep
    // the lane bounded while preserving the probing edge and normal
    // completion batch under expected load.
    const PUBLICATION_LIMIT: usize = HOST_LIMIT * 2 + 16;
    if state.publications.len() >= PUBLICATION_LIMIT {
        state.publications.pop_front();
    }
    state.publications.push_back(state.revision);
}

fn probe_worker(weak: Weak<SshController>, queue: Arc<ProbeQueue>) {
    loop {
        let job = {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.jobs.is_empty() && !state.closed {
                state = queue
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.closed {
                return;
            }
            state.jobs.pop_front().expect("non-empty SSH probe queue")
        };
        let Some(controller) = weak.upgrade() else {
            return;
        };
        // Admission refuses probes while ssh is unresolved, so this branch is
        // unreachable today; if that invariant ever breaks, still settle the
        // row instead of leaving it "probing" with a leaked active slot.
        let result = match controller.resolved_ssh() {
            Ok(ssh) => controller.runner.probe(&ssh, &job.id),
            Err(error) => ProbeResult {
                ok: false,
                error: error.to_string(),
                elapsed_ms: 0,
                checked_at: now_ms(),
            },
        };
        controller.apply_probe_result(job, result);
    }
}

#[derive(Debug)]
struct ScanData {
    config_included: bool,
    hosts: Vec<HostEntry>,
    keys: Vec<KeyEntry>,
    errors: Vec<String>,
}

enum ScanResult {
    Absent,
    Present(ScanData),
}

#[derive(Debug)]
enum CreateHostError {
    Exists,
    Failure(String),
}

#[derive(Debug)]
enum MoveHostError {
    Exists,
    Missing,
    Failure(String),
}

struct SshStore {
    root: PathBuf,
}

impl SshStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn open_root(&self) -> Result<Option<File>, String> {
        match secure_open(&self.root, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(root) => Ok(Some(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(secure_directory_error(&self.root, error)),
        }
    }

    fn required_root(&self) -> Result<File, String> {
        self.open_root()?
            .ok_or_else(|| format!("SSH store {} does not exist", self.root.display()))
    }

    fn required_directory(&self, name: &OsStr) -> Result<File, String> {
        let root = self.required_root()?;
        open_child_directory(&root, name)?.ok_or_else(|| {
            format!(
                "SSH store directory {} does not exist",
                self.root.join(name).display()
            )
        })
    }

    fn config_included(&self) -> Result<bool, String> {
        let root = self.required_root()?;
        config_tree_includes_hosts(&root, &self.root)
    }

    fn key_exists(&self, id: &str) -> Result<bool, String> {
        let keys = self.required_directory(OsStr::new("keys"))?;
        let name = format!("{id}.pub");
        match secure_open_at(keys.as_raw_fd(), OsStr::new(&name), libc::O_RDONLY, 0) {
            Ok(file) => file
                .metadata()
                .map(|metadata| metadata.is_file())
                .map_err(|error| format!("inspecting SSH key {id}: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("refusing unsafe SSH key {id}: {error}")),
        }
    }

    fn read_live_host(&self, id: &str) -> Result<HostEntry, String> {
        let hosts = self.required_directory(OsStr::new("hosts"))?;
        Ok(read_host_entry(
            &hosts,
            OsStr::new(id),
            id.to_owned(),
            false,
        ))
    }

    fn create_host(&self, id: &str, bytes: &[u8]) -> Result<(), CreateHostError> {
        if bytes.len() > HOST_FILE_LIMIT {
            return Err(CreateHostError::Failure(format!(
                "generated SSH host {id} exceeds the {HOST_FILE_LIMIT} byte limit"
            )));
        }
        let hosts = self
            .required_directory(OsStr::new("hosts"))
            .map_err(CreateHostError::Failure)?;
        let name = OsStr::new(id);
        let mut file = secure_open_at(
            hosts.as_raw_fd(),
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            HOST_FILE_MODE,
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CreateHostError::Exists
            } else {
                CreateHostError::Failure(format!("creating SSH host {id}: {error}"))
            }
        })?;
        let result = (|| {
            fchmod(&file, HOST_FILE_MODE)
                .map_err(|error| format!("setting mode on SSH host {id}: {error}"))?;
            file.write_all(bytes)
                .map_err(|error| format!("writing SSH host {id}: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("syncing SSH host {id}: {error}"))?;
            hosts
                .sync_all()
                .map_err(|error| format!("syncing SSH hosts directory: {error}"))
        })();
        if let Err(error) = result {
            let cleanup = unlink_at(&hosts, name);
            let cleanup_sync = hosts.sync_all();
            let mut message = error;
            if let Err(cleanup) = cleanup {
                message.push_str(&format!("; partial-file cleanup failed: {cleanup}"));
            }
            if let Err(cleanup_sync) = cleanup_sync {
                message.push_str(&format!("; cleanup directory sync failed: {cleanup_sync}"));
            }
            return Err(CreateHostError::Failure(message));
        }
        Ok(())
    }

    fn edit_path(&self, id: &str) -> Result<PathBuf, String> {
        let hosts = self.required_directory(OsStr::new("hosts"))?;
        let file = secure_open_at(hosts.as_raw_fd(), OsStr::new(id), libc::O_RDONLY, 0)
            .map_err(|error| format!("opening SSH host {id}: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("inspecting SSH host {id}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("refusing non-regular SSH host {id}"));
        }
        Ok(self.root.join("hosts").join(id))
    }

    fn move_host(&self, id: &str, from_trash: bool) -> Result<(), MoveHostError> {
        let hosts = self
            .required_directory(OsStr::new("hosts"))
            .map_err(MoveHostError::Failure)?;
        let live = id.to_owned();
        let trash = format!(".trashed-{id}");
        let (source, destination) = if from_trash {
            (trash.as_str(), live.as_str())
        } else {
            (live.as_str(), trash.as_str())
        };
        require_regular_at(&hosts, OsStr::new(source)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                MoveHostError::Missing
            } else {
                MoveHostError::Failure(format!("refusing unsafe SSH host {id}: {error}"))
            }
        })?;
        rename_noreplace_at(&hosts, OsStr::new(source), OsStr::new(destination)).map_err(
            |error| match error.raw_os_error() {
                Some(libc::EEXIST) => MoveHostError::Exists,
                Some(libc::ENOENT) => MoveHostError::Missing,
                _ => MoveHostError::Failure(format!(
                    "moving SSH host {id} {} trash: {error}",
                    if from_trash { "from" } else { "to" }
                )),
            },
        )?;
        if let Err(error) = hosts.sync_all() {
            eprintln!(
                "cosmix-trayd: SSH host {id} moved successfully, but syncing the hosts directory failed: {error}"
            );
        }
        Ok(())
    }

    fn purge_host(&self, id: &str) -> Result<(), String> {
        let hosts = self.required_directory(OsStr::new("hosts"))?;
        let name = format!(".trashed-{id}");
        require_regular_at(&hosts, OsStr::new(&name))
            .map_err(|error| format!("refusing unsafe trashed SSH host {id}: {error}"))?;
        unlink_at(&hosts, OsStr::new(&name))
            .map_err(|error| format!("purging trashed SSH host {id}: {error}"))?;
        if let Err(error) = hosts.sync_all() {
            eprintln!(
                "cosmix-trayd: SSH host {id} was purged, but syncing the hosts directory failed: {error}"
            );
        }
        Ok(())
    }

    fn scan(
        &self,
        runner: &Arc<dyn SshRunner>,
        cache: &Mutex<HashMap<FingerprintCacheKey, Result<String, String>>>,
    ) -> ScanResult {
        let root = match self.open_root() {
            Ok(Some(root)) => root,
            Ok(None) => return ScanResult::Absent,
            Err(error) => {
                return ScanResult::Present(ScanData {
                    config_included: false,
                    hosts: Vec::new(),
                    keys: Vec::new(),
                    errors: vec![error],
                });
            }
        };
        let mut errors = Vec::new();
        let config_included = match config_tree_includes_hosts(&root, &self.root) {
            Ok(included) => included,
            Err(error) => {
                errors.push(concise(&format!("cannot verify SSH Include: {error}")));
                false
            }
        };
        if !config_included {
            errors.push("~/.ssh/config does not Include ~/.ssh/hosts/*".into());
        }

        let hosts = match open_child_directory(&root, OsStr::new("hosts")) {
            Ok(Some(hosts)) => scan_hosts(&hosts, &mut errors),
            Ok(None) => Vec::new(),
            Err(error) => {
                errors.push(error);
                Vec::new()
            }
        };
        let keys = match open_child_directory(&root, OsStr::new("keys")) {
            Ok(Some(keys)) => scan_keys(&keys, runner, cache, &mut errors),
            Ok(None) => Vec::new(),
            Err(error) => {
                errors.push(error);
                Vec::new()
            }
        };
        ScanResult::Present(ScanData {
            config_included,
            hosts,
            keys,
            errors,
        })
    }
}

fn scan_hosts(directory: &File, errors: &mut Vec<String>) -> Vec<HostEntry> {
    let names = match directory_names(directory, errors) {
        Ok(names) => names,
        Err(error) => {
            errors.push(error);
            return Vec::new();
        }
    };
    let mut candidates = Vec::new();
    for name in names {
        let Some(name_text) = name.to_str() else {
            errors.push("ignored non-UTF-8 SSH host filename".into());
            continue;
        };
        let (id, trashed) = if let Some(id) = name_text.strip_prefix(".trashed-") {
            (id.to_owned(), true)
        } else if name_text.starts_with('.') {
            continue;
        } else {
            (name_text.to_owned(), false)
        };
        candidates.push((name, id, trashed));
    }
    candidates.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.1.cmp(&right.1)));
    if candidates.len() > HOST_LIMIT {
        errors.push(format!(
            "SSH host catalogue truncated to the first {HOST_LIMIT} sorted entries"
        ));
        candidates.truncate(HOST_LIMIT);
    }
    candidates
        .into_iter()
        .map(|(name, id, trashed)| read_host_entry(directory, &name, id, trashed))
        .collect()
}

fn read_host_entry(directory: &File, name: &OsStr, id: String, trashed: bool) -> HostEntry {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    if let Err(error) = canonical_id(&id) {
        problems.push(error);
    }
    let (bytes, mode, source_tag) = match read_bounded_at(directory, name, HOST_FILE_LIMIT) {
        Ok(value) => value,
        Err(error) => {
            return HostEntry {
                id,
                host_error: concise(&error),
                host_warning: String::new(),
                hostname: String::new(),
                port: 0,
                user: String::new(),
                identity: String::new(),
                trashed,
                probe_status: "unknown".into(),
                probe_error: String::new(),
                probe_ms: 0,
                probe_checked_at: 0,
                content_tag: 0,
                source_tag: 0,
            };
        }
    };
    if mode != 0o600 {
        warnings.push(format!("host file mode is {mode:04o}; 0600 recommended"));
    }
    let content_tag = content_tag(&bytes);
    let parsed = match String::from_utf8(bytes) {
        Ok(text) => parse_host(&id, &text),
        Err(_) => Err("host file is not UTF-8".into()),
    };
    let (hostname, port, user, identity) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            problems.push(error);
            (String::new(), 0, String::new(), String::new())
        }
    };
    HostEntry {
        id,
        host_error: concise(&problems.join("; ")),
        host_warning: concise(&warnings.join("; ")),
        hostname,
        port,
        user,
        identity,
        trashed,
        probe_status: "unknown".into(),
        probe_error: String::new(),
        probe_ms: 0,
        probe_checked_at: 0,
        content_tag,
        source_tag,
    }
}

fn parse_host(id: &str, text: &str) -> Result<(String, u16, String, String), String> {
    let mut aliases = Vec::new();
    let mut hostname = None;
    let mut port = None;
    let mut user = None;
    let mut identity = None;
    for (line_number, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = split_directive(line);
        let keyword = keyword.to_ascii_lowercase();
        if !matches!(
            keyword.as_str(),
            "host" | "hostname" | "port" | "user" | "identityfile"
        ) {
            continue;
        }
        let value = clean_field(value).map_err(|error| {
            format!("line {} {keyword}: {error}", line_number.saturating_add(1))
        })?;
        match keyword.as_str() {
            "host" => aliases.push(value),
            "hostname" => set_once(&mut hostname, value, "Hostname")?,
            "port" => {
                let parsed = value
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| "Port must be between 1 and 65535".to_owned())?;
                if port.replace(parsed).is_some() {
                    return Err("multiple Port directives".into());
                }
            }
            "user" => set_once(&mut user, value, "User")?,
            "identityfile" => set_once(&mut identity, value, "IdentityFile")?,
            _ => unreachable!(),
        }
    }
    if aliases.len() != 1 {
        return Err("host file must contain exactly one Host directive".into());
    }
    let alias_parts = aliases[0].split_whitespace().collect::<Vec<_>>();
    if alias_parts.len() != 1 || alias_parts[0] != id {
        return Err(format!("Host alias must be exactly {id:?}"));
    }
    Ok((
        hostname.unwrap_or_else(|| id.to_owned()),
        port.unwrap_or(22),
        user.unwrap_or_default(),
        identity.unwrap_or_default(),
    ))
}

fn split_directive(line: &str) -> (&str, &str) {
    let split = line
        .char_indices()
        .find(|(_, character)| character.is_whitespace() || *character == '=');
    match split {
        Some((index, _)) => (
            &line[..index],
            line[index..]
                .trim_start_matches(|character: char| character.is_whitespace() || character == '=')
                .trim(),
        ),
        None => (line, ""),
    }
}

fn clean_field(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("value is empty".into());
    }
    if value.starts_with('-') {
        return Err("value must not start with '-'".into());
    }
    if value.chars().any(|character| {
        character.is_control() || character.is_whitespace() || matches!(character, '#' | '"' | '=')
    }) {
        return Err("value contains a control, comment, quote, or separator character".into());
    }
    Ok(value.to_owned())
}

fn set_once(slot: &mut Option<String>, value: String, label: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        Err(format!("multiple {label} directives"))
    } else {
        Ok(())
    }
}

fn scan_keys(
    directory: &File,
    runner: &Arc<dyn SshRunner>,
    cache: &Mutex<HashMap<FingerprintCacheKey, Result<String, String>>>,
    errors: &mut Vec<String>,
) -> Vec<KeyEntry> {
    let names = match directory_names(directory, errors) {
        Ok(names) => names,
        Err(error) => {
            errors.push(error);
            return Vec::new();
        }
    };
    let mut keys = Vec::new();
    let mut live_cache_keys = BTreeSet::new();
    for name in names {
        let Some(name_text) = name.to_str() else {
            errors.push("ignored non-UTF-8 SSH key filename".into());
            continue;
        };
        let Some(id) = name_text.strip_suffix(".pub") else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        if keys.len() >= KEY_LIMIT {
            errors.push(format!(
                "SSH key catalogue exceeds the {KEY_LIMIT} entry limit"
            ));
            break;
        }
        let mut key_error = canonical_id(id).err().unwrap_or_default();
        let (fingerprint, cache_key) = match read_key(directory, &name, runner, cache) {
            Ok((cache_key, Ok(fingerprint))) => (fingerprint, Some(cache_key)),
            Ok((cache_key, Err(error))) => {
                key_error = concise(
                    &[key_error.as_str(), error.as_str()]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("; "),
                );
                (String::new(), Some(cache_key))
            }
            Err(error) => {
                key_error = concise(
                    &[key_error.as_str(), error.as_str()]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("; "),
                );
                (String::new(), None)
            }
        };
        if let Some(cache_key) = cache_key {
            live_cache_keys.insert(cache_key);
        }
        keys.push(KeyEntry {
            id: id.to_owned(),
            fingerprint,
            key_error,
        });
    }
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|key, _| live_cache_keys.contains(key));
    keys.sort_by(|left, right| left.id.cmp(&right.id));
    keys
}

fn read_key(
    directory: &File,
    name: &OsStr,
    runner: &Arc<dyn SshRunner>,
    cache: &Mutex<HashMap<FingerprintCacheKey, Result<String, String>>>,
) -> Result<(FingerprintCacheKey, Result<String, String>), String> {
    let file = secure_open_at(directory.as_raw_fd(), name, libc::O_RDONLY, 0)
        .map_err(|error| format!("refusing unsafe key file: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting key file: {error}"))?;
    if !metadata.is_file() {
        return Err("refusing non-regular key file".into());
    }
    let cache_key = FingerprintCacheKey {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mtime_ns: i128::from(metadata.mtime())
            .saturating_mul(1_000_000_000)
            .saturating_add(i128::from(metadata.mtime_nsec())),
        size: metadata.size(),
    };
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .cloned()
    {
        return Ok((cache_key, cached));
    }
    let bytes = read_bounded_file(file, &metadata, KEY_FILE_LIMIT)?;
    let result = runner
        .fingerprint(&bytes)
        .map(|fingerprint| concise(&fingerprint));
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key.clone(), result.clone());
    Ok((cache_key, result))
}

fn directory_names(
    directory: &File,
    errors: &mut Vec<String>,
) -> Result<Vec<std::ffi::OsString>, String> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut names = Vec::new();
    for entry in
        fs::read_dir(&path).map_err(|error| format!("reading SSH catalogue directory: {error}"))?
    {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(error) => errors.push(concise(&format!(
                "reading SSH catalogue directory entry: {error}"
            ))),
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

fn open_child_directory(root: &File, name: &OsStr) -> Result<Option<File>, String> {
    match secure_open_at(
        root.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    ) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "refusing unsafe SSH directory {}: {error}",
            name.to_string_lossy()
        )),
    }
}

fn read_bounded_at(
    directory: &File,
    name: &OsStr,
    limit: usize,
) -> Result<(Vec<u8>, u32, u64), String> {
    let file = secure_open_at(directory.as_raw_fd(), name, libc::O_RDONLY, 0)
        .map_err(|error| format!("refusing unsafe host file: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting host file: {error}"))?;
    let mode = metadata.permissions().mode() & 0o777;
    let bytes = read_bounded_file(file, &metadata, limit)?;
    let source_tag = file_identity_tag(&metadata, &bytes);
    Ok((bytes, mode, source_tag))
}

fn read_bounded_file(
    mut file: File,
    metadata: &fs::Metadata,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if !metadata.is_file() {
        return Err("refusing non-regular file".into());
    }
    if metadata.len() > limit as u64 {
        return Err(format!("file exceeds the {limit} byte limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading file: {error}"))?;
    if bytes.len() > limit {
        Err(format!("file exceeds the {limit} byte limit"))
    } else {
        Ok(bytes)
    }
}

fn config_tree_includes_hosts(root: &File, root_path: &Path) -> Result<bool, String> {
    let Some(config) = read_config_text(root, Path::new("config"))? else {
        return Ok(false);
    };
    let includes = top_level_includes(&config);
    if includes
        .iter()
        .any(|candidate| include_is_hosts_glob(candidate, root_path))
    {
        return Ok(true);
    }

    let mut checked = 0_usize;
    for candidate in includes {
        let Some(relative) = include_relative_path(&candidate, root_path) else {
            return Err(format!(
                "Include {candidate:?} is outside the trusted ~/.ssh tree"
            ));
        };
        if relative.file_name() != Some(OsStr::new("*")) {
            continue;
        }
        let Some(parent) = relative.parent() else {
            continue;
        };
        let Some(directory) = open_child_directory(root, parent.as_os_str())? else {
            continue;
        };
        let mut iteration_errors = Vec::new();
        let names = directory_names(&directory, &mut iteration_errors)?;
        if !iteration_errors.is_empty() {
            return Err(iteration_errors.join("; "));
        }
        for name in names {
            if name.as_bytes().starts_with(b".") {
                continue;
            }
            checked = checked.saturating_add(1);
            if checked > CONFIG_INCLUDE_LIMIT {
                return Err(format!(
                    "SSH Include preflight exceeds the {CONFIG_INCLUDE_LIMIT} file limit"
                ));
            }
            let child_path = parent.join(&name);
            let child = read_config_text(root, &child_path)?.ok_or_else(|| {
                format!("included SSH config {} disappeared", child_path.display())
            })?;
            if top_level_includes(&child)
                .iter()
                .any(|nested| include_is_hosts_glob(nested, root_path))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn read_config_text(root: &File, relative: &Path) -> Result<Option<String>, String> {
    let file = match secure_open_at(root.as_raw_fd(), relative.as_os_str(), libc::O_RDONLY, 0) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "refusing unsafe SSH config {}: {error}",
                relative.display()
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting SSH config {}: {error}", relative.display()))?;
    let bytes = read_bounded_file(file, &metadata, CONFIG_FILE_LIMIT)
        .map_err(|error| format!("reading SSH config {}: {error}", relative.display()))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("SSH config {} is not UTF-8", relative.display()))
}

fn top_level_includes(config: &str) -> Vec<String> {
    let mut top_level = true;
    let mut includes = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = split_directive(line);
        if keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match") {
            top_level = false;
            continue;
        }
        if top_level && keyword.eq_ignore_ascii_case("include") {
            if let Some(arguments) = include_arguments(value) {
                includes.extend(arguments);
            }
        }
    }
    includes
}

fn include_arguments(value: &str) -> Option<Vec<String>> {
    let mut characters = value.chars().peekable();
    let mut arguments = Vec::new();
    loop {
        while characters
            .next_if(|character| character.is_whitespace())
            .is_some()
        {}
        let Some(first) = characters.peek().copied() else {
            break;
        };
        if first == '#' {
            break;
        }

        let mut argument = String::new();
        let mut quoted = false;
        while let Some(character) = characters.peek().copied() {
            if !quoted && character.is_whitespace() {
                break;
            }
            characters.next();
            if character == '"' {
                quoted = !quoted;
            } else {
                argument.push(character);
            }
        }
        if quoted {
            return None;
        }
        arguments.push(argument);
    }
    Some(arguments)
}

fn include_is_hosts_glob(candidate: &str, root: &Path) -> bool {
    include_relative_path(candidate, root).is_some_and(|relative| relative == Path::new("hosts/*"))
}

fn include_relative_path(candidate: &str, root: &Path) -> Option<PathBuf> {
    let path = if let Some(relative) = candidate.strip_prefix("~/.ssh/") {
        PathBuf::from(relative)
    } else {
        let path = Path::new(candidate);
        if path.is_absolute() {
            path.strip_prefix(root).ok()?.to_path_buf()
        } else if candidate.starts_with('~') {
            return None;
        } else {
            path.to_path_buf()
        }
    };
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path)
}

fn watch_catalogue(weak: Weak<SshController>, generation: u64) {
    let Some(controller) = weak.upgrade() else {
        return;
    };
    controller.set_watcher_ready(generation, false);
    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(error) => {
            controller.watcher_failed(generation, format!("cannot initialise inotify: {error}"));
            return;
        }
    };
    let mut buffer = [0_u8; 16 * 1024];
    match stabilise_watches(&controller, &mut inotify, &mut buffer) {
        Ok(true) => controller.set_watcher_ready(generation, true),
        Ok(false) => {
            controller.watcher_absent(generation);
            return;
        }
        Err(error) => {
            controller.watcher_failed(generation, error);
            return;
        }
    }
    drop(controller);

    loop {
        if let Err(error) = inotify.read_events_blocking(&mut buffer) {
            if let Some(controller) = weak.upgrade() {
                controller
                    .watcher_failed(generation, format!("SSH catalogue watch failed: {error}"));
            }
            return;
        }
        let Some(controller) = weak.upgrade() else {
            return;
        };
        controller.set_watcher_ready(generation, false);
        match stabilise_watches(&controller, &mut inotify, &mut buffer) {
            Ok(true) => controller.set_watcher_ready(generation, true),
            Ok(false) => {
                controller.watcher_absent(generation);
                return;
            }
            Err(error) => {
                controller.watcher_failed(generation, error);
                return;
            }
        }
    }
}

fn stabilise_watches(
    controller: &SshController,
    inotify: &mut Inotify,
    buffer: &mut [u8],
) -> Result<bool, String> {
    stabilise_watches_with_hook(controller, inotify, buffer, || {})
}

fn stabilise_watches_with_hook(
    controller: &SshController,
    inotify: &mut Inotify,
    buffer: &mut [u8],
    mut after_scan: impl FnMut(),
) -> Result<bool, String> {
    let Some(root) = controller.store.open_root()? else {
        return Ok(false);
    };
    let mask = WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::CLOSE_WRITE
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF;
    add_fd_watch(inotify, &root, mask)?;
    loop {
        for name in [OsStr::new("hosts"), OsStr::new("keys")] {
            if let Some(directory) = open_child_directory(&root, name)? {
                add_fd_watch(inotify, &directory, mask)?;
            }
        }
        controller.clear_watch_error();
        controller.rescan();
        after_scan();
        let mut drained = 0;
        loop {
            match inotify.read_events(buffer) {
                Ok(events) => {
                    let count = events.count();
                    if count == 0 {
                        break;
                    }
                    drained += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(format!("SSH catalogue watch drain failed: {error}")),
            }
        }
        if drained == 0 {
            return Ok(true);
        }
    }
}

fn add_fd_watch(inotify: &mut Inotify, file: &File, mask: WatchMask) -> Result<(), String> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    inotify
        .watches()
        .add(&path, mask)
        .map(|_| ())
        .map_err(|error| format!("cannot watch SSH catalogue: {error}"))
}

fn canonical_id(id: &str) -> Result<String, String> {
    let bytes = id.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || bytes[1..]
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-'))
        || id.contains("..")
    {
        return Err(format!(
            "invalid SSH identity {id:?}: expected 1-64 characters, an alphanumeric first character, only A-Z/a-z/0-9/./_/- thereafter, and no '..'"
        ));
    }
    Ok(id.to_owned())
}

fn content_tag(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn file_identity_tag(metadata: &fs::Metadata, bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    metadata.dev().hash(&mut hasher);
    metadata.ino().hash(&mut hasher);
    metadata.mtime().hash(&mut hasher);
    metadata.mtime_nsec().hash(&mut hasher);
    metadata.size().hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn probe_failure(status: &std::process::ExitStatus, stderr: &[u8]) -> String {
    // timeout(1) reports an expiry as exit 124, or 128+9/raw SIGKILL when
    // --signal=KILL propagates the kill to timeout itself (the live-probed
    // shape on this platform is a raw signal-9 wait status, code() == None).
    use std::os::unix::process::ExitStatusExt;
    if matches!(status.code(), Some(124 | 137)) || status.signal() == Some(9) {
        return "SSH probe timed out after 10 seconds".into();
    }
    let stderr = String::from_utf8_lossy(stderr);
    if let Some(line) = stderr.lines().rev().find(|line| !line.trim().is_empty()) {
        return concise(line);
    }
    format!("ssh exited with {status}")
}

fn probe_exit_is_ok(status: &std::process::ExitStatus, stderr: &[u8]) -> bool {
    // ssh forwards the remote command's exit code: GitHub's authenticated
    // rejection is 1, while client/auth failures are 255 and timeout may be a
    // signal or 124/137. The stderr denylist below is belt-and-braces, not the
    // authentication proof.
    status.success() || (status.code() == Some(1) && forge_authentication_succeeded(stderr))
}

fn forge_authentication_succeeded(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    const CLIENT_FAILURES: &[&str] = &[
        "Permission denied",
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "Could not resolve hostname",
        "Connection timed out",
        "Connection refused",
        "No route to host",
        "kex_exchange_identification:",
        "Connection closed by",
        "Connection reset by",
    ];
    if CLIENT_FAILURES
        .iter()
        .any(|failure| stderr.contains(failure))
    {
        return false;
    }

    // Live-probed against github.com: its forced-command handler rejects `true`
    // only after authentication and emits this three-part git-proxy diagnostic.
    stderr.contains("Invalid command:")
        && stderr.contains("You appear to be using ssh to clone a git:// URL.")
        && stderr.contains("GIT_PROXY_COMMAND environment variable are NOT set.")
}

fn command_failure(label: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("{label} exited with {}", output.status)
    } else {
        concise(&format!("{label} exited with {}: {detail}", output.status))
    }
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 180;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

fn resolve_trusted_executable(program: &str) -> Result<PathBuf, String> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .into_iter()
        .map(|directory| Path::new(directory).join(program))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            format!(
                "cannot find executable {program} in trusted directories /usr/bin, /bin, /usr/local/bin"
            )
        })
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn secure_open(path: &Path, flags: i32, mode: u32) -> std::io::Result<File> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not absolute",
        ));
    }
    let root = File::open("/")?;
    let relative = path.strip_prefix("/").expect("absolute path has root");
    if relative.as_os_str().is_empty() {
        return Ok(root);
    }
    secure_open_at(root.as_raw_fd(), relative.as_os_str(), flags, mode)
}

fn secure_open_at(dirfd: RawFd, path: &OsStr, flags: i32, mode: u32) -> std::io::Result<File> {
    let path = CString::new(path.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: path is NUL-terminated, how has Linux open_how layout, and a
    // successful syscall returns a newly-owned descriptor.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful openat2 result is exclusively owned here.
    Ok(unsafe { File::from_raw_fd(fd as RawFd) })
}

fn fchmod(file: &File, mode: u32) -> std::io::Result<()> {
    // SAFETY: file owns a valid descriptor for the duration of the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn require_regular_at(directory: &File, name: &OsStr) -> std::io::Result<()> {
    let file = secure_open_at(directory.as_raw_fd(), name, libc::O_RDONLY, 0)?;
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "entry is not a regular file",
        ))
    }
}

fn rename_noreplace_at(
    directory: &File,
    source: &OsStr,
    destination: &OsStr,
) -> std::io::Result<()> {
    let source = CString::new(source.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source contains NUL")
    })?;
    let destination = CString::new(destination.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination contains NUL")
    })?;
    // SAFETY: both names are NUL-terminated, the directory descriptor remains
    // live for the call, and RENAME_NOREPLACE prevents clobbering.
    let result = unsafe {
        libc::renameat2(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlink_at(directory: &File, name: &OsStr) -> std::io::Result<()> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains NUL"))?;
    // SAFETY: name is NUL-terminated and the directory descriptor remains
    // live for the unlinkat call.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn secure_directory_error(path: &Path, error: std::io::Error) -> String {
    if error
        .raw_os_error()
        .is_some_and(|code| matches!(code, libc::ELOOP | libc::EXDEV | libc::ENOTDIR))
    {
        format!("refusing unsafe directory {}", path.display())
    } else {
        format!("inspecting {}: {error}", path.display())
    }
}

#[cfg(test)]
#[derive(Default)]
struct FakeProber {
    fingerprints: Mutex<usize>,
    probes: Mutex<Vec<String>>,
}

#[cfg(test)]
impl SshRunner for FakeProber {
    fn fingerprint(&self, public_key: &[u8]) -> Result<String, String> {
        *self.fingerprints.lock().expect("fingerprint calls") += 1;
        if public_key.starts_with(b"bad") {
            Err("invalid public key".into())
        } else {
            Ok("256 SHA256:fake test (ED25519)".into())
        }
    }

    fn probe(&self, _ssh: &Path, id: &str) -> ProbeResult {
        self.probes.lock().expect("probe calls").push(id.into());
        ProbeResult {
            ok: id != "fails",
            error: if id == "fails" {
                "name resolution failed"
            } else {
                ""
            }
            .into(),
            elapsed_ms: 7,
            checked_at: 42,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::process::ExitStatusExt;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join(".ssh");
        fs::create_dir_all(root.join("hosts")).expect("hosts");
        fs::create_dir_all(root.join("keys")).expect("keys");
        fs::write(root.join("config"), "Include ~/.ssh/hosts/*\n").expect("config");
        (temp, root)
    }

    fn write_host(root: &Path, id: &str, text: &str, mode: u32) {
        let path = root.join("hosts").join(id);
        fs::write(&path, text).expect("host file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("host mode");
    }

    fn write_key(root: &Path, id: &str) {
        fs::write(
            root.join("keys").join(format!("{id}.pub")),
            "ssh-ed25519 AAAA test\n",
        )
        .expect("public key");
    }

    fn valid_host(id: &str) -> String {
        format!(
            "Host {id}\n  Hostname {id}.example.com\n  Port 22\n  User root\n  IdentityFile ~/.ssh/keys/main\n  ForwardAgent yes\n"
        )
    }

    #[test]
    fn safe_stem_matches_mix_rules() {
        for valid in ["a", "alpha-1", "a_b.c"] {
            assert_eq!(canonical_id(valid).expect("valid"), valid);
        }
        for invalid in ["", ".dot", "-flag", "a..b", "a/b", "white space"] {
            assert!(canonical_id(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn config_include_preflight_accepts_only_the_effective_hosts_glob() {
        let root = Path::new("/home/test/.ssh");
        for accepted in ["~/.ssh/hosts/*", "/home/test/.ssh/hosts/*", "hosts/*"] {
            assert!(include_is_hosts_glob(accepted, root), "{accepted}");
        }
        for rejected in ["~/.ssh/config.d/*", "hosts/*.conf", "../hosts/*"] {
            assert!(!include_is_hosts_glob(rejected, root), "{rejected}");
        }
        assert!(top_level_includes("Include hosts/*").contains(&"hosts/*".into()));
        assert!(top_level_includes("Include=hosts/*").contains(&"hosts/*".into()));
        assert!(top_level_includes("Host *\n  Include hosts/*").is_empty());
        assert!(top_level_includes("Match all\n  Include hosts/*").is_empty());
        assert!(top_level_includes("# Include hosts/*").is_empty());
    }

    #[test]
    fn include_arguments_follow_openssh_quote_and_comment_rules() {
        for (config, expected) in [
            ("Include # disabled hosts/*", Vec::<String>::new()),
            ("Include #disabled hosts/*", Vec::new()),
            ("Include real/* #trailing", vec!["real/*".into()]),
            ("Include \"quoted/path/*\"", vec!["quoted/path/*".into()]),
            (
                "Include \"quoted path/*\" hosts/*",
                vec!["quoted path/*".into(), "hosts/*".into()],
            ),
            ("Include 'hosts/*'", vec!["'hosts/*'".into()]),
            ("Include hosts/* \"unterminated", Vec::new()),
        ] {
            assert_eq!(top_level_includes(config), expected, "{config:?}");
        }
        assert!(!include_is_hosts_glob(
            "'hosts/*'",
            Path::new("/home/test/.ssh")
        ));
    }

    #[test]
    fn config_include_preflight_follows_one_bounded_secure_level() {
        let (_temp, root) = fixture();
        let store = SshStore::new(root.clone());

        fs::write(root.join("config"), "Include hosts/*\n").expect("relative Include");
        assert!(store.config_included().expect("relative preflight"));

        fs::write(root.join("config"), "Host *\n  Include hosts/*\n").expect("scoped Include");
        assert!(!store.config_included().expect("scoped preflight"));

        fs::write(root.join("config"), "Include hosts/*.conf\n").expect("wrong glob");
        assert!(!store.config_included().expect("wrong-glob preflight"));

        fs::create_dir(root.join("conf.d")).expect("conf.d");
        fs::write(root.join("config"), "Include conf.d/*\n").expect("indirect Include");
        fs::write(root.join("conf.d/hosts.conf"), "Include ~/.ssh/hosts/*\n")
            .expect("indirect child");
        assert!(store.config_included().expect("indirect preflight"));

        fs::write(
            root.join("conf.d/hosts.conf"),
            "Match all\n  Include ~/.ssh/hosts/*\n",
        )
        .expect("scoped indirect child");
        assert!(!store.config_included().expect("scoped indirect preflight"));

        fs::remove_dir_all(root.join("conf.d")).expect("replace conf.d");
        fs::create_dir(root.join("conf.d")).expect("bounded conf.d");
        for index in 0..=CONFIG_INCLUDE_LIMIT {
            fs::write(
                root.join(format!("conf.d/{index:02}")),
                "# no hosts Include\n",
            )
            .expect("bounded child");
        }
        assert!(store
            .config_included()
            .expect_err("too many indirect files")
            .contains("16 file limit"));

        fs::remove_dir_all(root.join("conf.d")).expect("replace bounded conf.d");
        fs::create_dir(root.join("conf.d")).expect("oversized conf.d");
        fs::write(
            root.join("conf.d/oversized"),
            vec![b'x'; CONFIG_FILE_LIMIT + 1],
        )
        .expect("oversized child");
        assert!(store
            .config_included()
            .expect_err("oversized indirect file")
            .contains("65536 byte limit"));
    }

    #[test]
    fn hostile_host_files_remain_visible_with_entry_errors() {
        let (_temp, root) = fixture();
        let cases = [
            ("alias", "Host other\nHostname other\n"),
            ("pattern", "Host pattern*\nHostname pattern\n"),
            ("multi", "Host multi\nHost other\n"),
            (
                "newline",
                "Host newline\nHostname first.example.com\nHostname injected.example.com\n",
            ),
            ("hash", "Host hash\nHostname #bad\n"),
            ("quote", "Host quote\nUser \"root\"\n"),
            ("equals", "Host equals\nHostname bad=name\n"),
            ("empty", "Host empty\nHostname\n"),
            ("-prefix", "Host -prefix\nHostname example.com\n"),
        ];
        for (id, text) in cases {
            write_host(&root, id, text, 0o600);
        }
        write_host(&root, "mode644", &valid_host("mode644"), 0o644);
        write_host(&root, ".hidden", "Host hidden\n", 0o600);
        write_host(&root, ".trashed-old", &valid_host("old"), 0o600);
        let runner: Arc<dyn SshRunner> = Arc::new(FakeProber::default());
        let store = SshStore::new(root);
        let cache = Mutex::new(HashMap::new());
        let ScanResult::Present(scan) = store.scan(&runner, &cache) else {
            panic!("present catalogue");
        };
        assert_eq!(scan.hosts.len(), cases.len() + 2);
        assert!(scan
            .hosts
            .iter()
            .all(|host| { host.trashed || host.id == "mode644" || !host.host_error.is_empty() }));
        let mode644 = scan
            .hosts
            .iter()
            .find(|host| host.id == "mode644")
            .expect("mode-644 host");
        assert!(mode644.host_error.is_empty());
        assert!(mode644.host_warning.contains("0644"));
        assert!(mode644.actionable());
        assert!(scan
            .hosts
            .iter()
            .any(|host| host.id == "old" && host.trashed));
        assert!(!scan.hosts.iter().any(|host| host.id == ".hidden"));
    }

    #[test]
    fn oversized_and_non_utf8_hosts_do_not_hide_valid_entries() {
        let (_temp, root) = fixture();
        write_host(&root, "good", &valid_host("good"), 0o600);
        fs::write(root.join("hosts/huge"), vec![b'x'; HOST_FILE_LIMIT + 1]).expect("huge");
        fs::write(root.join("hosts/binary"), [0xff, 0xfe]).expect("binary");
        let runner: Arc<dyn SshRunner> = Arc::new(FakeProber::default());
        let ScanResult::Present(scan) =
            SshStore::new(root).scan(&runner, &Mutex::new(HashMap::new()))
        else {
            panic!("present catalogue");
        };
        assert!(scan
            .hosts
            .iter()
            .find(|host| host.id == "good")
            .unwrap()
            .actionable());
        assert!(!scan
            .hosts
            .iter()
            .find(|host| host.id == "huge")
            .unwrap()
            .host_error
            .is_empty());
        assert!(!scan
            .hosts
            .iter()
            .find(|host| host.id == "binary")
            .unwrap()
            .host_error
            .is_empty());
    }

    #[test]
    fn host_catalogue_truncates_sorted_at_cap_and_create_refuses() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        for index in 0..=HOST_LIMIT {
            let id = format!("host{index:03}");
            write_host(&root, &id, &valid_host(&id), 0o600);
        }
        let controller = SshController::new_test(root);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.hosts.len(), HOST_LIMIT);
        assert_eq!(snapshot.hosts.first().expect("first host").0, "host000");
        assert_eq!(snapshot.hosts.last().expect("last host").0, "host255");
        assert!(snapshot.error.contains("truncated"));
        assert_eq!(snapshot.state, "degraded");
        assert!(matches!(
            controller.create("another", "host.example", 22, "root", "main"),
            Err(SshError::SshStoreFailure(error)) if error.contains("256 entry cap")
        ));
    }

    #[test]
    fn symlinked_files_and_directories_are_refused() {
        let (_temp, root) = fixture();
        symlink("/etc/passwd", root.join("hosts/escape")).expect("file symlink");
        let runner: Arc<dyn SshRunner> = Arc::new(FakeProber::default());
        let ScanResult::Present(scan) =
            SshStore::new(root.clone()).scan(&runner, &Mutex::new(HashMap::new()))
        else {
            panic!("present catalogue");
        };
        assert!(!scan.hosts[0].host_error.is_empty());
        fs::remove_dir_all(root.join("keys")).expect("keys remove");
        symlink("/tmp", root.join("keys")).expect("dir symlink");
        let ScanResult::Present(scan) =
            SshStore::new(root).scan(&runner, &Mutex::new(HashMap::new()))
        else {
            panic!("present catalogue");
        };
        assert!(scan
            .errors
            .iter()
            .any(|error| error.contains("unsafe SSH directory")));
    }

    #[test]
    fn fingerprints_are_cached_by_file_identity_and_metadata() {
        let (_temp, root) = fixture();
        fs::write(root.join("keys/main.pub"), "ssh-ed25519 AAAA test\n").expect("key");
        let fake = Arc::new(FakeProber::default());
        let runner: Arc<dyn SshRunner> = fake.clone();
        let cache = Mutex::new(HashMap::new());
        let store = SshStore::new(root.clone());
        let _ = store.scan(&runner, &cache);
        let _ = store.scan(&runner, &cache);
        assert_eq!(*fake.fingerprints.lock().expect("calls"), 1);
        fs::write(root.join("keys/main.pub"), "ssh-ed25519 BBBB changed\n").expect("change key");
        let _ = store.scan(&runner, &cache);
        assert_eq!(*fake.fingerprints.lock().expect("calls"), 2);
    }

    #[test]
    fn fingerprint_failures_are_cached_and_oversized_keys_remain_visible() {
        let (_temp, root) = fixture();
        fs::write(root.join("keys/bad.pub"), "bad key\n").expect("bad key");
        fs::write(root.join("keys/huge.pub"), vec![b'x'; KEY_FILE_LIMIT + 1]).expect("huge key");
        let fake = Arc::new(FakeProber::default());
        let runner: Arc<dyn SshRunner> = fake.clone();
        let cache = Mutex::new(HashMap::new());
        let store = SshStore::new(root);
        let ScanResult::Present(first) = store.scan(&runner, &cache) else {
            panic!("present catalogue");
        };
        let ScanResult::Present(second) = store.scan(&runner, &cache) else {
            panic!("present catalogue");
        };
        assert_eq!(*fake.fingerprints.lock().expect("calls"), 1);
        for scan in [first, second] {
            assert_eq!(scan.keys.len(), 2);
            assert!(scan.keys.iter().all(|key| !key.key_error.is_empty()));
        }
    }

    #[test]
    fn connect_argv_contains_the_absolute_authority_resolved_ssh_path() {
        let (_temp, root) = fixture();
        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        let controller = SshController::new_test(root);
        assert_eq!(
            controller.connect_argv("alpha").expect("connect argv"),
            ["konsole", "-e", "/usr/bin/ssh", "alpha"]
        );
    }

    #[test]
    fn helper_resolution_is_confined_to_trusted_system_directories() {
        for program in ["ssh", "timeout", "ssh-keygen"] {
            let Some(path) = resolve_trusted_executable(program).ok() else {
                eprintln!("SKIPPING trusted resolver assertion: {program} is absent");
                continue;
            };
            assert!(
                [
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                    Path::new("/usr/local/bin"),
                ]
                .contains(&path.parent().expect("executable parent")),
                "untrusted helper path: {}",
                path.display()
            );
        }
        assert!(resolve_trusted_executable("cosmix-definitely-not-a-real-helper").is_err());
    }

    #[test]
    fn probe_error_truncation_retains_the_tail() {
        let mut tail = Vec::new();
        retain_probe_error_tail(&mut tail, b"discarded head\n");
        retain_probe_error_tail(&mut tail, &vec![b'x'; PROBE_ERROR_LIMIT + 128]);
        retain_probe_error_tail(&mut tail, b"\nactionable tail\n");
        assert_eq!(tail.len(), PROBE_ERROR_LIMIT);
        assert!(!String::from_utf8_lossy(&tail).contains("discarded head"));
        assert!(String::from_utf8_lossy(&tail).ends_with("actionable tail\n"));
    }

    fn probe_exit(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    const GITHUB_COMMAND_REJECTION: &[u8] = b"Invalid command: true\n  You appear to be using ssh to clone a git:// URL.\n  Make sure your core.gitProxy config option and the\n  GIT_PROXY_COMMAND environment variable are NOT set.\n";

    #[test]
    fn github_invalid_command_auth_reply_is_probe_ok() {
        assert!(probe_exit_is_ok(&probe_exit(1), GITHUB_COMMAND_REJECTION));
    }

    #[test]
    fn ordinary_probe_exit_classification_stays_strict() {
        assert!(probe_exit_is_ok(&probe_exit(0), b""));
        assert!(!probe_exit_is_ok(&probe_exit(1), b""));
        assert!(!probe_exit_is_ok(
            &probe_exit(1),
            b"git@github.com: Permission denied (publickey).\n"
        ));
        let mut pre_auth_spoof = GITHUB_COMMAND_REJECTION.to_vec();
        pre_auth_spoof.extend_from_slice(b"git@github.com: Permission denied (publickey).\n");
        assert!(!probe_exit_is_ok(&probe_exit(1), &pre_auth_spoof));
        assert!(!probe_exit_is_ok(
            &std::process::ExitStatus::from_raw(9),
            GITHUB_COMMAND_REJECTION
        ));
        assert!(!probe_exit_is_ok(
            &probe_exit(255),
            GITHUB_COMMAND_REJECTION
        ));
        assert!(!probe_exit_is_ok(
            &probe_exit(124),
            GITHUB_COMMAND_REJECTION
        ));
        assert!(!probe_exit_is_ok(
            &probe_exit(137),
            GITHUB_COMMAND_REJECTION
        ));
    }

    #[test]
    fn probe_failure_labels_every_timeout_shape_as_timeout() {
        for status in [
            probe_exit(124),
            probe_exit(137),
            std::process::ExitStatus::from_raw(9),
        ] {
            assert_eq!(
                probe_failure(&status, GITHUB_COMMAND_REJECTION),
                "SSH probe timed out after 10 seconds"
            );
        }
        assert_eq!(
            probe_failure(
                &probe_exit(255),
                b"ssh: connect to host x: Connection refused\n"
            ),
            "ssh: connect to host x: Connection refused"
        );
    }

    #[test]
    fn refresh_retries_previously_missing_ssh_and_helpers() {
        let (Some(expected_ssh), Some(expected_timeout), Some(expected_ssh_keygen)) = (
            resolve_trusted_executable("ssh").ok(),
            resolve_trusted_executable("timeout").ok(),
            resolve_trusted_executable("ssh-keygen").ok(),
        ) else {
            eprintln!("SKIPPING resolver recovery test: a trusted SSH helper is absent");
            return;
        };
        let (_temp, root) = fixture();
        let runner = Arc::new(SystemSshRunner::new());
        let controller =
            SshController::new_with(root, runner.clone(), Ok(PathBuf::from("/missing/ssh")));
        *controller
            .ssh_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Err("missing ssh".into());
        *runner
            .timeout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Err("missing timeout".into());
        *runner
            .ssh_keygen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Err("missing ssh-keygen".into());
        controller.refresh();
        assert_eq!(
            controller.resolved_ssh().expect("recovered ssh"),
            expected_ssh
        );
        assert_eq!(
            cached_resolution(&runner.timeout).expect("recovered timeout"),
            expected_timeout
        );
        assert_eq!(
            cached_resolution(&runner.ssh_keygen).expect("recovered ssh-keygen"),
            expected_ssh_keygen
        );
    }

    #[test]
    fn connect_and_probe_revalidate_fragments_before_admission() {
        let (_temp, root) = fixture();
        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        let controller = SshController::new_test(root.clone());

        write_host(
            &root,
            "alpha",
            "Host other\nHostname other.example.com\n",
            0o600,
        );
        assert!(matches!(
            controller.connect_argv("alpha"),
            Err(SshError::SshHostNotActionable(_))
        ));

        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        controller.refresh();
        write_host(
            &root,
            "alpha",
            "Host other\nHostname other.example.com\n",
            0o600,
        );
        assert!(matches!(
            controller.probe(vec!["alpha".into()]),
            Err(SshError::SshHostNotActionable(_))
        ));

        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        controller.refresh();
        fs::write(root.join("config"), "Host *\n").expect("remove Include");
        assert!(matches!(
            controller.probe(vec!["alpha".into()]),
            Err(SshError::SshConfigNotIncluded(_))
        ));
    }

    #[test]
    fn probe_options_match_the_pinned_connect_predictive_argv() {
        assert_eq!(
            PROBE_OPTIONS,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "AddKeysToAgent=no",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "UpdateHostKeys=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "RequestTTY=no",
                "-o",
                "RemoteCommand=none",
                "-o",
                "LogLevel=ERROR",
            ]
        );
    }

    #[test]
    fn missing_target_directory_recovers_from_the_root_inotify_watch() {
        let (_temp, root) = fixture();
        fs::remove_dir(root.join("hosts")).expect("remove hosts");
        let controller = SshController::new_test(root.clone());
        fs::create_dir(root.join("hosts")).expect("recreate hosts");
        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !controller
                .snapshot()
                .hosts
                .iter()
                .any(|host| host.0 == "alpha")
        {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(controller
            .snapshot()
            .hosts
            .iter()
            .any(|host| host.0 == "alpha"));
    }

    #[test]
    fn child_created_during_stabilise_is_watched_for_later_mutations() {
        let (_temp, root) = fixture();
        fs::remove_dir(root.join("hosts")).expect("remove hosts");
        let controller = SshController::new_test(root.clone());
        let mut inotify = Inotify::init().expect("inotify");
        let mut buffer = [0_u8; 16 * 1024];
        let mut created = false;
        assert!(
            stabilise_watches_with_hook(&controller, &mut inotify, &mut buffer, || {
                if !created {
                    created = true;
                    fs::create_dir(root.join("hosts")).expect("create hosts during stabilise");
                    write_host(&root, "alpha", &valid_host("alpha"), 0o600);
                }
            },)
            .expect("stabilise watches")
        );

        write_host(
            &root,
            "alpha",
            "Host alpha\nHostname changed.example.com\n",
            0o600,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_child_event = false;
        while std::time::Instant::now() < deadline && !saw_child_event {
            match inotify.read_events(&mut buffer) {
                Ok(events) => saw_child_event = events.count() != 0,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read child watch event: {error}"),
            }
            if !saw_child_event {
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert!(saw_child_event, "child mutation was not watched");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !controller
                .snapshot()
                .hosts
                .iter()
                .any(|host| host.0 == "alpha" && host.3 == "changed.example.com")
        {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(controller
            .snapshot()
            .hosts
            .iter()
            .any(|host| { host.0 == "alpha" && host.3 == "changed.example.com" }));
    }

    #[test]
    fn refresh_rekicks_a_catalogue_whose_ssh_root_was_absent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join(".ssh");
        let controller = SshController::new_test(root.clone());
        assert_eq!(controller.status(), "absent");
        fs::create_dir_all(root.join("hosts")).expect("hosts");
        fs::create_dir(root.join("keys")).expect("keys");
        fs::write(root.join("config"), "Include ~/.ssh/hosts/*\n").expect("config");
        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        controller.refresh();
        assert!(controller
            .snapshot()
            .hosts
            .iter()
            .any(|host| host.0 == "alpha"));
    }

    #[test]
    fn create_writes_the_exact_private_sshm_format_and_defaults() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        let controller = SshController::new_test(root.clone());
        controller
            .create("alpha", "alpha.example.com", 0, "", "main")
            .expect("create host");
        let path = root.join("hosts/alpha");
        assert_eq!(
            fs::read_to_string(&path).expect("host text"),
            "Host alpha\n  Hostname alpha.example.com\n  Port 22\n  User root\n  IdentityFile ~/.ssh/keys/main\n"
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("host metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let host = controller
            .snapshot()
            .hosts
            .into_iter()
            .find(|host| host.0 == "alpha")
            .expect("catalogued host");
        assert_eq!(host.3, "alpha.example.com");
        assert_eq!(host.4, 22);
        assert_eq!(host.5, "root");
        assert_eq!(host.6, "~/.ssh/keys/main");
    }

    #[test]
    fn create_rejects_the_hostile_field_table() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        let controller = SshController::new_test(root.clone());
        let cases = [
            ("empty-hostname", "empty-hostname", "", 22, "root", "main"),
            (
                "hash-hostname",
                "hash-hostname",
                "bad#host",
                22,
                "root",
                "main",
            ),
            (
                "quote-hostname",
                "quote-hostname",
                "\"bad\"",
                22,
                "root",
                "main",
            ),
            (
                "equals-hostname",
                "equals-hostname",
                "bad=name",
                22,
                "root",
                "main",
            ),
            (
                "leading-hostname",
                "leading-hostname",
                "-bad",
                22,
                "root",
                "main",
            ),
            (
                "newline-hostname",
                "newline-hostname",
                "bad\nhost",
                22,
                "root",
                "main",
            ),
            (
                "leading-name",
                "-leading-name",
                "host.example",
                22,
                "root",
                "main",
            ),
            ("hash-name", "hash#name", "host.example", 22, "root", "main"),
            (
                "leading-user",
                "leading-user",
                "host.example",
                22,
                "-root",
                "main",
            ),
            (
                "quote-user",
                "quote-user",
                "host.example",
                22,
                "\"root\"",
                "main",
            ),
            (
                "equals-user",
                "equals-user",
                "host.example",
                22,
                "root=x",
                "main",
            ),
            (
                "path-key",
                "path-key",
                "host.example",
                22,
                "root",
                "../../.bashrc",
            ),
            (
                "hash-key",
                "hash-key",
                "host.example",
                22,
                "root",
                "main#bad",
            ),
            (
                "quote-key",
                "quote-key",
                "host.example",
                22,
                "root",
                "\"main\"",
            ),
            (
                "equals-key",
                "equals-key",
                "host.example",
                22,
                "root",
                "main=bad",
            ),
            (
                "leading-key",
                "leading-key",
                "host.example",
                22,
                "root",
                "-main",
            ),
            (
                "high-port",
                "high-port",
                "host.example",
                65_536,
                "root",
                "main",
            ),
        ];
        for (label, name, hostname, port, user, key_id) in cases {
            assert!(
                matches!(
                    controller.create(name, hostname, port, user, key_id),
                    Err(SshError::InvalidSshField(_))
                ),
                "hostile create case {label:?} was accepted"
            );
        }
        assert!(fs::read_dir(root.join("hosts"))
            .expect("hosts")
            .next()
            .is_none());
    }

    #[test]
    fn create_enforces_include_key_membership_port_bounds_and_exclusive_collision() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        fs::write(root.join("keys/bad.pub"), "bad key\n").expect("bad public key");
        let controller = SshController::new_test(root.clone());
        assert!(matches!(
            controller.create("missing-key", "host.example", 22, "root", "missing"),
            Err(SshError::InvalidSshField(_))
        ));
        assert!(matches!(
            controller.create("bad-key", "host.example", 22, "root", "bad"),
            Err(SshError::InvalidSshField(_))
        ));
        controller
            .create("max-port", "host.example", 65_535, "root", "main")
            .expect("maximum port");
        assert!(matches!(
            controller.create("max-port", "other.example", 22, "root", "main"),
            Err(SshError::SshHostExists(_))
        ));

        fs::write(root.join("config"), "Host *\n").expect("remove Include");
        assert!(matches!(
            controller.create("no-include", "host.example", 22, "root", "main"),
            Err(SshError::SshConfigNotIncluded(_))
        ));
    }

    #[test]
    fn generated_fragment_passes_ssh_g_round_trip() {
        let Some(ssh) = resolve_trusted_executable("ssh").ok() else {
            eprintln!(
                "SKIPPING ssh -G round trip: ssh is absent from trusted directories /usr/bin, /bin, /usr/local/bin"
            );
            return;
        };
        let (_temp, root) = fixture();
        write_key(&root, "main");
        let controller = SshController::new_test(root.clone());
        controller
            .create("alpha", "2001:db8::1", 22, "deploy_user", "main")
            .expect("create host");
        let path = root.join("hosts/alpha");
        let output = Command::new(ssh)
            .arg("-G")
            .arg("-F")
            .arg(&path)
            .arg("alpha")
            .output()
            .expect("run ssh -G");
        assert!(
            output.status.success(),
            "ssh -G rejected generated fragment: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn edit_trash_restore_and_purge_round_trip() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        let controller = SshController::new_test(root.clone());
        controller
            .create("alpha", "alpha.example", 22, "root", "main")
            .expect("create");
        assert_eq!(
            controller.edit_path("alpha").expect("edit path"),
            root.join("hosts/alpha")
        );
        {
            let mut state = controller.state();
            state.hosts[0].probe_status = "ok".into();
            state.hosts[0].probe_ms = 5;
        }
        controller.trash("alpha").expect("trash");
        let trashed = controller
            .snapshot()
            .hosts
            .into_iter()
            .find(|host| host.0 == "alpha" && host.7)
            .expect("visible trash row");
        assert_eq!(trashed.8, "unknown");
        assert_eq!(trashed.10, 0);
        assert!(matches!(
            controller.edit_path("alpha"),
            Err(SshError::SshHostTrashed(_))
        ));

        controller.restore("alpha").expect("restore");
        assert!(controller
            .snapshot()
            .hosts
            .iter()
            .any(|host| host.0 == "alpha" && !host.7));
        controller.trash("alpha").expect("trash again");
        controller.purge("alpha").expect("purge");
        assert!(!controller
            .snapshot()
            .hosts
            .iter()
            .any(|host| host.0 == "alpha"));
        assert!(!root.join("hosts/.trashed-alpha").exists());
    }

    #[test]
    fn trash_and_restore_collisions_are_typed_and_never_overwrite() {
        let (_temp, root) = fixture();
        write_key(&root, "main");
        let controller = SshController::new_test(root.clone());
        controller
            .create("alpha", "alpha.example", 22, "root", "main")
            .expect("create");
        fs::write(root.join("hosts/.trashed-alpha"), "collision\n").expect("trash collision");
        assert!(matches!(
            controller.trash("alpha"),
            Err(SshError::SshTrashCollision(_))
        ));
        assert!(root.join("hosts/alpha").exists());

        fs::remove_file(root.join("hosts/.trashed-alpha")).expect("remove collision");
        controller.trash("alpha").expect("trash");
        fs::write(root.join("hosts/alpha"), "live collision\n").expect("restore collision");
        assert!(matches!(
            controller.restore("alpha"),
            Err(SshError::SshHostExists(_))
        ));
        assert!(root.join("hosts/.trashed-alpha").exists());
        assert_eq!(
            fs::read_to_string(root.join("hosts/alpha")).expect("live collision text"),
            "live collision\n"
        );
    }

    #[test]
    fn probe_request_is_immediate_deduped_and_results_update_snapshot() {
        let (_temp, root) = fixture();
        write_host(&root, "alpha", &valid_host("alpha"), 0o600);
        let fake = Arc::new(FakeProber::default());
        let runner: Arc<dyn SshRunner> = fake.clone();
        let controller = SshController::new_test_with(root, runner);
        controller
            .probe(vec!["alpha".into(), "alpha".into()])
            .expect("probe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && controller.active_probes() != 0 {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            controller.active_probes(),
            0,
            "probe did not finish by deadline"
        );
        assert_eq!(fake.probes.lock().expect("probes").as_slice(), ["alpha"]);
        let host = controller
            .snapshot()
            .hosts
            .into_iter()
            .find(|host| host.0 == "alpha")
            .unwrap();
        assert_eq!(host.8, "ok");
        assert_eq!(host.10, 7);
    }

    #[test]
    fn content_change_resets_probe_and_stale_result_is_dropped() {
        let mut old = read_host_entry_from_text("alpha", &valid_host("alpha"));
        old.probe_status = "ok".into();
        old.probe_ms = 3;
        let mut changed =
            read_host_entry_from_text("alpha", "Host alpha\nHostname changed.example.com\n");
        preserve_probe_results(&[old], std::slice::from_mut(&mut changed));
        assert_eq!(changed.probe_status, "unknown");
        assert_eq!(changed.probe_ms, 0);

        let mut in_flight = read_host_entry_from_text("alpha", &valid_host("alpha"));
        in_flight.probe_status = "probing".into();
        let mut replaced = read_host_entry_from_text("alpha", &valid_host("alpha"));
        replaced.source_tag = replaced.source_tag.wrapping_add(1);
        preserve_probe_results(&[in_flight], std::slice::from_mut(&mut replaced));
        assert_eq!(replaced.probe_status, "unknown");
    }

    fn read_host_entry_from_text(id: &str, text: &str) -> HostEntry {
        let parsed = parse_host(id, text).expect("host parse");
        HostEntry {
            id: id.into(),
            host_error: String::new(),
            host_warning: String::new(),
            hostname: parsed.0,
            port: parsed.1,
            user: parsed.2,
            identity: parsed.3,
            trashed: false,
            probe_status: "unknown".into(),
            probe_error: String::new(),
            probe_ms: 0,
            probe_checked_at: 0,
            content_tag: content_tag(text.as_bytes()),
            source_tag: content_tag(text.as_bytes()),
        }
    }

    #[test]
    fn dbus_errors_keep_the_pinned_names() {
        for (error, expected) in [
            (SshError::InvalidSshField(String::new()), "InvalidSshField"),
            (SshError::UnknownSshHost(String::new()), "UnknownSshHost"),
            (
                SshError::SshHostNotActionable(String::new()),
                "SshHostNotActionable",
            ),
            (SshError::SshHostExists(String::new()), "SshHostExists"),
            (
                SshError::SshTrashCollision(String::new()),
                "SshTrashCollision",
            ),
            (SshError::SshHostTrashed(String::new()), "SshHostTrashed"),
            (
                SshError::SshConfigNotIncluded(String::new()),
                "SshConfigNotIncluded",
            ),
            (SshError::SshStoreFailure(String::new()), "SshStoreFailure"),
            (SshError::SshProbeLimit(String::new()), "SshProbeLimit"),
            (
                SshError::SshLaunchFailure(String::new()),
                "SshLaunchFailure",
            ),
        ] {
            assert_eq!(
                zbus::DBusError::name(&error).as_str(),
                format!("dev.cosmix.trayd.Error.{expected}")
            );
        }
    }
}
