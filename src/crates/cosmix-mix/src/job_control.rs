//! Interactive process ownership. No evaluator, editor or Bus dependencies.
//!
//! Only this controller consumes wait statuses for registered interactive
//! children. Captured runners and noninteractive children never enter it.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CLOSE_GRACE: Duration = Duration::from_millis(500);
const STAGE_ARG: &str = "--internal-job-stage";

// Caught dispositions reset on exec; SIG_IGN would leak into captured runners
// whose spawning contract deliberately remains unchanged by interactive jobs.
static MANAGED_FOREGROUND: AtomicBool = AtomicBool::new(false);
static NEXT_LAUNCH_COMMAND_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_launch_command_id() -> u64 {
    NEXT_LAUNCH_COMMAND_ID.fetch_add(1, Ordering::Relaxed)
}

extern "C" fn shell_signal(signal: libc::c_int) {
    if signal != libc::SIGTSTP || MANAGED_FOREGROUND.load(Ordering::Acquire) {
        return;
    }
    // Legacy inherited-stdio waits still belong to the shell's group. Let
    // the outer shell observe and resume us, including on repeated suspends.
    // SA_NODEFER makes raise deliver before reinstalling the handler. Every
    // operation here is async-signal-safe; no controller locks or allocation.
    unsafe {
        let mut default: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&mut default.sa_mask);
        default.sa_sigaction = libc::SIG_DFL;
        let mut handler = std::mem::zeroed();
        libc::sigaction(signal, &default, &mut handler);
        libc::raise(signal);
        libc::sigaction(signal, &handler, std::ptr::null_mut());
    }
}

/// TTY handoff needs SIGTTOU blocked on the calling thread. Keep this scoped:
/// captured children must inherit the ordinary signal mask at an idle shell.
struct TtouGuard {
    previous: libc::sigset_t,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl TtouGuard {
    fn new() -> io::Result<Self> {
        let mut set = unsafe { std::mem::zeroed() };
        let mut previous = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTTOU);
        }
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        Ok(Self {
            previous,
            _thread: std::marker::PhantomData,
        })
    }
}
impl Drop for TtouGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

#[derive(Clone, Default)]
pub enum ExecutionPolicy {
    /// Includes SSH -c, scripts and serve. Never initialise terminal ownership.
    #[default]
    NonInteractive,
    Interactive {
        controller: Arc<Controller>,
        return_on_stop: bool,
    },
}

