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
pub(crate) const TASK_SUBMIT: &str = "shell.task.submit";
pub(crate) const TASK_RESULT: &str = "shell.task.result";
pub(crate) const TASK_CANCEL: &str = "shell.task.cancel";
/// Named so they can be REFUSED explicitly rather than falling into the
/// unknown-verb arm. A deferral that answers the same as a typo is not a
/// deferral a caller can act on.
pub(crate) const TASK_WATCH: &str = "shell.task.watch";
pub(crate) const TASK_LIST: &str = "shell.task.list";

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
/// Distinct actors whose spent-id marks are remembered. Bounded for the same
/// reason Term bounds its own actor table: an ambient caller is keyed by a
/// connection id that never returns, so an unbounded map grows by one key per
/// connection that ever submitted, for the life of the shell.
const ACTORS: usize = 64;
/// Settled TASK records retained. Far tighter than RECORDS because a task
/// record carries two stream captures and a result: eight is already ~1.5 MiB
/// where an evaluation record is a few hundred bytes.
pub(crate) const TASK_RECORDS: usize = 8;
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
/// The whole admission — three short editor round trips, the identity recheck,
/// the admit and its grace — must finish inside the deadline after which the
/// editor takes the prompt back on its own, or an admission in good standing
/// could commit into a reservation that had already lapsed.
///
/// Derived from the editor's OWN constant. A guard that cannot see the value it
/// guards is a comment: re-tuning RESERVATION would have left this asserting
/// against a number that no longer existed anywhere.
const _: () = assert!(
    EDITOR_BUDGET.as_millis() * 3
        + RECHECK.as_millis()
        + ADMIT_BUDGET.as_millis()
        + ADMIT_GRACE.as_millis()
        < crate::editor::runtime::RESERVATION.as_millis(),
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
    /// A forwarder's claim about who it is relaying for.
    ///
    /// On the real agent path the shell's direct caller is Term, so an
    /// announcement naming the direct caller would say "Term" for every
    /// submission and tell the human nothing about who is driving the pane.
    ///
    /// THE SHELL CANNOT VERIFY THIS. Any caller with `execute` may put any text
    /// here; what the shell authenticates is the SUFFIX it appends itself. The
    /// defence is therefore the rendering, not the field: the announcement
    /// always reads `<claim> via <authenticated caller>`, the claim is escaped
    /// by the same allowlist as the source, and " via " is escaped INSIDE the
    /// claim so a forged label cannot manufacture a second, more trustworthy-
    /// looking attribution. A reader who trusts only the text after the last
    /// " via " is reading a name the broker stamped.
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

/// Which surface an operation belongs to.
///
/// ONE dedupe space, records tagged — not two stores. Splitting the
/// (actor, request_id) space by kind would let one caller request id mint a
/// fresh operation on each surface and BOTH execute, which is the same
/// double-execution bug the shared space exists to prevent. The tag is used
/// only for ADDRESSING: `shell.execute.result` cannot read a task operation and
/// `shell.task.result` cannot read an evaluation, so neither surface can be
/// used to read the other's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Execute,
    Task,
}

