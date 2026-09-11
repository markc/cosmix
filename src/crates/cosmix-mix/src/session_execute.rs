//! Idle-prompt execution admission on the enrolled attachment (P0-J stage D).
//!
//! This is the half of BROKER-023's `execute` that lives in the SHELL. Holding
//! the capability never implied the shell could run anything; until this module
//! existed the answer was UNSUPPORTED on both sides.
//!
//! The sequence is the design memo's, and each step is here for a reason a
//! shorter one would violate:
//!
//! 1. The Bus arm validates schema, bounds and authority, and submits a
//!    candidate naming the prompt generation it believes is current.
//! 2. The admission owner reserves the eligible prompt. A reservation is NOT an
//!    acceptance: it only asks the editor to give up the terminal.
//! 3. The editor processes already-observed human activity FIRST, then either
//!    refuses (BUSY, having discarded nothing) or acknowledges the release with
//!    the prompt and edit revision it released at.
//! 4. The owner rechecks identity, lease deadline, generation and edit state.
//!    The editor round trip is a delay, and everything checked before it may
//!    have changed during it.
//! 5. The prompt generation is consumed and the acceptance recorded, atomically,
//!    on the thread that owns the editor.
//! 6. With reads stopped and cooked mode restored, the principal and command id
//!    are VISIBLY echoed, then the line executes down the same dispatch path a
//!    human line takes.
//! 7. The shell reclaims the terminal, builds the next prompt and restarts
//!    editing explicitly. There is no speculative readline.
//!
//! Admission is empty-primary-prompt only (Mark, ADR 2026-09-11). A half-typed
//! line, a history search, a paste in progress or a continuation prompt returns
//! BUSY and discards nothing. Nothing executes underneath a human draft.
//!
//! The surface exists only under the owned editor. The rustyline path cannot
//! release the terminal without a keypress, so it answers UNSUPPORTED — a
//! declared limitation, never BUSY.

use crate::editor::Generation;
use crate::editor::runtime::{Admitted, Control};
use crate::editor::{self, Reply as EditorReply};
use crate::session_state::{self, Phase, Source};
use cosmix_lib_bus::native_session::*;
use cosmix_lib_client::session::Hello;
use cosmix_lib_client::{VerifiedCommand, VerifiedConnection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const SUBMIT: &str = "shell.execute";
pub(crate) const RESULT: &str = "shell.execute.result";
pub(crate) const CANCEL: &str = "shell.execute.cancel";

/// Bodies are bounded well under Term's own 8 KiB request limit, so a
/// submission that Term will forward cannot be one this surface would refuse.
pub(crate) const MAX_REQUEST: usize = 8192;
const MAX_SOURCE: usize = 4096;
/// How much of a returned value is retained. Truncation is reported on the
/// value, separately from the execution outcome: a command that succeeded and
/// returned more than this still succeeded.
const MAX_VALUE: usize = 16 * 1024;
pub(crate) const MAX_ERROR: usize = 4096;
const MAX_ECHO_SOURCE: usize = 512;
/// Matches Term's retention so one retry policy spans the whole path.
const RETENTION: Duration = Duration::from_secs(900);
const RECORDS: usize = 256;
/// Each editor round trip in the admission sequence. Three of them plus the
/// identity recheck fit inside the 2s the request arm allows overall.
const EDITOR_BUDGET: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------- wire types

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    version: u8,
    target: Source,
    request_id: DecimalU64,
    /// The generation the caller believes is at the prompt. A snapshot is
    /// information, never a permit — this is what turns a stale one into a
    /// refusal instead of an execution.
    prompt_generation: DecimalU64,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    version: u8,
    target: Source,
    operation_id: DecimalU64,
}

/// The interpreter's value, serialised on the evaluator owner. Never a Value,
/// Scope or Rc: only owned, bounded data crosses the evaluator boundary.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Structured {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u8,
    pub bytes: DecimalU64,
    pub truncated: bool,
    pub text: String,
}
impl Structured {
    pub(crate) fn new(kind: &'static str, text: String) -> Self {
        let bytes = text.len();
        let mut text = text;
        let truncated = bytes > MAX_VALUE;
        if truncated {
            let mut end = MAX_VALUE;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Self {
            kind,
            version: 1,
            bytes: DecimalU64(bytes as u64),
            truncated,
            text,
        }
    }
}

/// What the evaluation did. `outcome` is the execution's verdict and is
/// deliberately independent of `value.truncated`, which is a property of how
/// much of the answer fitted.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Completion {
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Structured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: DecimalU64,
    pub cancellation: CancellationReport,
}