impl ExecutionPolicy {
    /// Future async host seam: source currently waits through stops instead of
    /// inventing completion of a suspended evaluator invocation.
    pub fn sourced(&self) -> Self {
        match self {
            Self::Interactive { controller, .. } => Self::Interactive {
                controller: controller.clone(),
                return_on_stop: false,
            },
            Self::NonInteractive => Self::NonInteractive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberState {
    Running,
    Stopped(i32),
    Exited(i32),
    Signalled(i32),
    Lost,
}
impl MemberState {
    fn terminal(self) -> bool {
        matches!(self, Self::Exited(_) | Self::Signalled(_) | Self::Lost)
    }
    fn code(self) -> i32 {
        match self {
            Self::Exited(c) => c,
            Self::Stopped(s) | Self::Signalled(s) => 128 + s,
            _ => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
    Done,
}
impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "Running",
            Self::Stopped => "Stopped",
            Self::Done => "Done",
        })
    }
}
#[derive(Clone, Debug)]
pub struct Member {
    pub pid: i32,
    pub state: MemberState,
}
#[derive(Clone)]
pub struct Job {
    pub id: usize,
    /// Session-local launch identity, allocated before spawning any member.
    pub launch_command_id: u64,
    pub pgid: i32,
    pub command: String,
    pub members: Vec<Member>,
    pub modes: Option<libc::termios>,
    pub foreground: bool,
}
impl Job {
    pub fn state(&self) -> JobState {
        if self.members.iter().all(|m| m.state.terminal()) {
            JobState::Done
        } else if self
            .members
            .iter()
            .filter(|m| !m.state.terminal())
            .all(|m| matches!(m.state, MemberState::Stopped(_)))
        {
            JobState::Stopped
        } else {
            JobState::Running
        }
    }
    fn code(&self) -> i32 {
        self.members.last().map(|m| m.state.code()).unwrap_or(1)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub code: i32,
    pub background: bool,
    pub stopped: bool,
}

struct State {
    jobs: BTreeMap<usize, Job>,
    next_id: usize,
    closing: bool,
    closed: bool,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    shell_modes: Mutex<libc::termios>,
}

pub struct Controller {
    shared: Arc<Shared>,
    tty: File,
    shell_pgid: i32,
    parent_pgid: i32,
    signals: signal_hook::iterator::Handle,
    worker: Mutex<Option<JoinHandle<()>>>,
    old_signals: Vec<(i32, libc::sigaction)>,
}

fn modes(fd: i32) -> io::Result<libc::termios> {
    let mut t = std::mem::MaybeUninit::uninit();
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { t.assume_init() })
}
fn set_modes(fd: i32, t: &libc::termios) -> io::Result<()> {
    let _ttou = TtouGuard::new()?;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn foreground(fd: i32, pgid: i32) -> io::Result<()> {
    let _ttou = TtouGuard::new()?;
    if unsafe { libc::tcsetpgrp(fd, pgid) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct SetupGuard<'a> {
    tty: &'a File,
    parent_pgid: i32,
    old_signals: Vec<(i32, libc::sigaction)>,
    committed: bool,
}
impl Drop for SetupGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = foreground(self.tty.as_raw_fd(), self.parent_pgid);
        if unsafe { libc::getpgrp() } != self.parent_pgid {
            unsafe {
                libc::setpgid(0, self.parent_pgid);
            }
        }
        for (sig, old) in &self.old_signals {
            unsafe {
                libc::sigaction(*sig, old, std::ptr::null_mut());
            }
        }
    }
}

impl Controller {
    /// Called ONLY from the interactive entry point. A redirected stdin or
    /// missing controlling terminal declines job management without mutation.
    pub fn interactive() -> io::Result<Option<Arc<Self>>> {
        if unsafe { libc::isatty(0) } == 0 || unsafe { libc::tcgetpgrp(0) } < 0 {
            return Ok(None);
        }
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let parent_pgid = unsafe { libc::getpgrp() };
        // Save SIGTTIN before admission changes it, including on decline.
        let mut inherited_ttin = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(libc::SIGTTIN, std::ptr::null(), &mut inherited_ttin) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut setup = SetupGuard {
            tty: &tty,
            parent_pgid,
            old_signals: vec![(libc::SIGTTIN, inherited_ttin)],
            committed: false,
        };
        let mut admission_attempts = 0;
        while unsafe { libc::tcgetpgrp(fd) } != unsafe { libc::getpgrp() } {
            // Orphaned groups discard SIGTTIN. Never spin forever there.
            if admission_attempts == 8 {
                return Err(io::Error::other(
                    "foreground admission did not stop or acquire terminal",
                ));
            }
            admission_attempts += 1;
            // A nested background shell asks its parent for foregrounding.
            unsafe {
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                // The evaluator runs on a dedicated thread. A process/group
                // directed signal may stop another thread after this thread
                // has already tested the loop again, leaving a stale second
                // stop after `fg`. raise targets this thread and completes
                // delivery before returning; SIGTTIN still stops the whole
                // shell process. No shell-owned children exist at this point.
                libc::raise(libc::SIGTTIN);
            }
        }
        for sig in [libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
            let mut old = unsafe { std::mem::zeroed() };
            let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
            ignore.sa_sigaction = shell_signal as *const () as usize;
            if sig == libc::SIGTSTP {
                ignore.sa_flags = libc::SA_NODEFER;
            }
            unsafe {
                libc::sigemptyset(&mut ignore.sa_mask);
                if libc::sigaction(sig, &ignore, &mut old) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if sig != libc::SIGTTIN {
                setup.old_signals.push((sig, old));
            }
        }
        let shell_pgid = unsafe { libc::getpid() };
        if unsafe { libc::getpgrp() } != shell_pgid && unsafe { libc::setpgid(0, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        foreground(fd, shell_pgid)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                jobs: BTreeMap::new(),
                next_id: 1,
                closing: false,
                closed: false,
            }),
            changed: Condvar::new(),
            shell_modes: Mutex::new(modes(fd)?),
        });
        // signal-hook's SA_RESTART is load-bearing for blocking legacy waits.
        let mut events = signal_hook::iterator::Signals::new([libc::SIGCHLD, libc::SIGHUP])?;
        let signals = events.handle();
        let monitor = shared.clone();
        let monitor_tty = tty.try_clone()?;
        let worker = std::thread::Builder::new()
            .name("mix-jobs".into())
            .spawn(move || {
                for signal in events.forever() {
                    if signal == libc::SIGHUP {
                        close_jobs(&monitor);
                        let _ = foreground(monitor_tty.as_raw_fd(), shell_pgid);
                        let _ = set_modes(
                            monitor_tty.as_raw_fd(),
                            &monitor.shell_modes.lock().unwrap(),
                        );
                        // HUP is a session shutdown, not evaluator cancellation.
                        std::process::exit(128 + libc::SIGHUP);
                    }
                    reap(&monitor);
                    if monitor.state.lock().unwrap().closing {
                        close_jobs(&monitor);
                    }
                }
            })?;
        setup.committed = true;
        let old_signals = std::mem::take(&mut setup.old_signals);
        drop(setup);
        Ok(Some(Arc::new(Self {
            shared,
            tty,
            shell_pgid,
            parent_pgid,
            signals,
            worker: Mutex::new(Some(worker)),
            old_signals,
        })))
    }