pub(crate) struct Record {
    actor: String,
    request_id: u64,
    digest: [u8; 32],
    operation: u64,
    kind: Kind,
    at: Instant,
    /// Present once the owner has published the outcome. For an evaluation that
    /// is the evaluator thread; for a task, its supervisor.
    completion: Option<Completion>,
    /// A settled task's full report. Kept beside `completion` rather than
    /// inside it because the two surfaces answer different shapes and flattening
    /// them would put stream captures on every evaluation result.
    task: Option<crate::session_task::TaskReport>,
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
        self.records.push(record);
        true
    }
    /// The attempt got far enough to be worth remembering. Only NOW is the id
    /// spent.
    ///
    /// Raising the mark inside `admit` spent the id before anything had been
    /// tried, so the commonest refusal of all — a human keystroke ending the
    /// reservation — burned it. The documented contract for that refusal is
    /// "the request id is untouched and the same submission may simply be
    /// retried", and the retry met a retired mark instead, forever.
    fn settle(&mut self, operation: u64) {
        let Some((actor, request_id)) = self
            .records
            .iter()
            .find(|r| r.operation == operation)
            .map(|r| (r.actor.clone(), r.request_id))
        else {
            return;
        };
        // One bounded key per actor. An ambient caller is keyed by its
        // connection id, which never returns, so without a cap a long-lived
        // shell accumulates a key per connection that ever submitted.
        if !self.high_water.contains_key(&actor) && self.high_water.len() >= ACTORS {
            let victim = self
                .high_water
                .keys()
                .find(|key| !self.records.iter().any(|r| &r.actor == *key))
                .cloned();
            // Prefer a key with no live record; failing that, any key. Losing a
            // mark costs one actor's replay protection for ids whose records
            // are already gone, which is strictly better than refusing to
            // record marks at all.
            if let Some(key) = victim.or_else(|| self.high_water.keys().next().cloned()) {
                self.high_water.remove(&key);
            }
        }
        let mark = self.high_water.entry(actor).or_insert(0);
        *mark = (*mark).max(request_id);
    }
    /// The attempt provably did nothing and left no mark anywhere. Remove it
    /// completely: the request id is free, exactly as the refusal promised.
    fn forget(&mut self, operation: u64) {
        self.records.retain(|r| r.operation != operation);
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
    fn owned(&self, actor: &str, operation: u64, kind: Kind) -> Option<&Record> {
        self.records
            .iter()
            .find(|r| r.operation == operation && r.actor == actor && r.kind == kind)
    }
    /// The supervisor publishes by operation alone: it is the owner of that
    /// task by construction and has no actor context to check against.
    fn task_record(&mut self, operation: u64) -> Option<&mut Record> {
        self.records
            .iter_mut()
            .find(|r| r.operation == operation && r.kind == Kind::Task)
    }
    /// Settled TASK records get their own, much tighter cap: a task carries two
    /// stream captures and a result, so eight of them is already ~1.5 MiB,
    /// where an evaluation record is a few hundred bytes. Running tasks are
    /// never evicted — they are bounded separately by TASKS, and their reports
    /// are still owed to somebody.
    fn trim_tasks(&mut self) {
        loop {
            let settled = self
                .records
                .iter()
                .filter(|r| r.kind == Kind::Task && r.completion.is_some())
                .count();
            if settled <= TASK_RECORDS {
                return;
            }
            let Some(index) = self
                .records
                .iter()
                .position(|r| r.kind == Kind::Task && r.completion.is_some())
            else {
                return;
            };
            self.records.remove(index);
        }
    }
    fn running_tasks(&self) -> usize {
        self.records
            .iter()
            .filter(|r| r.kind == Kind::Task && r.completion.is_none())
            .count()
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// Called from the EDITOR thread when an admission fails after claiming its
/// token: past that point the owner has already been answered, so only the
/// editor knows the line never ran. Without this the record reports `running`
/// forever and holds a slot eviction may never reclaim.
pub(crate) fn admission_failed(operation: u64) {
    finished(operation, Completion::not_started());
    cosmix_mix::cancel::forget(operation);
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
    /// Delivers a cancellation to the managed foreground job group. The one
    /// non-cooperative path in the guarantee table, and the reason a cancelled
    /// `sleep` stops rather than being merely asked to.
    interrupt_foreground: std::sync::Arc<dyn Fn(u64) -> Option<i32> + Send + Sync>,
}
fn surface() -> &'static Mutex<Option<Surface>> {
    static SURFACE: OnceLock<Mutex<Option<Surface>>> = OnceLock::new();
    SURFACE.get_or_init(Mutex::default)
}

/// The owned editor registers here at REPL startup. Nothing else can: the
/// rustyline path has no way to release the terminal without a keypress, so it
/// leaves the surface unregistered and every submission answers UNSUPPORTED.
pub(crate) fn register(
    control: Control,
    interrupt_foreground: std::sync::Arc<dyn Fn(u64) -> Option<i32> + Send + Sync>,
) {
    // Capture the test hook here, at startup, like its editor-side sibling.
    // Reading it lazily at first use would let Mix evaluated in this shell set
    // the variable and change the admission timing of a LATER submission.
    let _ = reserve_hold();
    *surface().lock().unwrap_or_else(|e| e.into_inner()) = Some(Surface {
        control,
        interrupt_foreground,
    });
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

/// How long to hold a granted reservation before committing. Zero in every
/// ordinary run; a fixture widens it to type into the window.
pub(crate) fn reserve_hold() -> Duration {
    static HOLD: OnceLock<Duration> = OnceLock::new();
    *HOLD.get_or_init(|| {
        std::env::var("MIX_RESERVE_HOLD_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(Duration::ZERO, Duration::from_millis)
    })
}

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
    // Escaped so a claimed label cannot contain the separator and fake an
    // authenticated suffix of its own — `evil via Term ffff` must not be
    // constructible from the caller-supplied half.
    let label = label.replace(" via ", " \\x76ia ");
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
        // Non-ASCII is admitted ONLY as letters, digits and the marks that
        // compose them. That is a real default-deny, not a list of characters
        // someone thought of: it is decided by the Unicode properties in std,
        // which move with the toolchain, and it admits nothing from Cf, Cn, Co,
        // Cc, Zl or Zp by construction — bidi overrides and isolates, the
        // invisible tag block at U+E0000, variation selectors, U+FFF9-FFFB,
        // U+180E, the BOM, U+2028/9 and every unassigned code point are escaped
        // because they are not letters, without any of them being named.
        //
        // The cost is that non-ASCII PUNCTUATION is escaped too, so an
        // announcement of CJK text is noisier than it strictly needs to be.
        // That is the right side to err on: a noisy announcement is readable,
        // and a forged one is not detectable.
        // Combining marks are escaped too, and that is deliberate rather than
        // an oversight: a decomposed "e" + U+0301 and a precomposed "é" look
        // identical on the glass, and an announcement whose whole job is to let
        // a human tell one submission from another should not render two
        // different byte sequences the same way.
        c if c.is_alphanumeric() => c.to_string(),
        c => {
            let code = c as u32;
            if code <= 0xff {
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
        TASK_SUBMIT => task_submit(bound, actor, &command.body),
        TASK_RESULT => task_retrieve(bound, actor, &command.body),
        TASK_CANCEL => task_cancel(bound, actor, &command.body),
        // Advertised, not stubbed. The §6 rule is that an absent operation says
        // so: a caller polling `shell.task.result` is doing the supported thing,
        // and would otherwise have to discover by silence that watching is not
        // implemented.
        // A DEFERRAL, and it says so. An arm answering exactly like the
        // unknown-verb fallthrough is not a deferral at all — it is
        // indistinguishable from a typo, to a caller and to a fixture, and
        // deleting it would change nothing observable. The reason field is
        // what makes "this verb exists and is not built yet" a fact the
        // surface actually states.
        TASK_WATCH | TASK_LIST => (
            10,
            serde_json::json!({
                "error_code": "UNSUPPORTED",
                "reason": "deferred",
                "detail": "task results are poll-only in v1; use shell.task.result",
            })
            .to_string(),
        ),
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
    let Some(record) = store.owned(&identity, request.operation_id.0, Kind::Execute) else {
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
        match store.owned(&identity, operation, Kind::Execute) {
            None => return refusal("UNKNOWN_OUTCOME"),
            Some(record) => record.completion.is_some(),
        }
    };
    let outcome = if finished {
        cosmix_mix::cancel::Outcome::AlreadyFinished
    } else {
        cosmix_mix::cancel::cancel(operation)
    };
    // The cooperative flag only reaches code that polls it. A managed
    // foreground child does not poll anything, so while THIS operation is the
    // running one the job controller delivers to its process group as well.
    let signalled = if outcome == cosmix_mix::cancel::Outcome::Requested
        && cosmix_mix::cancel::active() == operation
    {
        let hook = surface()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.interrupt_foreground.clone());
        // The hook re-validates the evaluation under the job lock: between
        // resolving the cancellation and delivering it, this operation can
        // finish and a successor's foreground job can take its place.
        let signalled = hook.and_then(|hook| hook(operation));
        if signalled.is_some() {
            // A group signal IS a delivery, and the only one that does not
            // depend on the target noticing a flag. Without recording it here
            // an external command killed by this signal would be reported as
            // having completed normally, because nothing in the evaluator ever
            // consumed an interrupt on its behalf.
            //
            // Delivered is NOT died: a child that ignores SIGINT and exits 0 is
            // reported by its own exit status, and the outcome below is built
            // from the recorded facts rather than from having sent a signal.
            cosmix_mix::cancel::note_delivery();
        }
        signalled
    } else {
        None
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
            // guarantee table, and nothing here promises more. The one
            // exception is reported, not implied — a caller can see from
            // `signalled_pgid` that a real group signal went out.
            "delivery": if signalled.is_some() {
                "cooperative, plus SIGINT delivered to the managed foreground job group"
            } else {
                "cooperative; no pre-emption of blocking builtins"
            },
            "signalled_pgid": signalled,
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------- task verbs

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskSubmit {
    version: u8,
    target: Source,
    request_id: DecimalU64,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    argv: Option<Vec<String>>,
    /// Optional to PARSE, required to run. Absent is a caller's omission, and
    /// it earns INVALID_ARGUMENT — a statement about the request — rather than
    /// INVALID_REQUEST, which says the body itself was unreadable and sends
    /// the caller looking for a JSON fault that is not there.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    timeout_ms: Option<DecimalU64>,
}

#[derive(Serialize)]
struct TaskAccepted {
    version: u8,
    operation_id: DecimalU64,
    state: &'static str,
    status: &'static str,
}

#[derive(Serialize)]
struct TaskRetrieved {
    version: u8,
    operation_id: DecimalU64,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<DecimalU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<crate::session_task::TaskReport>,
}

/// Live supervisors, so a cancel can reach one. Separate from the Store because
/// the Store holds RESULTS and this holds the means to affect a process; a
/// settled task keeps its record and loses its handle.
fn handles() -> &'static Mutex<std::collections::HashMap<u64, crate::session_task::Handle>> {
    static HANDLES: OnceLock<Mutex<std::collections::HashMap<u64, crate::session_task::Handle>>> =
        OnceLock::new();
    HANDLES.get_or_init(Mutex::default)
}

/// NO idle-prompt admission, deliberately. A task is a separate process; the
/// shell being busy has nothing to do with whether one can start, and refusing
/// BUSY here would destroy the independence that is the mode's entire purpose.
fn task_submit(bound: &SessionRecord, actor: &BrokerPrincipal, body: &str) -> (u8, String) {
    let Ok(request) = serde_json::from_str::<TaskSubmit>(body) else {
        return refusal("INVALID_REQUEST");
    };
    if request.version != 1 || request.request_id.0 == 0 {
        return refusal("INVALID_REQUEST");
    }
    if request.target != Source::from(bound) {
        return refusal("STALE_GENERATION");
    }
    let identity = actor_key(actor);
    let digest: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    // ONE dedupe space with the evaluation surface, records tagged. Splitting
    // it would let the same caller request id mint an operation on each surface
    // and both execute.
    enum Known {
        Replay { operation: u64, settled: bool },
        Conflict,
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
            Some(record) => Known::Replay {
                operation: record.operation,
                settled: record.completion.is_some(),
            },
            None => Known::Fresh,
        }
    };
    match known {
        Known::Conflict => return refusal("CONFLICT"),
        Known::Retired => return refusal("UNKNOWN_OUTCOME"),
        Known::Replay { operation, settled } => {
            return (
                0,
                serde_json::to_string(&TaskAccepted {
                    version: 1,
                    operation_id: DecimalU64(operation),
                    state: if settled { "settled" } else { "running" },
                    status: "accepted",
                })
                .expect("bounded acceptance serialises"),
            );
        }
        Known::Fresh => {}
    }
    // Both are contract, not convenience: a task that is not told where to run
    // or when to stop is under-specified, and guessing either would be the
    // implicit-context this mode exists to remove.
    let (Some(cwd), Some(timeout_ms)) = (request.cwd, request.timeout_ms) else {
        return refusal("INVALID_ARGUMENT");
    };
    let spec = match crate::session_task::Spec::validate(
        request.source,
        request.argv,
        cwd,
        request.env,
        timeout_ms.0,
    ) {
        Ok(spec) => spec,
        Err(code) => return refusal(code),
    };
    let Some(operation) = session_state::mint_command() else {
        return refusal("RESOURCE_LIMIT");
    };
    // Concurrency is a REAL limit here — processes and supervisor threads —
    // so refusing is correct where the record table evicts.
    {
        let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
        if store.running_tasks() >= crate::session_task::TASKS {
            return refusal("RESOURCE_LIMIT");
        }
        if !store.admit(Record {
            actor: identity,
            request_id: request.request_id.0,
            digest,
            operation,
            kind: Kind::Task,
            at: Instant::now(),
            completion: None,
            task: None,
        }) {
            return refusal("RESOURCE_LIMIT");
        }
    }
    // Deliberately NOT published into the evaluation cancel registry. A task's
    // cancellation is killpg with escalation, reached through its Handle; the
    // registry would only ever hold an entry nothing reads. It also has no
    // settle path for one — the entry would stay unfinished for the shell's
    // life, growing REGISTRY past its retention bound, making every later
    // publish pay a longer scan, and falsifying the registry's own invariant
    // that unfinished means "running or in flight".
    match crate::session_task::spawn(spec, move |report| {
        task_settled(operation, report);
    }) {
        Ok(handle) => {
            handles()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(operation, handle);
            store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .settle(operation);
            (
                0,
                serde_json::to_string(&TaskAccepted {
                    version: 1,
                    operation_id: DecimalU64(operation),
                    state: "running",
                    status: "accepted",
                })
                .expect("bounded acceptance serialises"),
            )
        }
        Err(error) => {
            // Nothing started, so the id stays free — the same discipline the
            // admission path uses for a pre-claim refusal.
            store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .forget(operation);
            (
                10,
                // The code is chosen by errno, not flattened: NOT_FOUND when
                // the caller named something that is not there, RESOURCE_LIMIT
                // when the box ran out of something. Never INVALID_ARGUMENT —
                // the request was well formed either way — and never a new
                // token, because the error set callers switch on is closed.
                serde_json::json!({
                    "error_code": error.code,
                    "reason": "spawn_failed",
                    "detail": error.detail,
                })
                .to_string(),
            )
        }
    }
}

/// Published by the supervisor thread once `wait()` has spoken.
fn task_settled(operation: u64, report: crate::session_task::TaskReport) {
    {
        let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = store.task_record(operation) {
            // A task's Completion is a marker that it settled; the report
            // carries the detail. Keeping them separate is what stops stream
            // captures appearing on every evaluation result.
            record.completion = Some(Completion {
                outcome: "settled",
                status: None,
                value: None,
                error: None,
                duration_ms: report.duration_ms,
                cancellation: CancellationReport {
                    requested: false,
                    source: None,
                    delivered: "none",
                },
            });
            record.task = Some(report);
            record.at = Instant::now();
        }
        store.trim_tasks();
    }
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&operation);
}

fn task_retrieve(bound: &SessionRecord, actor: &BrokerPrincipal, body: &str) -> (u8, String) {
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
    let Some(record) = store.owned(&identity, request.operation_id.0, Kind::Task) else {
        return refusal("UNKNOWN_OUTCOME");
    };
    let report = record.task.clone();
    let settled = record.completion.is_some();
    drop(store);
    // "cancelling" is a real reported state, not a gap between two others: a
    // caller that asked for cancellation and reads "running" cannot tell
    // whether its request arrived.
    let cancelling = handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&request.operation_id.0)
        .is_some_and(|handle| {
            handle
                .cancelling
                .load(std::sync::atomic::Ordering::Acquire)
        });
    let elapsed = (!settled)
        .then(|| {
            handles()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&request.operation_id.0)
                .map(|handle| DecimalU64(handle.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64))
        })
        .flatten();
    (
        0,
        serde_json::to_string(&TaskRetrieved {
            version: 1,
            operation_id: request.operation_id,
            state: match (settled, cancelling) {
                (true, _) => "settled",
                (false, true) => "cancelling",
                (false, false) => "running",
            },
            elapsed_ms: elapsed,
            report,
        })
        .expect("bounded task report serialises"),
    )
}

