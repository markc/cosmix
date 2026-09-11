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
use crate::editor::runtime::{AdmitRequest, Admission, Admitted, Control, OwnerToken};
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
const MAX_ECHO_PRINCIPAL: usize = 96;
/// Matches Term's retention so one retry policy spans the whole path.
const RETENTION: Duration = Duration::from_secs(900);
const RECORDS: usize = 256;
/// The short editor round trips: inspect, reserve, release. None of them writes
/// to the terminal.
const EDITOR_BUDGET: Duration = Duration::from_millis(400);
/// The admit envelope, which DOES write to the terminal. The echo's own drain
/// deadline is derived from this budget (60% of it) rather than fixed, so the
/// write can never outlive the answer its caller is waiting for. Worst-case
/// admission is inspect + reserve + recheck + this + grace = 3.1s, comfortably
/// inside the editor's 5s reservation deadline.
const ADMIT_BUDGET: Duration = Duration::from_millis(1_000);
/// After a lost abandon race the editor is provably mid-echo, so waiting out a
/// real answer beats guessing one.
const ADMIT_GRACE: Duration = Duration::from_millis(500);
/// How long a submission will wait for the admission lock before answering
/// BUSY. Shorter than one full admission on purpose: a caller queued behind a
/// whole other admission is, from its point of view, looking at a busy shell.
const ADMIT_QUEUE: Duration = Duration::from_millis(250);
const _: () = assert!(
    ADMIT_BUDGET.as_millis() + ADMIT_GRACE.as_millis() + 1_600 < 5_000,
    "the whole admission must fit inside the editor's reservation deadline"
);
/// The step-4 identity recheck. Two RPCs on the one shared connection, made
/// while a reservation stands over a human's prompt — so it is bounded, and an
/// admission that cannot confirm within it releases rather than waits.
const RECHECK: Duration = Duration::from_millis(800);

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
    /// Set ONLY by a forwarder relaying somebody else's submission, from its
    /// own trusted actor context — never from caller-supplied text.
    ///
    /// On the real agent path the shell's direct caller is Term, so an
    /// announcement naming the direct caller would name Term on every
    /// submission and tell the human nothing about who is actually driving the
    /// pane. The shell renders this through the same sanitiser as the source
    /// and labels it as relayed, because the shell cannot verify it itself —
    /// it is trusting the forwarder, and says so on the glass.
    #[serde(default)]
    on_behalf_of: Option<String>,
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
impl Completion {
    /// The admission was abandoned before the editor acted: no echo reached the
    /// pane and no line was delivered. Recorded rather than left blank, because
    /// a record with no completion reads as "still running" forever.
    pub(crate) fn not_started() -> Self {
        Self {
            outcome: "not_started",
            status: None,
            value: None,
            error: Some(
                "admission was abandoned before the editor acted; nothing executed".into(),
            ),
            duration_ms: DecimalU64(0),
            cancellation: CancellationReport {
                requested: false,
                source: None,
                delivered: "none",
            },
        }
    }
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
    /// Built from what the cancellation machinery RECORDED, never from the
    /// shape of an error message. A wrapped spelling of "interrupted", or a
    /// captured runner that reports interruption as an `Ok` result carrying a
    /// flag, would both be invisible to prose-matching and are not invisible
    /// here.
    pub(crate) fn for_evaluation(operation: u64) -> Self {
        match cosmix_mix::cancel::state(operation) {
            Some((true, source, delivered)) => Self {
                requested: true,
                source: Some(match source {
                    Some(cosmix_mix::cancel::Source::Signal) => "signal",
                    _ => "request",
                }),
                // Intent recorded is not the same as intent landed. An
                // evaluation that ran to completion despite a cancellation says
                // so, because a caller that reads "cooperative" will believe
                // the work stopped.
                delivered: if delivered {
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

/// A refusal that names the operation it settled. Only ever attached to an
/// answer for a caller that was already authorised and already reached
/// admission — it tells them which id to ask `shell.execute.result` about,
/// which is the only way to make progress from an undetermined outcome.
fn undetermined(operation: u64) -> (u8, String) {
    (
        10,
        serde_json::json!({
            "error_code": "UNKNOWN_OUTCOME",
            "operation_id": DecimalU64(operation),
            "reason": "admission_claimed_without_report",
        })
        .to_string(),
    )
}

/// Proven not to have executed, but the request id is spent: its outcome is
/// recorded, so a retry replays this rather than executing.
fn not_started(operation: u64) -> (u8, String) {
    (
        10,
        serde_json::json!({
            "error_code": "UNKNOWN_OUTCOME",
            "operation_id": DecimalU64(operation),
            "reason": "admission_abandoned_before_execution",
        })
        .to_string(),
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
    /// Highest request id accepted per actor, retained PAST the record itself.
    ///
    /// Ageing or evicting a record must never turn a spent request id back into
    /// an executable one: a retry arriving after its record is gone would then
    /// run the line a second time. Term and noded both keep this mark for
    /// exactly that reason, and the cap below is what makes it load-bearing
    /// here — a busy shell reaches 256 operations long before it reaches 15
    /// minutes, so the cap, not the clock, is usually what ends a record's life.
    high_water: std::collections::HashMap<String, u64>,
}
impl Store {
    fn sweep(&mut self) {
        self.records
            .retain(|r| r.at.elapsed() < RETENTION || r.completion.is_none());
    }
    /// Make room for one more, then admit it. A full table must EVICT rather
    /// than refuse: refusing is the S4-M1 failure, where a bounded table that
    /// only ever fills wedges the surface permanently for every caller. The
    /// high-water marks survive eviction, so nothing evicted can re-execute.
    ///
    /// `false` only when every record is still running, which is a real
    /// resource limit rather than a bookkeeping one.
    fn admit(&mut self, record: Record) -> bool {
        self.sweep();
        while self.records.len() >= RECORDS {
            // Oldest completed first; a running evaluation is never dropped,
            // because its result still has to be publishable.
            let Some(index) = self.records.iter().position(|r| r.completion.is_some()) else {
                return false;
            };
            self.records.remove(index);
        }
        let mark = self.high_water.entry(record.actor.clone()).or_insert(0);
        *mark = (*mark).max(record.request_id);
        self.records.push(record);
        true
    }
    /// Whether `request_id` from `actor` is a spent id whose record is gone.
    fn retired(&self, actor: &str, request_id: u64) -> bool {
        self.high_water
            .get(actor)
            .is_some_and(|mark| request_id <= *mark)
    }
    /// Operations are addressed only by the actor that submitted them. A
    /// mismatch answers exactly like an unknown id, so the surface cannot be
    /// used as an oracle for which operation numbers exist.
    fn owned(&self, actor: &str, operation: u64) -> Option<&Record> {
        self.records
            .iter()
            .find(|r| r.operation == operation && r.actor == actor)
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

/// The resident carries four concurrent dispatches. Submissions are the only
/// ones that can occupy a slot for seconds, so they get a share of that budget
/// rather than all of it: without this, four queued submissions starve every
/// status request on the connection into uniform refusals — and status is how a
/// caller finds out it should stop submitting.
const SUBMIT_SLOTS: usize = 2;
static SUBMITTING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct SubmitSlot;
impl SubmitSlot {
    fn take() -> Option<Self> {
        SUBMITTING
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |held| (held < SUBMIT_SLOTS).then_some(held + 1),
            )
            .ok()
            .map(|_| Self)
    }
}
impl Drop for SubmitSlot {
    fn drop(&mut self) {
        SUBMITTING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
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
    let mut shown = 0usize;
    for character in source.chars() {
        let escaped = escape(character);
        // Check BEFORE pushing. Appending first and testing afterwards let a
        // multi-byte character push the line past the cap — the cap has to bound
        // what is written, not notice afterwards that it was exceeded.
        if out.len() + escaped.len() > MAX_ECHO_SOURCE {
            break;
        }
        out.push_str(&escaped);
        shown += character.len_utf8();
    }
    // Nothing may execute with an unannounced tail. The hidden remainder is
    // named by length and fingerprinted, so a human who sees a truncated
    // announcement can still tell two different submissions apart.
    if shown < source.len() {
        let digest = Sha256::digest(source.as_bytes());
        let fingerprint: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!(
            " …[+{} bytes, sha256:{fingerprint}]",
            source.len() - shown
        ));
    }
    out
}

/// A relayed principal label goes through the same allowlist as the source —
/// it arrives in a request body and is no more trustworthy than one — and is
/// bounded far shorter, because a label is a name and not a payload.
fn sanitise_label(label: &str) -> String {
    let mut out = String::new();
    for character in label.chars() {
        let escaped = escape(character);
        if out.len() + escaped.len() > MAX_ECHO_PRINCIPAL {
            out.push('…');
            break;
        }
        out.push_str(&escaped);
    }
    out
}

/// Escape by printable ALLOWLIST, not by a blocklist of known-bad characters.
///
/// A blocklist has to enumerate every way a character can lie to a terminal, and
/// the interesting ones are not control codes at all: bidi overrides and
/// isolates reorder what the reader sees, zero-width characters hide
/// differences, and a soft hyphen or BOM is invisible. `is_control` catches none
/// of them. Everything outside the allowlist is rendered as an escape, so a new
/// Unicode trick is escaped by default rather than passed through by omission.
fn escape(character: char) -> String {
    match character {
        // Ordinary printable ASCII, minus the backslash which has to escape
        // itself or the escapes above become forgeable.
        ' '..='~' if character != '\\' => character.to_string(),
        '\\' => "\\\\".into(),
        '\n' => "\\n".into(),
        '\t' => "\\t".into(),
        '\r' => "\\r".into(),
        c => {
            let code = c as u32;
            let printable = !c.is_control()
                && code != 0x7f
                // Cf: bidi overrides U+202A-E, isolates U+2066-9, ZWJ/ZWNJ,
                // word joiner, the BOM, and the soft hyphen.
                && !matches!(code, 0x00ad | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f | 0xfeff)
                // Line and paragraph separators are line breaks that no
                // control-character test reports as one.
                && !matches!(code, 0x2028 | 0x2029)
                // Unassigned/private-use surrogate range cannot appear in a
                // Rust char, so what is left is ordinary text.
                ;
            if printable {
                c.to_string()
            } else if code <= 0xff {
                format!("\\x{code:02x}")
            } else {
                format!("\\u{{{code:04x}}}")
            }
        }
    }
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
        RESULT => retrieve(bound, actor, &command.body),
        CANCEL => cancel(bound, actor, &command.body),
        _ => refusal("UNSUPPORTED"),
    }
}

fn retrieve(bound: &SessionRecord, actor: &BrokerPrincipal, body: &str) -> (u8, String) {
    let Ok(request) = serde_json::from_str::<Operation>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let identity = actor_key(actor);
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    store.sweep();
    // Scoped to the submitting actor. An execution's result names what ran in
    // this shell and what it returned; holding `execute` authorises driving the
    // shell, not reading back what somebody else drove it to do.
    let Some(record) = store.owned(&identity, request.operation_id.0) else {
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

fn cancel(bound: &SessionRecord, actor: &BrokerPrincipal, body: &str) -> (u8, String) {
    let Ok(request) = serde_json::from_str::<Operation>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let identity = actor_key(actor);
    let operation = request.operation_id.0;
    // THE STORE IS AUTHORITATIVE, not the cancellation registry.
    //
    // An operation this surface has recorded exists, even in the window between
    // minting its id and the evaluation actually starting. Deferring to the
    // registry there produced two answers that contradicted each other: cancel
    // said the id was unknown while result said it was running. The registry
    // entry is published at mint for the same reason, so the intent recorded
    // below is adopted when the evaluation begins rather than lost.
    let finished = {
        let store = store().lock().unwrap_or_else(|e| e.into_inner());
        match store.owned(&identity, operation) {
            None => return refusal("UNKNOWN_OUTCOME"),
            Some(record) => record.completion.is_some(),
        }
    };
    let outcome = if finished {
        cosmix_mix::cancel::Outcome::AlreadyFinished
    } else {
        cosmix_mix::cancel::cancel(operation)
    };
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
    // An empty or whitespace-only submission would be announced on the pane,
    // burn a prompt generation and execute nothing. A request that cannot do
    // anything is a malformed request, not a no-op worth advertising.
    if request.source.trim().is_empty() {
        return refusal("INVALID_REQUEST");
    }
    if request
        .on_behalf_of
        .as_ref()
        .is_some_and(|label| label.len() > MAX_SOURCE)
    {
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
        /// A spent id whose recorded outcome is that nothing executed. Replayed
        /// as the refusal it originally produced, never as an acceptance.
        NotStarted(u64),
        Conflict,
        /// A spent id whose record is gone. Answering "unknown outcome" is the
        /// only safe reply: re-executing would run the line twice.
        Retired,
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
            None if store.retired(&identity, request.request_id.0) => Known::Retired,
            Some(record) if record.digest != digest => Known::Conflict,
            Some(record)
                if record
                    .completion
                    .as_ref()
                    .is_some_and(|c| c.outcome == "not_started") =>
            {
                Known::NotStarted(record.operation)
            }
            Some(record) => Known::Replay {
                operation: record.operation,
                running: record.completion.is_none(),
            },
            None => Known::Fresh,
        }
    };
    match known {
        Known::Conflict => return refusal("CONFLICT"),
        Known::Retired => return refusal("UNKNOWN_OUTCOME"),
        Known::NotStarted(operation) => return not_started(operation),
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
    // Bound how much of the resident's dispatch budget submissions may hold.
    let Some(_slot) = SubmitSlot::take() else {
        return refusal("BUSY");
    };
    // Serialise admissions: two candidates must not both pass eligibility.
    // Bounded, because an unbounded wait here occupies a dispatch slot for as
    // long as the holder takes — and the holder's own worst case is seconds.
    let Ok(_admitting) = tokio::time::timeout(ADMIT_QUEUE, admission_lock().lock()).await else {
        return refusal("BUSY");
    };

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
        Reserved::Granted(reserved) => reserved,
        Reserved::Busy => return refusal("BUSY"),
        Reserved::Stale => return refusal("STALE_GENERATION"),
    };

    // Step 4: recheck what the round trip could have invalidated. Bounded: the
    // correlated checks are RPCs on the one shared connection, and an admission
    // that waits indefinitely on the broker is holding a reservation over a
    // human's prompt.
    let still_ours = connection.client().is_connected()
        && tokio::time::timeout(
            RECHECK,
            crate::session_status::admitted(connection, hello, actor, bound, Capability::Execute),
        )
        .await
        .unwrap_or(false)
        && session_state::view().is_some_and(|now| {
            now.snapshot.source.as_ref() == Some(&request.target)
                && now.snapshot.prompt_generation == request.prompt_generation
                && !now.snapshot.continuation
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
    let admitted_to_store = store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .admit(Record {
            actor: identity,
            request_id: request.request_id.0,
            digest,
            operation,
            at: Instant::now(),
            completion: None,
        });
    if !admitted_to_store {
        release(&control, editor).await;
        return refusal("RESOURCE_LIMIT");
    }
    // Publish the identity before anything can run under it, so a cancellation
    // arriving during the echo/handoff window addresses this operation instead
    // of being told it does not exist — and so the intent it records is adopted
    // when the evaluation begins rather than lost in the handover.
    cosmix_mix::cancel::publish(operation);

    // Step 6: echo, consume, execute — one operation on the editor thread.
    let principal = match request.on_behalf_of.as_deref() {
        // "via" is load-bearing: the shell authenticated the forwarder, not the
        // name it relayed, and the announcement must not imply otherwise.
        Some(relayed) => format!(
            "{} via {}",
            sanitise_label(relayed),
            principal_label(actor)
        ),
        None => principal_label(actor),
    };
    let echo = format!(
        "mix: execute #{operation} admitted for {principal}: {}",
        sanitise(&request.source)
    );
    let admitted = Admitted {
        source: request.source,
        operation,
    };
    let token = OwnerToken::new();
    let handed = tokio::task::spawn_blocking({
        let control = control.clone();
        let token = token.clone();
        move || {
            control.admit(
                AdmitRequest {
                    generation: editor.0,
                    revision: editor.1,
                    echo,
                    admitted,
                    budget: ADMIT_BUDGET,
                    grace: ADMIT_GRACE,
                },
                &token,
            )
        }
    })
    .await;
    // A panicked blocking task is the one case with no answer at all. Abandon
    // on its behalf: winning proves nothing ran, losing is a genuine unknown.
    let handed = handed.unwrap_or_else(|_| {
        if token.abandon() {
            Admission::NotStarted
        } else {
            Admission::Unknown
        }
    });
    match handed {
        Admission::Executed => {}
        Admission::Refused => {
            // The editor refused before touching anything — most often a human
            // keystroke arriving inside the reservation window. Nothing was
            // announced and the id is not spent, so this is a plain BUSY the
            // caller may simply retry.
            {
                let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
                store.records.retain(|r| r.operation != operation);
            }
            release(&control, editor).await;
            return refusal("BUSY");
        }
        Admission::NotStarted => {
            // Proven: no echo reached the pane and no line was delivered. The
            // request id is still SPENT — its fate is written, so a retry
            // replays "did not start" instead of becoming a second chance at
            // execution under an id whose outcome is already recorded. The
            // caller submits a new id to try again, and is told which operation
            // to ask about.
            finished(operation, Completion::not_started());
            release(&control, editor).await;
            return not_started(operation);
        }
        Admission::Unknown => {
            // The editor claimed the work and did not report back. It may have
            // executed. The record stays resolvable with no completion, so
            // whatever happened lands in it and `result` answers truthfully —
            // and the prompt is NOT released, because the editor owns it now.
            return undetermined(operation);
        }
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

/// The editor's own answer to a reservation, kept distinct because BUSY and
/// STALE_GENERATION tell a caller two different things to do next: wait, or
/// re-read the generation. A failed round trip is BUSY — the prompt's state is
/// then unknown to us, and claiming staleness would be a guess.
enum Reserved {
    Granted((Generation, u64)),
    Busy,
    Stale,
}

async fn reserve(control: &Control, expected: u64) -> Reserved {
    let control = control.clone();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let view = control.inspect_within(EDITOR_BUDGET)?;
        // The reducer's generation was already checked; this is the editor's
        // own, and the two disagreeing means the prompt moved during the round
        // trip rather than that the shell is busy.
        if view.generation.prompt != expected {
            return Ok(Reserved::Stale);
        }
        if view.state != editor::State::Editing
            || !view.text.is_empty()
            || view.paste
            || view.search.is_some()
            || view.decoder_pending
        {
            return Ok(Reserved::Busy);
        }
        // A reserve that times out would otherwise still be processed later and
        // grant a reservation over a prompt whose owner has already given up.
        let token = OwnerToken::new();
        match control.reserve(view.generation, view.revision, &token, EDITOR_BUDGET)? {
            EditorReply::Suspended {
                generation,
                edit_revision,
            } => Ok(Reserved::Granted((generation, edit_revision))),
            _ => Ok(Reserved::Busy),
        }
    })
    .await;
    match result {
        Ok(Ok(reserved)) => reserved,
        _ => Reserved::Busy,
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