    /// Owned snapshots are the attachment point for stage A; no publication
    /// or external callbacks occur under the controller lock.
    pub fn snapshot(&self) -> Vec<Job> {
        self.shared
            .state
            .lock()
            .unwrap()
            .jobs
            .values()
            .cloned()
            .collect()
    }

    /// Seed after the REPL repairs cold-start output modes, before readline.
    pub fn seed_shell_modes(&self) {
        if let Ok(saved) = modes(self.tty.as_raw_fd()) {
            *self.shared.shell_modes.lock().unwrap() = saved;
        }
    }

    pub fn register(
        &self,
        launch_command_id: u64,
        pgid: i32,
        children: Vec<Child>,
        command: String,
        foreground: bool,
    ) -> usize {
        let mut s = self.shared.state.lock().unwrap();
        let id = s.next_id;
        s.next_id += 1;
        // Dropping Child does not reap; from here only the monitor waits.
        let members = children
            .into_iter()
            .map(|c| {
                cosmix_mix::builtins::register_managed_pid(c.id() as i32);
                Member {
                    pid: c.id() as i32,
                    state: MemberState::Running,
                }
            })
            .collect();
        s.jobs.insert(
            id,
            Job {
                id,
                launch_command_id,
                pgid,
                command,
                members,
                modes: None,
                foreground,
            },
        );
        drop(s);
        self.wake();
        id
    }
    fn wake(&self) {
        unsafe {
            libc::kill(libc::getpid(), libc::SIGCHLD);
        }
    }
    pub fn take_terminal(&self, pgid: i32) -> io::Result<TerminalLease<'_>> {
        let ttou = TtouGuard::new()?;
        let saved = modes(self.tty.as_raw_fd())?;
        *self.shared.shell_modes.lock().unwrap() = saved;
        MANAGED_FOREGROUND.store(true, Ordering::Release);
        if let Err(error) = foreground(self.tty.as_raw_fd(), pgid) {
            MANAGED_FOREGROUND.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(TerminalLease {
            controller: self,
            saved,
            _ttou: ttou,
        })
    }
    pub fn finish(
        &self,
        id: usize,
        background: bool,
        return_on_stop: bool,
        mut lease: Option<TerminalLease<'_>>,
    ) -> io::Result<Outcome> {
        if background {
            let pid = self.shared.state.lock().unwrap().jobs[&id]
                .members
                .last()
                .unwrap()
                .pid;
            println!("[{}] {}", id, pid);
            return Ok(Outcome {
                code: 0,
                background: true,
                stopped: false,
            });
        }
        let mut s = self.shared.state.lock().unwrap();
        loop {
            let job = &s.jobs[&id];
            if job.state() == JobState::Done || (return_on_stop && job.state() == JobState::Stopped)
            {
                break;
            }
            s = self.shared.changed.wait(s).unwrap();
        }
        let job = s.jobs.get_mut(&id).unwrap();
        let stopped = job.state() == JobState::Stopped;
        let code = if stopped {
            job.members
                .iter()
                .find_map(|m| {
                    if let MemberState::Stopped(sig) = m.state {
                        Some(128 + sig)
                    } else {
                        None
                    }
                })
                .unwrap_or(1)
        } else {
            job.code()
        };
        if stopped {
            job.modes = modes(self.tty.as_raw_fd()).ok();
        }
        // Preserve deliberate cooked-mode changes such as `stty tostop`.
        // Raw/noncanonical leakage, signals and suspension restore the shell
        // baseline. Terminal modes alone cannot identify user intent beyond
        // this explicit policy.
        if !stopped
            && job.modes.is_none()
            && job
                .members
                .iter()
                .all(|m| matches!(m.state, MemberState::Exited(_)))
            && let Some(lease) = &mut lease
            && let Ok(current) = modes(self.tty.as_raw_fd())
            && current.c_lflag & (libc::ICANON | libc::ISIG) == (libc::ICANON | libc::ISIG)
        {
            lease.saved = current;
        }
        job.foreground = false;
        drop(s);
        drop(lease); // cooked shell ownership before any prompt/notification
        if stopped {
            println!("[{}] Stopped (signal {})", id, code - 128);
        } else {
            self.shared.state.lock().unwrap().jobs.remove(&id);
        }
        Ok(Outcome {
            code,
            background: false,
            stopped,
        })
    }
    pub fn foreground_job(&self, id: Option<usize>) -> io::Result<i32> {
        let job = self.select(id)?;
        let lease = self.take_terminal(job.pgid)?;
        if let Some(t) = job.modes {
            set_modes(self.tty.as_raw_fd(), &t)?;
        }
        self.continue_job(job.id, true)?;
        println!("{}", job.command);
        Ok(self.finish(job.id, false, true, Some(lease))?.code)
    }
    pub fn background_job(&self, id: Option<usize>) -> io::Result<()> {
        let job = self.select(id)?;
        self.continue_job(job.id, false)?;
        println!("[{}] Running {}", job.id, job.command);
        Ok(())
    }
    fn select(&self, id: Option<usize>) -> io::Result<Job> {
        let s = self.shared.state.lock().unwrap();
        let j = match id {
            Some(id) => s.jobs.get(&id),
            None => s.jobs.values().rev().find(|j| j.state() != JobState::Done),
        };
        j.filter(|j| j.state() != JobState::Done)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such live job"))
    }
    fn continue_job(&self, id: usize, fg: bool) -> io::Result<()> {
        let mut s = self.shared.state.lock().unwrap();
        let job = s
            .jobs
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("job disappeared"))?;
        if unsafe { libc::kill(-job.pgid, libc::SIGCONT) } < 0 {
            return Err(io::Error::last_os_error());
        }
        for m in &mut job.members {
            if matches!(m.state, MemberState::Stopped(_)) {
                m.state = MemberState::Running;
            }
        }
        job.foreground = fg;
        Ok(())
    }
    pub fn notify_done(&self) {
        let mut s = self.shared.state.lock().unwrap();
        let mut done = Vec::new();
        s.jobs.retain(|id, j| {
            if !j.foreground && j.state() == JobState::Done {
                done.push((*id, j.command.clone()));
                false
            } else {
                true
            }
        });
        drop(s);
        for (id, command) in done {
            println!("[{id}] Done {command}");
        }
    }
    pub fn abort_launch(&self, id: usize) {
        // Bound both grace periods; a D-state member cannot be synchronously
        // reaped even after SIGKILL. Keep survivors registered with the monitor.
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            {
                let s = self.shared.state.lock().unwrap();
                if let Some(j) = s.jobs.get(&id) {
                    unsafe {
                        libc::kill(-j.pgid, signal);
                        libc::kill(-j.pgid, libc::SIGCONT);
                        // A released target may have moved itself out of the
                        // original group before a later stage fails. Retain direct
                        // child ownership and reap those members as well.
                        for member in j.members.iter().filter(|m| !m.state.terminal()) {
                            libc::kill(member.pid, signal);
                            libc::kill(member.pid, libc::SIGCONT);
                        }
                    }
                }
            }
            self.wake();
            let mut s = self.shared.state.lock().unwrap();
            let end = Instant::now() + CLOSE_GRACE;
            while s.jobs.get(&id).is_some_and(|j| j.state() != JobState::Done) {
                let Some(remaining) = end.checked_duration_since(Instant::now()) else {
                    break;
                };
                s = self.shared.changed.wait_timeout(s, remaining).unwrap().0;
            }
            if s.jobs.get(&id).is_none_or(|j| j.state() == JobState::Done) {
                s.jobs.remove(&id);
                return;
            }
        }
        let mut s = self.shared.state.lock().unwrap();
        if let Some(job) = s.jobs.get_mut(&id) {
            job.foreground = false;
            let survivors: Vec<_> = job
                .members
                .iter()
                .filter(|m| !m.state.terminal())
                .map(|m| m.pid)
                .collect();
            eprintln!("mix: failed job {id} survived TERM/KILL grace: {survivors:?}");
        }
    }
    fn launch_stopped(&self, id: usize) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&id)
            .is_some_and(|j| {
                j.members
                    .iter()
                    .any(|m| matches!(m.state, MemberState::Stopped(_)))
            })
    }
    pub fn shutdown(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.closing = true;
        self.wake();
        while !state.closed {
            state = self.shared.changed.wait(state).unwrap();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.shutdown();
        self.signals.close();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        let _ = foreground(self.tty.as_raw_fd(), self.parent_pgid);
        for (sig, old) in &self.old_signals {
            unsafe {
                libc::sigaction(*sig, old, std::ptr::null_mut());
            }
        }
    }
}