/// The honest cancellation story for one evaluation. `delivered` never claims
/// more than the guarantee table in `docs/mix/cli.md` allows.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CancellationReport {
    pub requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    pub delivered: &'static str,
}
impl CancellationReport {
    pub(crate) fn for_evaluation(operation: u64, interrupted: bool) -> Self {
        match cosmix_mix::cancel::state(operation) {
            Some((true, source, _)) => Self {
                requested: true,
                source: Some(match source {
                    Some(cosmix_mix::cancel::Source::Signal) => "signal",
                    _ => "request",
                }),
                // The evaluation ended; whether cancellation is what ended it
                // is only knowable from the error it produced. Saying
                // "requested" when it completed anyway is the truthful answer.
                delivered: if interrupted {
                    "cooperative"
                } else {
                    "completed_anyway"
                },
            },
            _ => Self {
                requested: false,
                source: None,
                delivered: "none",
            },
        }
    }
}

#[derive(Serialize)]
struct Accepted {
    version: u8,
    operation_id: DecimalU64,
    state: &'static str,
    status: &'static str,
    /// The generation this admission consumed. A caller that wants the next
    /// prompt reads it from a status snapshot; this only says which one ran.
    prompt_generation: DecimalU64,
}

#[derive(Serialize)]
struct Retrieved {
    version: u8,
    operation_id: DecimalU64,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Completion>,
}

fn refusal(code: &'static str) -> (u8, String) {
    (
        10,
        serde_json::json!({ "error_code": code }).to_string(),
    )
}

// --------------------------------------------------------------------- store

struct Record {
    actor: String,
    request_id: u64,
    digest: [u8; 32],
    operation: u64,
    at: Instant,
    /// Present once the evaluator owner has published the outcome.
    completion: Option<Completion>,
}

#[derive(Default)]
struct Store {
    records: Vec<Record>,
}
impl Store {
    fn sweep(&mut self) {
        self.records
            .retain(|r| r.at.elapsed() < RETENTION || r.completion.is_none());
        while self.records.len() > RECORDS {
            // Oldest completed first; a running evaluation is never dropped,
            // because its result still has to be publishable.
            let Some(index) = self.records.iter().position(|r| r.completion.is_some()) else {
                break;
            };
            self.records.remove(index);
        }
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// Published by the evaluator owner when an admitted evaluation ends. The only
/// call into this module from the REPL thread.
pub(crate) fn finished(operation: u64, completion: Completion) {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(record) = store.records.iter_mut().find(|r| r.operation == operation) {
        record.completion = Some(completion);
        record.at = Instant::now();
    }
    store.sweep();
}

// ------------------------------------------------------------------- surface

struct Surface {
    control: Control,
}
fn surface() -> &'static Mutex<Option<Surface>> {
    static SURFACE: OnceLock<Mutex<Option<Surface>>> = OnceLock::new();
    SURFACE.get_or_init(Mutex::default)
}

/// The owned editor registers here at REPL startup. Nothing else can: the
/// rustyline path has no way to release the terminal without a keypress, so it
/// leaves the surface unregistered and every submission answers UNSUPPORTED.
pub(crate) fn register(control: Control) {
    *surface().lock().unwrap_or_else(|e| e.into_inner()) = Some(Surface { control });
}
pub(crate) fn withdraw() {
    *surface().lock().unwrap_or_else(|e| e.into_inner()) = None;
}
pub(crate) fn available() -> bool {
    surface()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}
fn control() -> Option<Control> {
    surface()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| s.control.clone())
}

/// One admission at a time. Two concurrent submissions would both pass their
/// eligibility check before either reserved, and the loser would then be
/// refused by the editor with a state error rather than a clean BUSY.
fn admission_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(tokio::sync::Mutex::default)
}

// ------------------------------------------------------------------ dispatch

/// Stable identity for retry dedupe. Same construction as Term's: a bound
/// caller is its record, an ambient one is its connection, which never returns.
fn actor_key(actor: &BrokerPrincipal) -> String {
    match &actor.session {
        Some(s) => format!("{}:{:?}:{:?}", actor.unix_uid, s.record_id, s.incarnation),
        None => format!(
            "{}:{:?}:{:?}",
            actor.unix_uid, actor.broker_epoch, actor.connection_id
        ),
    }
}

