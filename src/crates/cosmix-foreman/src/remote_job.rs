//! Controller-side state for detached jobs on build workers.
//!
//! This module is deliberately dark: no verification gate calls it yet and it
//! performs no SSH or subprocess I/O. It owns the transport-independent facts
//! which must be settled before a remote result can become a verdict:
//!
//! - a task attempt is identified by task id *and* claim generation;
//! - every connection handshakes the worker protocol before doing work;
//! - a fresh controller probes the deterministic unit before deciding whether
//!   to start or reattach;
//! - `systemctl show` is parsed as exact key/value records and classified from
//!   `LoadState`, then `SubState`, then `Result`;
//! - log progress is an acknowledged byte offset, never a line count.
//!
//! The future transport supplies the bytes and executes [`CommandSpec`]s. By
//! keeping that outside this module, captured systemd output can exercise the
//! same state machine without a worker or a user systemd session.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Protocol spoken by the foreman controller and detached-job worker helper.
pub const WORKER_PROTOCOL: &str = "cosmix-foreman-detached-job";
/// First protocol version. A mismatch is an infrastructure error, never a
/// request to guess at an older helper's policy.
pub const WORKER_PROTOCOL_VERSION: u32 = 1;

const SHOW_PROPERTIES: [&str; 5] = [
    "LoadState",
    "ActiveState",
    "SubState",
    "Result",
    "ExecMainStatus",
];

const IN_FLIGHT_SUBSTATES: [&str; 5] =
    ["running", "start", "start-pre", "start-post", "activating"];

/// A controller-side protocol, state, or harness error.
///
/// Runnable gate failures and wall-clock timeouts are represented by
/// [`JobCompletion`], not this error type. This distinction is what prevents a
/// worker failure being charged to a task's quality ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteJobError(String);

impl RemoteJobError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RemoteJobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RemoteJobError {}

/// Stable identity for one claimed attempt.
///
/// The generation is load-bearing. A stale controller can address only its
/// own old unit; it cannot adopt or release a newer attempt for the same task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoteJobIdentity {
    task_id: i64,
    claim_generation: i64,
}

impl RemoteJobIdentity {
    pub fn new(task_id: i64, claim_generation: i64) -> Result<Self, RemoteJobError> {
        if task_id <= 0 {
            return Err(RemoteJobError::new(format!(
                "remote job task id must be positive, got {task_id}"
            )));
        }
        if claim_generation <= 0 {
            return Err(RemoteJobError::new(format!(
                "remote job claim generation must be positive, got {claim_generation}"
            )));
        }
        Ok(Self {
            task_id,
            claim_generation,
        })
    }

    pub fn task_id(self) -> i64 {
        self.task_id
    }

    pub fn claim_generation(self) -> i64 {
        self.claim_generation
    }

    pub fn unit_name(self) -> String {
        format!(
            "foreman-task-{}-generation-{}.service",
            self.task_id, self.claim_generation
        )
    }
}

/// Exact worker handshake. Unknown or older helpers are refused before a unit
/// is inspected or started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerHandshake;

impl WorkerHandshake {
    pub fn parse(output: &[u8]) -> Result<Self, RemoteJobError> {
        let fields = parse_exact_fields(output, &["Protocol", "Version"], "worker handshake")?;
        let protocol = &fields["Protocol"];
        if protocol != WORKER_PROTOCOL {
            return Err(RemoteJobError::new(format!(
                "worker protocol mismatch: expected {WORKER_PROTOCOL}, got {protocol:?}"
            )));
        }
        let version = fields["Version"].parse::<u32>().map_err(|_| {
            RemoteJobError::new(format!(
                "worker protocol Version is not an unsigned integer: {:?}",
                fields["Version"]
            ))
        })?;
        if version != WORKER_PROTOCOL_VERSION {
            return Err(RemoteJobError::new(format!(
                "worker protocol version mismatch: expected {WORKER_PROTOCOL_VERSION}, got {version}"
            )));
        }
        Ok(Self)
    }
}

