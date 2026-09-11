//! One editor thread owns tty reads, decoder, buffer and terminal modes.
//! All cross-thread payloads are owned; evaluator state never enters this module.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use super::buffer::Buffer;
use super::input::{self, Decoder, Key};
use super::terminal::Terminal;
use super::{Command, Editor, Effect, Generation, ModeAction, PromptProfile, Reply, State};

const QUEUE: usize = 16;
/// How long a granted reservation may stand before the editor takes the prompt
/// back by itself. The admission owner normally consumes or releases it within
/// microseconds; this exists so that an owner which dies, loses its transport,
/// or is cancelled mid-sequence cannot leave a human staring at a dead prompt.
/// It is a deadline on an existing wait, not a clock anything ticks on.
const RESERVATION: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_CANDIDATES: usize = 4096;
const MAX_COMPLETION_RESULT_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_ENTRY_BYTES: usize = 1024 * 1024;

pub const MIX_SUBCOMMANDS: &[&str] = &[
    "vars",
    "aliases",
    "functions",
    "all",
    "type",
    "history",
    "config",
    "reload",
    "build",
    "test",
    "update",
    "help",
    "man",
    "status",
    "check",
    "trace",
    "time",
    "mesh",
    "ports",
    "ping",
];

#[derive(Clone, Debug, Default)]
pub struct CompletionSnapshot {
    pub variables: Vec<String>,
    pub commands: Arc<Vec<String>>,
    pub cwd: PathBuf,
    pub home: PathBuf,
}
impl CompletionSnapshot {
    fn complete(&self, text: &str, cursor: usize) -> (usize, Vec<String>) {
        let before = &text[..cursor];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace() || *c == '|' || *c == ';')
            .map_or(0, |(i, c)| i + c.len_utf8());
        let word = &before[start..];
        let mut candidates = Vec::new();
        if let Some(prefix) = word.strip_prefix('$') {
            candidates.extend(
                self.variables
                    .iter()
                    .filter(|v| v.starts_with(prefix))
                    .take(MAX_CANDIDATES)
                    .map(|v| format!("${v}")),
            );
        } else if before[..start].trim().is_empty() {
            candidates.extend(
                self.commands
                    .iter()
                    .filter(|v| v.starts_with(word))
                    .take(MAX_CANDIDATES)
                    .cloned(),
            );
        } else if before[..start].trim() == "mix" {
            candidates.extend(
                MIX_SUBCOMMANDS
                    .iter()
                    .filter(|v| v.starts_with(word))
                    .map(|v| (*v).to_owned()),
            );
        } else {
            let split = word.rfind('/').map_or(0, |i| i + 1);
            let (dir, prefix) = word.split_at(split);
            let path = if let Some(rest) = dir.strip_prefix('~') {
                PathBuf::from(format!("{}{rest}", self.home.display()))
            } else {
                self.cwd.join(dir)
            };
            let mut budget = MAX_COMPLETION_RESULT_BYTES;
            if let Ok(entries) = std::fs::read_dir(path) {
                // Directory enumeration and metadata never block the tty owner.
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(prefix) {
                        let suffix = if entry.path().is_dir() { "/" } else { "" };
                        let candidate = format!("{dir}{name}{suffix}");
                        if candidate.len() > budget {
                            break;
                        }
                        budget -= candidate.len();
                        candidates.push(candidate);
                        if candidates.len() == MAX_CANDIDATES {
                            break;
                        }
                    }
                }
            }
            candidates.sort();
        }
        let mut budget = MAX_COMPLETION_RESULT_BYTES;
        candidates.retain(|candidate| {
            if candidate.len() > budget {
                false
            } else {
                budget -= candidate.len();
                true
            }
        });
        (start, candidates)
    }
}

/// A line the shell never typed. Carries the operation identity the reducer and
/// the result store both key off, so one admitted submission is one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    pub source: String,
    pub operation: u64,
}

const TOKEN_WAITING: u8 = 0;
const TOKEN_CLAIMED: u8 = 1;
const TOKEN_ABANDONED: u8 = 2;

