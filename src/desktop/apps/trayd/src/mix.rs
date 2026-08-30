//! User-owned Mix script catalogue, lifecycle and transient run supervision.
//!
//! Script identities are safe, human-readable executable filenames. Trayd owns every
//! path and command line, and script content is edited through the desktop's
//! external editor rather than transported over D-Bus.

use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use inotify::{Inotify, WatchMask};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(test)]
use crate::bus::TestOpenGate;

const MIX_BINARY: &str = "/opt/cosmix/bin/mix";
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub(crate) const XDG_OPEN: &str = "/usr/bin/xdg-open";

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o700;
const GROUP_OTHER_MODE_MASK: u32 = 0o077;
const METADATA_LIMIT: usize = 64 * 1024;
const SCRIPT_LIMIT: usize = 1024 * 1024;
const MAX_ACTIVE_RUNS: usize = 4;
const MAX_RUN_HISTORY: usize = 32;
const MAX_TAIL_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const MAX_SIGNAL_CHUNKS: usize = 16;
const MAX_SIGNAL_BYTES: usize = 64 * 1024;
const MAX_PENDING_SIGNAL_BYTES: usize = MAX_TAIL_BYTES * MAX_ACTIVE_RUNS;
const RUNNER_EVENT_CAPACITY: usize = 128;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

pub(crate) type WireMixScript = (String, String, String, bool, u64, u64);
pub(crate) type WireMixRun = (
    String,
    String,
    String,
    String,
    u64,
    u64,
    bool,
    i32,
    String,
    String,
    u64,
    u64,
    u64,
);
pub(crate) type WireMixOutput = (u64, String, String);

#[derive(Clone, Serialize, zbus::zvariant::Type)]
pub(crate) struct WireMixSnapshot {
    revision: u64,
    state: String,
    error: String,
    scripts: Vec<WireMixScript>,
    runs: Vec<WireMixRun>,
    active_runs: u32,
}