/// Decode a complete structured worker result.
///
/// The detached unit's successful completion says only that the worker helper
/// ran. A malformed or partially written result file establishes no gate
/// verdict and therefore remains an error. The concrete result type arrives
/// in the worker-integration slice; keeping this decoder generic lets that
/// slice use the same fail-closed boundary without coupling this dark state
/// machine to a particular report schema.
pub fn parse_result_json<T: serde::de::DeserializeOwned>(
    output: &[u8],
) -> Result<T, RemoteJobError> {
    if output.is_empty() {
        return Err(RemoteJobError::new(
            "worker result JSON is empty; no gate verdict was established",
        ));
    }
    serde_json::from_slice(output).map_err(|error| {
        RemoteJobError::new(format!(
            "worker result JSON is malformed or incomplete; no gate verdict was established: {error}"
        ))
    })
}

/// Parsed output from one exact `systemctl show` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSnapshot {
    load_state: String,
    active_state: String,
    sub_state: String,
    result: String,
    exec_main_status: String,
}

impl UnitSnapshot {
    pub fn parse(output: &[u8]) -> Result<Self, RemoteJobError> {
        let fields = parse_exact_fields(output, &SHOW_PROPERTIES, "systemctl show")?;
        Ok(Self {
            load_state: fields["LoadState"].clone(),
            active_state: fields["ActiveState"].clone(),
            sub_state: fields["SubState"].clone(),
            result: fields["Result"].clone(),
            exec_main_status: fields["ExecMainStatus"].clone(),
        })
    }

    pub fn load_state(&self) -> &str {
        &self.load_state
    }

    pub fn active_state(&self) -> &str {
        &self.active_state
    }

    pub fn sub_state(&self) -> &str {
        &self.sub_state
    }

    pub fn result(&self) -> &str {
        &self.result
    }

    pub fn exec_main_status(&self) -> &str {
        &self.exec_main_status
    }

    /// Classify a snapshot which is expected to describe a job that has
    /// already been started.
    ///
    /// The ordering is intentional and mirrors the measured systemd traps:
    /// `LoadState` proves that a unit exists; `SubState` alone determines
    /// whether it is in flight; only then may `Result` decide whether an exit
    /// status belongs to the gate.
    pub fn classify(&self) -> Result<JobState, RemoteJobError> {
        if self.load_state != "loaded" {
            return Err(RemoteJobError::new(format!(
                "unit LoadState={} is not loaded; the detached job does not have a result and this is not a pass",
                self.load_state
            )));
        }

        if IN_FLIGHT_SUBSTATES.contains(&self.sub_state.as_str()) {
            return Ok(JobState::InFlight {
                sub_state: self.sub_state.clone(),
            });
        }

        match self.result.as_str() {
            "success" => {
                let exit_code = self.exit_code()?;
                if exit_code != 0 {
                    return Err(RemoteJobError::new(format!(
                        "systemd Result=success is inconsistent with ExecMainStatus={exit_code}; this is a harness fault, not a gate result"
                    )));
                }
                Ok(JobState::Finished(JobCompletion::Passed))
            }
            "exit-code" => {
                let exit_code = self.exit_code()?;
                if exit_code == 0 {
                    return Err(RemoteJobError::new(
                        "systemd Result=exit-code is inconsistent with ExecMainStatus=0; this is a harness fault, not a gate result",
                    ));
                }
                Ok(JobState::Finished(JobCompletion::GateFailed { exit_code }))
            }
            // TimeoutStartSec produces Result=timeout. ExecMainStatus is a
            // signal/status detail in this case, not the gate's exit code.
            "timeout" => Ok(JobState::Finished(JobCompletion::TimedOut)),
            other => Err(RemoteJobError::new(format!(
                "systemd Result={other} outranks ExecMainStatus={}; this is a harness fault, not a gate result",
                self.exec_main_status
            ))),
        }
    }

    fn exit_code(&self) -> Result<i32, RemoteJobError> {
        self.exec_main_status.parse::<i32>().map_err(|_| {
            RemoteJobError::new(format!(
                "systemd ExecMainStatus is not an integer: {:?}",
                self.exec_main_status
            ))
        })
    }
}