fn task_cancel(bound: &SessionRecord, actor: &BrokerPrincipal, body: &str) -> (u8, String) {
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
    let settled = {
        let store = store().lock().unwrap_or_else(|e| e.into_inner());
        match store.owned(&identity, operation, Kind::Task) {
            None => return refusal("UNKNOWN_OUTCOME"),
            Some(record) => record.completion.is_some(),
        }
    };
    if !settled
        && let Some(handle) = handles()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&operation)
    {
        handle.cancel();
    }
    (
        0,
        serde_json::json!({
            "version": 1,
            "operation_id": request.operation_id,
            // `requested` can race a task that settles between the store read
            // above and this reply. That is accepted rather than locked away:
            // holding both locks across the cancel would not remove the race,
            // only move it a few microseconds later, since the task can settle
            // at any instant including after the reply is written. The reply is
            // explicit that it reports an INTENT, and the wait status in the
            // report is what is authoritative about what happened.
            "outcome": if settled { "already_settled" } else { "requested" },
            // The one HARD guarantee in the arc, and it is still not a claim
            // that the process has stopped YET — only that it will be made to.
            // The proof is the wait status in the report, never this reply.
            "delivery": "SIGTERM to the task group, 2s grace, then SIGKILL; \
                         the outcome is read from wait(), not from this request",
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

    // Test hook for the reservation window — the interval in which the editor
    // holds a suspended prompt for an owner that has not committed yet. It is
    // naturally sub-millisecond, so a fixture cannot type into it without a way
    // to widen it. Read once, zero by default.
    if !reserve_hold().is_zero() {
        tokio::time::sleep(reserve_hold()).await;
    }

    // Step 4: recheck what the round trip could have invalidated. Bounded: the
    // correlated checks are RPCs on the one shared connection, and an admission
    // that waits indefinitely on the broker is holding a reservation over a
    // human's prompt.
    // Each cause gets its OWN code. Collapsing them into one refusal told a
    // caller nothing about what to do next: an identity loss is permanent, a
    // moved generation means re-read it, and a continuation means wait.
    let lost_identity = !connection.client().is_connected()
        || !tokio::time::timeout(
            RECHECK,
            crate::session_status::admitted(connection, hello, actor, bound, Capability::Execute),
        )
        .await
        .unwrap_or(false);
    let recheck = if lost_identity {
        Some("REFUSED")
    } else {
        match session_state::view() {
            None => Some("REFUSED"),
            Some(now) if now.snapshot.source.as_ref() != Some(&request.target) => {
                Some("STALE_GENERATION")
            }
            // The commonest cause is a human who used the prompt while the
            // reservation stood, which is exactly a stale generation.
            Some(now) if now.snapshot.prompt_generation != request.prompt_generation => {
                Some("STALE_GENERATION")
            }
            Some(now) if now.snapshot.continuation => Some("BUSY"),
            Some(_) => None,
        }
    };
    if let Some(code) = recheck {
        release(&control, editor).await;
        return refusal(code);
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
            kind: Kind::Execute,
            at: Instant::now(),
            completion: None,
            task: None,
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
    // Spend the id only for an attempt that got somewhere. A pre-claim refusal
    // is handled below and must leave no trace at all.
    if handed != Admission::Refused {
        store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settle(operation);
    }
    match handed {
        Admission::Executed => {}
        Admission::Refused => {
            // The editor refused before touching anything — most often a human
            // keystroke arriving inside the reservation window. Nothing was
            // announced and the id is not spent, so this is a plain BUSY the
            // caller may simply retry, and the retry must find no trace of this
            // attempt: no record, no high-water mark, no registry entry.
            store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .forget(operation);
            cosmix_mix::cancel::forget(operation);
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
            // Nothing will ever run under this id, so its registry entry has no
            // outcome left to carry. Leaving it would pin an unfinished entry
            // the eviction rule refuses to touch.
            cosmix_mix::cancel::forget(operation);
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
            EditorReply::Reserved {
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
    }

    /// The characters that reorder or hide text without being control codes.
    /// `is_control` reports none of these, which is exactly why the escape is
    /// an allowlist and not a list of known-bad characters.
    #[test]
    fn the_echo_escapes_the_characters_is_control_does_not_report() {
        for (name, hostile) in [
            ("RLO", "print(1)\u{202e})1(tnirp"),
            ("LRI", "print(1)\u{2066}hidden\u{2069}"),
            ("ZWJ", "pri\u{200d}nt(1)"),
            ("ZWSP", "pri\u{200b}nt(1)"),
            ("BOM", "\u{feff}print(1)"),
            ("soft hyphen", "pri\u{ad}nt(1)"),
            ("line separator", "print(1)\u{2028}print(2)"),
            ("paragraph separator", "print(1)\u{2029}print(2)"),
            ("word joiner", "pri\u{2060}nt(1)"),
        ] {
            let echoed = sanitise(hostile);
            for character in hostile.chars().filter(|c| !c.is_ascii()) {
                assert!(
                    !echoed.contains(character),
                    "{name}: {character:?} survived into the announcement: {echoed}"
                );
            }
            // Sub-256 code points render as \xNN and the rest as \u{NNNN};
            // which form is used is presentation, escaped-at-all is the rule.
            assert!(
                echoed.contains("\\u{") || echoed.contains("\\x"),
                "{name} was not escaped: {echoed}"
            );
        }
        // Ordinary non-ASCII LETTERS are not mangled — an allowlist that
        // escaped every accent would make the announcement unreadable.
        assert_eq!(sanitise("print(\"héllo wörld\")"), "print(\"héllo wörld\")");
        assert_eq!(sanitise("print(\"日本語\")"), "print(\"日本語\")");
    }

    /// The characters that make the previous blocklist a blocklist. None of
    /// them is named in `escape`; they are escaped because they are not
    /// letters, which is what makes the rule default-deny rather than a list
    /// someone has to keep extending.
    #[test]
    fn the_echo_escapes_invisible_and_unassigned_characters_nobody_enumerated() {
        for (name, hostile) in [
            // Cf, the same reordering power as RLO and absent from the old list.
            ("ALM U+061C", "print(1)\u{61c}x"),
            // The tag block: fully invisible, can spell an entire second
            // command beside the one on screen.
            ("tag block", "print(1)\u{e0041}\u{e0042}"),
            ("annotation U+FFF9", "print(1)\u{fff9}x"),
            ("Mongolian vowel separator", "print(1)\u{180e}x"),
            ("variation selector", "print(1)\u{fe0f}"),
            // Unassigned today; a future assignment must not silently become
            // printable in an announcement.
            ("unassigned U+0870-ish", "print(1)\u{2fe0}"),
        ] {
            let echoed = sanitise(hostile);
            for character in hostile.chars().filter(|c| !c.is_ascii()) {
                assert!(
                    !echoed.contains(character),
                    "{name}: {character:?} survived: {echoed}"
                );
            }
        }
    }

    #[test]
    fn a_truncated_announcement_names_its_hidden_tail() {
        let long = "a".repeat(MAX_ECHO_SOURCE * 2);
        let echoed = sanitise(&long);
        assert!(echoed.contains("…[+"), "{echoed}");
        assert!(echoed.contains("sha256:"), "{echoed}");
        // Two submissions sharing a head are still distinguishable.
        let other = format!("{}b", "a".repeat(MAX_ECHO_SOURCE * 2 - 1));
        assert_ne!(sanitise(&other), echoed);
    }

    /// The cap bounds what is WRITTEN. Checking after the push let a multi-byte
    /// character carry the line past it.
    #[test]
    fn the_echo_cap_is_never_overshot_by_a_multibyte_character() {
        for pad in 0..8 {
            // Land a 4-byte character exactly on the boundary from each offset.
            let source = format!("{}{}", "a".repeat(MAX_ECHO_SOURCE - pad), "𝄞".repeat(4));
            let echoed = sanitise(&source);
            let head = echoed.split(" …[").next().unwrap();
            assert!(
                head.len() <= MAX_ECHO_SOURCE,
                "pad {pad}: head is {} bytes",
                head.len()
            );
            assert!(echoed.is_char_boundary(head.len()));
        }
        // An escape that would straddle the cap is dropped whole, never split.
        let source = format!("{}\u{202e}", "a".repeat(MAX_ECHO_SOURCE - 2));
        let echoed = sanitise(&source);
        assert!(!echoed.contains("\\u{20"), "a split escape leaked: {echoed}");
    }

    #[test]
    fn a_relayed_principal_is_escaped_and_bounded_like_any_other_input() {
        let hostile = "Term\u{202e}\x1b[2J evil".to_owned() + &"x".repeat(500);
        let label = sanitise_label(&hostile);
        assert!(!label.contains('\u{202e}'));
        assert!(!label.contains('\x1b'));
        assert!(label.len() <= MAX_ECHO_PRINCIPAL + 4, "{}", label.len());
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

    fn completed() -> Completion {
        Completion {
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
        }
    }
    fn record(operation: u64, finished: bool) -> Record {
        Record {
            actor: "a".into(),
            request_id: operation + 1,
            digest: [0; 32],
            operation,
            kind: Kind::Execute,
            at: Instant::now(),
            completion: finished.then(completed),
            task: None,
        }
    }

    /// A bounded table that only ever fills is the S4-M1 failure: it wedges the
    /// surface permanently for everyone. It must evict — and evicting must not
    /// resurrect a spent request id, or the retry that follows executes twice.
    #[test]
    fn a_full_store_evicts_instead_of_wedging_and_keeps_ids_spent() {
        let mut store = Store::default();
        for operation in 0..(RECORDS as u64 + 64) {
            assert!(store.admit(record(operation, true)), "wedged at {operation}");
            // Settling is what spends the id. `admit` alone must not, or the
            // commonest refusal of all burns the caller's request id.
            store.settle(operation);
        }
        assert!(store.records.len() <= RECORDS);
        // The earliest ids were evicted, and every one of them is still spent.
        assert!(store.records.iter().all(|r| r.operation >= 64));
        assert!(store.retired("a", 1));
        assert!(store.retired("a", RECORDS as u64));
        assert!(!store.retired("a", RECORDS as u64 + 999));
        // Another actor's ids are its own.
        assert!(!store.retired("b", 1));
    }

    #[test]
    fn a_running_evaluation_is_never_evicted_and_a_full_table_of_them_refuses() {
        let mut store = Store::default();
        store.admit(record(0, false));
        for operation in 1..(RECORDS as u64 + 8) {
            assert!(store.admit(record(operation, true)));
        }
        assert!(
            store.records.iter().any(|r| r.operation == 0),
            "a result that still has to be publishable was dropped"
        );
        // Now fill it entirely with running records: eviction has nothing to
        // take, and refusing is the honest answer rather than dropping a
        // result somebody is waiting for.
        let mut store = Store::default();
        for operation in 0..RECORDS as u64 {
            assert!(store.admit(record(operation, false)));
        }
        assert!(!store.admit(record(9_999, false)));
    }

    #[test]
    fn operations_are_addressable_only_by_the_actor_that_submitted_them() {
        let mut store = Store::default();
        store.admit(record(7, true));
        assert!(store.owned("a", 7, Kind::Execute).is_some());
        assert!(
            store.owned("b", 7, Kind::Execute).is_none(),
            "another actor reached an operation it did not submit"
        );
    }

    /// The documented contract for a pre-claim refusal is that the request id
    /// is untouched and the same submission may simply be retried. Spending the
    /// id inside `admit` broke that for the COMMONEST refusal there is — a
    /// human keystroke ending the reservation — and the retry then met a
    /// retired mark, permanently.
    #[test]
    fn a_refused_attempt_leaves_the_request_id_free_to_retry() {
        let mut store = Store::default();
        assert!(store.admit(record(1, false)));
        // Refused: nothing ran, nothing announced, so nothing is remembered.
        store.forget(1);
        assert!(store.records.is_empty());
        assert!(
            !store.retired("a", 2),
            "a pre-claim refusal burned the caller's request id"
        );
        // The identical retry is therefore a fresh submission, not a replay.
        assert!(store.admit(record(1, false)));
        // And an attempt that DID get somewhere still spends it.
        store.settle(1);
        assert!(store.retired("a", 2));
    }

    #[test]
    fn an_expired_record_leaves_its_id_spent() {
        let mut store = Store::default();
        let mut aged = record(1, true);
        aged.at = Instant::now() - RETENTION * 2;
        store.admit(aged);
        store.settle(1);
        store.sweep();
        assert!(store.records.is_empty(), "the record should have aged out");
        assert!(
            store.retired("a", 2),
            "ageing a record out must not make its id executable again"
        );
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