#[derive(Debug, zbus::DBusError, PartialEq, Eq)]
#[zbus(prefix = "dev.cosmix.trayd.Error", impl_display = true)]
pub(crate) enum MixError {
    InvalidMixId(String),
    UnknownMixScript(String),
    MixScriptTrashed(String),
    InvalidMixMetadata(String),
    MixStoreFailure(String),
    MixScriptExists(String),
    MixTrashCollision(String),
    MixAlreadyTrashed(String),
    MixNotTrashed(String),
    MixScriptBusy(String),
    MixRunLimit(String),
    UnknownMixRun(String),
    MixRunNotActive(String),
    MixLaunchFailure(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScriptMetadata {
    id: String,
    name: String,
    description: String,
    created_ms: u64,
    updated_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyScriptMetadata {
    schema: u32,
    id: String,
    name: String,
    description: String,
    #[serde(rename = "created_ms")]
    _created_ms: u64,
    #[serde(rename = "updated_ms")]
    _updated_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScriptEntry {
    metadata: ScriptMetadata,
    trashed: bool,
}

impl ScriptEntry {
    fn wire(&self) -> WireMixScript {
        (
            self.metadata.id.clone(),
            self.metadata.name.clone(),
            self.metadata.description.clone(),
            self.trashed,
            self.metadata.created_ms,
            self.metadata.updated_ms,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OutputTail {
    chunks: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

impl OutputTail {
    fn push(&mut self, text: String) {
        let bytes = text.len();
        self.bytes = self.bytes.saturating_add(bytes);
        self.chunks.push_back(text);
        while self.bytes > MAX_TAIL_BYTES {
            let Some(removed) = self.chunks.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.len());
            self.dropped = self.dropped.saturating_add(removed.len() as u64);
        }
    }

    fn joined(&self) -> String {
        let mut output = String::with_capacity(self.bytes);
        for chunk in &self.chunks {
            output.push_str(chunk);
        }
        output
    }
}

#[derive(Clone, Debug)]
struct RunRecord {
    id: String,
    script_id: String,
    script_name: String,
    unit: String,
    state: String,
    started_ms: u64,
    finished_ms: u64,
    exit_code: Option<i32>,
    stdout: OutputTail,
    stderr: OutputTail,
    stdout_signal_dropped: u64,
    stderr_signal_dropped: u64,
    next_sequence: u64,
}

impl RunRecord {
    fn active(&self) -> bool {
        matches!(self.state.as_str(), "starting" | "running" | "stopping")
    }

    fn wire(&self) -> WireMixRun {
        (
            self.id.clone(),
            self.script_id.clone(),
            self.script_name.clone(),
            self.state.clone(),
            self.started_ms,
            self.finished_ms,
            self.exit_code.is_some(),
            self.exit_code.unwrap_or_default(),
            self.stdout.joined(),
            self.stderr.joined(),
            self.stdout
                .dropped
                .saturating_add(self.stdout_signal_dropped),
            self.stderr
                .dropped
                .saturating_add(self.stderr_signal_dropped),
            self.next_sequence,
        )
    }
}

#[derive(Clone, Debug)]
struct PendingOutput {
    run_id: String,
    chunk: WireMixOutput,
    bytes: usize,
}

#[derive(Debug)]
struct MixState {
    revision: u64,
    state: String,
    error: String,
    scripts: Vec<ScriptEntry>,
    runs: VecDeque<RunRecord>,
    catalogue_pending: bool,
    run_pending: BTreeSet<String>,
    outputs_pending: VecDeque<PendingOutput>,
    outputs_pending_bytes: usize,
}

impl Default for MixState {
    fn default() -> Self {
        Self {
            revision: 0,
            state: "absent".into(),
            error: String::new(),
            scripts: Vec::new(),
            runs: VecDeque::new(),
            catalogue_pending: false,
            run_pending: BTreeSet::new(),
            outputs_pending: VecDeque::new(),
            outputs_pending_bytes: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct RunRequest {
    run_id: String,
    unit: String,
    script_handle: Arc<File>,
    working_directory_handle: Arc<File>,
}

#[derive(Clone, Debug)]
enum RunnerEvent {
    Output {
        run_id: String,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Finished {
        run_id: String,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    StopFailed {
        run_id: String,
        error: String,
    },
}

trait MixRunner: Send + Sync {
    fn reconcile(&self) -> Result<(), String> {
        Ok(())
    }
    fn start(&self, request: RunRequest, events: SyncSender<RunnerEvent>) -> Result<(), String>;
    fn stop(&self, run_id: &str, unit: &str, events: SyncSender<RunnerEvent>)
        -> Result<(), String>;
}

#[derive(Default)]
struct SystemdRunner;

impl MixRunner for SystemdRunner {
    fn reconcile(&self) -> Result<(), String> {
        stop_transient_unit_with_retry("cosmix-mix-run-*.service")
    }

    fn start(&self, request: RunRequest, events: SyncSender<RunnerEvent>) -> Result<(), String> {
        let mut command = Command::new(SYSTEMD_RUN);
        command
            .args(transient_args(&request))
            .env("DBUS_SESSION_BUS_ADDRESS", systemd_user_bus_address()?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot run transient Mix unit: {error}"))?;
        let Some(stdout) = child.stdout.take() else {
            return Err(cleanup_launch_failure(
                "systemd-run did not provide stdout",
                &mut child,
                &request.unit,
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(cleanup_launch_failure(
                "systemd-run did not provide stderr",
                &mut child,
                &request.unit,
            ));
        };

        let stdout_events = events.clone();
        let stdout_id = request.run_id.clone();
        let stdout_reader = match thread::Builder::new()
            .name(format!("cosmix-mix-stdout-{}", short_id(&request.run_id)))
            .spawn(move || read_output(stdout, stdout_id, OutputStream::Stdout, stdout_events))
        {
            Ok(reader) => reader,
            Err(error) => {
                return Err(cleanup_launch_failure(
                    &format!("cannot start Mix stdout reader: {error}"),
                    &mut child,
                    &request.unit,
                ));
            }
        };
        let stderr_events = events.clone();
        let stderr_id = request.run_id.clone();
        let stderr_reader = match thread::Builder::new()
            .name(format!("cosmix-mix-stderr-{}", short_id(&request.run_id)))
            .spawn(move || read_output(stderr, stderr_id, OutputStream::Stderr, stderr_events))
        {
            Ok(reader) => reader,
            Err(error) => {
                let failure = cleanup_launch_failure(
                    &format!("cannot start Mix stderr reader: {error}"),
                    &mut child,
                    &request.unit,
                );
                let _ = stdout_reader.join();
                return Err(failure);
            }
        };

        let cleanup_unit = request.unit.clone();
        let supervised_child = Arc::new(Mutex::new(Some(child)));
        let waiter_child = Arc::clone(&supervised_child);
        match thread::Builder::new()
            .name(format!("cosmix-mix-wait-{}", short_id(&request.run_id)))
            .spawn(move || {
                // Capture the WHOLE request, not just run_id: the manager
                // resolves OpenFile= from /proc/<trayd>/fd/N at unit spawn,
                // so the pinned script/cwd descriptors must outlive that
                // resolution. Disjoint field capture would drop them when
                // start() returns and the unit dies at step FDS (202).
                let request = request;
                let status = waiter_child
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                    .expect("Mix child is taken once")
                    .wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                let (exit_code, error) = match status {
                    Ok(status) => (status.code(), None),
                    Err(error) => (None, Some(format!("cannot wait for Mix unit: {error}"))),
                };
                let _ = events.send(RunnerEvent::Finished {
                    run_id: request.run_id,
                    exit_code,
                    error,
                });
            }) {
            Ok(_) => Ok(()),
            Err(error) => {
                let failure = if let Some(mut child) = supervised_child
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    cleanup_launch_failure(
                        &format!("cannot start Mix unit waiter: {error}"),
                        &mut child,
                        &cleanup_unit,
                    )
                } else {
                    match stop_transient_unit_with_retry(&cleanup_unit) {
                        Ok(()) => format!("cannot start Mix unit waiter: {error}"),
                        Err(cleanup) => format!(
                            "cannot start Mix unit waiter: {error}; cleanup failed: {cleanup}"
                        ),
                    }
                };
                Err(failure)
            }
        }
    }

    fn stop(
        &self,
        run_id: &str,
        unit: &str,
        events: SyncSender<RunnerEvent>,
    ) -> Result<(), String> {
        let mut command = Command::new(SYSTEMCTL);
        command
            .args(["--user", "stop", unit])
            .env("DBUS_SESSION_BUS_ADDRESS", systemd_user_bus_address()?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let run_id = run_id.to_owned();
        let unit = unit.to_owned();
        thread::Builder::new()
            .name(format!("cosmix-mix-stop-{}", short_id(&unit)))
            .spawn(move || {
                let result = command.output();
                let error = match result {
                    Ok(output) if output.status.success() => return,
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let detail = if stderr.trim().is_empty() {
                            stdout.trim()
                        } else {
                            stderr.trim()
                        };
                        if detail.is_empty() {
                            format!("systemctl stop {unit} exited with {}", output.status)
                        } else {
                            format!("systemctl stop {unit} failed: {detail}")
                        }
                    }
                    Err(error) => format!("cannot stop transient Mix unit {unit}: {error}"),
                };
                let _ = events.send(RunnerEvent::StopFailed { run_id, error });
            })
            .map(|_| ())
            .map_err(|error| format!("cannot start Mix stop waiter: {error}"))
    }
}

fn stop_transient_unit(unit: &str) -> Result<(), String> {
    let output = Command::new(SYSTEMCTL)
        .args(["--user", "stop", unit])
        .env("DBUS_SESSION_BUS_ADDRESS", systemd_user_bus_address()?)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot stop transient Mix unit {unit}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(format!("systemctl stop {unit} failed: {}", detail.trim()))
}

fn stop_transient_unit_with_retry(unit: &str) -> Result<(), String> {
    match stop_transient_unit(unit) {
        Ok(()) => Ok(()),
        Err(first) => stop_transient_unit(unit)
            .map_err(|second| format!("{first}; immediate retry failed: {second}")),
    }
}

fn cleanup_launch_failure(reason: &str, child: &mut std::process::Child, unit: &str) -> String {
    let mut failures = Vec::new();
    if let Err(error) = stop_transient_unit_with_retry(unit) {
        failures.push(error);
    }
    if let Err(error) = child.kill() {
        failures.push(format!("cannot kill systemd-run helper: {error}"));
    }
    if let Err(error) = child.wait() {
        failures.push(format!("cannot reap systemd-run helper: {error}"));
    }
    if failures.is_empty() {
        reason.to_owned()
    } else {
        format!("{reason}; cleanup failed: {}", failures.join("; "))
    }
}

fn transient_args(request: &RunRequest) -> Vec<OsString> {
    let pinned_source = format!(
        "/proc/{}/fd/{}:mix-script:read-only",
        std::process::id(),
        request.script_handle.as_raw_fd()
    );
    let pinned_working_directory = format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        request.working_directory_handle.as_raw_fd()
    );
    [
        OsString::from("--user"),
        OsString::from("--collect"),
        OsString::from("--wait"),
        OsString::from("--pipe"),
        OsString::from("--quiet"),
        OsString::from(format!(
            "--unit={}",
            request.unit.trim_end_matches(".service")
        )),
        OsString::from("--service-type=exec"),
        OsString::from("--slice=app.slice"),
        OsString::from("--property=StandardInput=null"),
        OsString::from("--property=TimeoutStopSec=5s"),
        OsString::from(format!("--property=OpenFile={pinned_source}")),
        OsString::from(format!("--working-directory={pinned_working_directory}")),
        OsString::from("--"),
        OsString::from(MIX_BINARY),
        OsString::from("/dev/fd/3"),
    ]
    .into()
}

fn systemd_user_bus_address() -> Result<String, String> {
    let runtime =
        env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err("XDG_RUNTIME_DIR is not absolute".into());
    }
    Ok(format!("unix:path={}/bus", runtime.display()))
}

fn read_output(
    mut input: impl Read,
    run_id: String,
    stream: OutputStream,
    events: SyncSender<RunnerEvent>,
) {
    let mut buffer = [0_u8; MAX_OUTPUT_CHUNK_BYTES];
    let mut pending = Vec::with_capacity(MAX_OUTPUT_CHUNK_BYTES + 4);
    loop {
        match input.read(&mut buffer) {
            Ok(0) => {
                if !pending.is_empty() {
                    let bytes = String::from_utf8_lossy(&pending).into_owned().into_bytes();
                    let _ = events.send(RunnerEvent::Output {
                        run_id,
                        stream,
                        bytes,
                    });
                }
                return;
            }
            Ok(read) => {
                pending.extend_from_slice(&buffer[..read]);
                if !flush_complete_utf8(&mut pending, &run_id, stream, &events) {
                    return;
                }
            }
            Err(error) => {
                let _ = events.send(RunnerEvent::Output {
                    run_id,
                    stream,
                    bytes: format!("output capture failed: {error}\n").into_bytes(),
                });
                return;
            }
        }
    }
}

fn flush_complete_utf8(
    pending: &mut Vec<u8>,
    run_id: &str,
    stream: OutputStream,
    events: &SyncSender<RunnerEvent>,
) -> bool {
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                if text.is_empty() {
                    return true;
                }
                let bytes = text.as_bytes().to_vec();
                pending.clear();
                return events
                    .send(RunnerEvent::Output {
                        run_id: run_id.to_owned(),
                        stream,
                        bytes,
                    })
                    .is_ok();
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    let bytes = pending.drain(..valid).collect();
                    if events
                        .send(RunnerEvent::Output {
                            run_id: run_id.to_owned(),
                            stream,
                            bytes,
                        })
                        .is_err()
                    {
                        return false;
                    }
                    continue;
                }
                let Some(invalid) = error.error_len() else {
                    // An incomplete scalar at the end is carried into the next
                    // read instead of being replaced at the read boundary.
                    return true;
                };
                pending.drain(..invalid);
                if events
                    .send(RunnerEvent::Output {
                        run_id: run_id.to_owned(),
                        stream,
                        bytes: "\u{fffd}".as_bytes().to_vec(),
                    })
                    .is_err()
                {
                    return false;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct MixStore {
    root: PathBuf,
    legacy_root: Option<PathBuf>,
}

impl MixStore {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            legacy_root: None,
        }
    }

    fn with_legacy(root: PathBuf, legacy_root: PathBuf) -> Self {
        Self {
            root,
            legacy_root: Some(legacy_root),
        }
    }

    fn default_store() -> Result<Self, String> {
        let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_owned())?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOME is not absolute".into());
        }
        Self::default_store_for_home(&home, env::var_os("XDG_DATA_HOME").as_deref())
    }

    fn default_store_for_home(home: &Path, xdg_data_home: Option<&OsStr>) -> Result<Self, String> {
        let legacy_data_home = match xdg_data_home.filter(|value| !value.is_empty()) {
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err("XDG_DATA_HOME is not absolute".into());
                }
                path
            }
            None => home.join(".local/share"),
        };
        Ok(Self::with_legacy(
            home.join(".local/mix"),
            legacy_data_home.join("cosmix/mix"),
        ))
    }

    fn trash(&self) -> PathBuf {
        self.root.join(".trash")
    }

    fn category(&self, trashed: bool) -> PathBuf {
        if trashed {
            self.trash()
        } else {
            self.root.clone()
        }
    }

    fn entry(&self, id: &str, trashed: bool) -> PathBuf {
        self.category(trashed).join(id)
    }

    fn legacy_exists(&self) -> bool {
        self.legacy_root.as_ref().is_some_and(|root| root.exists())
    }

    fn ensure(&self) -> Result<(), String> {
        ensure_directory(&self.root)?;
        ensure_directory(&self.trash())
    }

    fn create(&self, name: &str, description: &str) -> Result<ScriptMetadata, MixError> {
        let id = slugify_name(name);
        if id.is_empty() {
            return Err(MixError::InvalidMixMetadata(
                "script name must contain at least one ASCII letter or digit".into(),
            ));
        }
        let description =
            sanitise_description(description).map_err(MixError::InvalidMixMetadata)?;
        self.ensure().map_err(MixError::MixStoreFailure)?;
        if self.path_exists(&self.entry(&id, false)) || self.path_exists(&self.entry(&id, true)) {
            return Err(MixError::MixScriptExists(format!(
                "Mix script {id} already exists"
            )));
        }
        let path = self.entry(&id, false);
        let content = format!("#!{MIX_BINARY}\n-- description: {description}\n\n");
        create_file(&path, content.as_bytes()).map_err(|error| match error {
            CreateFileError::Exists => {
                MixError::MixScriptExists(format!("Mix script {id} already exists"))
            }
            CreateFileError::Failure(error) => MixError::MixStoreFailure(error),
        })?;
        self.read_entry(&id, false)
            .map(|entry| entry.metadata)
            .map_err(MixError::MixStoreFailure)
    }

    fn update(&self, id: &str, name: &str, description: &str) -> Result<(), MixError> {
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        let name = slugify_name(name);
        if name.is_empty() {
            return Err(MixError::InvalidMixMetadata(
                "script name must contain at least one ASCII letter or digit".into(),
            ));
        }
        let description =
            sanitise_description(description).map_err(MixError::InvalidMixMetadata)?;
        let source = self
            .checked_entry(&id, false)
            .map_err(MixError::MixStoreFailure)?;
        if name != id
            && (self.path_exists(&self.entry(&name, false))
                || self.path_exists(&self.entry(&name, true)))
        {
            return Err(MixError::MixScriptExists(format!(
                "Mix script {name} already exists"
            )));
        }
        rewrite_description(&source, &description).map_err(MixError::MixStoreFailure)?;
        if name != id {
            let destination = self.entry(&name, false);
            secure_rename(&source, &destination, true).map_err(|error| {
                if self.path_exists(&destination) {
                    MixError::MixScriptExists(format!("Mix script {name} already exists"))
                } else {
                    MixError::MixStoreFailure(format!("renaming Mix script {id}: {error}"))
                }
            })?;
        }
        Ok(())
    }

    fn move_entry(&self, id: &str, from_trash: bool) -> Result<(), MixError> {
        self.ensure().map_err(MixError::MixStoreFailure)?;
        let source = self
            .checked_entry(id, from_trash)
            .map_err(MixError::MixStoreFailure)?;
        let destination = self.entry(id, !from_trash);
        check_directory(&self.category(!from_trash)).map_err(MixError::MixStoreFailure)?;
        if self.path_exists(&destination) {
            let message = format!(
                "Mix script {id} already exists in {}",
                if from_trash { "store root" } else { ".trash" }
            );
            return Err(if from_trash {
                MixError::MixScriptExists(message)
            } else {
                MixError::MixTrashCollision(message)
            });
        }
        secure_rename(&source, &destination, true).map_err(|error| {
            if self.path_exists(&destination) {
                let message = format!(
                    "Mix script {id} already exists in {}",
                    if from_trash { "store root" } else { ".trash" }
                );
                if from_trash {
                    MixError::MixScriptExists(message)
                } else {
                    MixError::MixTrashCollision(message)
                }
            } else {
                MixError::MixStoreFailure(format!("moving Mix script {id}: {error}"))
            }
        })
    }

    fn purge(&self, id: &str) -> Result<(), String> {
        let path = self.checked_entry(id, true)?;
        secure_remove_child(
            path.parent()
                .ok_or_else(|| "trash file has no parent".to_owned())?,
            path.file_name()
                .ok_or_else(|| "trash file has no name".to_owned())?,
            false,
        )
        .map_err(|error| format!("purging Mix script {id}: {error}"))
    }

    fn script_path(&self, id: &str, trashed: bool) -> Result<PathBuf, String> {
        self.checked_entry(id, trashed)
    }

    fn script_source(&self, id: &str, trashed: bool) -> Result<(Arc<File>, Arc<File>), String> {
        let id = canonical_script_id(id)?;
        check_directory(&self.root)?;
        let category = self.category(trashed);
        let directory_handle = secure_open(&category, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|error| secure_directory_error(&category, error))?;
        let file_name = id;
        let file = secure_open_at(
            directory_handle.as_raw_fd(),
            OsStr::new(&file_name),
            libc::O_RDONLY,
            0,
        )
        .map_err(|error| {
            format!(
                "refusing unsafe file {}: {error}",
                category.join(&file_name).display()
            )
        })?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "inspecting {}: {error}",
                category.join(&file_name).display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "{} is not a regular file",
                category.join(&file_name).display()
            ));
        }
        if metadata.len() > SCRIPT_LIMIT as u64 {
            return Err(format!(
                "{} exceeds the {} byte limit",
                category.join(&file_name).display(),
                SCRIPT_LIMIT
            ));
        }
        check_private_file_mode(&metadata, &category.join(&file_name))?;
        Ok((Arc::new(file), Arc::new(directory_handle)))
    }

    fn checked_entry(&self, id: &str, trashed: bool) -> Result<PathBuf, String> {
        let id = canonical_script_id(id)?;
        check_directory(&self.root)?;
        check_directory(&self.category(trashed))?;
        let path = self.entry(&id, trashed);
        check_regular_file(&path, SCRIPT_LIMIT)?;
        Ok(path)
    }

    fn scan(&self) -> Result<(Vec<ScriptEntry>, String), String> {
        check_directory(&self.root)?;
        check_directory(&self.trash())?;
        let mut scripts = Vec::new();
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for trashed in [false, true] {
            let category = self.category(trashed);
            for child in
                fs::read_dir(&category).map_err(|error| format!("reading catalogue: {error}"))?
            {
                let child = child.map_err(|error| format!("reading catalogue: {error}"))?;
                let file_name = child.file_name();
                if file_name.as_bytes().starts_with(b".") {
                    continue;
                }
                let file_type = child
                    .file_type()
                    .map_err(|error| format!("inspecting catalogue entry: {error}"))?;
                if file_type.is_dir() {
                    continue;
                }
                let Some(file_name) = file_name.to_str() else {
                    problems.push(format!(
                        "ignored non-UTF-8 Mix filename {}",
                        child.path().display()
                    ));
                    continue;
                };
                let id = file_name;
                if canonical_script_id(file_name).is_err() {
                    problems.push(format!("ignored invalid Mix script filename {file_name}"));
                    continue;
                }
                match self.read_entry(id, trashed) {
                    Ok(entry) if seen.insert(id.to_owned()) => scripts.push(entry),
                    Ok(_) => problems.push(format!(
                        "ignored duplicate Mix script identity {id} in trash"
                    )),
                    Err(error) => problems.push(format!("ignored Mix script {id}: {error}")),
                }
            }
        }
        scripts.sort_by(|left, right| {
            left.trashed
                .cmp(&right.trashed)
                .then_with(|| {
                    left.metadata
                        .name
                        .to_lowercase()
                        .cmp(&right.metadata.name.to_lowercase())
                })
                .then_with(|| left.metadata.id.cmp(&right.metadata.id))
        });
        Ok((scripts, problems.join("; ")))
    }

    fn read_entry(&self, id: &str, trashed: bool) -> Result<ScriptEntry, String> {
        let path = self.entry(id, trashed);
        let mut file = secure_regular_file(&path)?;
        let file_metadata = file
            .metadata()
            .map_err(|error| format!("inspecting {}: {error}", path.display()))?;
        if file_metadata.len() > SCRIPT_LIMIT as u64 {
            return Err(format!(
                "{} exceeds the {} byte limit",
                path.display(),
                SCRIPT_LIMIT
            ));
        }
        check_private_file_mode(&file_metadata, &path)?;
        let mut bytes = Vec::with_capacity(file_metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(SCRIPT_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        if bytes.len() > SCRIPT_LIMIT {
            return Err(format!(
                "{} exceeds the {} byte limit",
                path.display(),
                SCRIPT_LIMIT
            ));
        }
        let (created_ms, updated_ms) = file_times_ms(&file, &file_metadata);
        Ok(ScriptEntry {
            metadata: ScriptMetadata {
                id: id.to_owned(),
                name: id.to_owned(),
                description: description_from_leading_comments(&bytes),
                created_ms,
                updated_ms,
            },
            trashed,
        })
    }

    fn path_exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn watch_base_paths(&self) -> Result<[PathBuf; 2], String> {
        check_directory(&self.root)?;
        check_directory(&self.trash())?;
        Ok([self.root.clone(), self.trash()])
    }

    fn migrate_legacy(&self) -> Result<Vec<String>, String> {
        let Some(legacy_root) = self.legacy_root.as_ref() else {
            return Ok(Vec::new());
        };
        if !legacy_root.exists() {
            return Ok(Vec::new());
        }
        check_directory(legacy_root)?;
        self.ensure()?;
        let legacy_device = fs::metadata(legacy_root)
            .map_err(|error| format!("inspecting {}: {error}", legacy_root.display()))?
            .dev();
        let target_device = fs::metadata(&self.root)
            .map_err(|error| format!("inspecting {}: {error}", self.root.display()))?
            .dev();
        if legacy_device != target_device {
            return Err(format!(
                "legacy Mix tree {} and target {} are on different filesystems; refusing non-atomic migration",
                legacy_root.display(),
                self.root.display()
            ));
        }
        let mut used = BTreeSet::new();
        for category in [self.root.clone(), self.trash()] {
            for child in fs::read_dir(&category)
                .map_err(|error| format!("reading new Mix catalogue for migration: {error}"))?
            {
                let child = child
                    .map_err(|error| format!("reading new Mix catalogue for migration: {error}"))?;
                let Some(name) = child.file_name().to_str().map(ToOwned::to_owned) else {
                    continue;
                };
                if !name.starts_with('.') && canonical_script_id(&name).is_ok() {
                    used.insert(name);
                }
            }
        }

        let mut messages = Vec::new();
        for trashed in [false, true] {
            let legacy_category = legacy_root.join(if trashed { "trash" } else { "scripts" });
            if !legacy_category.exists() {
                continue;
            }
            check_directory(&legacy_category)?;
            match sweep_rewrite_temporaries(&legacy_category) {
                Ok(warnings) => messages.extend(warnings),
                Err(error) => messages.push(format!(
                    "warning: cannot sweep stale Mix rewrite files in {}: {error}",
                    legacy_category.display()
                )),
            }
            let mut sources = Vec::new();
            for child in fs::read_dir(&legacy_category)
                .map_err(|error| format!("reading legacy Mix catalogue: {error}"))?
            {
                let child = match child {
                    Ok(child) => child,
                    Err(error) => {
                        messages.push(format!(
                            "warning: cannot inspect a legacy Mix catalogue entry: {error}"
                        ));
                        continue;
                    }
                };
                let Some(name) = child.file_name().to_str().map(ToOwned::to_owned) else {
                    continue;
                };
                if canonical_run_id(&name).is_ok() {
                    if let Err(error) = check_directory(&child.path()) {
                        messages.push(format!(
                            "warning: cannot migrate legacy Mix script {name}: {error}"
                        ));
                        continue;
                    }
                    match sweep_rewrite_temporaries(&child.path()) {
                        Ok(warnings) => messages.extend(warnings),
                        Err(error) => messages.push(format!(
                            "warning: cannot sweep stale Mix rewrite files in {}: {error}",
                            child.path().display()
                        )),
                    }
                    let script = child.path().join("script.mix");
                    match check_regular_file(&script, SCRIPT_LIMIT) {
                        Ok(()) => sources.push((
                            name,
                            script,
                            Some(child.path()),
                            LegacySourceKind::UuidDirectory,
                        )),
                        Err(error) => messages.push(format!(
                            "warning: cannot migrate legacy Mix script {name}: {error}"
                        )),
                    }
                } else if let Some(stem) = name.strip_suffix(".mix") {
                    if let Err(error) = canonical_script_id(stem) {
                        messages.push(format!(
                            "warning: cannot migrate legacy Mix script {name}: {error}"
                        ));
                        continue;
                    }
                    match check_regular_file(&child.path(), SCRIPT_LIMIT) {
                        Ok(()) => sources.push((
                            stem.to_owned(),
                            child.path(),
                            None,
                            LegacySourceKind::FlatV1,
                        )),
                        Err(error) => messages.push(format!(
                            "warning: cannot migrate legacy Mix script {name}: {error}"
                        )),
                    }
                }
            }
            sources.sort_by(|left, right| left.0.cmp(&right.0));
            for (source_id, source, legacy_directory, source_kind) in sources {
                let mut sidecar_can_remove = true;
                let metadata = if let Some(directory) = legacy_directory.as_deref() {
                    let sidecar = directory.join("metadata.conf.mix");
                    match fs::symlink_metadata(&sidecar) {
                        Ok(_) => match read_legacy_metadata(directory, &source_id) {
                            Ok(metadata) => Some(metadata),
                            Err(error) => {
                                sidecar_can_remove = false;
                                messages.push(format!(
                                    "warning: keeping unreadable legacy metadata {}: {error}",
                                    sidecar.display()
                                ));
                                None
                            }
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => {
                            sidecar_can_remove = false;
                            messages.push(format!(
                                "warning: keeping legacy metadata {} after inspection failed: {error}",
                                sidecar.display()
                            ));
                            None
                        }
                    }
                } else {
                    None
                };
                let fallback = match source_kind {
                    LegacySourceKind::UuidDirectory => short_id(&source_id).to_owned(),
                    LegacySourceKind::FlatV1 => sanitise_legacy_stem(&source_id),
                };
                let base = metadata
                    .as_ref()
                    .map(|value| sanitise_legacy_stem(&value.name))
                    .filter(|value| !value.is_empty())
                    .unwrap_or(fallback);
                let original = fs::metadata(&source).ok();
                let migration = (|| {
                    let stem = deduplicate_stem(&base, &used)?;
                    ensure_mix_shebang(&source)?;
                    if let Some(description) = metadata
                        .as_ref()
                        .map(|value| value.description.as_str())
                        .filter(|value| !value.is_empty())
                    {
                        let description = single_line_description(description);
                        let bytes = read_bounded(&source, SCRIPT_LIMIT)?;
                        if description_from_leading_comments(&bytes).is_empty()
                            && !leading_comments_have_description(&bytes)
                        {
                            rewrite_description(&source, &description)?;
                        }
                    }
                    set_file_mode(&source, FILE_MODE)?;
                    let destination = self.entry(&stem, trashed);
                    secure_rename(&source, &destination, true).map_err(|error| {
                        format!("migrating legacy Mix script {source_id}: {error}")
                    })?;
                    Ok::<_, String>((stem, destination))
                })();
                let (stem, destination) = match migration {
                    Ok(migration) => migration,
                    Err(error) => {
                        messages.push(format!(
                            "warning: cannot migrate legacy Mix script {source_id}: {error}"
                        ));
                        continue;
                    }
                };
                if let Some(original) = original.as_ref() {
                    if let Err(error) = restore_modified_time(&destination, original) {
                        messages.push(format!(
                            "warning: migrated legacy Mix script {source_id}, but {error}"
                        ));
                    }
                }
                if let Some(directory) = legacy_directory {
                    let sidecar = directory.join("metadata.conf.mix");
                    if sidecar_can_remove && fs::symlink_metadata(&sidecar).is_ok() {
                        if let Err(error) =
                            secure_remove_child(&directory, OsStr::new("metadata.conf.mix"), false)
                        {
                            messages.push(format!(
                                "warning: migrated legacy Mix script {source_id}, but could not remove {}: {error}",
                                sidecar.display()
                            ));
                        }
                    }
                    if sidecar_can_remove {
                        if let Err(error) =
                            secure_remove_child(&legacy_category, OsStr::new(&source_id), true)
                        {
                            messages.push(format!(
                                "warning: migrated legacy Mix script {source_id}, but could not remove {}: {error}",
                                directory.display()
                            ));
                        }
                    }
                }
                used.insert(stem.clone());
                messages.push(format!(
                    "migrated legacy Mix script {source_id} to {}",
                    destination.display()
                ));
            }
        }
        for child in [legacy_root.join("scripts"), legacy_root.join("trash")] {
            let _ = remove_directory_if_empty(&child);
        }
        if !remove_directory_if_empty(legacy_root)? && legacy_root.exists() {
            messages.push(format!(
                "warning: legacy Mix tree {} is not empty and was left in place",
                legacy_root.display()
            ));
        }
        Ok(messages)
    }
}

#[derive(Clone, Copy)]
enum LegacySourceKind {
    UuidDirectory,
    FlatV1,
}

pub(crate) struct MixPublication {
    pub(crate) revision: u64,
    pub(crate) catalogue_changed: bool,
    pub(crate) run_ids: Vec<String>,
    pub(crate) output_run_id: String,
    pub(crate) output: Vec<WireMixOutput>,
    pub(crate) stdout_dropped: u64,
    pub(crate) stderr_dropped: u64,
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
}

#[derive(Clone, Default)]
struct StartupProblems {
    ensure_failure: String,
    migration_warnings: String,
}

impl StartupProblems {
    fn is_empty(&self) -> bool {
        self.ensure_failure.is_empty() && self.migration_warnings.is_empty()
    }

    fn compose(&self, current: &str) -> String {
        [
            self.ensure_failure.as_str(),
            self.migration_warnings.as_str(),
            current,
        ]
        .into_iter()
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
    }
}

pub(crate) struct MixController {
    store: MixStore,
    state: Mutex<MixState>,
    operations: Mutex<()>,
    runner: Arc<dyn MixRunner>,
    runner_events: SyncSender<RunnerEvent>,
    publish_tx: SyncSender<()>,
    publish_rx: Mutex<Option<Receiver<()>>>,
    watcher_lifecycle: Mutex<WatcherLifecycle>,
    watcher_ready: AtomicBool,
    startup_problems: Mutex<StartupProblems>,
    #[cfg(test)]
    test_create_gate: Mutex<Option<Arc<TestOpenGate>>>,
}

impl MixController {
    pub(crate) fn new_default() -> Arc<Self> {
        let store = MixStore::default_store().unwrap_or_else(|_| {
            MixStore::new(PathBuf::from("/nonexistent/cosmix-trayd-home/.local/mix"))
        });
        Self::new_with_store(store, Arc::new(SystemdRunner))
    }

    #[cfg(test)]
    pub(crate) fn new_test(path: PathBuf) -> Arc<Self> {
        Self::new_with(path, Arc::new(SystemdRunner))
    }

    #[cfg(test)]
    fn new_with(path: PathBuf, runner: Arc<dyn MixRunner>) -> Arc<Self> {
        Self::new_with_store(MixStore::new(path), runner)
    }

    fn new_with_store(store: MixStore, runner: Arc<dyn MixRunner>) -> Arc<Self> {
        let (runner_events, event_rx) = mpsc::sync_channel(RUNNER_EVENT_CAPACITY);
        let (publish_tx, publish_rx) = mpsc::sync_channel(1);
        let controller = Arc::new(Self {
            store,
            state: Mutex::new(MixState::default()),
            operations: Mutex::new(()),
            runner,
            runner_events,
            publish_tx,
            publish_rx: Mutex::new(Some(publish_rx)),
            watcher_lifecycle: Mutex::new(WatcherLifecycle::default()),
            watcher_ready: AtomicBool::new(false),
            startup_problems: Mutex::new(StartupProblems::default()),
            #[cfg(test)]
            test_create_gate: Mutex::new(None),
        });
        Self::start_event_worker(&controller, event_rx);
        let mut startup_problems = StartupProblems::default();
        let store_ready = match controller.store.ensure() {
            Err(error) => {
                let error = format!("cannot prepare Mix catalogue: {error}");
                eprintln!("cosmix-trayd: {error}");
                startup_problems.ensure_failure = error;
                false
            }
            Ok(()) => {
                for category in [controller.store.root.clone(), controller.store.trash()] {
                    match sweep_rewrite_temporaries(&category) {
                        Ok(warnings) => {
                            for warning in warnings {
                                eprintln!("cosmix-trayd: {warning}");
                            }
                        }
                        Err(error) => eprintln!(
                            "cosmix-trayd: cannot sweep stale Mix rewrite files in {}: {error}",
                            category.display()
                        ),
                    }
                }
                true
            }
        };
        let mut migration_warnings = Vec::new();
        if store_ready && controller.store.legacy_exists() {
            match controller.store.migrate_legacy() {
                Ok(migrations) => {
                    for migration in migrations {
                        eprintln!("cosmix-trayd: {migration}");
                        if migration.starts_with("warning:") {
                            migration_warnings.push(migration);
                        }
                    }
                }
                Err(error) => {
                    let error = format!("Mix catalogue migration failed: {error}");
                    eprintln!("cosmix-trayd: {error}");
                    migration_warnings.push(error);
                }
            }
        }
        startup_problems.migration_warnings = migration_warnings.join("; ");
        *controller
            .startup_problems
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = startup_problems;
        controller.rescan();
        controller.start_watcher();
        controller
    }

    #[cfg(test)]
    pub(crate) fn block_next_create(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_create_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    pub(crate) fn reconcile_orphans(&self) -> Result<(), String> {
        self.runner.reconcile()
    }

    fn state(&self) -> MutexGuard<'_, MixState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
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

    pub(crate) fn active_runs(&self) -> u32 {
        self.state().runs.iter().filter(|run| run.active()).count() as u32
    }

    pub(crate) fn snapshot(&self) -> WireMixSnapshot {
        let state = self.state();
        WireMixSnapshot {
            revision: state.revision,
            state: state.state.clone(),
            error: state.error.clone(),
            scripts: state.scripts.iter().map(ScriptEntry::wire).collect(),
            runs: state.runs.iter().rev().map(RunRecord::wire).collect(),
            active_runs: state.runs.iter().filter(|run| run.active()).count() as u32,
        }
    }

    pub(crate) fn create(
        self: &Arc<Self>,
        name: &str,
        description: &str,
    ) -> Result<String, MixError> {
        let _operation = self.operation();
        #[cfg(test)]
        if let Some(gate) = self
            .test_create_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            gate.block();
        }
        let metadata = self.store.create(name, description)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(metadata.id)
    }

    pub(crate) fn update(
        self: &Arc<Self>,
        id: &str,
        name: &str,
        description: &str,
    ) -> Result<(), MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        self.require_script(&id, false)?;
        if self
            .state()
            .runs
            .iter()
            .any(|run| run.script_id == id && run.active())
        {
            return Err(MixError::MixScriptBusy(format!(
                "script {id} has an active run"
            )));
        }
        self.store.update(&id, name, description)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn trash(self: &Arc<Self>, id: &str) -> Result<(), MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        if self.find_script(&id, false).is_some()
            && self.store.path_exists(&self.store.entry(&id, true))
        {
            return Err(MixError::MixTrashCollision(format!(
                "Mix script {id} already exists in .trash"
            )));
        }
        if self.find_script(&id, true).is_some() {
            return Err(MixError::MixAlreadyTrashed(format!(
                "script {id} is already in trash"
            )));
        }
        self.require_script(&id, false)?;
        if self
            .state()
            .runs
            .iter()
            .any(|run| run.script_id == id && run.active())
        {
            return Err(MixError::MixScriptBusy(format!(
                "script {id} has an active run"
            )));
        }
        self.store.move_entry(&id, false)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn restore(self: &Arc<Self>, id: &str) -> Result<(), MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        if self.find_script(&id, false).is_some()
            && self.store.path_exists(&self.store.entry(&id, true))
        {
            return Err(MixError::MixScriptExists(format!(
                "Mix script {id} already exists in the store root"
            )));
        }
        if self.find_script(&id, false).is_some() {
            return Err(MixError::MixNotTrashed(format!(
                "script {id} is not in trash"
            )));
        }
        self.require_script(&id, true)?;
        self.store.move_entry(&id, true)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn purge(self: &Arc<Self>, id: &str) -> Result<(), MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        self.require_script(&id, true)?;
        self.store.purge(&id).map_err(MixError::MixStoreFailure)?;
        self.rescan_locked();
        drop(_operation);
        self.start_watcher();
        Ok(())
    }

    pub(crate) fn edit_path(self: &Arc<Self>, id: &str) -> Result<PathBuf, MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        if self.find_script(&id, true).is_some() {
            return Err(MixError::MixScriptTrashed(format!(
                "script {id} is in trash"
            )));
        }
        self.require_script(&id, false)?;
        let path = self
            .store
            .script_path(&id, false)
            .map_err(MixError::MixStoreFailure)?;
        drop(_operation);
        self.start_watcher();
        Ok(path)
    }

    pub(crate) fn run(self: &Arc<Self>, id: &str) -> Result<String, MixError> {
        self.start_watcher();
        let _operation = self.operation();
        let id = canonical_script_id(id).map_err(MixError::InvalidMixId)?;
        if self.find_script(&id, true).is_some() {
            return Err(MixError::MixScriptTrashed(format!(
                "script {id} is in trash"
            )));
        }
        let script = self.require_script(&id, false)?;
        let (script_handle, working_directory_handle) = self
            .store
            .script_source(&id, false)
            .map_err(MixError::MixStoreFailure)?;
        let run_id = Uuid::new_v4().to_string();
        let unit = format!("cosmix-mix-run-{run_id}.service");
        {
            let mut state = self.state();
            if state.runs.iter().filter(|run| run.active()).count() >= MAX_ACTIVE_RUNS {
                return Err(MixError::MixRunLimit(format!(
                    "at most {MAX_ACTIVE_RUNS} Mix runs may be active"
                )));
            }
            while state.runs.len() >= MAX_RUN_HISTORY {
                let Some(index) = state.runs.iter().position(|run| !run.active()) else {
                    break;
                };
                state.runs.remove(index);
            }
            state.runs.push_back(RunRecord {
                id: run_id.clone(),
                script_id: id,
                script_name: script.metadata.name,
                unit: unit.clone(),
                state: "starting".into(),
                started_ms: now_ms(),
                finished_ms: 0,
                exit_code: None,
                stdout: OutputTail::default(),
                stderr: OutputTail::default(),
                stdout_signal_dropped: 0,
                stderr_signal_dropped: 0,
                next_sequence: 1,
            });
            mark_run_changed(&mut state, &run_id);
        }
        self.wake_publisher();

        let request = RunRequest {
            run_id: run_id.clone(),
            unit,
            script_handle,
            working_directory_handle,
        };
        if let Err(error) = self.runner.start(request, self.runner_events.clone()) {
            let mut state = self.state();
            if let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) {
                run.state = "launch_failed".into();
                run.finished_ms = now_ms();
                run.stderr.push(format!("{error}\n"));
            }
            mark_run_changed(&mut state, &run_id);
            drop(state);
            self.wake_publisher();
            return Err(MixError::MixLaunchFailure(error));
        }
        {
            let mut state = self.state();
            if let Some(run) = state
                .runs
                .iter_mut()
                .find(|run| run.id == run_id && run.state == "starting")
            {
                run.state = "running".into();
                mark_run_changed(&mut state, &run_id);
            }
        }
        self.wake_publisher();
        drop(_operation);
        self.start_watcher();
        Ok(run_id)
    }

    pub(crate) fn stop(&self, run_id: &str) -> Result<(), MixError> {
        let _operation = self.operation();
        let run_id = canonical_run_id(run_id).map_err(MixError::InvalidMixId)?;
        let unit = {
            let mut state = self.state();
            let run = state
                .runs
                .iter_mut()
                .find(|run| run.id == run_id)
                .ok_or_else(|| MixError::UnknownMixRun(format!("unknown Mix run: {run_id}")))?;
            if !run.active() || run.state == "stopping" {
                return Err(MixError::MixRunNotActive(format!(
                    "Mix run {run_id} is not running"
                )));
            }
            run.state = "stopping".into();
            let unit = run.unit.clone();
            mark_run_changed(&mut state, &run_id);
            unit
        };
        self.wake_publisher();
        if let Err(error) = self.runner.stop(&run_id, &unit, self.runner_events.clone()) {
            let mut state = self.state();
            if let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) {
                run.state = "running".into();
            }
            mark_run_changed(&mut state, &run_id);
            drop(state);
            self.wake_publisher();
            return Err(MixError::MixLaunchFailure(error));
        }
        Ok(())
    }

    pub(crate) fn take_publish_receiver(&self) -> Receiver<()> {
        self.publish_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("Mix publisher receiver may only be taken once")
    }

    pub(crate) fn take_publication(&self) -> Option<MixPublication> {
        let mut state = self.state();
        if !state.catalogue_pending
            && state.run_pending.is_empty()
            && state.outputs_pending.is_empty()
        {
            return None;
        }
        let catalogue_changed = std::mem::take(&mut state.catalogue_pending);
        let run_ids = std::mem::take(&mut state.run_pending)
            .into_iter()
            .collect::<Vec<_>>();
        let output_run_id = state
            .outputs_pending
            .front()
            .map(|pending| pending.run_id.clone())
            .unwrap_or_default();
        let mut output = Vec::new();
        let mut output_bytes = 0;
        while output.len() < MAX_SIGNAL_CHUNKS {
            let Some(front) = state.outputs_pending.front() else {
                break;
            };
            if front.run_id != output_run_id
                || (!output.is_empty() && output_bytes + front.bytes > MAX_SIGNAL_BYTES)
            {
                break;
            }
            let pending = state.outputs_pending.pop_front().expect("front exists");
            state.outputs_pending_bytes = state.outputs_pending_bytes.saturating_sub(pending.bytes);
            output_bytes += pending.bytes;
            output.push(pending.chunk);
        }
        let (stdout_dropped, stderr_dropped) = state
            .runs
            .iter()
            .find(|run| run.id == output_run_id)
            .map(|run| {
                (
                    run.stdout.dropped.saturating_add(run.stdout_signal_dropped),
                    run.stderr.dropped.saturating_add(run.stderr_signal_dropped),
                )
            })
            .unwrap_or_default();
        let publication = MixPublication {
            revision: state.revision,
            catalogue_changed,
            run_ids,
            output_run_id,
            output,
            stdout_dropped,
            stderr_dropped,
        };
        let more = state.catalogue_pending
            || !state.run_pending.is_empty()
            || !state.outputs_pending.is_empty();
        drop(state);
        if more {
            self.wake_publisher();
        }
        Some(publication)
    }

    fn require_script(&self, id: &str, trashed: bool) -> Result<ScriptEntry, MixError> {
        self.find_script(id, trashed)
            .ok_or_else(|| MixError::UnknownMixScript(format!("unknown Mix script: {id}")))
    }

    fn find_script(&self, id: &str, trashed: bool) -> Option<ScriptEntry> {
        self.state()
            .scripts
            .iter()
            .find(|script| script.metadata.id == id && script.trashed == trashed)
            .cloned()
    }

    fn operation(&self) -> MutexGuard<'_, ()> {
        // D-Bus callers wait here from blocking::unblock, so queued Mix work
        // can occupy the shared blocking pool. This deliberately favours a
        // free zbus executor over introducing a dedicated request worker and
        // its queue, shutdown, and reply-failure lifecycle this late.
        self.operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn rescan(&self) {
        let _operation = self.operation();
        self.rescan_locked();
    }

    fn rescan_locked(&self) {
        let mut startup_problems = self
            .startup_problems
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !startup_problems.ensure_failure.is_empty() {
            match self.store.ensure() {
                Ok(()) => {
                    startup_problems.ensure_failure.clear();
                    for category in [self.store.root.clone(), self.store.trash()] {
                        match sweep_rewrite_temporaries(&category) {
                            Ok(warnings) => {
                                for warning in warnings {
                                    eprintln!("cosmix-trayd: {warning}");
                                }
                            }
                            Err(error) => eprintln!(
                                "cosmix-trayd: cannot sweep stale Mix rewrite files in {}: {error}",
                                category.display()
                            ),
                        }
                    }
                    // Recovery stays simple: migration after a failed startup
                    // ensure is deferred until the next trayd restart.
                }
                Err(error) => {
                    startup_problems.ensure_failure =
                        format!("cannot prepare Mix catalogue: {error}")
                }
            }
        }
        if !startup_problems.migration_warnings.is_empty() && !self.store.legacy_exists() {
            startup_problems.migration_warnings.clear();
        }
        let scan = self.store.scan();
        let mut state = self.state();
        match scan {
            Ok((scripts, warning)) => {
                let error = startup_problems.compose(&warning);
                let status = if startup_problems.is_empty() {
                    "watching"
                } else {
                    "degraded"
                };
                let changed =
                    state.scripts != scripts || state.state != status || state.error != error;
                if changed {
                    state.scripts = scripts;
                    state.state = status.into();
                    state.error = error;
                    mark_catalogue_changed(&mut state);
                }
            }
            Err(error) => {
                let error = startup_problems.compose(&error);
                let changed = state.state != "degraded" || state.error != error;
                if changed {
                    state.state = "degraded".into();
                    state.error = error;
                    mark_catalogue_changed(&mut state);
                }
            }
        }
        let changed = state.catalogue_pending;
        drop(state);
        drop(startup_problems);
        if changed {
            self.wake_publisher();
        }
    }

    fn start_watcher(self: &Arc<Self>) {
        let generation = {
            let mut lifecycle = match self.watcher_lifecycle.lock() {
                Ok(lifecycle) => lifecycle,
                Err(poisoned) => poisoned.into_inner(),
            };
            lifecycle.request_start()
        };
        let Some(generation) = generation else {
            return;
        };
        self.spawn_watcher(generation);
    }

    fn spawn_watcher(self: &Arc<Self>, generation: u64) {
        let weak = Arc::downgrade(self);
        if let Err(error) = thread::Builder::new()
            .name("cosmix-trayd-mix-inotify".into())
            .spawn(move || watch_catalogue(weak, generation))
        {
            self.watcher_failed(
                generation,
                format!("cannot start Mix catalogue watcher: {error}"),
            );
        }
    }

    fn set_watcher_ready(&self, generation: u64, ready: bool) {
        let current = {
            let lifecycle = match self.watcher_lifecycle.lock() {
                Ok(lifecycle) => lifecycle,
                Err(poisoned) => poisoned.into_inner(),
            };
            lifecycle.running && !lifecycle.failing && lifecycle.generation == generation
        };
        if current {
            self.watcher_ready.store(ready, Ordering::Release);
        }
    }

    fn watcher_failed(self: &Arc<Self>, generation: u64, error: String) {
        let publish = {
            let mut lifecycle = match self.watcher_lifecycle.lock() {
                Ok(lifecycle) => lifecycle,
                Err(poisoned) => poisoned.into_inner(),
            };
            lifecycle.begin_failure(generation)
        };
        if !publish {
            return;
        }

        // Ownership is already in the failing hand-off state before the
        // degraded event is published. A concurrent catalogue operation now
        // records restart_requested instead of observing a stale "running".
        self.watcher_ready.store(false, Ordering::Release);
        self.set_watch_error(error);

        let restart_generation = {
            let mut lifecycle = match self.watcher_lifecycle.lock() {
                Ok(lifecycle) => lifecycle,
                Err(poisoned) => poisoned.into_inner(),
            };
            lifecycle.complete_failure(generation)
        };
        if let Some(restart_generation) = restart_generation {
            self.spawn_watcher(restart_generation);
        }
    }

    fn set_watch_error(&self, error: String) {
        let startup_problems = self
            .startup_problems
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let error = startup_problems.compose(&error);
        let mut state = self.state();
        state.state = "degraded".into();
        state.error = error;
        mark_catalogue_changed(&mut state);
        drop(state);
        drop(startup_problems);
        self.wake_publisher();
    }

    fn start_event_worker(controller: &Arc<Self>, receiver: Receiver<RunnerEvent>) {
        let weak = Arc::downgrade(controller);
        thread::Builder::new()
            .name("cosmix-trayd-mix-events".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let Some(controller) = weak.upgrade() else {
                        return;
                    };
                    controller.apply_runner_event(event);
                }
            })
            .expect("cannot start Mix event worker");
    }

    fn apply_runner_event(&self, event: RunnerEvent) {
        match event {
            RunnerEvent::Output {
                run_id,
                stream,
                bytes,
            } => {
                for text in bounded_output_text(&bytes) {
                    let mut state = self.state();
                    let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
                        continue;
                    };
                    let sequence = run.next_sequence;
                    run.next_sequence = run.next_sequence.saturating_add(1);
                    match stream {
                        OutputStream::Stdout => run.stdout.push(text.clone()),
                        OutputStream::Stderr => run.stderr.push(text.clone()),
                    }
                    state.revision = state.revision.saturating_add(1);
                    let bytes = text.len();
                    state.outputs_pending_bytes = state.outputs_pending_bytes.saturating_add(bytes);
                    state.outputs_pending.push_back(PendingOutput {
                        run_id: run_id.clone(),
                        chunk: (sequence, stream.label().into(), text),
                        bytes,
                    });
                    while state.outputs_pending_bytes > MAX_PENDING_SIGNAL_BYTES {
                        let Some(removed) = state.outputs_pending.pop_front() else {
                            break;
                        };
                        state.outputs_pending_bytes =
                            state.outputs_pending_bytes.saturating_sub(removed.bytes);
                        if let Some(run) =
                            state.runs.iter_mut().find(|run| run.id == removed.run_id)
                        {
                            match removed.chunk.1.as_str() {
                                "stdout" => {
                                    run.stdout_signal_dropped = run
                                        .stdout_signal_dropped
                                        .saturating_add(removed.bytes as u64);
                                }
                                "stderr" => {
                                    run.stderr_signal_dropped = run
                                        .stderr_signal_dropped
                                        .saturating_add(removed.bytes as u64);
                                }
                                _ => {}
                            }
                        }
                    }
                    drop(state);
                    self.wake_publisher();
                }
            }
            RunnerEvent::Finished {
                run_id,
                exit_code,
                error,
            } => {
                let mut state = self.state();
                let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
                    return;
                };
                if let Some(error) = error {
                    run.stderr.push(format!("{error}\n"));
                    run.state = "launch_failed".into();
                } else if run.state == "stopping" {
                    run.state = "stopped".into();
                } else if exit_code == Some(0) {
                    run.state = "succeeded".into();
                } else {
                    run.state = "failed".into();
                }
                run.exit_code = exit_code;
                run.finished_ms = now_ms();
                mark_run_changed(&mut state, &run_id);
                drop(state);
                self.wake_publisher();
            }
            RunnerEvent::StopFailed { run_id, error } => {
                let mut state = self.state();
                let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
                    return;
                };
                if run.state != "stopping" {
                    return;
                }
                run.state = "running".into();
                run.stderr.push(format!("{error}\n"));
                mark_run_changed(&mut state, &run_id);
                drop(state);
                self.wake_publisher();
            }
        }
    }

    fn wake_publisher(&self) {
        match self.publish_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {}
        }
    }
}