/// Current worker-side job state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    InFlight { sub_state: String },
    Finished(JobCompletion),
}

/// Terminal job outcomes which are safe to distinguish after systemd's
/// `Result` has established what the exit status means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobCompletion {
    Passed,
    GateFailed {
        exit_code: i32,
    },
    /// The gate exhausted its `TimeoutStartSec` wall-clock deadline. This is
    /// neither green nor a runnable red verdict.
    TimedOut,
}

/// An argv-only command for the future transport to execute on a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Everything systemd must receive explicitly when starting a detached gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    command: Vec<String>,
    working_directory: PathBuf,
    timeout_secs: u64,
    home: PathBuf,
    path: String,
}

impl LaunchSpec {
    pub fn new(
        command: Vec<String>,
        working_directory: impl Into<PathBuf>,
        timeout_secs: u64,
        home: impl Into<PathBuf>,
        path: impl Into<String>,
    ) -> Result<Self, RemoteJobError> {
        if command.is_empty() || command[0].is_empty() {
            return Err(RemoteJobError::new(
                "detached job command must contain a non-empty program",
            ));
        }
        if command.iter().any(|arg| arg.contains('\0')) {
            return Err(RemoteJobError::new(
                "detached job command arguments must not contain NUL",
            ));
        }
        let working_directory = working_directory.into();
        require_absolute_utf8_path(&working_directory, "working directory")?;
        if timeout_secs == 0 {
            return Err(RemoteJobError::new(
                "detached job TimeoutStartSec must be greater than zero",
            ));
        }
        let home = home.into();
        require_absolute_utf8_path(&home, "HOME")?;
        let path = path.into();
        if path.is_empty() || path.contains(['\0', '\n', '\r']) {
            return Err(RemoteJobError::new(
                "detached job PATH must be non-empty and contain no control separators",
            ));
        }
        Ok(Self {
            command,
            working_directory,
            timeout_secs,
            home,
            path,
        })
    }

    fn start_command(&self, identity: RemoteJobIdentity) -> CommandSpec {
        let mut args = vec![
            format!("--unit={}", identity.unit_name()),
            "--no-block".to_string(),
            "--property=Type=oneshot".to_string(),
            "--property=RemainAfterExit=yes".to_string(),
            format!("--property=TimeoutStartSec={}s", self.timeout_secs),
            format!("--setenv=HOME={}", self.home.display()),
            format!("--setenv=PATH={}", self.path),
            format!("--working-directory={}", self.working_directory.display()),
            "--".to_string(),
        ];
        args.extend(self.command.iter().cloned());
        CommandSpec {
            program: "systemd-run".to_string(),
            args,
        }
    }
}

fn require_absolute_utf8_path(path: &Path, label: &str) -> Result<(), RemoteJobError> {
    if !path.is_absolute() {
        return Err(RemoteJobError::new(format!(
            "detached job {label} must be absolute: {}",
            path.display()
        )));
    }
    let value = path
        .to_str()
        .ok_or_else(|| RemoteJobError::new(format!("detached job {label} must be valid UTF-8")))?;
    if value.contains(['\0', '\n', '\r']) {
        return Err(RemoteJobError::new(format!(
            "detached job {label} contains a control separator"
        )));
    }
    Ok(())
}

/// Controller-local phase. It can be reconstructed after a process restart by
/// starting again at `AwaitingHandshake`: the deterministic unit probe is the
/// durable source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerPhase {
    AwaitingHandshake,
    AwaitingInitialProbe,
    ReadyToStart,
    StartInFlight,
    Polling,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialProbe {
    StartRequired,
    Reattached(JobState),
}

/// Pure controller state machine for one detached attempt.
#[derive(Debug, Clone)]
pub struct DetachedJobController {
    identity: RemoteJobIdentity,
    phase: ControllerPhase,
    completion: Option<JobCompletion>,
    log_offset: u64,
}

impl DetachedJobController {
    pub fn new(identity: RemoteJobIdentity) -> Self {
        Self::with_log_offset(identity, 0)
    }