/// What the human sees announced. Short by design: the point is that the pane
/// names who is driving it, not that it reproduces broker identifiers.
fn principal_label(actor: &BrokerPrincipal) -> String {
    match &actor.session {
        Some(s) => {
            // HexBytes renders as lowercase hex through Serialize, not Debug —
            // Debug would put the type name on the glass where the identity
            // should be. The first bytes are enough to tell two callers apart.
            let record: String = s
                .record_id
                .0
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{:?} {}", s.role, record)
        }
        None => format!("uid {} pid {}", actor.unix_uid, actor.peer_pid),
    }
}

/// Escape everything that could move the cursor or change modes. The echo
/// renders attacker-chosen bytes into a terminal the human is reading; a raw
/// pass-through would let a submission draw whatever it liked, including a
/// convincing forgery of a different announcement.
fn sanitise(source: &str) -> String {
    let mut out = String::new();
    for character in source.chars() {
        if out.len() >= MAX_ECHO_SOURCE {
            out.push('…');
            break;
        }
        match character {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            c if (c.is_control() || c == '\u{7f}') => {
                out.push_str(&format!("\\x{:02x}", c as u32 & 0xff));
            }
            c => out.push(c),
        }
    }
    out
}

pub(crate) async fn dispatch(
    connection: &VerifiedConnection,
    hello: &Hello,
    bound: &SessionRecord,
    event: &VerifiedCommand,
    actor: &BrokerPrincipal,
) -> (u8, String) {
    let command = event.command();
    if command.body.len() > MAX_REQUEST {
        return refusal("INVALID_REQUEST");
    }
    // The request arm resolved this family's capability before doing any
    // correlated work, so this is the same decision restated. It is restated
    // deliberately: this module's guarantee that nothing executes without
    // `Execute` should not depend on which caller reached it.
    if !crate::session_status::permitted(actor, bound, Capability::Execute) {
        return refusal("REFUSED");
    }
    match command.command.as_str() {
        SUBMIT => submit(connection, hello, bound, event, actor).await,
        RESULT => retrieve(bound, &command.body),
        CANCEL => cancel(bound, &command.body),
        _ => refusal("UNSUPPORTED"),
    }
}

fn retrieve(bound: &SessionRecord, body: &str) -> (u8, String) {
    let Ok(request) = serde_json::from_str::<Operation>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    store.sweep();
    let Some(record) = store
        .records
        .iter()
        .find(|r| r.operation == request.operation_id.0)
    else {
        return refusal("UNKNOWN_OUTCOME");
    };
    (
        0,
        serde_json::to_string(&Retrieved {
            version: 1,
            operation_id: request.operation_id,
            state: if record.completion.is_some() {
                "finished"
            } else {
                "running"
            },
            result: record.completion.clone(),
        })
        .expect("bounded result serialises"),
    )
}

fn cancel(bound: &SessionRecord, body: &str) -> (u8, String) {
    let Ok(request) = serde_json::from_str::<Operation>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let operation = request.operation_id.0;
    // Resolve against THIS surface's record first: an id this shell never
    // admitted must not be able to address an evaluation by number.
    let known = {
        let store = store().lock().unwrap_or_else(|e| e.into_inner());
        store.records.iter().any(|r| r.operation == operation)
    };
    if !known {
        return refusal("UNKNOWN_OUTCOME");
    }
    let outcome = cosmix_mix::cancel::cancel(operation);
    (
        0,
        serde_json::json!({
            "version": 1,
            "operation_id": request.operation_id,
            "outcome": match outcome {
                cosmix_mix::cancel::Outcome::Requested => "requested",
                cosmix_mix::cancel::Outcome::AlreadyFinished => "already_finished",
                cosmix_mix::cancel::Outcome::Unknown => "unknown",
            },
            // Said plainly rather than implied: recording intent is not
            // stopping anything. What it is worth per code path is in the
            // guarantee table, and nothing here promises more.
            "delivery": "cooperative; no pre-emption of blocking builtins",
        })
        .to_string(),
    )
}