fn mark_catalogue_changed(state: &mut MixState) {
    state.revision = state.revision.saturating_add(1);
    state.catalogue_pending = true;
}

fn bounded_output_text(bytes: &[u8]) -> Vec<String> {
    let lossy = String::from_utf8_lossy(bytes);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for character in lossy.chars() {
        if !chunk.is_empty() && chunk.len() + character.len_utf8() > MAX_OUTPUT_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn mark_run_changed(state: &mut MixState, run_id: &str) {
    state.revision = state.revision.saturating_add(1);
    state.run_pending.insert(run_id.to_owned());
}

fn watch_catalogue(weak: Weak<MixController>, generation: u64) {
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
    if let Err(error) = stabilise_catalogue_watches(&controller, &mut inotify, &mut buffer) {
        controller.watcher_failed(generation, error);
        return;
    }
    controller.set_watcher_ready(generation, true);
    drop(controller);

    loop {
        if let Err(error) = inotify.read_events_blocking(&mut buffer) {
            if let Some(controller) = weak.upgrade() {
                controller
                    .watcher_failed(generation, format!("Mix catalogue watch failed: {error}"));
            }
            return;
        }
        let Some(controller) = weak.upgrade() else {
            return;
        };
        controller.set_watcher_ready(generation, false);
        if let Err(error) = stabilise_catalogue_watches(&controller, &mut inotify, &mut buffer) {
            controller.watcher_failed(generation, error);
            return;
        }
        controller.set_watcher_ready(generation, true);
    }
}

fn stabilise_catalogue_watches(
    controller: &MixController,
    inotify: &mut Inotify,
    buffer: &mut [u8],
) -> Result<(), String> {
    let base_mask = WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::CLOSE_WRITE
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF;
    // Parent watches land before the scan. In-place writes, atomic saves and
    // chmod changes therefore queue an edge below. Scans no longer chmod
    // files, so ATTRIB cannot feed back into this loop.
    for path in controller.store.watch_base_paths()? {
        inotify
            .watches()
            .add(&path, base_mask)
            .map_err(|error| format!("cannot watch {}: {error}", path.display()))?;
    }
    loop {
        controller.rescan();

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
                Err(error) => return Err(format!("Mix catalogue watch drain failed: {error}")),
            }
        }
        if drained == 0 {
            return Ok(());
        }
        // A queued edge drives another rescan pass. No clock participates in
        // convergence, and flat files need no per-entry watch re-arming.
    }
}