    /// Restore a separately persisted log cursor after a controller restart.
    /// Unit state itself is intentionally not persisted: it is reattached by
    /// deterministic name after a fresh protocol handshake.
    pub fn with_log_offset(identity: RemoteJobIdentity, log_offset: u64) -> Self {
        Self {
            identity,
            phase: ControllerPhase::AwaitingHandshake,
            completion: None,
            log_offset,
        }
    }

    pub fn identity(&self) -> RemoteJobIdentity {
        self.identity
    }

    pub fn phase(&self) -> ControllerPhase {
        self.phase
    }

    pub fn completion(&self) -> Option<&JobCompletion> {
        self.completion.as_ref()
    }

    pub fn accept_handshake(&mut self, output: &[u8]) -> Result<(), RemoteJobError> {
        self.require_phase(ControllerPhase::AwaitingHandshake, "accept handshake")?;
        WorkerHandshake::parse(output)?;
        self.phase = ControllerPhase::AwaitingInitialProbe;
        Ok(())
    }

    /// Probe before start. `not-found` means start is required in this one
    /// context; it is never converted to a successful result. Any loaded unit
    /// is reattached, whether it is running or already finished.
    pub fn observe_initial(&mut self, output: &[u8]) -> Result<InitialProbe, RemoteJobError> {
        self.require_phase(
            ControllerPhase::AwaitingInitialProbe,
            "observe initial unit",
        )?;
        let snapshot = UnitSnapshot::parse(output)?;
        if snapshot.load_state() == "not-found" {
            self.phase = ControllerPhase::ReadyToStart;
            return Ok(InitialProbe::StartRequired);
        }
        let state = snapshot.classify()?;
        self.record_state(&state);
        Ok(InitialProbe::Reattached(state))
    }

    /// Consume the sole start permission for this controller instance. A
    /// second call cannot emit another command. If the transport loses the
    /// reply, a restarted controller probes the same unit before acting.
    pub fn take_start_command(
        &mut self,
        launch: &LaunchSpec,
    ) -> Result<CommandSpec, RemoteJobError> {
        self.require_phase(ControllerPhase::ReadyToStart, "start detached job")?;
        let command = launch.start_command(self.identity);
        self.phase = ControllerPhase::StartInFlight;
        Ok(command)
    }

    pub fn start_acknowledged(&mut self) -> Result<(), RemoteJobError> {
        self.require_phase(ControllerPhase::StartInFlight, "acknowledge start")?;
        self.phase = ControllerPhase::Polling;
        Ok(())
    }

    /// A racing controller may create the deterministic unit between probe
    /// and start. The worker must report that collision rather than replacing
    /// the unit; this transition re-probes and adopts it.
    pub fn start_found_existing_unit(&mut self) -> Result<(), RemoteJobError> {
        self.require_phase(ControllerPhase::StartInFlight, "reattach after start race")?;
        self.phase = ControllerPhase::AwaitingInitialProbe;
        Ok(())
    }

    /// Apply a poll after start/reattach. Here `LoadState != loaded` is always
    /// a harness fault and its error explicitly says it is not a pass.
    pub fn observe_running(&mut self, output: &[u8]) -> Result<JobState, RemoteJobError> {
        if !matches!(
            self.phase,
            ControllerPhase::StartInFlight | ControllerPhase::Polling
        ) {
            return Err(self.phase_error("observe started unit"));
        }
        let state = UnitSnapshot::parse(output)?.classify()?;
        self.record_state(&state);
        Ok(state)
    }

    /// Exact property request. `LoadState` is listed first and the response is
    /// parsed with keys intact (never `--value` positional or substring data).
    pub fn show_command(&self) -> Result<CommandSpec, RemoteJobError> {
        if !matches!(
            self.phase,
            ControllerPhase::AwaitingInitialProbe
                | ControllerPhase::StartInFlight
                | ControllerPhase::Polling
        ) {
            return Err(self.phase_error("inspect detached unit"));
        }
        let mut args = vec!["show".to_string(), self.identity.unit_name()];
        args.extend(
            SHOW_PROPERTIES
                .iter()
                .map(|property| format!("--property={property}")),
        );
        args.push("--no-pager".to_string());
        Ok(CommandSpec {
            program: "systemctl".to_string(),
            args,
        })
    }

