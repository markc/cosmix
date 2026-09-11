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

#[derive(Debug)]
pub enum Line {
    Submitted(String),
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
        let (wake_read, wake_write) = UnixStream::pair()?;
        wake_read.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;
        let (signal_read, signal_write) = UnixStream::pair()?;
        signal_read.set_nonblocking(true)?;
        let registration = super::signals::Registration::new(signal_write.try_clone()?)?;
        let continued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cont_flag = signal_hook::flag::register(libc::SIGCONT, continued.clone())?;
        let cont_wake =
            signal_hook::low_level::pipe::register(libc::SIGCONT, signal_write.try_clone()?)?;
        let resize = signal_hook::low_level::pipe::register(libc::SIGWINCH, signal_write)?;
        let terminal = Terminal::new(input, output)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let control = Control {
            sender,
            wake: Arc::new(Mutex::new(wake_write)),
            cleanup: Arc::new(Cleanup::default()),
        };
        let (line_tx, lines) = mpsc::sync_channel(1);
        let worker_control = control.clone();
        let worker = std::thread::Builder::new()
            .name("mix-editor".into())
            .spawn(move || {
                let mut owner = Owner {
                    editor: Editor::new(1),
                    // Attachment/session generation is intentionally stage-D work.
                    generation: Generation {
                        session: 1,
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
                };
                // Receiver remains alive until cleanup is complete. HUP waits
                // on the latch even on channel failure, rather than inferring
                // restoration from disconnect or from a protocol reply.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner.run(wake_read, signal_read, &receiver)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("editor worker panicked")));
                let restored = owner.terminal.restore();
                owner.control.cleanup.finish(restored);
                if let Err(error) = result {
                    let _ = line_tx.try_send(Err(error));
                }
                drop(registration);
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
}
fn protocol(error: super::ProtocolError) -> io::Error {
    io::Error::other(format!("editor protocol: {error:?}"))
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
                let reply = self
                    .editor
                    .modes_completed(token, result.is_ok())
                    .map_err(protocol);
                result?;
                let mut reply = reply?;
                if action == ModeAction::EnterEditing {
                    self.decoder.resume();
                    if let Some(result) = self.deferred.take() {
                        self.request(result)?;
                    }
                    self.draw()?;
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
            let ready = input::wait(
                editing.then(|| self.terminal.fd()),
                wake.as_raw_fd(),
                signals.as_raw_fd(),
                self.terminal.output_fd(),
                if editing { self.decoder.timeout() } else { -1 },
            )?;
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
        }
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
        super::signals::editing(false);
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        super::signals::editing(true);
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
            self.stopped = false;
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
                            let (start, candidates) = snapshot.complete(&text, cursor);
                            running.store(false, std::sync::atomic::Ordering::SeqCst);
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