fn canonical_script_id(id: &str) -> Result<String, String> {
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
            "invalid Mix script identity {id:?}: expected 1-64 characters, an alphanumeric first character, only A-Z/a-z/0-9/./_/- thereafter, and no '..'"
        ));
    }
    Ok(id.to_owned())
}

fn canonical_run_id(id: &str) -> Result<String, String> {
    let parsed = Uuid::parse_str(id).map_err(|_| format!("invalid UUID identity: {id}"))?;
    let canonical = parsed.to_string();
    if id != canonical {
        return Err(format!("UUID identity is not canonical: {id}"));
    }
    Ok(canonical)
}

fn sanitise_description(description: &str) -> Result<String, String> {
    let sanitised = single_line_description(description);
    if sanitised.chars().count() > 500
        || sanitised
            .chars()
            .any(|character| character.is_control() && character != '\t')
    {
        return Err("script description must be at most 500 printable characters".into());
    }
    Ok(sanitised)
}

fn single_line_description(description: &str) -> String {
    let mut sanitised = String::with_capacity(description.len());
    let mut in_newline = false;
    for character in description.chars() {
        if matches!(character, '\n' | '\r') {
            if !in_newline {
                sanitised.push(' ');
                in_newline = true;
            }
        } else {
            in_newline = false;
            sanitised.push(character);
        }
    }
    sanitised
}

fn leading_comment_lines(bytes: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut lines = Vec::new();
    let mut start = shebang_line_end(bytes).unwrap_or(0);
    while bytes
        .get(start..)
        .is_some_and(|line| line.starts_with(b"--"))
    {
        let newline = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset);
        let line_end = newline.unwrap_or(bytes.len());
        let content_end = if line_end > start && bytes[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        let next = newline.map_or(bytes.len(), |index| index + 1);
        lines.push((start, content_end, next));
        if next >= bytes.len() {
            break;
        }
        start = next;
    }
    lines
}