    /// Generation-fenced release commands. There is no task-id-only cleanup
    /// spelling in this API.
    pub fn release_commands(&self) -> [CommandSpec; 2] {
        let unit = self.identity.unit_name();
        [
            CommandSpec {
                program: "systemctl".to_string(),
                args: vec!["stop".to_string(), unit.clone()],
            },
            CommandSpec {
                program: "systemctl".to_string(),
                args: vec!["reset-failed".to_string(), unit],
            },
        ]
    }

    pub fn log_request(&self, max_bytes: usize) -> Result<LogRequest, RemoteJobError> {
        if max_bytes == 0 {
            return Err(RemoteJobError::new(
                "detached log request max_bytes must be greater than zero",
            ));
        }
        if matches!(
            self.phase,
            ControllerPhase::AwaitingHandshake
                | ControllerPhase::AwaitingInitialProbe
                | ControllerPhase::ReadyToStart
        ) {
            return Err(self.phase_error("read detached logs"));
        }
        Ok(LogRequest {
            identity: self.identity,
            byte_offset: self.log_offset,
            max_bytes,
        })
    }

    /// Advance only after the caller has durably consumed this exact chunk.
    /// Retrying before acknowledgement requests the same bytes again.
    pub fn acknowledge_log_chunk(
        &mut self,
        request: &LogRequest,
        chunk: &[u8],
    ) -> Result<u64, RemoteJobError> {
        if matches!(
            self.phase,
            ControllerPhase::AwaitingHandshake
                | ControllerPhase::AwaitingInitialProbe
                | ControllerPhase::ReadyToStart
        ) {
            return Err(self.phase_error("acknowledge detached logs"));
        }
        if request.identity != self.identity {
            return Err(RemoteJobError::new(
                "detached log acknowledgement belongs to another task generation",
            ));
        }
        if request.byte_offset != self.log_offset {
            return Err(RemoteJobError::new(format!(
                "detached log acknowledgement starts at byte {}, current offset is {}",
                request.byte_offset, self.log_offset
            )));
        }
        let bytes_received = chunk.len();
        if bytes_received > request.max_bytes {
            return Err(RemoteJobError::new(format!(
                "detached log response contained {bytes_received} bytes, above requested maximum {}",
                request.max_bytes
            )));
        }
        let received = u64::try_from(bytes_received)
            .map_err(|_| RemoteJobError::new("detached log byte count does not fit u64"))?;
        self.log_offset = self
            .log_offset
            .checked_add(received)
            .ok_or_else(|| RemoteJobError::new("detached log byte offset overflow"))?;
        Ok(self.log_offset)
    }

    fn record_state(&mut self, state: &JobState) {
        match state {
            JobState::InFlight { .. } => {
                self.phase = ControllerPhase::Polling;
                self.completion = None;
            }
            JobState::Finished(completion) => {
                self.phase = ControllerPhase::Finished;
                self.completion = Some(completion.clone());
            }
        }
    }

    fn require_phase(&self, expected: ControllerPhase, action: &str) -> Result<(), RemoteJobError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(self.phase_error(action))
        }
    }

    fn phase_error(&self, action: &str) -> RemoteJobError {
        RemoteJobError::new(format!(
            "cannot {action} while controller phase is {:?}",
            self.phase
        ))
    }
}

/// One resumable worker log read. `byte_offset` is suitable for a file seek;
/// it deliberately has no line-number representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRequest {
    identity: RemoteJobIdentity,
    byte_offset: u64,
    max_bytes: usize,
}

impl LogRequest {
    pub fn identity(self) -> RemoteJobIdentity {
        self.identity
    }

    pub fn byte_offset(self) -> u64 {
        self.byte_offset
    }

    pub fn max_bytes(self) -> usize {
        self.max_bytes
    }
}