async fn submit(
    connection: &VerifiedConnection,
    hello: &Hello,
    bound: &SessionRecord,
    event: &VerifiedCommand,
    actor: &BrokerPrincipal,
) -> (u8, String) {
    let body = &event.command().body;
    let Ok(request) = serde_json::from_str::<Submit>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 || request.source.len() > MAX_SOURCE || request.request_id.0 == 0 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let identity = actor_key(actor);
    let digest: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    // BROKER-018: a retry of a submission this shell already accepted answers
    // with that submission's operation, never a second execution. The lock is
    // released before the decision is acted on — nothing in this module may
    // hold a std mutex across an await.
    enum Known {
        Replay { operation: u64, running: bool },
        Conflict,
        Full,
        Fresh,
    }
    let known = {
        let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
        store.sweep();
        match store
            .records
            .iter()
            .find(|r| r.actor == identity && r.request_id == request.request_id.0)
        {
            Some(record) if record.digest != digest => Known::Conflict,
            Some(record) => Known::Replay {
                operation: record.operation,
                running: record.completion.is_none(),
            },
            None if store.records.len() >= RECORDS => Known::Full,
            None => Known::Fresh,
        }
    };
    match known {
        Known::Conflict => return refusal("CONFLICT"),
        Known::Full => return refusal("RESOURCE_LIMIT"),
        Known::Replay { operation, running } => {
            return (
                0,
                serde_json::to_string(&Accepted {
                    version: 1,
                    operation_id: DecimalU64(operation),
                    state: if running { "running" } else { "finished" },
                    status: "accepted",
                    prompt_generation: request.prompt_generation,
                })
                .expect("bounded acceptance serialises"),
            );
        }
        Known::Fresh => {}
    }
    let Some(control) = control() else {
        // A declared limitation, never BUSY: the answer would be the same at
        // every prompt, and BUSY invites a retry that can never succeed.
        return refusal("UNSUPPORTED");
    };
    // Serialise admissions: two candidates must not both pass eligibility.
    let _admitting = admission_lock().lock().await;

    // Step 1 eligibility, from the owned reducer. Cheap, and it produces the
    // right refusal before the editor is disturbed at all.
    let Some(view) = session_state::view() else {
        return refusal("REFUSED");
    };
    if view.snapshot.source.as_ref() != Some(&request.target) {
        return refusal("STALE_GENERATION");
    }
    if view.snapshot.prompt_generation != request.prompt_generation {
        return refusal("STALE_GENERATION");
    }
    if view.snapshot.phase != Phase::PromptReady || view.snapshot.continuation {
        return refusal("BUSY");
    }

    // Step 2-3: reserve. The editor drains observed human activity before it
    // answers, so a keystroke that arrived first wins here.
    let editor = match reserve(&control, request.prompt_generation.0).await {
        Ok(Some(reserved)) => reserved,
        Ok(None) => return refusal("BUSY"),
        Err(()) => return refusal("BUSY"),
    };

    // Step 4: recheck what the round trip could have invalidated.
    let still_ours = view.snapshot.source.as_ref() == Some(&request.target)
        && connection.client().is_connected()
        && crate::session_status::admitted(connection, hello, actor, bound, Capability::Execute)
            .await
        && session_state::view().is_some_and(|now| {
            now.snapshot.source.as_ref() == Some(&request.target)
                && now.snapshot.prompt_generation == request.prompt_generation
        });
    if !still_ours {
        release(&control, editor).await;
        return refusal("REFUSED");
    }

    // Step 5: mint the command identity and record the acceptance BEFORE the
    // line is handed over. The reducer adopts this id, so one admitted
    // submission is one command everywhere it is observed.
    let operation = session_state::mint_command();
    let Some(operation) = operation else {
        release(&control, editor).await;
        return refusal("RESOURCE_LIMIT");
    };
    {
        let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
        store.records.push(Record {
            actor: identity,
            request_id: request.request_id.0,
            digest,
            operation,
            at: Instant::now(),
            completion: None,
        });
    }

    // Step 6: echo, consume, execute — one operation on the editor thread.
    let echo = format!(
        "mix: execute #{operation} admitted for {}: {}",
        principal_label(actor),
        sanitise(&request.source)
    );
    let admitted = Admitted {
        source: request.source,
        operation,
    };
    let handed = tokio::task::spawn_blocking({
        let control = control.clone();
        move || control.admit(editor.0, editor.1, echo, admitted, EDITOR_BUDGET)
    })
    .await;
    if !matches!(handed, Ok(Ok(()))) {
        // Nothing ran. Drop the acceptance so the request id is free again and
        // the caller's retry is a real submission rather than a replay of an
        // execution that never happened.
        {
            let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
            store.records.retain(|r| r.operation != operation);
        }
        release(&control, editor).await;
        return refusal("BUSY");
    }
    (
        0,
        serde_json::to_string(&Accepted {
            version: 1,
            operation_id: DecimalU64(operation),
            state: "running",
            status: "accepted",
            prompt_generation: request.prompt_generation,
        })
        .expect("bounded acceptance serialises"),
    )
}