/// Proof that the admission owner is still waiting for a queued envelope.
///
/// A bounded `recv_timeout` gives up on the REPLY CHANNEL, not on the work: the
/// envelope is still in the editor's queue and will be processed. Without this
/// the editor would echo and execute an admission whose caller had already been
/// told it did not happen — and the caller's retry would then be a SECOND
/// execution of the same line.
///
/// Exactly one of `claim` (the editor, immediately before it acts) and
/// `abandon` (the owner, the moment it stops waiting) can win. Losing `abandon`
/// is not a failure: it is the owner learning that it may no longer assume
/// nothing ran.
#[derive(Clone, Debug)]
pub struct OwnerToken(Arc<std::sync::atomic::AtomicU8>);
impl Default for OwnerToken {
    fn default() -> Self {
        Self::new()
    }
}
impl OwnerToken {
    pub fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU8::new(TOKEN_WAITING)))
    }
    /// The editor's side. `true` means the owner is still waiting and this
    /// envelope may proceed.
    pub fn claim(&self) -> bool {
        self.0
            .compare_exchange(
                TOKEN_WAITING,
                TOKEN_CLAIMED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
    /// The owner's side. `true` PROVES the editor had not acted and never will.
    pub fn abandon(&self) -> bool {
        self.0
            .compare_exchange(
                TOKEN_WAITING,
                TOKEN_ABANDONED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// One admission attempt's parameters, kept as a named thing so the seam that
/// carries them reads as a request rather than a run of positional arguments.
pub struct AdmitRequest {
    pub generation: Generation,
    pub revision: u64,
    pub echo: String,
    pub admitted: Admitted,
    pub budget: std::time::Duration,
    pub grace: std::time::Duration,
}

/// What an admission attempt actually did. `NotStarted` is a PROOF, not a
/// guess; `Unknown` is the honest answer when the editor claimed the work and
/// then did not report back, and it is the only case in which the caller must
/// be told its outcome is undetermined rather than refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Executed,
    /// The editor refused BEFORE claiming: no echo, no consume, no mark on the
    /// pane at all. The request id is not spent and the caller may simply
    /// retry, so this is the one failure that is honestly a plain BUSY.
    Refused,
    /// Provably nothing executed, but the attempt got far enough to be worth
    /// recording — the id is spent and its outcome written.
    NotStarted,
    Unknown,
}

#[derive(Debug)]
pub enum Line {
    Submitted(String),
    Admitted(Admitted),
    Interrupted,
    Eof,
}

#[derive(Clone, Debug)]
pub struct View {
    pub generation: Generation,
    pub revision: u64,
    pub state: State,
    pub text: String,
    pub decoder_pending: bool,
    pub paste: bool,
    pub search: Option<String>,
    /// Process-local count of incomplete bounded output cleanup attempts.
    pub output_tears: usize,
}

enum Request {
    Begin {
        generation: Generation,
        profile: PromptProfile,
        completion: CompletionSnapshot,
        history: Vec<String>,
    },
    Protocol(Command),
    Pause {
        generation: Generation,
        revision: u64,
    },
    Consume {
        generation: Generation,
        revision: u64,
    },
    Admit {
        generation: Generation,
        revision: u64,
        echo: String,
        admitted: Admitted,
        token: OwnerToken,
        drain: std::time::Duration,
    },
    Reserve {
        generation: Generation,
        revision: u64,
        token: OwnerToken,
    },
    Inspect,
    HistoryLoad(String),
    HistoryAppend(String),
    HistoryRead,
    HistoryEncode,
    Stop,
    Completed {
        generation: Generation,
        revision: u64,
        start: usize,
        candidates: Vec<String>,
    },
}
#[derive(Debug)]
enum Response {
    Reply(Reply),
    View(View),
    Stopped,
    History(Vec<String>),
    EncodedHistory(String),
    AddedHistory(bool),
}
struct Envelope {
    request: Request,
    reply: mpsc::SyncSender<io::Result<Response>>,
}

#[derive(Clone)]
pub struct Control {
    sender: mpsc::SyncSender<Envelope>,
    wake: Arc<Mutex<UnixStream>>,
    cleanup: Arc<Cleanup>,
}
#[derive(Default)]
struct Cleanup {
    result: Mutex<Option<Result<(), String>>>,
    done: Condvar,
}
impl Cleanup {
    fn finish(&self, result: io::Result<()>) {
        *self.result.lock().unwrap() = Some(result.map_err(|e| e.to_string()));
        self.done.notify_all();
    }
    fn wait(&self) -> io::Result<()> {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.done.wait(result).unwrap();
        }
        result.as_ref().unwrap().clone().map_err(io::Error::other)
    }
}
impl Control {
    pub fn load_history(&self, text: String) -> io::Result<()> {
        if text.len() > 16 * 1024 * 1024 {
            return Err(io::Error::other("history file limit"));
        }
        self.call(Request::HistoryLoad(text)).map(|_| ())
    }
    pub fn append_history(&self, text: &str) -> io::Result<bool> {
        if text.len() > MAX_HISTORY_ENTRY_BYTES {
            return Err(io::Error::other("history entry limit"));
        }
        match self.call(Request::HistoryAppend(text.into()))? {
            Response::AddedHistory(added) => Ok(added),
            _ => Err(io::Error::other("unexpected history reply")),
        }
    }
    pub fn history(&self) -> io::Result<Vec<String>> {
        match self.call(Request::HistoryRead)? {
            Response::History(entries) => Ok(entries),
            _ => Err(io::Error::other("unexpected history reply")),
        }
    }
    pub fn encode_history(&self) -> io::Result<String> {
        match self.call(Request::HistoryEncode)? {
            Response::EncodedHistory(text) => Ok(text),
            _ => Err(io::Error::other("unexpected history reply")),
        }
    }
    fn completed(&self, request: Request) {
        let (reply, _) = mpsc::sync_channel(1);
        // Only the single completion worker may wait for queue space. Losing
        // this result on RESOURCE_LIMIT would strand the completion busy bit.
        if self.sender.send(Envelope { request, reply }).is_ok() {
            let _ = self.wake.lock().unwrap().write(&[1]);
        }
    }
    fn send(&self, request: Request) -> io::Result<mpsc::Receiver<io::Result<Response>>> {
        // Full queue is an immediate resource-limit error, not deferred work;
        // disconnection is failure, never evidence of terminal restoration.
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .try_send(Envelope { request, reply: tx })
            .map_err(|e| io::Error::other(e.to_string()))?;
        // One socket byte per bounded queue entry. Nonblocking and coalescible.
        match self.wake.lock().unwrap().write(&[1]) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        Ok(rx)
    }
    fn call(&self, request: Request) -> io::Result<Response> {
        self.send(request)?
            .recv()
            .map_err(|_| io::Error::other("editor stopped without reply"))?
    }
    /// Bounded variant for the admission owner, which runs on a Bus task and
    /// must not park a blocking-pool thread on an editor that is wedged. A
    /// timeout here is not a hang: a reservation the owner abandons is released
    /// by the editor's own reservation deadline.
    fn call_within(&self, request: Request, budget: std::time::Duration) -> io::Result<Response> {
        self.send(request)?
            .recv_timeout(budget)
            .map_err(|_| io::Error::other("editor did not answer within the admission budget"))?
    }
    pub fn command(&self, command: Command) -> io::Result<Reply> {
        match self.call(Request::Protocol(command))? {
            Response::Reply(reply) => Ok(reply),
            _ => Err(io::Error::other("unexpected editor reply")),
        }
    }
    pub fn pause(&self, generation: Generation, revision: u64) -> io::Result<Reply> {
        match self.call(Request::Pause {
            generation,
            revision,
        })? {
            Response::Reply(reply) => Ok(reply),
            _ => Err(io::Error::other("unexpected pause reply")),
        }
    }
    pub fn inspect(&self) -> io::Result<View> {
        match self.call(Request::Inspect)? {
            Response::View(view) => Ok(view),
            _ => Err(io::Error::other("unexpected view reply")),
        }
    }
    /// The admission owner calls this only after its identity/deadline checks.
    pub fn consume_reservation(&self, generation: Generation, revision: u64) -> io::Result<()> {
        self.call(Request::Consume {
            generation,
            revision,
        })
        .map(|_| ())
    }
    /// Steps 5-6 of the admission sequence, as ONE operation on the thread that
    /// owns the terminal: echo the announcement while the reservation still
    /// stands, then consume the prompt generation, then hand the line over.
    ///
    /// Doing it anywhere else would let the announcement and the execution be
    /// separated by a failure. Here the only failure after the echo is the
    /// consume, and the editor thread is the sole writer of the state it
    /// checks, so the two cannot disagree.
    /// Never returns an error, because "the request failed" is not an answer an
    /// admission can act on: the caller has to know whether a line executed.
    ///
    /// `budget` bounds the normal reply. If it expires the owner tries to
    /// ABANDON the envelope; winning that race proves the editor never acted.
    /// Losing it means the editor is already mid-echo, so the owner waits out a
    /// second bounded `grace` for the real answer rather than guessing — and
    /// only reports `Unknown` when even that produces nothing.
    pub fn admit(&self, attempt: AdmitRequest, token: &OwnerToken) -> Admission {
        let AdmitRequest {
            generation,
            revision,
            echo,
            admitted,
            budget,
            grace,
        } = attempt;
        // The echo's drain deadline is DERIVED from the budget its caller is
        // held to rather than fixed, so a write can never outlive the answer
        // somebody is waiting on.
        let drain = budget.mul_f32(0.6);
        let request = Request::Admit {
            generation,
            revision,
            echo,
            admitted,
            token: token.clone(),
            drain,
        };
        // Never queued: a full queue means no editor turn will ever see it.
        let Ok(reply) = self.send(request) else {
            return Admission::Refused;
        };
        // Only the Ok path delivers a line. An editor-side error is graded by
        // the token: still un-claimed means the editor refused before touching
        // anything, which is a clean retryable BUSY; already claimed means it
        // got as far as the pane, so the attempt is recorded instead.
        match reply.recv_timeout(budget) {
            Ok(Ok(_)) => return Admission::Executed,
            Ok(Err(_)) => return self.grade(token),
            Err(_) => {}
        }
        if token.abandon() {
            return Admission::NotStarted;
        }
        match reply.recv_timeout(grace) {
            Ok(Ok(_)) => Admission::Executed,
            Ok(Err(_)) => Admission::NotStarted,
            Err(_) => Admission::Unknown,
        }
    }
    fn grade(&self, token: &OwnerToken) -> Admission {
        if token.abandon() {
            Admission::Refused
        } else {
            Admission::NotStarted
        }
    }
    /// Release a reservation without executing anything (step 4 refusal,
    /// cancellation, identity loss). The editor returns to editing with the
    /// same, untouched, empty draft.
    pub fn release(
        &self,
        generation: Generation,
        revision: u64,
        budget: std::time::Duration,
    ) -> io::Result<Reply> {
        match self.call_within(
            Request::Protocol(Command::Resume {
                generation,
                edit_revision: revision,
            }),
            budget,
        )? {
            Response::Reply(reply) => Ok(reply),
            _ => Err(io::Error::other("unexpected release reply")),
        }
    }
    /// Step 2-3: ask the editor to give up the terminal for an execution. A
    /// `Busy` reply is a refusal that changed nothing — the draft, the search
    /// and the paste in progress are all still there.
    ///
    /// Carries an owner token for the same reason `admit` does: without one, a
    /// reserve that timed out would still be processed later and would grant an
    /// ORPHANED reservation over a prompt whose owner had already given up.
    pub fn reserve(
        &self,
        generation: Generation,
        revision: u64,
        token: &OwnerToken,
        budget: std::time::Duration,
    ) -> io::Result<Reply> {
        let result = self.call_within(
            Request::Reserve {
                generation,
                revision,
                token: token.clone(),
            },
            budget,
        );
        if result.is_err() {
            // Whether this wins or loses, the editor's own reservation deadline
            // is the backstop; winning it means the suspension never happened
            // at all, which is the case worth making impossible to miss.
            token.abandon();
        }
        match result? {
            Response::Reply(reply) => Ok(reply),
            _ => Err(io::Error::other("unexpected reserve reply")),
        }
    }
    pub fn inspect_within(&self, budget: std::time::Duration) -> io::Result<View> {
        match self.call_within(Request::Inspect, budget)? {
            Response::View(view) => Ok(view),
            _ => Err(io::Error::other("unexpected view reply")),
        }
    }
    pub fn shutdown(&self) -> io::Result<()> {
        // Shutdown cannot be discarded on queue saturation: Drop must be able
        // to join, and the HUP owner must wait for restoration before exit.
        let (reply, receive) = mpsc::sync_channel(1);
        if self
            .sender
            .send(Envelope {
                request: Request::Stop,
                reply,
            })
            .is_err()
        {
            return self.cleanup.wait();
        }
        match self.wake.lock().unwrap().write(&[1]) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return self.cleanup.wait(),
        }
        let _ = receive.recv();
        self.cleanup.wait()
    }
}

pub struct OwnedEditor {
    pub control: Control,
    lines: mpsc::Receiver<io::Result<Line>>,
    worker: Option<JoinHandle<()>>,
}
impl OwnedEditor {
    pub fn start(input: File, output: File) -> io::Result<Self> {
        Self::start_with_activation(input, output, None)
    }
    /// A Send-only owned acknowledgement; no session or evaluator dependency.
    pub fn start_with_activation(
        input: File,
        output: File,
        activation: Option<fn(Generation)>,
    ) -> io::Result<Self> {
        // Fail before installing signal hooks when an independent tty writer
        // cannot be opened (the REPL can then safely fall back to rustyline).
        let terminal = Terminal::new(input, output)?;
        let (wake_read, wake_write) = UnixStream::pair()?;
        wake_read.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;
        let (signal_read, signal_write) = UnixStream::pair()?;
        signal_read.set_nonblocking(true)?;
        // Presence covers the entire editor thread, including cooked phases;
        // it is independent of the reducer's State::Editing.
        let registration = super::signals::Registration::new(signal_write.try_clone()?)?;
        let continued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cont_flag = signal_hook::flag::register(libc::SIGCONT, continued.clone())?;
        let cont_wake =
            signal_hook::low_level::pipe::register(libc::SIGCONT, signal_write.try_clone()?)?;
        let resize = signal_hook::low_level::pipe::register(libc::SIGWINCH, signal_write)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let control = Control {
            sender,
            wake: Arc::new(Mutex::new(wake_write)),
            cleanup: Arc::new(Cleanup::default()),
        };
        let (line_tx, lines) = mpsc::sync_channel(1);
        let worker_control = control.clone();
        // Captured here, before the worker starts, so no later environment
        // mutation by evaluated Mix can reach it.
        let admit_delay = std::env::var("MIX_ADMIT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(std::time::Duration::ZERO, std::time::Duration::from_millis);
        let worker = std::thread::Builder::new()
            .name("mix-editor".into())
            .spawn(move || {
                let mut owner = Owner {
                    activation,
                    editor: Editor::new(0),
                    // Zero means unbound until the local session owner supplies Begin.
                    generation: Generation {
                        session: 0,
                        prompt: 0,
                    },
                    terminal,
                    stopped: false,
                    continued,
                    decoder: Decoder::default(),
                    profile: PromptProfile::Primary(String::new()),
                    completion: Arc::new(CompletionSnapshot::default()),
                    history: Vec::new(),
                    saved_history: super::history::History::default(),
                    history_index: 0,
                    draft: String::new(),
                    cycle: None,
                    completing: false,
                    completion_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    deferred: None,
                    search_draft: None,
                    search_index: None,
                    search_forward: false,
                    line_tx: line_tx.clone(),
                    control: worker_control,
                    reserved_until: None,
                    admit_delay,
                };
                // Receiver remains alive until cleanup is complete. HUP waits
                // on the latch even on channel failure, rather than inferring
                // restoration from disconnect or from a protocol reply.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner.run(wake_read, signal_read, &receiver)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("editor worker panicked")));
                let restored = owner.terminal.restore();
                drop(registration);
                owner.control.cleanup.finish(restored);
                if let Err(error) = result {
                    let _ = line_tx.try_send(Err(error));
                }
                signal_hook::low_level::unregister(resize);
                signal_hook::low_level::unregister(cont_flag);
                signal_hook::low_level::unregister(cont_wake);
            });
        match worker {
            Ok(worker) => Ok(Self {
                control,
                lines,
                worker: Some(worker),
            }),
            Err(error) => {
                signal_hook::low_level::unregister(resize);
                signal_hook::low_level::unregister(cont_flag);
                signal_hook::low_level::unregister(cont_wake);
                Err(error)
            }
        }
    }
    pub fn begin(
        &self,
        generation: Generation,
        profile: PromptProfile,
        completion: CompletionSnapshot,
        history: Vec<String>,
    ) -> io::Result<Reply> {
        if profile.text().len() > 64 * 1024
            || history.len() > 100
            || history.iter().any(|s| s.len() > MAX_HISTORY_ENTRY_BYTES)
        {
            return Err(io::Error::other("editor prompt/history limit"));
        }
        match self.control.call(Request::Begin {
            generation,
            profile,
            completion,
            history,
        })? {
            Response::Reply(reply) => Ok(reply),
            _ => Err(io::Error::other("unexpected begin reply")),
        }
    }
    pub fn readline(&self) -> io::Result<Line> {
        self.lines
            .recv()
            .map_err(|_| io::Error::other("editor thread stopped"))?
    }
}
impl Drop for OwnedEditor {
    fn drop(&mut self) {
        let _ = self.control.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Cycle {
    start: usize,
    end: usize,
    candidates: Vec<String>,
    next: usize,
}
struct Owner {
    activation: Option<fn(Generation)>,
    stopped: bool,
    continued: Arc<std::sync::atomic::AtomicBool>,
    editor: Editor,
    generation: Generation,
    terminal: Terminal,
    decoder: Decoder,
    profile: PromptProfile,
    completion: Arc<CompletionSnapshot>,
    history: Vec<String>,
    saved_history: super::history::History,
    history_index: usize,
    draft: String,
    cycle: Option<Cycle>,
    completing: bool,
    completion_running: Arc<std::sync::atomic::AtomicBool>,
    deferred: Option<Request>,
    search_draft: Option<Buffer>,
    search_index: Option<usize>,
    search_forward: bool,
    line_tx: mpsc::SyncSender<io::Result<Line>>,
    control: Control,
    /// Set when a SuspendRequested was granted for an execution admission.
    reserved_until: Option<std::time::Instant>,
    /// Test-only stall before an admission claims its token, so a fixture can
    /// produce the owner-gives-up-while-queued interleaving deterministically.
    /// Captured at editor start; zero in every ordinary run.
    admit_delay: std::time::Duration,
}
fn protocol(error: super::ProtocolError) -> io::Error {
    io::Error::other(format!("editor protocol: {error:?}"))
}
struct CompletionRunning(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CompletionRunning {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Owner {
    fn effect(&mut self, effect: Effect) -> io::Result<Reply> {
        match effect {
            Effect::Reply(reply) => Ok(reply),
            Effect::Modes { token, action } => {
                let result = match action {
                    ModeAction::EnterEditing => self.terminal.enter(),
                    ModeAction::Restore => self.terminal.restore(),
                };
                if action == ModeAction::EnterEditing
                    && result
                        .as_ref()
                        .is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
                {
                    self.stopped = true;
                    return self.editor.modes_waiting(token).map_err(protocol);
                }
                let reply = self
                    .editor
                    .modes_completed(token, result.is_ok())
                    .map_err(protocol);
                result?;
                let mut reply = reply?;
                if action == ModeAction::EnterEditing {
                    self.stopped = false;
                    self.decoder.resume();
                    if let Some(result) = self.deferred.take() {
                        self.request(result)?;
                    }
                    self.draw()?;
                    if let Some(activation) = self.activation {
                        activation(self.generation);
                    }
                    reply = Reply::Editing {
                        generation: self.generation,
                        edit_revision: self.editor.edit_revision(),
                    };
                }
                Ok(reply)
            }
        }
    }
    fn draw(&mut self) -> io::Result<()> {
        // Bounded O(buffer) reflow per keystroke is accepted for this preview;
        // incremental layout/render optimisation is deferred to parity work.
        let layout = super::render::layout(
            self.profile.text(),
            self.editor.buffer(),
            self.terminal.size().0,
        )
        .map_err(|e| io::Error::other(format!("editor layout: {e:?}")))?;
        self.terminal.draw(&layout, self.profile.text())
    }
    fn finish(&mut self, line: Line) -> io::Result<()> {
        let layout = super::render::layout(
            self.profile.text(),
            self.editor.buffer(),
            self.terminal.size().0,
        )
        .map_err(|e| io::Error::other(format!("editor layout: {e:?}")))?;
        self.terminal.finish(&layout)?;
        self.terminal.restore()?;
        self.editor.finish_line().map_err(protocol)?;
        self.line_tx
            .try_send(Ok(line))
            .map_err(|e| io::Error::other(e.to_string()))
    }
    fn run(
        &mut self,
        mut wake: UnixStream,
        mut signals: UnixStream,
        requests: &mpsc::Receiver<Envelope>,
    ) -> io::Result<()> {
        loop {
            let editing = self.editor.state() == State::Editing;
            // §8's human-first rule has to hold through the RESERVATION too, not
            // just while editing. The window is cooked mode with kernel echo and
            // an unpolled tty: input landing there would be echoed into the
            // announcement line and then handed to the admitted execution as
            // stdin. Watching the descriptor is what lets the human refuse the
            // admission instead of being eaten by it.
            //
            // LIMIT, stated because it is not obvious: the window is CANONICAL
            // mode, so the line discipline holds a partial line and `poll` sees
            // nothing until Enter. This therefore defends a completed line —
            // the case where a whole human command would otherwise be consumed
            // as somebody else's stdin — and not a half-typed one. Closing that
            // needs the mode restore moved from reservation time to commit
            // time, which is a change to the stage-C editor protocol.
            let reserved = self.reserved_until.is_some();
            let ready = input::wait(
                (editing || reserved).then(|| self.terminal.fd()),
                wake.as_raw_fd(),
                signals.as_raw_fd(),
                self.terminal.output_fd(),
                self.wait_timeout(editing),
            )?;
            if ready[0] && reserved && !editing {
                // Deliberately NOT read: the byte belongs to the editor, which
                // is about to take the prompt back and will decode it normally.
                // Resuming makes `admissible` false, so an Admit envelope
                // already in flight aborts before it echoes anything.
                self.expire_reservation()?;
                continue;
            }
            // Human input observed in this poll wins before control admission.
            if ready[0] && editing {
                match input::read(self.terminal.fd()) {
                    Ok(Some(byte)) => {
                        let key = self.decoder.feed(byte);
                        let mut interaction = self.editor.interaction().clone();
                        interaction.decoder_pending = self.decoder.pending();
                        interaction.paste = self.decoder.pasting();
                        self.editor.set_interaction(interaction).map_err(protocol)?;
                        if let Some(key) = key {
                            self.key(key)?;
                        }
                    }
                    Ok(None) => {
                        self.finish(Line::Eof)?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if ready[2] {
                drain(&mut signals)?;
                if super::signals::take_stop() {
                    self.stop()?;
                }
                if self
                    .continued
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    self.resume_foreground()?;
                }
                if self.editor.state() == State::Editing {
                    self.draw()?;
                }
            }
            if ready[3] && self.terminal.flush_ready()? && self.editor.state() == State::Editing {
                self.draw()?;
            }
            if ready[1] {
                drain(&mut wake)?;
                for envelope in requests.try_iter().take(QUEUE) {
                    let stop = matches!(
                        envelope.request,
                        Request::Stop | Request::Protocol(Command::Shutdown { .. })
                    );
                    let response = self.request(envelope.request);
                    let fatal = self.editor.state() == State::Failed;
                    let succeeded = response.is_ok();
                    let _ = envelope.reply.send(response);
                    if fatal {
                        return Err(io::Error::other("terminal mode operation failed"));
                    }
                    if stop && succeeded {
                        return Ok(());
                    }
                }
            }
            if self.editor.state() == State::Editing
                && let Some(key) = self.decoder.expire()
            {
                let mut interaction = self.editor.interaction().clone();
                interaction.decoder_pending = false;
                self.editor.set_interaction(interaction).map_err(protocol)?;
                self.key(key)?;
            }
            self.sync_reservation();
            if self
                .reserved_until
                .is_some_and(|until| std::time::Instant::now() >= until)
            {
                self.expire_reservation()?;
            }
        }
    }
    /// The editor waits on input, control and output readiness; a standing
    /// reservation adds its own deadline to that same wait so an abandoned
    /// admission cannot leave the human without a prompt.
    fn wait_timeout(&self, editing: bool) -> i32 {
        let decoder = if editing { self.decoder.timeout() } else { -1 };
        let Some(until) = self.reserved_until else {
            return decoder;
        };
        let left = until.saturating_duration_since(std::time::Instant::now());
        let reservation = left
            .as_millis()
            .saturating_add(u128::from(!left.is_zero()))
            .min(i32::MAX as u128) as i32;
        if decoder < 0 {
            reservation
        } else {
            decoder.min(reservation)
        }
    }
    /// Derive the deadline from the editor's own state rather than keeping a
    /// second copy of it: whatever ended the reservation — consumption, a
    /// release, a shutdown — has already been recorded there.
    fn sync_reservation(&mut self) {
        if self.editor.reserved() && self.editor.state() == State::Suspended {
            self.reserved_until
                .get_or_insert_with(|| std::time::Instant::now() + RESERVATION);
        } else {
            self.reserved_until = None;
        }
    }
    /// Take the prompt back from an admission owner that never came back. The
    /// draft is empty by construction (only an empty primary prompt can be
    /// reserved), so this restores exactly what the human was looking at.
    fn expire_reservation(&mut self) -> io::Result<()> {
        self.reserved_until = None;
        if self.editor.state() != State::Suspended || !self.editor.reserved() {
            return Ok(());
        }
        let effect = self
            .editor
            .command(Command::Resume {
                generation: self.generation,
                edit_revision: self.editor.edit_revision(),
            })
            .map_err(protocol)?;
        self.effect(effect)?;
        Ok(())
    }
    fn request(&mut self, request: Request) -> io::Result<Response> {
        let effect = match request {
            Request::Begin {
                generation,
                profile,
                mut completion,
                mut history,
            } => {
                if matches!(profile, PromptProfile::Restricted) {
                    completion = CompletionSnapshot::default();
                    history.clear();
                }
                self.editor
                    .bind_prompt_session(generation)
                    .map_err(protocol)?;
                let effect = self
                    .editor
                    .command(Command::BeginPrompt {
                        generation,
                        profile: profile.clone(),
                    })
                    .map_err(protocol)?;
                self.generation = generation;
                self.profile = profile;
                self.completion = Arc::new(completion);
                self.history = if matches!(self.profile, PromptProfile::Restricted) {
                    Vec::new()
                } else if history.is_empty() {
                    self.saved_history.entries().to_vec()
                } else {
                    history
                };
                self.history_index = self.history.len();
                self.draft.clear();
                self.cycle = None;
                self.completing = false;
                // A previous generation's worker must not clear this flag.
                self.completion_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.deferred = None;
                self.search_draft = None;
                self.search_index = None;
                self.search_forward = false;
                self.decoder = Decoder::default();
                effect
            }
            Request::Protocol(Command::BeginPrompt { .. }) => {
                return Err(io::Error::other("begin requires owned prompt snapshots"));
            }
            Request::Protocol(command) => self.editor.command(command).map_err(protocol)?,
            Request::Consume {
                generation,
                revision,
            } => {
                self.editor
                    .consume_reservation(generation, revision)
                    .map_err(protocol)?;
                return Ok(Response::Stopped);
            }
            Request::Reserve {
                generation,
                revision,
                token,
            } => {
                // An owner that stopped waiting must not be given a
                // reservation it will never consume: the prompt would sit
                // suspended until its deadline with nobody driving it.
                if !token.claim() {
                    return Err(io::Error::other("reservation abandoned by its owner"));
                }
                self.editor
                    .command(Command::SuspendRequested {
                        generation,
                        edit_revision: revision,
                    })
                    .map_err(protocol)?
            }
            Request::Admit {
                generation,
                revision,
                echo,
                admitted,
                token,
                drain,
            } => {
                // Refuse BEFORE the echo. Everything `admissible` reads is
                // owned by this thread, so a true answer here still holds after
                // the write below — the announcement and the execution it
                // announces cannot be separated by a concurrent change.
                if !self.editor.admissible(generation, revision) {
                    return Err(protocol(super::ProtocolError::InvalidState));
                }
                // Test hook for the one interleaving that cannot be produced by
                // timing alone: an envelope whose owner gives up while it is
                // still queued. Read ONCE at editor start, so nothing later in
                // the process can turn it on, and zero by default — the cost in
                // production is one comparison against a field.
                if !self.admit_delay.is_zero() {
                    std::thread::sleep(self.admit_delay);
                }
                // The LAST thing checked before the echo. An envelope whose
                // owner has stopped waiting leaves no mark on the pane and
                // executes nothing; the owner then knows that for certain
                // rather than having to assume it.
                if !token.claim() {
                    return Err(io::Error::other("admission abandoned by its owner"));
                }
                self.terminal.echo(&echo, drain)?;
                if let Err(error) = self.editor.consume_reservation(generation, revision) {
                    // Unreachable given the check above, but a consumed prompt
                    // with no delivered line would park the REPL on a readline
                    // that never returns. Unblock it, then fail loudly.
                    let _ = self.line_tx.try_send(Ok(Line::Interrupted));
                    self.reserved_until = None;
                    return Err(protocol(error));
                }
                self.reserved_until = None;
                self.line_tx
                    .try_send(Ok(Line::Admitted(admitted)))
                    .map_err(|e| io::Error::other(e.to_string()))?;
                return Ok(Response::Stopped);
            }
            Request::Pause {
                generation,
                revision,
            } => self.editor.pause(generation, revision).map_err(protocol)?,
            Request::Stop => {
                let effect = self
                    .editor
                    .command(Command::Shutdown {
                        generation: self.generation,
                    })
                    .map_err(protocol)?;
                self.effect(effect)?;
                return Ok(Response::Stopped);
            }
            Request::Inspect => {
                return Ok(Response::View(View {
                    generation: self.generation,
                    revision: self.editor.edit_revision(),
                    state: self.editor.state(),
                    text: self.editor.buffer().text().into(),
                    decoder_pending: self.decoder.pending(),
                    paste: self.decoder.pasting(),
                    search: self.editor.interaction().search.clone(),
                    output_tears: super::terminal::OUTPUT_TEARS
                        .load(std::sync::atomic::Ordering::Relaxed),
                }));
            }
            Request::HistoryRead => {
                return Ok(Response::History(self.saved_history.entries().to_vec()));
            }
            Request::HistoryEncode => {
                return Ok(Response::EncodedHistory(self.saved_history.encode()));
            }
            Request::HistoryLoad(text) => {
                if self.editor.state() != State::Idle {
                    return Err(io::Error::other("history load requires idle editor"));
                }
                self.saved_history.load(&text);
                return Ok(Response::Stopped);
            }
            Request::HistoryAppend(text) => {
                if self.editor.state() != State::Idle {
                    return Err(io::Error::other("history append requires idle editor"));
                }
                return Ok(Response::AddedHistory(self.saved_history.append(&text)));
            }
            Request::Completed {
                generation,
                revision,
                start,
                candidates,
            } => {
                // A previous prompt's worker must not clear a new prompt's busy
                // state or install a deferred result after Begin reset it.
                if generation != self.generation {
                    return Ok(Response::Stopped);
                }
                if self.editor.state() == State::Suspended {
                    self.deferred = Some(Request::Completed {
                        generation,
                        revision,
                        start,
                        candidates,
                    });
                    return Ok(Response::Stopped);
                }
                self.completing = false;
                if self.editor.state() == State::Editing {
                    let valid =
                        generation == self.generation && revision == self.editor.edit_revision();
                    let mut interaction = self.editor.interaction().clone();
                    interaction.completion = false;
                    self.editor.set_interaction(interaction).map_err(protocol)?;
                    if valid && !candidates.is_empty() {
                        self.cycle = Some(Cycle {
                            start,
                            end: self.editor.buffer().cursor(),
                            candidates,
                            next: 0,
                        });
                        self.cycle()?;
                        self.draw()?;
                    }
                }
                return Ok(Response::Stopped);
            }
        };
        self.effect(effect).map(Response::Reply)
    }
    fn stop(&mut self) -> io::Result<()> {
        if self.editor.state() == State::Editing {
            let effect = self
                .editor
                .pause(self.generation, self.editor.edit_revision())
                .map_err(protocol)?;
            self.effect(effect)?;
            self.stopped = true;
        }
        // Only bypass the cooperative handler while cooked. The controller
        // still owns default-stop disposition and process-group behaviour.
        super::signals::cooperative(false);
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        super::signals::cooperative(true);
        self.resume_foreground()
    }
    fn resume_foreground(&mut self) -> io::Result<()> {
        // bg sends SIGCONT too. Remain cooked and exclude tty reads until fg's
        // later SIGCONT; no timer or background tcsetattr retries.
        if self.stopped && self.terminal.foreground() {
            let effect = self
                .editor
                .command(Command::Resume {
                    generation: self.generation,
                    edit_revision: self.editor.edit_revision(),
                })
                .map_err(protocol)?;
            self.effect(effect)?;
            self.stopped = self.editor.state() != State::Editing;
        }
        Ok(())
    }
    fn cycle(&mut self) -> io::Result<()> {
        if let Some(cycle) = &mut self.cycle {
            let text = &cycle.candidates[cycle.next % cycle.candidates.len()];
            match self
                .editor
                .edit(|b| b.replace(cycle.start..cycle.end, text))
            {
                Ok(_) => {
                    cycle.end = cycle.start + text.len();
                    cycle.next += 1;
                }
                Err(super::ProtocolError::Edit(_)) => self.terminal.bell()?,
                Err(e) => return Err(protocol(e)),
            }
        }
        Ok(())
    }
    fn key(&mut self, key: Key) -> io::Result<()> {
        if key != Key::Control(9) {
            self.cycle = None;
        }
        if self.editor.interaction().search.is_some() && !matches!(key, Key::Control(26)) {
            match key {
                Key::Text(text) => {
                    let mut interaction = self.editor.interaction().clone();
                    let term = interaction.search.as_mut().unwrap();
                    if term.len() + text.len() <= 4096 {
                        term.push_str(&text);
                    }
                    self.editor.set_interaction(interaction).map_err(protocol)?;
                    self.search(false)?;
                }
                Key::Control(18) => {
                    self.search_forward = false;
                    self.search(true)?;
                }
                Key::Control(19) => {
                    self.search_forward = true;
                    self.search(true)?;
                }
                Key::Control(8 | 127) => {
                    let mut interaction = self.editor.interaction().clone();
                    interaction.search.as_mut().unwrap().pop();
                    self.editor.set_interaction(interaction).map_err(protocol)?;
                    self.search(false)?;
                }
                Key::Escape | Key::Control(7) => {
                    if let Some(draft) = self.search_draft.take() {
                        self.editor
                            .edit(|b| {
                                *b = draft;
                                Ok(true)
                            })
                            .map_err(protocol)?;
                    }
                    self.end_search()?;
                }
                Key::Control(13 | 10) => {
                    // Deliberate promotion-gate divergence: select, don't submit.
                    self.end_search()?;
                }
                Key::Control(3) => {
                    self.end_search()?;
                    return self.finish(Line::Interrupted);
                }
                _ => {
                    self.end_search()?;
                    return self.key(key);
                }
            }
            return self.draw();
        }
        match key {
            Key::Control(18 | 19) => {
                self.search_draft = Some(self.editor.buffer().clone());
                self.search_index = None;
                self.search_forward = key == Key::Control(19);
                let mut interaction = self.editor.interaction().clone();
                interaction.search = Some(String::new());
                self.editor.set_interaction(interaction).map_err(protocol)?;
            }
            Key::Control(13 | 10) => {
                if matches!(self.profile, PromptProfile::Restricted)
                    && !restricted_command(self.editor.buffer().text())
                {
                    self.terminal.bell()?;
                    return Ok(());
                }
                return self.finish(Line::Submitted(self.editor.buffer().text().into()));
            }
            Key::Control(3) => return self.finish(Line::Interrupted),
            Key::Control(4) if self.editor.buffer().text().is_empty() => {
                return self.finish(Line::Eof);
            }
            Key::Control(26) => {
                return self.stop();
            }
            Key::Control(9) if self.profile.allows_completion() => {
                if self.cycle.is_some() {
                    self.cycle()?;
                } else if !self.completing
                    && !self
                        .completion_running
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let mut interaction = self.editor.interaction().clone();
                    interaction.completion = true;
                    self.editor.set_interaction(interaction).map_err(protocol)?;
                    let revision = self.editor.edit_revision();
                    let generation = self.generation;
                    let snapshot = self.completion.clone();
                    let text = self.editor.buffer().text().to_owned();
                    let cursor = self.editor.buffer().cursor();
                    let control = self.control.clone();
                    let running = self.completion_running.clone();
                    let spawned = std::thread::Builder::new()
                        .name("mix-completion".into())
                        .spawn(move || {
                            let _running = CompletionRunning(running);
                            // Still deliver a result on panic so the owner's
                            // completion interaction/Busy state also clears.
                            let (start, candidates) =
                                std::panic::catch_unwind(|| snapshot.complete(&text, cursor))
                                    .unwrap_or_else(|_| (cursor, Vec::new()));
                            control.completed(Request::Completed {
                                generation,
                                revision,
                                start,
                                candidates,
                            });
                        });
                    if spawned.is_ok() {
                        self.completing = true;
                    } else {
                        self.completion_running
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        let mut interaction = self.editor.interaction().clone();
                        interaction.completion = false;
                        self.editor.set_interaction(interaction).map_err(protocol)?;
                        self.terminal.bell()?;
                    }
                }
            }
            Key::Up | Key::Down | Key::Control(16 | 14) => {
                let previous = matches!(key, Key::Up | Key::Control(16));
                if self.history_index == self.history.len() {
                    self.draft = self.editor.buffer().text().into();
                }
                if previous {
                    self.history_index = self.history_index.saturating_sub(1);
                } else {
                    self.history_index = (self.history_index + 1).min(self.history.len());
                }
                let text = self
                    .history
                    .get(self.history_index)
                    .unwrap_or(&self.draft)
                    .clone();
                self.edit(|b| b.replace(0..b.text().len(), &text))?;
            }
            Key::Text(text) => self.edit(|b| b.insert(&text))?,
            Key::Paste(text) => self.edit(|b| b.paste(&text))?,
            Key::PasteOverflow => {
                let layout = super::render::layout(
                    self.profile.text(),
                    self.editor.buffer(),
                    self.terminal.size().0,
                )
                .map_err(|e| io::Error::other(format!("editor layout: {e:?}")))?;
                self.terminal.notice(
                    &layout,
                    "mix: paste exceeds 64 KiB; paste rejected, draft preserved",
                )?;
            }
            Key::Left | Key::Control(2) => self.edit(Buffer::left)?,
            Key::Right | Key::Control(6) => self.edit(Buffer::right)?,
            Key::WordLeft => self.edit(Buffer::word_left)?,
            Key::WordRight => self.edit(Buffer::word_right)?,
            Key::Home | Key::Control(1) => self.edit(|b| b.move_to(0))?,
            Key::End | Key::Control(5) => self.edit(|b| b.move_to(b.text().len()))?,
            Key::Delete | Key::Control(4) => self.edit(Buffer::delete)?,
            Key::Control(8 | 127) => self.edit(Buffer::backspace)?,
            Key::Control(11) => self.edit(|b| b.kill(b.cursor()..b.text().len(), false))?,
            Key::Control(21) => self.edit(|b| b.kill(0..b.cursor(), true))?,
            Key::Control(23) => self.edit(|b| {
                let end = b.cursor();
                b.word_left()?;
                b.kill(b.cursor()..end, true)
            })?,
            Key::Control(25) => self.edit(Buffer::yank)?,
            Key::Control(31) => self.edit(Buffer::undo)?,
            Key::Redo => self.edit(Buffer::redo)?,
            Key::YankPop => self.edit(Buffer::yank_pop)?,
            Key::Invalid => self.terminal.bell()?,
            _ => {}
        }
        self.draw()
    }
    fn edit(
        &mut self,
        operation: impl FnOnce(&mut Buffer) -> Result<bool, super::buffer::EditError>,
    ) -> io::Result<()> {
        match self.editor.edit(operation) {
            Ok(_) => Ok(()),
            Err(super::ProtocolError::Edit(_)) => self.terminal.bell(),
            Err(e) => Err(protocol(e)),
        }
    }
    fn end_search(&mut self) -> io::Result<()> {
        let mut interaction = self.editor.interaction().clone();
        interaction.search = None;
        self.editor.set_interaction(interaction).map_err(protocol)?;
        self.search_draft = None;
        self.search_index = None;
        Ok(())
    }
    fn search(&mut self, previous: bool) -> io::Result<()> {
        let term = self.editor.interaction().search.as_deref().unwrap_or("");
        let initial = if self.search_forward {
            (!self.history.is_empty()).then_some(0)
        } else {
            self.history.len().checked_sub(1)
        };
        let start = match (previous, self.search_index) {
            (true, Some(index)) if self.search_forward => {
                index.checked_add(1).filter(|i| *i < self.history.len())
            }
            (true, Some(index)) => index.checked_sub(1),
            (_, index) => index.or(initial),
        };
        let direction = if self.search_forward {
            super::history::Direction::Forward
        } else {
            super::history::Direction::Reverse
        };
        let found = start.and_then(|start| {
            if term.is_empty() {
                Some(start)
            } else {
                super::history::search(
                    &self.history,
                    term,
                    start,
                    direction,
                    super::history::SearchKind::FullText,
                )
                .map(|found| found.index)
            }
        });
        if let Some(index) = found {
            let text = self.history[index].clone();
            self.search_index = Some(index);
            self.edit(|b| b.replace(0..b.text().len(), &text))?;
        }
        Ok(())
    }
}
fn drain(stream: &mut UnixStream) -> io::Result<()> {
    let mut bytes = [0; QUEUE];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => return Err(io::Error::other("editor wake closed")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

fn restricted_command(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let Some(command) = words.next() else {
        return true;
    };
    if !PromptProfile::Restricted.allows_command(command) {
        return false;
    }
    match words.next() {
        None => true,
        Some(id) if matches!(command, "fg" | "bg" | "cancel") => {
            let id = id.strip_prefix('%').unwrap_or(id);
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && words.next().is_none()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn completion_running_clears_during_unwind() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_flag = flag.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _running = super::CompletionRunning(worker_flag);
                panic!("completion failure");
            })
            .is_err()
        );
        assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[test]
    fn completion_filters_before_applying_result_cap() {
        let mut commands: Vec<_> = (0..5000).map(|i| format!("aaa{i}")).collect();
        commands.push("ssh".into());
        let snapshot = super::CompletionSnapshot {
            commands: std::sync::Arc::new(commands),
            ..Default::default()
        };
        assert_eq!(snapshot.complete("ss", 2).1, ["ssh"]);
        assert_eq!(snapshot.complete("a", 1).1.len(), super::MAX_CANDIDATES);
    }
    use super::*;
    #[test]
    fn completion_spans_stay_utf8_boundaries_after_unicode_space() {
        let snapshot = CompletionSnapshot {
            variables: vec!["wide_name".into()],
            ..Default::default()
        };
        let text = "print(\u{3000}$wide";
        let (start, results) = snapshot.complete(text, text.len());
        assert_eq!(&text[start..], "$wide");
        assert_eq!(results, ["$wide_name"]);
    }
}