fn parse_exact_fields(
    output: &[u8],
    expected: &[&str],
    context: &str,
) -> Result<HashMap<String, String>, RemoteJobError> {
    if !output.ends_with(b"\n") {
        return Err(RemoteJobError::new(format!(
            "{context} output is incomplete: missing final newline"
        )));
    }
    let text = std::str::from_utf8(output)
        .map_err(|error| RemoteJobError::new(format!("{context} output is not UTF-8: {error}")))?;
    let mut fields = HashMap::with_capacity(expected.len());
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            return Err(RemoteJobError::new(format!(
                "{context} output contains an empty record at line {}",
                index + 1
            )));
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            RemoteJobError::new(format!(
                "{context} output line {} is not key=value",
                index + 1
            ))
        })?;
        if !expected.contains(&key) {
            return Err(RemoteJobError::new(format!(
                "{context} output contains unexpected key {key:?}"
            )));
        }
        if value.is_empty() {
            return Err(RemoteJobError::new(format!(
                "{context} output contains empty {key}"
            )));
        }
        if fields.insert(key.to_string(), value.to_string()).is_some() {
            return Err(RemoteJobError::new(format!(
                "{context} output contains duplicate key {key:?}"
            )));
        }
    }
    for key in expected {
        if !fields.contains_key(*key) {
            return Err(RemoteJobError::new(format!(
                "{context} output is incomplete: missing {key}"
            )));
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDSHAKE: &[u8] = b"Protocol=cosmix-foreman-detached-job\nVersion=1\n";
    // Captured verbatim from the measured never-created unit case.
    const NONEXISTENT: &[u8] = b"LoadState=not-found\nActiveState=inactive\nSubState=dead\nResult=success\nExecMainStatus=0\n";
    const RUNNING: &[u8] = b"LoadState=loaded\nActiveState=activating\nSubState=running\nResult=success\nExecMainStatus=0\n";
    const PASSED_REMAINING_ACTIVE: &[u8] = b"LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\nExecMainStatus=0\n";
    const TIMED_OUT: &[u8] = b"LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=timeout\nExecMainStatus=15\n";

    fn identity(generation: i64) -> RemoteJobIdentity {
        RemoteJobIdentity::new(104, generation).unwrap()
    }

    fn launch() -> LaunchSpec {
        LaunchSpec::new(
            vec!["cargo".into(), "test".into(), "--workspace".into()],
            "/build/slot1/.cos/src",
            600,
            "/root",
            "/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
        )
        .unwrap()
    }

    fn machine_at_initial_probe(generation: i64) -> DetachedJobController {
        let mut machine = DetachedJobController::new(identity(generation));
        machine.accept_handshake(HANDSHAKE).unwrap();
        machine
    }

    #[test]
    fn nonexistent_unit_defaults_are_not_a_pass() {
        let snapshot = UnitSnapshot::parse(NONEXISTENT).unwrap();
        let error = snapshot.classify().unwrap_err();
        assert!(error.to_string().contains("LoadState=not-found"), "{error}");
        assert!(error.to_string().contains("not a pass"), "{error}");

        let mut machine = machine_at_initial_probe(1);
        assert_eq!(
            machine.observe_initial(NONEXISTENT).unwrap(),
            InitialProbe::StartRequired
        );
        assert_eq!(machine.phase(), ControllerPhase::ReadyToStart);
        assert!(machine.completion().is_none());
    }

    #[test]
    fn passing_remain_after_exit_job_finishes_from_substate_not_active_state() {
        let state = UnitSnapshot::parse(PASSED_REMAINING_ACTIVE)
            .unwrap()
            .classify()
            .unwrap();
        assert_eq!(state, JobState::Finished(JobCompletion::Passed));

        for sub_state in IN_FLIGHT_SUBSTATES {
            let fixture = format!(
                "LoadState=loaded\nActiveState=active\nSubState={sub_state}\nResult=success\nExecMainStatus=0\n"
            );
            assert_eq!(
                UnitSnapshot::parse(fixture.as_bytes())
                    .unwrap()
                    .classify()
                    .unwrap(),
                JobState::InFlight {
                    sub_state: sub_state.to_string()
                }
            );
        }
    }

    #[test]
    fn result_outranks_exec_status_and_timeout_is_a_third_outcome() {
        assert_eq!(
            UnitSnapshot::parse(TIMED_OUT).unwrap().classify().unwrap(),
            JobState::Finished(JobCompletion::TimedOut)
        );

        let harness_fault = b"LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=signal\nExecMainStatus=0\n";
        let error = UnitSnapshot::parse(harness_fault)
            .unwrap()
            .classify()
            .unwrap_err();
        assert!(error.to_string().contains("Result=signal"), "{error}");
        assert!(error.to_string().contains("harness fault"), "{error}");

        let red = b"LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainStatus=7\n";
        assert_eq!(
            UnitSnapshot::parse(red).unwrap().classify().unwrap(),
            JobState::Finished(JobCompletion::GateFailed { exit_code: 7 })
        );
    }

    #[test]
    fn launch_uses_timeout_start_sec_for_oneshot_and_never_runtime_max_sec() {
        let mut machine = machine_at_initial_probe(2);
        machine.observe_initial(NONEXISTENT).unwrap();
        let command = machine.take_start_command(&launch()).unwrap();
        assert_eq!(command.program, "systemd-run");
        assert!(command.args.contains(&"--property=Type=oneshot".into()));
        assert!(
            command
                .args
                .contains(&"--property=TimeoutStartSec=600s".into())
        );
        assert!(
            command
                .args
                .contains(&"--property=RemainAfterExit=yes".into())
        );
        assert!(command.args.contains(&"--no-block".into()));
        assert!(
            command
                .args
                .iter()
                .all(|arg| !arg.contains("RuntimeMaxSec"))
        );
    }

    #[test]
    fn launch_sets_home_path_and_absolute_working_directory_explicitly() {
        let mut machine = machine_at_initial_probe(3);
        machine.observe_initial(NONEXISTENT).unwrap();
        let command = machine.take_start_command(&launch()).unwrap();
        assert!(command.args.contains(&"--setenv=HOME=/root".into()));
        assert!(
            command
                .args
                .contains(&"--setenv=PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin".into())
        );
        assert!(
            command
                .args
                .contains(&"--working-directory=/build/slot1/.cos/src".into())
        );
        assert!(
            command
                .args
                .iter()
                .all(|arg| arg != "/bin/sh" && arg != "-lc"),
            "a login shell is not the environment contract"
        );
    }

    #[test]
    fn claim_generation_fences_start_and_release_identity() {
        let old = DetachedJobController::new(identity(8));
        let new = DetachedJobController::new(identity(9));
        assert_ne!(old.identity().unit_name(), new.identity().unit_name());
        assert!(old.identity().unit_name().contains("generation-8"));
        assert!(new.identity().unit_name().contains("generation-9"));
        for command in old.release_commands() {
            assert!(command.args.contains(&old.identity().unit_name()));
            assert!(!command.args.contains(&new.identity().unit_name()));
        }
    }

    #[test]
    fn start_is_single_use_and_restart_reattaches_without_starting_again() {
        let mut original = machine_at_initial_probe(11);
        original.observe_initial(NONEXISTENT).unwrap();
        original.take_start_command(&launch()).unwrap();
        assert!(original.take_start_command(&launch()).is_err());
        original.start_acknowledged().unwrap();
        assert_eq!(
            original.observe_running(RUNNING).unwrap(),
            JobState::InFlight {
                sub_state: "running".into()
            }
        );

        // Simulated process restart: local phase is gone, but identity is
        // durable. The new controller handshakes and probes before any start.
        let mut restarted = machine_at_initial_probe(11);
        assert_eq!(
            restarted.observe_initial(RUNNING).unwrap(),
            InitialProbe::Reattached(JobState::InFlight {
                sub_state: "running".into()
            })
        );
        assert_eq!(restarted.phase(), ControllerPhase::Polling);
        assert!(restarted.take_start_command(&launch()).is_err());
    }

    #[test]
    fn racing_start_reprobes_and_adopts_instead_of_replacing_the_unit() {
        let mut machine = machine_at_initial_probe(15);
        machine.observe_initial(NONEXISTENT).unwrap();
        machine.take_start_command(&launch()).unwrap();

        // Another controller won systemd's unit-name race. The losing start
        // does not stop/recycle that unit; it returns to the attach probe.
        machine.start_found_existing_unit().unwrap();
        assert_eq!(machine.phase(), ControllerPhase::AwaitingInitialProbe);
        assert_eq!(
            machine.observe_initial(RUNNING).unwrap(),
            InitialProbe::Reattached(JobState::InFlight {
                sub_state: "running".into()
            })
        );
        assert!(machine.take_start_command(&launch()).is_err());
    }

    #[test]
    fn exact_key_value_parser_rejects_substrings_duplicates_and_partial_output() {
        let substring = b"LoadState=loaded\nActiveState=active\nSubState=exited\nStatusText=Result=success\nExecMainStatus=0\n";
        let error = UnitSnapshot::parse(substring).unwrap_err();
        assert!(error.to_string().contains("StatusText"), "{error}");

        let duplicate = b"LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\nResult=exit-code\nExecMainStatus=0\n";
        let error = UnitSnapshot::parse(duplicate).unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");

        let partial = b"LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\nExecMainStatus=0";
        let error = UnitSnapshot::parse(partial).unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");

        let partial_at_record_boundary =
            b"LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\n";
        let error = UnitSnapshot::parse(partial_at_record_boundary).unwrap_err();
        assert!(error.to_string().contains("ExecMainStatus"), "{error}");
    }

    #[test]
    fn show_request_names_load_state_first() {
        let machine = machine_at_initial_probe(12);
        let command = machine.show_command().unwrap();
        assert_eq!(command.program, "systemctl");
        assert_eq!(command.args[0], "show");
        assert_eq!(command.args[2], "--property=LoadState");
        assert!(command.args.contains(&"--property=Result".into()));
        assert!(command.args.contains(&"--property=ExecMainStatus".into()));
    }

    #[test]
    fn worker_protocol_mismatch_is_an_error_before_unit_work() {
        let mut machine = DetachedJobController::new(identity(13));
        let old = b"Protocol=cosmix-foreman-detached-job\nVersion=0\n";
        let error = machine.accept_handshake(old).unwrap_err();
        assert!(error.to_string().contains("version mismatch"), "{error}");
        assert_eq!(machine.phase(), ControllerPhase::AwaitingHandshake);
        assert!(machine.show_command().is_err());
    }

    #[test]
    fn partial_or_malformed_result_json_never_becomes_a_verdict() {
        #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
        struct ResultFixture {
            pass: bool,
        }

        let partial = br#"{"pass":true"#;
        let error = parse_result_json::<ResultFixture>(partial).unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");

        let concatenated = br#"{"pass":true}{"pass":false}"#;
        assert!(parse_result_json::<ResultFixture>(concatenated).is_err());

        assert_eq!(
            parse_result_json::<ResultFixture>(br#"{"pass":false}"#).unwrap(),
            ResultFixture { pass: false }
        );
    }

    #[test]
    fn log_resume_is_acknowledged_in_bytes_and_survives_restart() {
        let mut machine = machine_at_initial_probe(14);
        machine.observe_initial(RUNNING).unwrap();
        let request = machine.log_request(1024).unwrap();
        assert_eq!(request.byte_offset(), 0);
        // Two lines, but nine UTF-8 bytes. A line cursor would say 2.
        let chunk = "a\né🙂\n".as_bytes();
        assert_eq!(chunk.len(), 9);
        assert_eq!(machine.acknowledge_log_chunk(&request, chunk).unwrap(), 9);

        let mut restarted = DetachedJobController::with_log_offset(identity(14), 9);
        restarted.accept_handshake(HANDSHAKE).unwrap();
        restarted.observe_initial(RUNNING).unwrap();
        assert_eq!(restarted.log_request(1024).unwrap().byte_offset(), 9);
    }
}