/// `Ok(Some(..))` is a granted reservation; `Ok(None)` is the editor's own
/// refusal, which changed nothing. `Err` is a failed round trip, reported as
/// BUSY because the prompt's state is then unknown to us.
async fn reserve(control: &Control, expected: u64) -> Result<Option<(Generation, u64)>, ()> {
    let control = control.clone();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let view = control.inspect_within(EDITOR_BUDGET)?;
        if view.state != editor::State::Editing
            || view.generation.prompt != expected
            || !view.text.is_empty()
            || view.paste
            || view.search.is_some()
            || view.decoder_pending
        {
            return Ok(None);
        }
        match control.reserve(view.generation, view.revision, EDITOR_BUDGET)? {
            EditorReply::Suspended {
                generation,
                edit_revision,
            } => Ok(Some((generation, edit_revision))),
            _ => Ok(None),
        }
    })
    .await;
    match result {
        Ok(Ok(reserved)) => Ok(reserved),
        _ => Err(()),
    }
}

/// Give the prompt back with nothing executed. Best effort by design: if this
/// fails the editor's own reservation deadline takes the prompt back, so a
/// human is never left without one.
async fn release(control: &Control, (generation, revision): (Generation, u64)) {
    let control = control.clone();
    let _ = tokio::task::spawn_blocking(move || {
        control.release(generation, revision, EDITOR_BUDGET)
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_echo_cannot_carry_terminal_control_sequences() {
        let hostile = "print(1)\x1b[2J\x1b[Hmix: execute #1 admitted for root\r\n";
        let echoed = sanitise(hostile);
        assert!(!echoed.contains('\x1b'));
        assert!(!echoed.contains('\r'));
        assert!(!echoed.contains('\n'));
        assert!(echoed.starts_with("print(1)\\x1b[2J"));
        // Long submissions are bounded, and the bound is visible.
        let long = sanitise(&"a".repeat(MAX_ECHO_SOURCE * 2));
        assert!(long.ends_with('…'));
        assert!(long.len() <= MAX_ECHO_SOURCE + 4);
    }

    #[test]
    fn value_truncation_is_reported_separately_from_the_outcome() {
        let small = Structured::new("string", "ok".into());
        assert!(!small.truncated);
        assert_eq!(small.bytes, DecimalU64(2));
        let big = Structured::new("string", "é".repeat(MAX_VALUE));
        assert!(big.truncated);
        assert_eq!(big.bytes, DecimalU64(MAX_VALUE as u64 * 2));
        assert!(big.text.len() <= MAX_VALUE);
        // A truncated value still belongs to a completed execution.
        let completion = Completion {
            outcome: "completed",
            status: Some(0),
            value: Some(big),
            error: None,
            duration_ms: DecimalU64(1),
            cancellation: CancellationReport {
                requested: false,
                source: None,
                delivered: "none",
            },
        };
        let json = serde_json::to_value(&completion).unwrap();
        assert_eq!(json["outcome"], "completed");
        assert_eq!(json["value"]["truncated"], true);
        assert_eq!(json["value"]["type"], "string");
    }

    #[test]
    fn retention_never_drops_a_running_evaluation() {
        let mut store = Store::default();
        for operation in 0..(RECORDS as u64 + 16) {
            store.records.push(Record {
                actor: "a".into(),
                request_id: operation + 1,
                digest: [0; 32],
                operation,
                at: Instant::now() - RETENTION * 2,
                completion: (operation != 5).then_some(Completion {
                    outcome: "completed",
                    status: None,
                    value: None,
                    error: None,
                    duration_ms: DecimalU64(0),
                    cancellation: CancellationReport {
                        requested: false,
                        source: None,
                        delivered: "none",
                    },
                }),
            });
        }
        store.sweep();
        assert!(store.records.iter().any(|r| r.operation == 5));
        assert!(store.records.len() <= RECORDS);
    }

    #[test]
    fn submissions_are_bounded_and_reject_unknown_fields() {
        let target = serde_json::json!({
            "broker_epoch":"00000000000000000000000000000000",
            "record":{"record_id":"00000000000000000000000000000000",
                      "incarnation":"00000000000000000000000000000000",
                      "binding_generation":"1"},
            "instance_id":"00000000000000000000000000000000",
            "pane_id":"1","pane_generation":"1"
        });
        let mut value = serde_json::json!({
            "version":1,"target":target,"request_id":"1",
            "prompt_generation":"4","source":"print(1)"
        });
        assert!(serde_json::from_value::<Submit>(value.clone()).is_ok());
        value["detach"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Submit>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("detach");
        value.as_object_mut().unwrap().remove("prompt_generation");
        assert!(
            serde_json::from_value::<Submit>(value).is_err(),
            "an execution must always name the generation it expects"
        );
    }
}