pub struct TerminalLease<'a> {
    controller: &'a Controller,
    saved: libc::termios,
    _ttou: TtouGuard,
}
impl Drop for TerminalLease<'_> {
    fn drop(&mut self) {
        *self.controller.shared.shell_modes.lock().unwrap() = self.saved;
        if let Err(e) = foreground(self.controller.tty.as_raw_fd(), self.controller.shell_pgid)
            .and_then(|_| set_modes(self.controller.tty.as_raw_fd(), &self.saved))
        {
            eprintln!("mix: terminal restore: {e}");
        }
        MANAGED_FOREGROUND.store(false, Ordering::Release);
    }
}

fn reap(shared: &Shared) {
    let mut s = shared.state.lock().unwrap();
    for job in s.jobs.values_mut() {
        for member in &mut job.members {
            if member.state.terminal() {
                continue;
            }
            loop {
                let mut status = 0;
                let rc = unsafe {
                    libc::waitpid(
                        member.pid,
                        &mut status,
                        libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                    )
                };
                if rc == 0 {
                    break;
                }
                if rc < 0 {
                    if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    member.state = MemberState::Lost;
                    cosmix_mix::builtins::unregister_managed_pid(member.pid);
                    break;
                }
                member.state = if libc::WIFEXITED(status) {
                    MemberState::Exited(libc::WEXITSTATUS(status))
                } else if libc::WIFSIGNALED(status) {
                    MemberState::Signalled(libc::WTERMSIG(status))
                } else if libc::WIFSTOPPED(status) {
                    MemberState::Stopped(libc::WSTOPSIG(status))
                } else {
                    MemberState::Running
                };
                if member.state.terminal() {
                    cosmix_mix::builtins::unregister_managed_pid(member.pid);
                    break;
                }
            }
        }
    }
    shared.changed.notify_all();
}
fn close_jobs(shared: &Shared) {
    {
        let mut s = shared.state.lock().unwrap();
        if s.closed {
            return;
        }
        s.closing = true;
        for job in s.jobs.values().filter(|j| j.state() != JobState::Done) {
            unsafe {
                libc::kill(-job.pgid, libc::SIGHUP);
                libc::kill(-job.pgid, libc::SIGCONT);
            }
        }
    }
    let end = Instant::now() + CLOSE_GRACE;
    loop {
        reap(shared);
        let mut s = shared.state.lock().unwrap();
        if s.jobs.values().all(|j| j.state() == JobState::Done) {
            s.closed = true;
            shared.changed.notify_all();
            return;
        }
        if Instant::now() >= end {
            let survivors: Vec<_> = s
                .jobs
                .values()
                .filter(|j| j.state() != JobState::Done)
                .map(|j| (j.id, j.pgid))
                .collect();
            drop(s);
            let _ttou = TtouGuard::new();
            for (id, pgid) in survivors {
                eprintln!("mix: job {} (pgid {}) survived HUP/CONT grace", id, pgid);
            }
            shared.state.lock().unwrap().closed = true;
            shared.changed.notify_all();
            return;
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// The barrier is AFTER exec of this binary, never inside pre_exec. Thus
/// Command::spawn's exec-error pipe closes before the trampoline blocks.
pub struct Stage {
    pub command: Command,
    gate: File,
    error: File,
    inherited: (File, File),
}
impl Stage {
    pub fn new(program: &str, args: &[String], pgid: i32) -> io::Result<Self> {
        let (gate_read, gate) = pipe()?;
        let (error, error_write) = pipe()?;
        let g = gate_read.as_raw_fd();
        let e = error_write.as_raw_fd();
        // The proc link remains executable after an installer unlinks us;
        // current_exe() would return an unusable path ending in " (deleted)".
        let mut command = Command::new("/proc/self/exe");
        command
            .args([STAGE_ARG, &g.to_string(), &e.to_string(), program])
            .args(args);
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, pgid) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for sig in [
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTSTP,
                    libc::SIGTTIN,
                    libc::SIGTTOU,
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGPIPE,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                let mut empty = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                if libc::fcntl(g, libc::F_SETFD, 0) < 0 || libc::fcntl(e, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            command,
            gate,
            error,
            inherited: (gate_read, error_write),
        })
    }
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.command.spawn()
    }
    pub fn release(self, controller: &Controller, id: usize) -> io::Result<()> {
        self.release_with_stop(|| controller.launch_stopped(id))
    }
    fn release_with_stop(mut self, stopped: impl Fn() -> bool) -> io::Result<()> {
        drop(self.inherited);
        self.gate.write_all(&[1])?;
        drop(self.gate);
        let mut errno = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if stopped() {
                return Err(io::Error::other("job stopped before exec acknowledgement"));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stage exec acknowledgement timed out",
                ));
            }
            let mut fd = libc::pollfd {
                fd: self.error.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut fd, 1, 20) };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if rc == 0 {
                continue;
            }
            let mut bytes = [0; 4];
            match self.error.read(&mut bytes[..4 - errno.len()]) {
                Ok(0) => break,
                Ok(n) => errno.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            if errno.len() == 4 {
                break;
            }
        }
        if errno.is_empty() {
            Ok(())
        } else if let Ok(bytes) = <[u8; 4]>::try_from(errno.as_slice()) {
            Err(io::Error::from_raw_os_error(i32::from_ne_bytes(bytes)))
        } else if errno.len() == 1 {
            Err(io::Error::other(format!(
                "stage trampoline failure (reason {})",
                errno[0]
            )))
        } else {
            Err(io::Error::other("incomplete stage exec acknowledgement"))
        }
    }
}