fn shebang_line_end(bytes: &[u8]) -> Option<usize> {
    let shebang_start = if bytes.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    if !bytes[shebang_start..].starts_with(b"#!") {
        return None;
    }
    Some(
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |newline| newline + 1),
    )
}

fn leading_description_range(bytes: &[u8]) -> Option<(usize, usize, usize)> {
    const PREFIX: &[u8] = b"-- description:";
    leading_comment_lines(bytes)
        .into_iter()
        .find(|(start, content_end, _)| bytes[*start..*content_end].starts_with(PREFIX))
}

fn leading_comments_have_description(bytes: &[u8]) -> bool {
    leading_description_range(bytes).is_some()
}

fn description_from_leading_comments(bytes: &[u8]) -> String {
    const PREFIX: &[u8] = b"-- description:";
    let Some((start, content_end, _)) = leading_description_range(bytes) else {
        return String::new();
    };
    let mut value = &bytes[start + PREFIX.len()..content_end];
    if value.first() == Some(&b' ') {
        value = &value[1..];
    }
    String::from_utf8_lossy(value)
        .chars()
        .filter_map(|character| {
            if character == '\t' {
                Some(' ')
            } else if character.is_control() {
                None
            } else {
                Some(character)
            }
        })
        .take(500)
        .collect()
}

fn rewrite_description(path: &Path, description: &str) -> Result<(), String> {
    let header = format!("-- description: {description}");
    rewrite_script_bytes(path, |bytes| {
        if let Some((start, content_end, _)) = leading_description_range(bytes) {
            let mut rewritten = Vec::with_capacity(bytes.len() + header.len());
            rewritten.extend_from_slice(&bytes[..start]);
            rewritten.extend_from_slice(header.as_bytes());
            rewritten.extend_from_slice(&bytes[content_end..]);
            return rewritten;
        }

        let insertion = shebang_line_end(bytes).unwrap_or(0);
        let mut rewritten = Vec::with_capacity(bytes.len() + header.len() + 2);
        rewritten.extend_from_slice(&bytes[..insertion]);
        if insertion == bytes.len() && insertion > 0 && !bytes.ends_with(b"\n") {
            rewritten.push(b'\n');
        }
        rewritten.extend_from_slice(header.as_bytes());
        rewritten.push(b'\n');
        rewritten.extend_from_slice(&bytes[insertion..]);
        rewritten
    })
}

fn ensure_mix_shebang(path: &Path) -> Result<(), String> {
    rewrite_script_bytes(path, |bytes| {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        if bytes.starts_with(b"#!") {
            return bytes.to_vec();
        }
        let mut rewritten = Vec::with_capacity(bytes.len() + MIX_BINARY.len() + 3);
        rewritten.extend_from_slice(format!("#!{MIX_BINARY}\n").as_bytes());
        rewritten.extend_from_slice(bytes);
        rewritten
    })
}

fn rewrite_script_bytes(
    path: &Path,
    transform: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Result<(), String> {
    let mut file = secure_open(path, libc::O_RDWR, 0)
        .map_err(|error| format!("opening {} for metadata update: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("refusing unsafe file {}", path.display()));
    }
    if metadata.len() > SCRIPT_LIMIT as u64 {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            SCRIPT_LIMIT
        ));
    }
    check_private_file_mode(&metadata, path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(SCRIPT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    if bytes.len() > SCRIPT_LIMIT {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            SCRIPT_LIMIT
        ));
    }
    let rewritten = transform(&bytes);
    if rewritten.len() > SCRIPT_LIMIT {
        return Err(format!(
            "{} exceeds the {} byte limit after metadata update",
            path.display(),
            SCRIPT_LIMIT
        ));
    }
    if rewritten == bytes {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let temporary = parent.join(format!(".rewrite-{}.tmp", Uuid::new_v4()));
    create_file(&temporary, &rewritten).map_err(|error| match error {
        CreateFileError::Exists => format!("temporary file {} already exists", temporary.display()),
        CreateFileError::Failure(error) => error,
    })?;
    let opened_metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            let cleanup = cleanup_rewrite_temporary(parent, &temporary);
            return Err(match cleanup {
                Ok(()) => format!("rechecking {} before update: {error}", path.display()),
                Err(cleanup) => format!(
                    "rechecking {} before update: {error}; temporary cleanup failed: {cleanup}",
                    path.display()
                ),
            });
        }
    };
    let current_metadata = fs::metadata(path);
    let path_is_still_opened_inode = current_metadata.as_ref().is_ok_and(|current| {
        opened_metadata.dev() == current.dev() && opened_metadata.ino() == current.ino()
    });
    if !path_is_still_opened_inode {
        let cleanup = cleanup_rewrite_temporary(parent, &temporary);
        return Err(match cleanup {
            Ok(()) => "script changed during update; retry".into(),
            Err(cleanup) => {
                format!("script changed during update; retry; temporary cleanup failed: {cleanup}")
            }
        });
    }
    // External editors take no catalogue lock. A replacement can still land in
    // the microsecond window between this inode check and renameat2; this check
    // is the best honest bound without imposing a cross-process lock protocol.
    if let Err(error) = secure_rename(&temporary, path, false) {
        let cleanup = cleanup_rewrite_temporary(parent, &temporary);
        return Err(match cleanup {
            Ok(()) => format!("installing rewritten script {}: {error}", path.display()),
            Err(cleanup) => format!(
                "installing rewritten script {}: {error}; temporary cleanup failed: {cleanup}",
                path.display()
            ),
        });
    }
    Ok(())
}

fn cleanup_rewrite_temporary(parent: &Path, temporary: &Path) -> Result<(), String> {
    let name = temporary
        .file_name()
        .ok_or_else(|| format!("{} has no file name", temporary.display()))?;
    secure_remove_child(parent, name, false)
}

fn read_legacy_metadata(
    directory: &Path,
    expected_id: &str,
) -> Result<LegacyScriptMetadata, String> {
    let bytes = read_bounded(&directory.join("metadata.conf.mix"), METADATA_LIMIT)?;
    let text =
        String::from_utf8(bytes).map_err(|_| "metadata.conf.mix is not valid UTF-8".to_owned())?;
    let metadata: LegacyScriptMetadata = cosmix_mix::from_conf_mix_str(&text)
        .map_err(|error| format!("invalid strict-data metadata: {error}"))?;
    if metadata.schema != 1 || metadata.id != expected_id {
        return Err("legacy metadata identity or schema is invalid".into());
    }
    Ok(metadata)
}

fn sanitise_legacy_stem(name: &str) -> String {
    slugify_name(name.strip_suffix(".mix").unwrap_or(name))
}

fn slugify_name(name: &str) -> String {
    let mut stem = String::with_capacity(name.len().min(64));
    for character in name.chars() {
        let candidate = if character.is_ascii_whitespace() {
            Some('-')
        } else if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            Some(character)
        } else {
            None
        };
        let Some(character) = candidate else {
            continue;
        };
        if stem.is_empty() && !character.is_ascii_alphanumeric() {
            continue;
        }
        if character == '.' && stem.ends_with('.') {
            continue;
        }
        if stem.len() == 64 {
            break;
        }
        stem.push(character);
    }
    stem
}

fn deduplicate_stem(base: &str, used: &BTreeSet<String>) -> Result<String, String> {
    if !used.contains(base) {
        return Ok(base.to_owned());
    }
    for sequence in 2_u64..=u64::MAX {
        let suffix = format!("-{sequence}");
        let prefix_len = 64_usize.saturating_sub(suffix.len()).min(base.len());
        let candidate = format!("{}{}", &base[..prefix_len], suffix);
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot allocate a unique Mix script identity derived from {base:?}"
    ))
}

fn file_times_ms(file: &File, fallback: &fs::Metadata) -> (u64, u64) {
    // SAFETY: statx is a plain output structure initialised before the syscall.
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the empty C string is valid, AT_EMPTY_PATH addresses file's fd,
    // and stat remains writable for the duration of the call.
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_BTIME | libc::STATX_MTIME,
            &mut stat,
        )
    };
    let fallback_ms = metadata_mtime_ms(fallback);
    if result != 0 {
        return (fallback_ms, fallback_ms);
    }
    let updated_ms = if stat.stx_mask & libc::STATX_MTIME != 0 {
        statx_timestamp_ms(&stat.stx_mtime)
    } else {
        fallback_ms
    };
    let created_ms = if stat.stx_mask & libc::STATX_BTIME != 0 {
        statx_timestamp_ms(&stat.stx_btime)
    } else {
        updated_ms
    };
    (created_ms, updated_ms)
}

fn statx_timestamp_ms(timestamp: &libc::statx_timestamp) -> u64 {
    if timestamp.tv_sec < 0 {
        return 0;
    }
    (timestamp.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add((timestamp.tv_nsec as u64) / 1_000_000)
}

fn metadata_mtime_ms(metadata: &fs::Metadata) -> u64 {
    if metadata.mtime() < 0 {
        return 0;
    }
    (metadata.mtime() as u64)
        .saturating_mul(1000)
        .saturating_add((metadata.mtime_nsec().max(0) as u64) / 1_000_000)
}

fn restore_modified_time(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    let file = secure_regular_file(path)?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    // SAFETY: file owns a valid descriptor and times contains two timespecs.
    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "restoring {} mtime: {}",
            path.display(),
            std::io::Error::last_os_error()
        ))
    }
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    let directory = match secure_open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_directory_chain(path)?;
            secure_open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|error| secure_directory_error(path, error))?
        }
        Err(error) => return Err(secure_directory_error(path, error)),
    };
    fchmod(&directory, DIRECTORY_MODE)
        .map_err(|error| format!("setting mode on {}: {error}", path.display()))
}

fn create_directory_chain(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{} is not absolute", path.display()));
    }
    let mut current = File::open("/").map_err(|error| format!("opening /: {error}"))?;
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        let next = match secure_open_at(
            current.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        ) {
            Ok(next) => next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                mkdir_at(current.as_raw_fd(), name, DIRECTORY_MODE)?;
                let created = secure_open_at(
                    current.as_raw_fd(),
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )
                .map_err(|error| secure_directory_error(path, error))?;
                fchmod(&created, DIRECTORY_MODE)
                    .map_err(|error| format!("setting directory mode: {error}"))?;
                created
            }
            Err(error) => return Err(secure_directory_error(path, error)),
        };
        current = next;
    }
    Ok(())
}

fn check_directory(path: &Path) -> Result<(), String> {
    secure_open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map(|_| ())
        .map_err(|error| secure_directory_error(path, error))
}

fn check_regular_file(path: &Path, limit: usize) -> Result<(), String> {
    let file = secure_regular_file(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting {}: {error}", path.display()))?;
    if metadata.len() > limit as u64 {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            limit
        ));
    }
    check_private_file_mode(&metadata, path)
}