/// Private argv entry, dispatched before runtime/evaluator/startup hooks.
pub fn stage_entry() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_none_or(|v| v != STAGE_ARG) {
        return;
    }
    let parse = |i: usize| {
        args.get(i)
            .and_then(|v| v.to_str())
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|fd| *fd > 2)
    };
    let fail = |reason: u8| -> ! {
        if let Some(e) = parse(3) {
            unsafe {
                libc::write(e, (&reason as *const u8).cast(), 1);
            }
        }
        std::process::exit(126);
    };
    let (Some(g), Some(e), Some(program)) = (parse(2), parse(3), args.get(4)) else {
        fail(1);
    };
    // The hidden entry is not an authority boundary, but malformed argv must
    // never construct File from an invalid or multiply-owned descriptor.
    if g == e
        || unsafe { libc::fcntl(g, libc::F_GETFD) } < 0
        || unsafe { libc::fcntl(e, libc::F_GETFD) } < 0
    {
        fail(2);
    }
    let mut gate = unsafe { File::from_raw_fd(g) };
    let mut error = unsafe { File::from_raw_fd(e) };
    if unsafe { libc::fcntl(e, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        fail(3);
    }
    let mut byte = [0];
    if gate.read_exact(&mut byte).is_err() || byte[0] != 1 {
        fail(4);
    }
    drop(gate);
    let err = Command::new(program).args(&args[5..]).exec();
    let _ = error.write_all(&err.raw_os_error().unwrap_or(libc::EIO).to_ne_bytes());
    std::process::exit(127);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(states: &[MemberState]) -> Job {
        Job {
            id: 1,
            launch_command_id: 1,
            pgid: 1,
            command: "fixture".into(),
            members: states
                .iter()
                .enumerate()
                .map(|(i, state)| Member {
                    pid: i as i32 + 1,
                    state: *state,
                })
                .collect(),
            modes: None,
            foreground: false,
        }
    }
    #[test]
    fn aggregate_stop_and_completion_require_every_live_member() {
        use MemberState::*;
        assert_eq!(
            job(&[Stopped(libc::SIGTSTP), Running]).state(),
            JobState::Running
        );
        assert_eq!(
            job(&[Stopped(libc::SIGTTIN), Exited(0)]).state(),
            JobState::Stopped
        );
        assert_eq!(
            job(&[Stopped(libc::SIGTSTP), Stopped(libc::SIGTTOU)]).state(),
            JobState::Stopped
        );
        let completed = job(&[Signalled(libc::SIGPIPE), Exited(7)]);
        assert_eq!(completed.state(), JobState::Done);
        assert_eq!(completed.code(), 7);
        assert_eq!(job(&[Exited(0), Signalled(libc::SIGINT)]).code(), 130);
        assert_eq!(job(&[Exited(0), Lost]).code(), 1);
    }
}