fn check_private_file_mode(metadata: &fs::Metadata, path: &Path) -> Result<(), String> {
    let mode = metadata.mode() & 0o777;
    if mode & GROUP_OTHER_MODE_MASK != 0 {
        return Err(format!(
            "refusing group/other-accessible file {} with mode {mode:04o}",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum CreateFileError {
    Exists,
    Failure(String),
}

fn create_file(path: &Path, bytes: &[u8]) -> Result<(), CreateFileError> {
    create_file_with(path, |file| {
        file.write_all(bytes)
            .map_err(|error| format!("writing {}: {error}", path.display()))
    })
}

fn create_file_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<(), CreateFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| CreateFileError::Failure(format!("{} has no parent", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| CreateFileError::Failure(format!("{} has no file name", path.display())))?;
    let directory = secure_open(parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|error| CreateFileError::Failure(secure_directory_error(parent, error)))?;
    let mut file = secure_open_at(
        directory.as_raw_fd(),
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        FILE_MODE,
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            CreateFileError::Exists
        } else {
            CreateFileError::Failure(format!("creating {}: {error}", path.display()))
        }
    })?;
    let result = (|| {
        fchmod(&file, FILE_MODE)
            .map_err(|error| format!("setting mode on {}: {error}", path.display()))?;
        write(&mut file)?;
        file.sync_all()
            .map_err(|error| format!("syncing {}: {error}", path.display()))?;
        directory
            .sync_all()
            .map_err(|error| format!("syncing directory {}: {error}", parent.display()))
    })();
    if let Err(error) = result {
        let cleanup = secure_remove_child(parent, name, false);
        let cleanup_sync = directory.sync_all();
        let mut failures = Vec::new();
        if let Err(cleanup) = cleanup {
            failures.push(format!("partial-file cleanup failed: {cleanup}"));
        }
        if let Err(cleanup_sync) = cleanup_sync {
            failures.push(format!("cleanup directory sync failed: {cleanup_sync}"));
        }
        return Err(CreateFileError::Failure(if failures.is_empty() {
            error
        } else {
            format!("{error}; {}", failures.join("; "))
        }));
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let mut file = secure_regular_file(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting {}: {error}", path.display()))?;
    if metadata.len() > limit as u64 {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            limit
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            limit
        ));
    }
    Ok(bytes)
}

fn secure_regular_file(path: &Path) -> Result<File, String> {
    let file = secure_open(path, libc::O_RDONLY, 0)
        .map_err(|error| format!("refusing unsafe file {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspecting {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("refusing unsafe file {}", path.display()));
    }
    Ok(file)
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
    // SAFETY: `path` is NUL-terminated, `how` has the Linux open_how layout,
    // and a successful syscall returns a newly-owned descriptor.
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

fn fchmod(file: &File, mode: u32) -> std::io::Result<()> {
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    let file = secure_regular_file(path)?;
    fchmod(&file, mode).map_err(|error| format!("setting mode on {}: {error}", path.display()))
}

fn mkdir_at(dirfd: RawFd, name: &OsStr, mode: u32) -> Result<(), String> {
    let name = CString::new(name.as_bytes()).map_err(|_| "directory name contains NUL")?;
    // SAFETY: `name` is NUL-terminated and `dirfd` is held by the caller.
    if unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "creating directory: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn secure_rename(source: &Path, destination: &Path, no_replace: bool) -> Result<(), String> {
    let source_parent = source
        .parent()
        .ok_or_else(|| format!("{} has no parent", source.display()))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent", destination.display()))?;
    let source_name = CString::new(
        source
            .file_name()
            .ok_or_else(|| format!("{} has no file name", source.display()))?
            .as_bytes(),
    )
    .map_err(|_| "source name contains NUL")?;
    let destination_name = CString::new(
        destination
            .file_name()
            .ok_or_else(|| format!("{} has no file name", destination.display()))?
            .as_bytes(),
    )
    .map_err(|_| "destination name contains NUL")?;
    let source_directory = secure_open(source_parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|error| secure_directory_error(source_parent, error))?;
    let destination_directory =
        secure_open(destination_parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|error| secure_directory_error(destination_parent, error))?;
    let flags = if no_replace {
        libc::RENAME_NOREPLACE
    } else {
        0
    };
    // SAFETY: names are NUL-terminated and both directory descriptors remain
    // alive for the atomic renameat2 operation.
    if unsafe {
        libc::renameat2(
            source_directory.as_raw_fd(),
            source_name.as_ptr(),
            destination_directory.as_raw_fd(),
            destination_name.as_ptr(),
            flags,
        )
    } == 0
    {
        if let Err(error) = destination_directory.sync_all() {
            eprintln!(
                "cosmix-trayd: rename {} to {} succeeded, but syncing destination directory {} failed: {error}",
                source.display(),
                destination.display(),
                destination_parent.display()
            );
        }
        if source_parent != destination_parent {
            if let Err(error) = source_directory.sync_all() {
                eprintln!(
                    "cosmix-trayd: rename {} to {} succeeded, but syncing source directory {} failed: {error}",
                    source.display(),
                    destination.display(),
                    source_parent.display()
                );
            }
        }
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn secure_remove_child(parent: &Path, name: &OsStr, directory: bool) -> Result<(), String> {
    let parent = secure_open(parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|error| secure_directory_error(parent, error))?;
    let name = CString::new(name.as_bytes()).map_err(|_| "path name contains NUL")?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `name` is NUL-terminated and `parent` pins the directory.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn sweep_rewrite_temporaries(directory: &Path) -> Result<Vec<String>, String> {
    check_directory(directory)?;
    let mut warnings = Vec::new();
    for child in fs::read_dir(directory)
        .map_err(|error| format!("reading {}: {error}", directory.display()))?
    {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                warnings.push(format!(
                    "warning: cannot inspect a stale Mix rewrite file in {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        let name = child.file_name();
        let bytes = name.as_bytes();
        const PREFIX: &[u8] = b".rewrite-";
        const SUFFIX: &[u8] = b".tmp";
        if bytes.len() <= PREFIX.len() + SUFFIX.len()
            || !bytes.starts_with(PREFIX)
            || !bytes.ends_with(SUFFIX)
        {
            continue;
        }
        match child.file_type() {
            Ok(file_type) if file_type.is_file() => {}
            Ok(_) => {
                warnings.push(format!(
                    "warning: refusing non-regular stale Mix rewrite entry {}",
                    child.path().display()
                ));
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "warning: cannot inspect stale Mix rewrite file {}: {error}",
                    child.path().display()
                ));
                continue;
            }
        }
        if let Err(error) = secure_remove_child(directory, &name, false) {
            warnings.push(format!(
                "warning: cannot remove stale Mix rewrite file {}: {error}",
                child.path().display()
            ));
        }
    }
    Ok(warnings)
}

fn remove_directory_if_empty(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    check_directory(path)?;
    if fs::read_dir(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?
        .next()
        .is_some()
    {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    secure_remove_child(parent, name, true)
        .map_err(|error| format!("removing empty directory {}: {error}", path.display()))?;
    Ok(true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join_with_timeout<T>(handle: thread::JoinHandle<T>, context: &str) -> thread::Result<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "{context} did not finish within five seconds"
        );
        handle.join()
    }
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn watcher_failure_handoff_preserves_concurrent_rearm_request() {
        let mut lifecycle = WatcherLifecycle::default();
        let first = lifecycle.request_start().expect("first watcher generation");
        assert_eq!(first, 1);

        // The failed watcher relinquishes running ownership before publishing
        // its error. A catalogue operation in that publication window must
        // request, rather than lose, the next generation.
        assert!(lifecycle.begin_failure(first));
        assert!(!lifecycle.running);
        assert!(lifecycle.failing);
        assert_eq!(lifecycle.request_start(), None);
        assert!(lifecycle.restart_requested);

        let second = lifecycle
            .complete_failure(first)
            .expect("concurrent request claims replacement generation");
        assert_eq!(second, 2);
        assert!(lifecycle.running);
        assert!(!lifecycle.failing);
        assert!(!lifecycle.restart_requested);
    }

    #[derive(Clone, Copy)]
    enum FakeMode {
        Hold,
        Succeed,
        Fail,
    }

    struct FakeRunner {
        mode: Mutex<FakeMode>,
        starts: Mutex<Vec<RunRequest>>,
        stops: Mutex<Vec<String>>,
        reconciles: AtomicUsize,
        start_count: AtomicUsize,
    }

    impl FakeRunner {
        fn new(mode: FakeMode) -> Arc<Self> {
            Arc::new(Self {
                mode: Mutex::new(mode),
                starts: Mutex::new(Vec::new()),
                stops: Mutex::new(Vec::new()),
                reconciles: AtomicUsize::new(0),
                start_count: AtomicUsize::new(0),
            })
        }

        fn set_mode(&self, mode: FakeMode) {
            *self.mode.lock().expect("fake mode") = mode;
        }
    }

    impl MixRunner for FakeRunner {
        fn reconcile(&self) -> Result<(), String> {
            self.reconciles.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn start(
            &self,
            request: RunRequest,
            events: SyncSender<RunnerEvent>,
        ) -> Result<(), String> {
            self.start_count.fetch_add(1, Ordering::SeqCst);
            self.starts
                .lock()
                .expect("fake starts")
                .push(request.clone());
            match *self.mode.lock().expect("fake mode") {
                FakeMode::Hold => {}
                FakeMode::Succeed => {
                    events
                        .send(RunnerEvent::Output {
                            run_id: request.run_id.clone(),
                            stream: OutputStream::Stdout,
                            bytes: b"alpha output\n".to_vec(),
                        })
                        .expect("send fake stdout");
                    events
                        .send(RunnerEvent::Finished {
                            run_id: request.run_id,
                            exit_code: Some(0),
                            error: None,
                        })
                        .expect("send fake success");
                }
                FakeMode::Fail => {
                    events
                        .send(RunnerEvent::Output {
                            run_id: request.run_id.clone(),
                            stream: OutputStream::Stderr,
                            bytes: b"beta failure\n".to_vec(),
                        })
                        .expect("send fake stderr");
                    events
                        .send(RunnerEvent::Finished {
                            run_id: request.run_id,
                            exit_code: Some(1),
                            error: None,
                        })
                        .expect("send fake failure");
                }
            }
            Ok(())
        }

        fn stop(
            &self,
            _run_id: &str,
            unit: &str,
            _events: SyncSender<RunnerEvent>,
        ) -> Result<(), String> {
            self.stops.lock().expect("fake stops").push(unit.into());
            Ok(())
        }
    }

    fn fixture(mode: FakeMode) -> (TempDir, Arc<MixController>, Arc<FakeRunner>) {
        let temporary = tempfile::tempdir().expect("temporary store");
        let runner = FakeRunner::new(mode);
        let controller = MixController::new_with(temporary.path().join("mix"), runner.clone());
        (temporary, controller, runner)
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_state(controller: &MixController, run_id: &str) -> String {
        controller
            .state()
            .runs
            .iter()
            .find(|run| run.id == run_id)
            .map(|run| run.state.clone())
            .expect("run exists")
    }

    fn write_private_file(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>, mode: u32) {
        let path = path.as_ref();
        fs::write(path, bytes).expect("write private Mix fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .expect("set private Mix fixture mode");
    }

    #[test]
    fn default_store_honours_absolute_nonempty_xdg_data_home() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let home = temporary.path().join("home");
        let xdg = temporary.path().join("redirected-data");
        let store = MixStore::default_store_for_home(&home, Some(xdg.as_os_str()))
            .expect("redirected XDG store");
        assert_eq!(store.root, home.join(".local/mix"));
        assert_eq!(store.legacy_root, Some(xdg.join("cosmix/mix")));

        let fallback = MixStore::default_store_for_home(&home, Some(OsStr::new("")))
            .expect("empty XDG falls back");
        assert_eq!(
            fallback.legacy_root,
            Some(home.join(".local/share/cosmix/mix"))
        );
        assert!(MixStore::default_store_for_home(&home, Some(OsStr::new("relative"))).is_err());
    }

    #[test]
    fn partial_create_failure_removes_the_new_file() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let path = temporary.path().join("Partial");
        let error = create_file_with(&path, |file| {
            file.write_all(b"partial")
                .map_err(|error| format!("write partial fixture: {error}"))?;
            Err("simulated write failure".into())
        })
        .expect_err("partial create must fail");
        assert!(matches!(
            error,
            CreateFileError::Failure(message) if message.contains("simulated write failure")
        ));
        assert!(!path.exists());
    }

    #[test]
    fn startup_store_failure_recovers_and_sweeps_stale_rewrite_file() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let blocker = temporary.path().join("not-a-directory");
        let root = blocker.join("mix");
        let legacy_root = temporary.path().join("legacy");
        fs::write(&blocker, "block directory creation").expect("write blocker");
        fs::create_dir(&legacy_root).expect("legacy root");
        let controller = MixController::new_with_store(
            MixStore::with_legacy(root.clone(), legacy_root),
            FakeRunner::new(FakeMode::Hold),
        );
        assert_eq!(controller.status(), "degraded");
        assert!(controller.error().contains("cannot prepare Mix catalogue"));
        assert!(!controller
            .error()
            .contains("Mix catalogue migration failed"));
        wait_for(|| {
            let lifecycle = controller
                .watcher_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            !lifecycle.running && !lifecycle.failing
        });

        fs::remove_file(&blocker).expect("remove blocker");
        fs::create_dir_all(&root).expect("repair store root");
        write_private_file(root.join(".rewrite-stale.tmp"), "stale\n", 0o700);
        controller.rescan();

        assert_eq!(controller.status(), "watching");
        assert!(controller.error().is_empty());
        assert!(!root.join(".rewrite-stale.tmp").exists());
    }

    #[test]
    fn store_is_eager_and_materialises_private_modes() {
        let (temporary, controller, runner) = fixture(FakeMode::Hold);
        assert_eq!(runner.reconciles.load(Ordering::SeqCst), 0);
        controller.reconcile_orphans().unwrap();
        assert_eq!(runner.reconciles.load(Ordering::SeqCst), 1);
        let root = temporary.path().join("mix");
        assert!(root.exists());
        assert_eq!(controller.status(), "watching");

        for directory in [root.clone(), root.join(".trash")] {
            let mode = fs::metadata(directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, DIRECTORY_MODE);
        }

        let id = controller
            .create("Alpha", "A private script")
            .expect("create script");
        let script = root.join(&id);
        assert!(!root.join(format!("{id}.mix")).exists());
        assert!(!root.join("scripts").exists());
        let mode = fs::metadata(&script)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, FILE_MODE);
        assert_eq!(
            fs::read_to_string(script).expect("starter content"),
            "#!/opt/cosmix/bin/mix\n-- description: A private script\n\n"
        );
    }

    #[test]
    fn startup_sweeps_only_stale_rewrite_temporaries() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("mix");
        let trash = root.join(".trash");
        fs::create_dir_all(&trash).expect("pre-existing store");
        write_private_file(root.join(".rewrite-live.tmp"), "stale\n", 0o700);
        write_private_file(trash.join(".rewrite-trash.tmp"), "stale\n", 0o700);
        write_private_file(root.join(".rewrite-keep"), "operator file\n", 0o600);

        let _controller = MixController::new_with(root.clone(), FakeRunner::new(FakeMode::Hold));

        assert!(!root.join(".rewrite-live.tmp").exists());
        assert!(!trash.join(".rewrite-trash.tmp").exists());
        assert!(root.join(".rewrite-keep").exists());
    }

    #[test]
    fn symlinked_script_is_refused_without_touching_its_target() {
        let (temporary, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let script = controller.store.entry(&id, false);
        let outside = temporary.path().join("outside.mix");
        fs::write(&outside, "print(\"outside\")\n").expect("outside fixture");
        fs::remove_file(&script).expect("remove script");
        symlink(&outside, &script).expect("install symlink");

        assert!(matches!(
            controller.store.script_source(&id, false),
            Err(message) if message.contains("unsafe file")
        ));
        assert_eq!(
            fs::read_to_string(outside).expect("outside remains"),
            "print(\"outside\")\n"
        );
    }

    #[test]
    fn run_pins_the_extensionless_script_and_store_root_as_cwd() {
        let (temporary, controller, runner) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        controller.run(&id).expect("start run");
        let starts = runner.starts.lock().expect("fake starts");
        let request = starts.last().expect("run request");
        let descriptor_path = |file: &File| {
            fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .expect("resolve pinned descriptor")
        };
        assert_eq!(
            descriptor_path(&request.script_handle),
            temporary.path().join("mix/Alpha")
        );
        assert_eq!(
            descriptor_path(&request.working_directory_handle),
            temporary.path().join("mix")
        );
    }

    #[test]
    fn symlinked_store_component_cannot_escape_the_data_root() {
        let temporary = tempfile::tempdir().expect("temporary data root");
        let data = temporary.path().join("data");
        let outside = temporary.path().join("outside");
        fs::create_dir(&data).expect("data root");
        fs::create_dir(&outside).expect("outside root");
        symlink(&outside, data.join("cosmix")).expect("symlinked store component");
        let controller =
            MixController::new_with(data.join("cosmix/mix"), FakeRunner::new(FakeMode::Hold));

        assert!(matches!(
            controller.create("Alpha", ""),
            Err(MixError::MixStoreFailure(message)) if message.contains("unsafe directory")
        ));
        assert!(!outside.join("mix").exists());
    }

    #[test]
    fn description_is_read_only_from_the_leading_comment_block() {
        assert_eq!(
            description_from_leading_comments(b"-- description: present\n\nprint(1)\n"),
            "present"
        );
        assert_eq!(
            description_from_leading_comments(b"-- ordinary comment\n\nprint(1)\n"),
            ""
        );
        assert_eq!(
            description_from_leading_comments(b"-- description: safe\0bad\x01\tkept\n\n"),
            "safebad kept"
        );
        assert_eq!(
            description_from_leading_comments(
                b"-- ordinary comment\n-- description: middle\n-- final comment\n\nprint(1)\n"
            ),
            "middle"
        );
        assert_eq!(
            description_from_leading_comments(
                b"#!/opt/cosmix/bin/mix\n-- ordinary comment\n-- description: after shebang\n\nprint(1)\n"
            ),
            "after shebang"
        );
        assert_eq!(
            description_from_leading_comments(
                b"#!/opt/cosmix/bin/mix\n-- ordinary comment\n\n-- description: too late\nprint(1)\n"
            ),
            ""
        );
    }

    #[test]
    fn update_rewrites_only_the_header_and_sanitises_newlines() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "old").expect("create script");
        let path = controller.store.entry(&id, false);
        fs::write(
            &path,
            b"#!/opt/cosmix/bin/mix\r\n-- description: old\r\n-- keep\r\n\r\nprint(\"same\")\n",
        )
        .expect("replace script");

        controller
            .update(&id, "Beta", "new\r\ndescription")
            .expect("update script");
        let renamed = controller.store.entry("Beta", false);
        assert_eq!(
            fs::read(&renamed).expect("updated bytes"),
            b"#!/opt/cosmix/bin/mix\r\n-- description: new description\r\n-- keep\r\n\r\nprint(\"same\")\n"
        );
        assert!(!path.exists());
    }

    #[test]
    fn description_insertion_stays_after_the_shebang() {
        let temporary = tempfile::tempdir().expect("temporary script");
        let path = temporary.path().join("script");
        write_private_file(&path, b"#!/opt/cosmix/bin/mix\nprint(\"keep\")\n", 0o700);
        rewrite_description(&path, "inserted").expect("insert description");
        assert_eq!(
            fs::read(&path).expect("rewritten script"),
            b"#!/opt/cosmix/bin/mix\n-- description: inserted\nprint(\"keep\")\n"
        );
    }

    #[test]
    fn description_insertion_stays_after_a_bom_prefixed_shebang() {
        let temporary = tempfile::tempdir().expect("temporary script");
        let path = temporary.path().join("script");
        write_private_file(
            &path,
            b"\xef\xbb\xbf#!/opt/cosmix/bin/mix\nprint(\"keep\")\n",
            0o700,
        );
        rewrite_description(&path, "inserted").expect("insert description");
        assert_eq!(
            fs::read(&path).expect("rewritten script"),
            b"\xef\xbb\xbf#!/opt/cosmix/bin/mix\n-- description: inserted\nprint(\"keep\")\n"
        );
    }

    #[test]
    fn description_rewrite_atomically_replaces_the_path_and_leaves_no_temp() {
        let temporary = tempfile::tempdir().expect("temporary script");
        let path = temporary.path().join("Atomic");
        let original = b"#!/opt/cosmix/bin/mix\n-- description: old\n\nprint(\"keep\")\n";
        write_private_file(&path, original, 0o700);
        let mut pinned = secure_regular_file(&path).expect("pin original inode");

        rewrite_description(&path, "new").expect("atomic rewrite");

        let mut pinned_bytes = Vec::new();
        pinned
            .read_to_end(&mut pinned_bytes)
            .expect("read pinned old inode");
        assert_eq!(pinned_bytes, original);
        assert_eq!(
            fs::read(&path).expect("read replaced path"),
            b"#!/opt/cosmix/bin/mix\n-- description: new\n\nprint(\"keep\")\n"
        );
        assert!(!fs::read_dir(temporary.path())
            .expect("list script directory")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".rewrite-")));
    }

    #[test]
    fn description_rewrite_refuses_to_clobber_an_editor_replacement() {
        let temporary = tempfile::tempdir().expect("temporary script");
        let path = temporary.path().join("Edited");
        let editor_temporary = temporary.path().join(".editor-save");
        write_private_file(
            &path,
            b"#!/opt/cosmix/bin/mix\n-- description: old\n\nprint(\"old\")\n",
            0o700,
        );

        let error = rewrite_script_bytes(&path, |_| {
            write_private_file(
                &editor_temporary,
                b"#!/opt/cosmix/bin/mix\n-- description: editor\n\nprint(\"new body\")\n",
                0o700,
            );
            fs::rename(&editor_temporary, &path).expect("atomic editor save");
            b"#!/opt/cosmix/bin/mix\n-- description: trayd\n\nprint(\"old\")\n".to_vec()
        })
        .expect_err("stale trayd rewrite must be rejected");

        assert!(error.contains("script changed during update; retry"));
        assert_eq!(
            fs::read(&path).expect("editor replacement survives"),
            b"#!/opt/cosmix/bin/mix\n-- description: editor\n\nprint(\"new body\")\n"
        );
        assert!(!fs::read_dir(temporary.path())
            .expect("list script directory")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".rewrite-")));
    }

    #[test]
    fn migration_shebang_handling_strips_a_utf8_bom_without_duplication() {
        let temporary = tempfile::tempdir().expect("temporary script");
        let path = temporary.path().join("Bom-script");
        write_private_file(
            &path,
            b"\xef\xbb\xbf#!/opt/cosmix/bin/mix\nprint(\"keep\")\n",
            0o600,
        );
        ensure_mix_shebang(&path).expect("normalise BOM shebang");
        assert_eq!(
            fs::read(&path).expect("normalised script"),
            b"#!/opt/cosmix/bin/mix\nprint(\"keep\")\n"
        );
    }

    #[test]
    fn drop_in_file_appears_in_the_inotify_scan() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        controller.create("Alpha", "").expect("materialise store");
        wait_for(|| controller.watcher_ready.load(Ordering::Acquire));
        write_private_file(
            controller.store.entry("Drop-in", false),
            "#!/opt/cosmix/bin/mix\n-- description: created outside trayd\n\nprint(\"beta\")\n",
            0o700,
        );
        wait_for(|| {
            controller
                .snapshot()
                .scripts
                .iter()
                .any(|script| script.0 == "Drop-in" && script.2 == "created outside trayd")
        });
    }

    #[test]
    fn startup_adopts_a_preexisting_path_root_and_creates_dot_trash() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("mix");
        fs::create_dir(&root).expect("operator-created Mix root");
        write_private_file(
            root.join("Preexisting"),
            "#!/opt/cosmix/bin/mix\n-- description: already here\n\n",
            0o700,
        );

        let controller = MixController::new_with(root.clone(), FakeRunner::new(FakeMode::Hold));
        assert!(root.join(".trash").is_dir());
        let script = controller
            .snapshot()
            .scripts
            .into_iter()
            .find(|script| script.0 == "Preexisting")
            .expect("preexisting drop-in adopted");
        assert_eq!(script.2, "already here");
    }

    #[test]
    fn scan_ignores_dot_entries_and_subdirectories_in_the_path_root() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        controller.create("Alpha", "").expect("materialise store");
        write_private_file(
            controller.store.root.join(".operator-note"),
            "not a script\n",
            0o600,
        );
        fs::create_dir(controller.store.root.join("Looks-like-a-script"))
            .expect("ignored subdirectory");
        controller.rescan();
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.scripts.len(), 1);
        assert_eq!(snapshot.scripts[0].0, "Alpha");
        assert!(snapshot.error.is_empty());
    }

    #[test]
    fn inotify_rescans_external_script_edits() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "old").expect("create script");
        wait_for(|| controller.watcher_ready.load(Ordering::Acquire));
        fs::write(
            controller.store.entry(&id, false),
            "#!/opt/cosmix/bin/mix\n-- description: external edit\n\n",
        )
        .expect("external edit");
        wait_for(|| {
            controller
                .snapshot()
                .scripts
                .iter()
                .any(|script| script.0 == id && script.2 == "external edit")
        });
    }

    #[test]
    fn create_and_rename_collisions_are_typed() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        controller.create("Alpha", "").expect("create Alpha");
        controller.create("Beta", "").expect("create Beta");
        assert!(matches!(
            controller.create("Alpha", "again"),
            Err(MixError::MixScriptExists(_))
        ));
        assert!(matches!(
            controller.update("Alpha", "Beta", "collision"),
            Err(MixError::MixScriptExists(_))
        ));
    }

    #[test]
    fn create_slugifies_free_text_and_rejects_an_empty_slug() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller
            .create("My script!", "slugged")
            .expect("create slugged script");
        assert_eq!(id, "My-script");
        assert!(controller.store.entry("My-script", false).is_file());
        controller
            .update(&id, "Renamed script?", "slugged again")
            .expect("rename with a slugged display name");
        assert!(controller.store.entry("Renamed-script", false).is_file());
        assert!(matches!(
            controller.create("!!!", "empty"),
            Err(MixError::InvalidMixMetadata(_))
        ));
    }

    #[test]
    fn live_name_slugging_preserves_a_mix_suffix() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller
            .create("backup.mix", "created")
            .expect("create dotted script name");
        assert_eq!(id, "backup.mix");
        controller
            .update(&id, "backup.mix", "updated")
            .expect("unchanged dotted script name");
        assert!(controller.store.entry("backup.mix", false).is_file());
        assert!(!controller.store.entry("backup", false).exists());
    }

    #[test]
    fn scan_accepts_private_non_executable_and_executable_modes_only() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let path = controller.store.entry(&id, false);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("remove exec bit");
        controller.rescan();
        assert!(controller.find_script(&id, false).is_some());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("add group read bit");
        controller.rescan();
        assert!(controller.find_script(&id, false).is_none());
        assert!(controller.error().contains("group/other-accessible"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("restore exec bit");
        controller.rescan();
        assert!(controller.find_script(&id, false).is_some());
    }

    #[test]
    fn inotify_rescans_permission_changes_without_an_attrib_loop() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let path = controller.store.entry(&id, false);
        wait_for(|| controller.watcher_ready.load(Ordering::Acquire));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("make script group-readable");
        wait_for(|| controller.find_script(&id, false).is_none());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("restore private executable mode");
        wait_for(|| controller.find_script(&id, false).is_some());
        wait_for(|| controller.watcher_ready.load(Ordering::Acquire));
    }

    #[test]
    fn trash_restore_and_purge_are_identity_only() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        controller.trash(&id).expect("trash");
        assert!(controller.snapshot().scripts[0].3);
        assert!(matches!(
            controller.run(&id),
            Err(MixError::MixScriptTrashed(_))
        ));
        controller.restore(&id).expect("restore");
        assert!(!controller.snapshot().scripts[0].3);
        controller.trash(&id).expect("trash again");
        controller.purge(&id).expect("purge");
        assert!(controller.snapshot().scripts.is_empty());
    }

    #[test]
    fn trash_and_restore_collisions_are_typed_and_never_overwrite() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let trash = controller.store.entry(&id, true);
        write_private_file(&trash, "trash collision\n", 0o600);
        assert!(matches!(
            controller.trash(&id),
            Err(MixError::MixTrashCollision(_))
        ));
        assert!(controller.store.entry(&id, false).exists());
        assert_eq!(fs::read_to_string(&trash).unwrap(), "trash collision\n");

        fs::remove_file(controller.store.entry(&id, false)).expect("remove active collision");
        controller.rescan();
        write_private_file(
            controller.store.entry(&id, false),
            "active collision\n",
            0o600,
        );
        controller.rescan();
        assert!(matches!(
            controller.restore(&id),
            Err(MixError::MixScriptExists(_))
        ));
        assert_eq!(fs::read_to_string(&trash).unwrap(), "trash collision\n");
    }

    #[test]
    fn legacy_directory_migration_is_flat_embedded_and_idempotent() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        let scripts = legacy_root.join("scripts");
        let trash = legacy_root.join("trash");
        fs::create_dir_all(&scripts).expect("scripts directory");
        fs::create_dir(&trash).expect("trash directory");
        let id = "00000000-0000-4000-8000-000000000123";
        let legacy = scripts.join(id);
        fs::create_dir(&legacy).expect("legacy directory");
        write_private_file(legacy.join("script.mix"), "print(\"legacy\")\n", 0o600);
        write_private_file(legacy.join(".rewrite-nested.tmp"), "stale\n", 0o700);
        let metadata = LegacyScriptMetadata {
            schema: 1,
            id: id.into(),
            name: "Legacy name!.mix".into(),
            description: "from sidecar".into(),
            _created_ms: 1,
            _updated_ms: 2,
        };
        let mut encoded = cosmix_mix::to_conf_mix_string(&metadata).expect("encode sidecar");
        encoded.push('\n');
        write_private_file(legacy.join("metadata.conf.mix"), encoded, 0o600);
        write_private_file(
            scripts.join("Flat-script.mix"),
            "-- description: v1 flat\n\nprint(\"flat\")\n",
            0o600,
        );
        write_private_file(
            trash.join("Old-trash.mix"),
            "#!/opt/cosmix/bin/mix\n-- description: trashed\n\n",
            0o600,
        );
        write_private_file(scripts.join(".rewrite-abandoned.tmp"), "stale\n", 0o700);
        write_private_file(trash.join(".rewrite-abandoned.tmp"), "stale\n", 0o700);

        let store = MixStore::with_legacy(root, legacy_root.clone());
        let first = store.migrate_legacy().expect("first migration");
        assert_eq!(first.len(), 3);
        assert_eq!(
            fs::read_to_string(store.entry("Legacy-name", false)).expect("flat script"),
            "#!/opt/cosmix/bin/mix\n-- description: from sidecar\nprint(\"legacy\")\n"
        );
        assert_eq!(
            fs::read_to_string(store.entry("Flat-script", false)).expect("v1 flat script"),
            "#!/opt/cosmix/bin/mix\n-- description: v1 flat\n\nprint(\"flat\")\n"
        );
        assert_eq!(
            fs::read_to_string(store.entry("Old-trash", true)).expect("v1 flat trash"),
            "#!/opt/cosmix/bin/mix\n-- description: trashed\n\n"
        );
        for path in [
            store.entry("Legacy-name", false),
            store.entry("Flat-script", false),
            store.entry("Old-trash", true),
        ] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o700);
        }
        assert!(!legacy_root.exists());
        assert!(store.migrate_legacy().expect("second migration").is_empty());
    }

    #[test]
    fn one_oversized_migration_result_does_not_block_later_files() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        let scripts = legacy_root.join("scripts");
        fs::create_dir_all(&scripts).expect("legacy scripts");
        fs::create_dir(legacy_root.join("trash")).expect("legacy trash");
        write_private_file(scripts.join("Bad.mix"), vec![b'x'; SCRIPT_LIMIT], 0o600);
        write_private_file(
            scripts.join("Good.mix"),
            "-- description: migrates\n\nprint(\"good\")\n",
            0o600,
        );

        let store = MixStore::with_legacy(root, legacy_root);
        let messages = store.migrate_legacy().expect("best-effort migration");
        assert!(messages
            .iter()
            .any(|message| { message.starts_with("warning:") && message.contains("Bad") }));
        assert!(scripts.join("Bad.mix").exists());
        assert!(store.entry("Good", false).exists());
    }

    #[test]
    fn invalid_legacy_candidates_are_reported_per_file() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        let scripts = legacy_root.join("scripts");
        fs::create_dir_all(&scripts).expect("legacy scripts");
        fs::create_dir(legacy_root.join("trash")).expect("legacy trash");
        let not_directory = "aaaaaaaa-0000-4000-8000-000000000001";
        let missing_script = "bbbbbbbb-0000-4000-8000-000000000002";
        write_private_file(scripts.join(not_directory), "not a directory\n", 0o600);
        fs::create_dir(scripts.join(missing_script)).expect("UUID directory without script");

        let store = MixStore::with_legacy(root, legacy_root);
        let messages = store.migrate_legacy().expect("candidate scan");
        for name in [not_directory, missing_script] {
            assert!(messages.iter().any(|message| {
                message.starts_with("warning: cannot migrate legacy Mix script")
                    && message.contains(name)
            }));
        }
    }

    #[test]
    fn invalid_legacy_sidecar_is_kept_while_the_script_migrates() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        let scripts = legacy_root.join("scripts");
        fs::create_dir_all(&scripts).expect("legacy scripts");
        fs::create_dir(legacy_root.join("trash")).expect("legacy trash");
        let id = "12345678-0000-4000-8000-000000000123";
        let legacy = scripts.join(id);
        fs::create_dir(&legacy).expect("legacy UUID directory");
        write_private_file(legacy.join("script.mix"), "print(\"legacy\")\n", 0o600);
        write_private_file(
            legacy.join("metadata.conf.mix"),
            b"not valid strict data\n",
            0o600,
        );

        let store = MixStore::with_legacy(root, legacy_root);
        let messages = store.migrate_legacy().expect("fallback migration");
        assert!(store.entry("12345678", false).exists());
        assert!(legacy.join("metadata.conf.mix").exists());
        assert!(messages.iter().any(|message| {
            message.starts_with("warning:")
                && message.contains("keeping unreadable legacy metadata")
                && message.contains("metadata.conf.mix")
        }));
    }

    #[test]
    fn startup_migration_warning_merges_with_watcher_error_and_recovers() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        let scripts = legacy_root.join("scripts");
        fs::create_dir_all(&scripts).expect("legacy scripts");
        fs::create_dir(legacy_root.join("trash")).expect("legacy trash");
        let id = "87654321-0000-4000-8000-000000000123";
        let legacy = scripts.join(id);
        fs::create_dir(&legacy).expect("legacy UUID directory");
        write_private_file(legacy.join("script.mix"), "print(\"legacy\")\n", 0o600);
        write_private_file(
            legacy.join("metadata.conf.mix"),
            b"not valid strict data\n",
            0o600,
        );

        let controller = MixController::new_with_store(
            MixStore::with_legacy(root, legacy_root.clone()),
            FakeRunner::new(FakeMode::Hold),
        );
        assert_eq!(controller.status(), "degraded");
        assert!(controller
            .error()
            .contains("keeping unreadable legacy metadata"));

        controller.set_watch_error("simulated watcher failure".into());
        assert!(controller
            .error()
            .contains("keeping unreadable legacy metadata"));
        assert!(controller.error().contains("simulated watcher failure"));

        fs::remove_dir_all(&legacy_root).expect("operator removes resolved legacy tree");
        controller.rescan();
        assert_eq!(controller.status(), "watching");
        assert!(controller.error().is_empty());
    }

    #[test]
    fn migration_leaves_and_reports_a_nonempty_old_tree() {
        let temporary = tempfile::tempdir().expect("temporary store");
        let root = temporary.path().join("new/.local/mix");
        let legacy_root = temporary.path().join("old/.local/share/cosmix/mix");
        fs::create_dir_all(legacy_root.join("scripts")).expect("legacy scripts");
        fs::create_dir(legacy_root.join("trash")).expect("legacy trash");
        fs::write(legacy_root.join("scripts/keep.txt"), "leave me\n")
            .expect("unmanaged legacy file");

        let store = MixStore::with_legacy(root, legacy_root.clone());
        let messages = store.migrate_legacy().expect("migration scan");
        assert!(messages
            .iter()
            .any(|message| message.contains("not empty and was left in place")));
        assert_eq!(
            fs::read_to_string(legacy_root.join("scripts/keep.txt")).unwrap(),
            "leave me\n"
        );
    }

    #[test]
    fn script_stem_validation_rejects_unsafe_shapes() {
        let too_long = "a".repeat(65);
        for invalid in ["has..dots", ".hidden", too_long.as_str()] {
            assert!(
                canonical_script_id(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        for valid in ["a", "A-1", "name.with_one.dot", "x_y"] {
            assert_eq!(canonical_script_id(valid).unwrap(), valid);
        }
    }

    #[test]
    fn utf8_decoder_carries_a_scalar_across_pipe_reads() {
        struct SplitReader {
            parts: VecDeque<Vec<u8>>,
        }
        impl Read for SplitReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let Some(part) = self.parts.pop_front() else {
                    return Ok(0);
                };
                output[..part.len()].copy_from_slice(&part);
                Ok(part.len())
            }
        }

        let text = "alpha 🦀 beta\n";
        let bytes = text.as_bytes();
        let split = text.find('🦀').unwrap() + 2;
        let input = SplitReader {
            parts: VecDeque::from([bytes[..split].to_vec(), bytes[split..].to_vec()]),
        };
        let (events, receiver) = mpsc::sync_channel(8);
        read_output(input, "run".into(), OutputStream::Stdout, events);
        let decoded = receiver
            .try_iter()
            .filter_map(|event| match event {
                RunnerEvent::Output { bytes, .. } => Some(String::from_utf8(bytes).unwrap()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(decoded, text);
    }

    #[test]
    fn runner_event_lane_is_strictly_bounded() {
        let (events, _receiver) = mpsc::sync_channel(RUNNER_EVENT_CAPACITY);
        for sequence in 0..RUNNER_EVENT_CAPACITY {
            events
                .try_send(RunnerEvent::Output {
                    run_id: sequence.to_string(),
                    stream: OutputStream::Stdout,
                    bytes: vec![b'x'; MAX_OUTPUT_CHUNK_BYTES],
                })
                .expect("bounded slot");
        }
        assert!(matches!(
            events.try_send(RunnerEvent::Output {
                run_id: "overflow".into(),
                stream: OutputStream::Stdout,
                bytes: vec![b'x'; MAX_OUTPUT_CHUNK_BYTES],
            }),
            Err(mpsc::TrySendError::Full(_))
        ));
    }

    #[test]
    fn fake_runner_surfaces_separate_success_and_failure_output() {
        let (_, controller, runner) = fixture(FakeMode::Succeed);
        let id = controller.create("Alpha", "").expect("create script");
        let success = controller.run(&id).expect("start success");
        wait_for(|| run_state(&controller, &success) == "succeeded");
        let success_snapshot = controller.snapshot();
        let success_run = success_snapshot
            .runs
            .iter()
            .find(|run| run.0 == success)
            .expect("success run");
        assert_eq!(success_run.8, "alpha output\n");
        assert!(success_run.9.is_empty());

        runner.set_mode(FakeMode::Fail);
        let failure = controller.run(&id).expect("start failure");
        wait_for(|| run_state(&controller, &failure) == "failed");
        let failure_snapshot = controller.snapshot();
        let failure_run = failure_snapshot
            .runs
            .iter()
            .find(|run| run.0 == failure)
            .expect("failure run");
        assert!(failure_run.8.is_empty());
        assert_eq!(failure_run.9, "beta failure\n");
        assert_eq!(failure_run.7, 1);
    }

    #[test]
    fn four_active_runs_are_allowed_and_the_fifth_is_rejected() {
        let (_, controller, runner) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let mut runs = Vec::new();
        for _ in 0..MAX_ACTIVE_RUNS {
            runs.push(controller.run(&id).expect("allowed concurrent run"));
        }
        assert_eq!(controller.active_runs(), MAX_ACTIVE_RUNS as u32);
        assert!(matches!(controller.run(&id), Err(MixError::MixRunLimit(_))));
        controller.stop(&runs[0]).expect("stop by run identity");
        assert_eq!(run_state(&controller, &runs[0]), "stopping");
        assert_eq!(
            runner.stops.lock().expect("fake stops").as_slice(),
            [format!("cosmix-mix-run-{}.service", runs[0])]
        );
    }

    #[test]
    fn update_rejects_a_script_with_an_active_run() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller
            .create("Alpha", "original")
            .expect("create script");
        controller.run(&id).expect("start held run");
        assert!(matches!(
            controller.update(&id, "Beta", "changed"),
            Err(MixError::MixScriptBusy(_))
        ));
        assert!(controller.store.entry("Alpha", false).exists());
        assert!(!controller.store.entry("Beta", false).exists());
        assert_eq!(
            description_from_leading_comments(
                &fs::read(controller.store.entry("Alpha", false)).expect("unchanged script")
            ),
            "original"
        );
    }

    #[test]
    fn trash_cannot_pass_the_run_start_critical_section() {
        struct BlockingRunner {
            entered: SyncSender<()>,
            release: Mutex<Receiver<()>>,
        }
        impl MixRunner for BlockingRunner {
            fn start(
                &self,
                _request: RunRequest,
                _events: SyncSender<RunnerEvent>,
            ) -> Result<(), String> {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv()
                    .unwrap();
                Ok(())
            }

            fn stop(
                &self,
                _run_id: &str,
                _unit: &str,
                _events: SyncSender<RunnerEvent>,
            ) -> Result<(), String> {
                Ok(())
            }
        }

        let temporary = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let controller = MixController::new_with(
            temporary.path().join("mix"),
            Arc::new(BlockingRunner {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        );
        let id = controller.create("Alpha", "").unwrap();
        let run_controller = Arc::clone(&controller);
        let run_id = id.clone();
        let run = thread::spawn(move || run_controller.run(&run_id));
        entered_rx.recv().unwrap();

        let trash_controller = Arc::clone(&controller);
        let trash_id = id.clone();
        let (trash_tx, trash_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            trash_tx.send(trash_controller.trash(&trash_id)).unwrap();
        });
        assert!(matches!(
            trash_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        assert!(join_with_timeout(run, "blocked Mix run").unwrap().is_ok());
        assert!(matches!(
            trash_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(MixError::MixScriptBusy(_))
        ));
    }

    #[test]
    fn transient_runner_argv_is_fixed_and_identity_derived() {
        let script_handle = Arc::new(File::open("/dev/null").unwrap());
        let working_directory_handle = Arc::new(File::open("/tmp").unwrap());
        let request = RunRequest {
            run_id: "00000000-0000-4000-8000-000000000001".into(),
            unit: "cosmix-mix-run-00000000-0000-4000-8000-000000000001.service".into(),
            script_handle,
            working_directory_handle,
        };
        let args = transient_args(&request);
        let mut expected = [
            "--user",
            "--collect",
            "--wait",
            "--pipe",
            "--quiet",
            "--unit=cosmix-mix-run-00000000-0000-4000-8000-000000000001",
            "--service-type=exec",
            "--slice=app.slice",
            "--property=StandardInput=null",
            "--property=TimeoutStopSec=5s",
            "--",
            MIX_BINARY,
            "/dev/fd/3",
        ]
        .map(OsString::from)
        .to_vec();
        expected.insert(
            10,
            OsString::from(format!(
                "--property=OpenFile=/proc/{}/fd/{}:mix-script:read-only",
                std::process::id(),
                request.script_handle.as_raw_fd()
            )),
        );
        expected.insert(
            11,
            OsString::from(format!(
                "--working-directory=/proc/{}/fd/{}",
                std::process::id(),
                request.working_directory_handle.as_raw_fd()
            )),
        );
        assert_eq!(args, expected);
    }

    #[test]
    fn output_tail_chunks_and_signal_batches_stay_bounded() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let run_id = controller.run(&id).expect("start run");
        while controller.take_publication().is_some() {}

        controller.apply_runner_event(RunnerEvent::Output {
            run_id: run_id.clone(),
            stream: OutputStream::Stdout,
            bytes: vec![b'x'; MAX_TAIL_BYTES + 80 * 1024],
        });
        controller.apply_runner_event(RunnerEvent::Output {
            run_id: run_id.clone(),
            stream: OutputStream::Stderr,
            bytes: vec![0xff; MAX_OUTPUT_CHUNK_BYTES],
        });
        let snapshot = controller.snapshot();
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.0 == run_id)
            .expect("bounded run");
        assert!(run.8.len() <= MAX_TAIL_BYTES);
        assert!(run.10 > 0);

        let mut observed_chunks = 0;
        while let Some(publication) = controller.take_publication() {
            if publication.output.is_empty() {
                continue;
            }
            assert!(publication.output.len() <= MAX_SIGNAL_CHUNKS);
            assert!(
                publication
                    .output
                    .iter()
                    .map(|chunk| chunk.2.len())
                    .sum::<usize>()
                    <= MAX_SIGNAL_BYTES
            );
            assert!(publication
                .output
                .iter()
                .all(|chunk| chunk.2.len() <= MAX_OUTPUT_CHUNK_BYTES));
            observed_chunks += publication.output.len();
        }
        assert!(observed_chunks > MAX_SIGNAL_CHUNKS);
    }

    #[test]
    fn mix_snapshot_carries_the_next_output_sequence_baseline() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").expect("create script");
        let run_id = controller.run(&id).expect("start run");
        controller.apply_runner_event(RunnerEvent::Output {
            run_id: run_id.clone(),
            stream: OutputStream::Stdout,
            bytes: b"one\n".to_vec(),
        });
        let run = controller
            .snapshot()
            .runs
            .into_iter()
            .find(|run| run.0 == run_id)
            .expect("snapshot run");
        assert_eq!(run.12, 2);
    }

    #[test]
    fn publication_queue_evictions_are_counted() {
        let (_, controller, _) = fixture(FakeMode::Hold);
        let id = controller.create("Alpha", "").unwrap();
        let run_id = controller.run(&id).unwrap();
        while controller.take_publication().is_some() {}
        controller.apply_runner_event(RunnerEvent::Output {
            run_id: run_id.clone(),
            stream: OutputStream::Stdout,
            bytes: vec![b'x'; MAX_PENDING_SIGNAL_BYTES + MAX_OUTPUT_CHUNK_BYTES],
        });
        let state = controller.state();
        let run = state.runs.iter().find(|run| run.id == run_id).unwrap();
        assert!(run.stdout_signal_dropped > 0);
        assert!(run.wire().10 >= run.stdout_signal_dropped);
    }

    #[test]
    fn run_history_keeps_only_the_newest_thirty_two_records() {
        let (_, controller, runner) = fixture(FakeMode::Succeed);
        let id = controller.create("Alpha", "").expect("create script");
        for _ in 0..(MAX_RUN_HISTORY + 5) {
            let run = controller.run(&id).expect("start historical run");
            wait_for(|| {
                !matches!(
                    run_state(&controller, &run).as_str(),
                    "starting" | "running"
                )
            });
        }
        assert_eq!(controller.snapshot().runs.len(), MAX_RUN_HISTORY);
        assert_eq!(
            runner.start_count.load(Ordering::SeqCst),
            MAX_RUN_HISTORY + 5
        );
    }

    #[test]
    fn typed_error_names_are_stable() {
        for (error, expected) in [
            (
                MixError::InvalidMixId(String::new()),
                "dev.cosmix.trayd.Error.InvalidMixId",
            ),
            (
                MixError::UnknownMixScript(String::new()),
                "dev.cosmix.trayd.Error.UnknownMixScript",
            ),
            (
                MixError::MixScriptTrashed(String::new()),
                "dev.cosmix.trayd.Error.MixScriptTrashed",
            ),
            (
                MixError::InvalidMixMetadata(String::new()),
                "dev.cosmix.trayd.Error.InvalidMixMetadata",
            ),
            (
                MixError::MixStoreFailure(String::new()),
                "dev.cosmix.trayd.Error.MixStoreFailure",
            ),
            (
                MixError::MixScriptExists(String::new()),
                "dev.cosmix.trayd.Error.MixScriptExists",
            ),
            (
                MixError::MixTrashCollision(String::new()),
                "dev.cosmix.trayd.Error.MixTrashCollision",
            ),
            (
                MixError::MixAlreadyTrashed(String::new()),
                "dev.cosmix.trayd.Error.MixAlreadyTrashed",
            ),
            (
                MixError::MixNotTrashed(String::new()),
                "dev.cosmix.trayd.Error.MixNotTrashed",
            ),
            (
                MixError::MixScriptBusy(String::new()),
                "dev.cosmix.trayd.Error.MixScriptBusy",
            ),
            (
                MixError::MixRunLimit(String::new()),
                "dev.cosmix.trayd.Error.MixRunLimit",
            ),
            (
                MixError::UnknownMixRun(String::new()),
                "dev.cosmix.trayd.Error.UnknownMixRun",
            ),
            (
                MixError::MixRunNotActive(String::new()),
                "dev.cosmix.trayd.Error.MixRunNotActive",
            ),
            (
                MixError::MixLaunchFailure(String::new()),
                "dev.cosmix.trayd.Error.MixLaunchFailure",
            ),
        ] {
            assert_eq!(zbus::DBusError::name(&error).as_str(), expected);
        }
    }
}
