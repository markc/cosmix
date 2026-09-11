//! Real PTYs and a re-executed Rust fixture (no shell/interpreter helpers).
#![cfg(target_os = "linux")]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROMPT: &str = "P0J> ";
const LIMIT: Duration = Duration::from_secs(10);
static RESIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn resized(_: i32) {
    RESIZED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < end, "fixture deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}
fn state(pid: i32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}
fn tty_modes(fd: i32) -> libc::termios {
    let mut t = std::mem::MaybeUninit::uninit();
    assert_eq!(unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) }, 0);
    unsafe { t.assume_init() }
}

// Runs as a distinct process inside the managed job. Its entry is deliberately
// not in the Mix executable and cannot bypass the normal launch protocol.
#[test]
fn fixture_process() {
    let Ok(mode) = std::env::var("P0J_MODE") else {
        return;
    };
    let report = PathBuf::from(std::env::var_os("P0J_REPORT").unwrap());
    let tty = File::open("/dev/tty").ok();
    let foreground = tty
        .as_ref()
        .map(|f| unsafe { libc::tcgetpgrp(f.as_raw_fd()) })
        .unwrap_or(-1);
    let data = format!(
        "{} {} {} {}",
        unsafe { libc::getpid() },
        unsafe { libc::getpgrp() },
        foreground,
        unsafe { libc::getsid(0) }
    );
    if mode == "ignore-hup" {
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }
    fs::write(&report, data).unwrap();
    match mode.as_str() {
        "outer-shell" => {
            // A real same-session parent keeps the nested shell group from
            // being orphaned, and observes its stop rather than a /proc guess.
            let mut child = Command::new(env!("CARGO_BIN_EXE_mix")).spawn().unwrap();
            let pid = child.id() as i32;
            fs::write(report.with_extension("child"), pid.to_string()).unwrap();
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
                pid
            );
            assert!(libc::WIFSTOPPED(status));
            assert_eq!(libc::WSTOPSIG(status), libc::SIGTSTP);
            fs::write(report.with_extension("stopped"), "yes").unwrap();
            assert_eq!(unsafe { libc::kill(-pid, libc::SIGCONT) }, 0);
            assert!(child.wait().unwrap().success());
        }
        "hold" | "ignore-hup" => loop {
            unsafe {
                libc::pause();
            }
        },
        "read" => {
            let mut b = [0];
            std::io::stdin().read_exact(&mut b).unwrap();
        }
        "write" => {
            println!("BACKGROUND-WRITE");
            std::io::stdout().flush().unwrap();
        }
        "stop" => {
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
            fs::write(report.with_extension("continued"), "yes").unwrap();
        }
        "stop-modes" => {
            let fd = tty.as_ref().unwrap().as_raw_fd();
            let mut t = tty_modes(fd);
            t.c_lflag &= !libc::ECHO;
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) }, 0);
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
            assert_eq!(tty_modes(fd).c_lflag & libc::ECHO, 0);
            fs::write(report.with_extension("continued"), "yes").unwrap();
        }
        "raw" | "raw-hold" => {
            let fd = tty.as_ref().unwrap().as_raw_fd();
            let mut t = tty_modes(fd);
            unsafe {
                libc::cfmakeraw(&mut t);
            }
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) }, 0);
            if mode == "raw-hold" {
                fs::write(report.with_extension("raw"), "yes").unwrap();
                loop {
                    unsafe {
                        libc::pause();
                    }
                }
            }
        }
        "resize-silent" => {
            unsafe {
                libc::signal(libc::SIGWINCH, resized as *const () as usize);
            }
            fs::write(report.with_extension("ready"), "yes").unwrap();
            while !RESIZED.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::ioctl(
                        tty.as_ref().unwrap().as_raw_fd(),
                        libc::TIOCGWINSZ,
                        &mut size,
                    )
                },
                0
            );
            fs::write(
                report.with_extension("resized"),
                format!("{} {}", size.ws_row, size.ws_col),
            )
            .unwrap();
        }
        "canonical" => {
            let t = tty_modes(tty.as_ref().unwrap().as_raw_fd());
            assert_ne!(t.c_lflag & libc::ICANON, 0);
            assert_ne!(t.c_lflag & libc::ECHO, 0);
            fs::write(report.with_extension("canonical"), "yes").unwrap();
        }
        "signals" => {
            let mut mask = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) },
                0
            );
            for sig in [libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::sigaction(sig, std::ptr::null(), &mut action) },
                    0
                );
                assert_eq!(
                    action.sa_sigaction,
                    libc::SIG_DFL,
                    "inherited ignored signal {sig}"
                );
                assert_eq!(
                    unsafe { libc::sigismember(&mask, sig) },
                    0,
                    "inherited blocked signal {sig}"
                );
            }
            fs::write(report.with_extension("signals"), "yes").unwrap();
        }
        "exit" | "identity" => {}
        _ => panic!("unknown fixture mode"),
    }
}

struct Pty {
    master: File,
    slave: File,
    initial_modes: libc::termios,
    shell: Child,
    home: tempfile::TempDir,
    pending: String,
    jobs: Vec<i32>,
}
impl Pty {
    fn new(args: &[&str], redirected: bool, controlling: bool) -> Self {
        Self::spawn(args, redirected, controlling, false)
    }
    fn spawn(args: &[&str], redirected: bool, controlling: bool, traced: bool) -> Self {
        Self::spawn_with_signals(args, redirected, controlling, traced, false)
    }
    fn spawn_with_signals(
        args: &[&str],
        redirected: bool,
        controlling: bool,
        traced: bool,
        inherited_signals: bool,
    ) -> Self {
        Self::spawn_config(
            args,
            redirected,
            controlling,
            traced,
            inherited_signals,
            false,
        )
    }
    fn spawn_config(
        args: &[&str],
        redirected: bool,
        controlling: bool,
        traced: bool,
        inherited_signals: bool,
        cold_output: bool,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        // Stable executable name even if another build replaces Cargo's test
        // binary while this process is running.
        fs::copy("/proc/self/exe", home.path().join("fixture")).unwrap();
        fs::write(
            home.path().join(".mixrc"),
            if cold_output {
                "print(\"COLD-READY\"); sleep(300)\n"
            } else {
                "fn prompt()\nreturn \"P0J> \"\nend\n"
            },
        )
        .unwrap();
        let (mut m, mut s) = (-1, -1);
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(m) };
        let slave = unsafe { File::from_raw_fd(s) };
        let initial_modes = tty_modes(s);
        if cold_output {
            let mut broken = initial_modes;
            broken.c_oflag &= !(libc::OPOST | libc::ONLCR);
            assert_eq!(unsafe { libc::tcsetattr(s, libc::TCSANOW, &broken) }, 0);
        }
        // Keep fixture FDs out of child exec; stdio clones are explicitly duped.
        unsafe {
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let executable = home.path().join("mix");
        fs::copy(env!("CARGO_BIN_EXE_mix"), &executable).unwrap();
        let mut cmd = Command::new(executable);
        cmd.args(args)
            .env("HOME", home.path())
            .env("TERM", "xterm-256color")
            .env("COSMIX", home.path())
            .env("COSMIX_SRC", home.path())
            .env("MIX_STATS", "off")
            .stdin(if redirected {
                Stdio::null()
            } else {
                Stdio::from(slave.try_clone().unwrap())
            })
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            cmd.pre_exec(move || {
                if inherited_signals {
                    libc::signal(libc::SIGQUIT, libc::SIG_IGN);
                    let mut mask = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGQUIT);
                    libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
                }
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if controlling && libc::ioctl(1, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if traced && libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shell = cmd.spawn().unwrap();
        Self {
            master,
            slave,
            initial_modes,
            shell,
            home,
            pending: String::new(),
            jobs: vec![],
        }
    }
    fn interactive() -> Self {
        let mut p = Self::new(&[], false, true);
        p.until(PROMPT);
        p
    }
    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).unwrap();
    }
    fn until(&mut self, marker: &str) -> String {
        if marker == PROMPT {
            // Ordinary repaints do not re-enable bracketed paste. Suspend /
            // resume does, even within the same readline: those tests must
            // observe command output before waiting for the following prompt.
            // Never inject typeahead into a job.
            let mut out = self.read_until("\x1b[?2004h");
            out.push_str(&self.read_until(PROMPT));
            return out;
        }
        self.read_until(marker)
    }
    fn read_until(&mut self, marker: &str) -> String {
        let end = Instant::now() + LIMIT;
        loop {
            if let Some(i) = self.pending.find(marker) {
                return self.pending.drain(..i + marker.len()).collect();
            }
            assert!(
                Instant::now() < end,
                "waiting for {marker:?}: {:?}",
                self.pending
            );
            let mut fd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
                continue;
            }
            let mut b = [0; 4096];
            let n = self
                .master
                .read(&mut b)
                .unwrap_or_else(|e| panic!("pty {e}: {:?}", self.pending));
            assert_ne!(n, 0, "pty EOF: {:?}", self.pending);
            self.pending.push_str(&String::from_utf8_lossy(&b[..n]));
            if self.pending.contains("\x1b[6n") {
                self.master.write_all(b"\x1b[1;1R").unwrap();
                self.pending = self.pending.replace("\x1b[6n", "");
            }
        }
    }
    fn command(&mut self, source: &str) -> String {
        self.send(&format!("{source}\n"));
        self.until(PROMPT)
    }
    fn fixture(&self, mode: &str, name: &str) -> String {
        format!(
            "P0J_MODE={mode} P0J_REPORT={} {} --exact fixture_process --nocapture",
            self.home.path().join(name).display(),
            self.home.path().join("fixture").display()
        )
    }
    fn report(&mut self, name: &str) -> Vec<i32> {
        let path = self.home.path().join(name);
        wait_for(|| fs::read_to_string(&path).is_ok_and(|v| v.split_whitespace().count() == 4));
        let fields = fs::read_to_string(path)
            .unwrap()
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect::<Vec<_>>();
        self.jobs.push(fields[0]);
        fields
    }
    fn exit(&mut self) {
        self.send("\x04");
        wait_for(|| self.shell.try_wait().unwrap().is_some());
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        // Only fixture-owned IDs. Kill jobs before their session-leader shell.
        for pid in &self.jobs {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

#[test]
fn foreground_barrier_and_fast_exit_pipeline() {
    // Repetition increases the likelihood of exposing a missing barrier;
    // scheduling observations are not proof of its absence or correctness.
    let mut p = Pty::interactive();
    for i in 0..12 {
        let a = format!("a{i}");
        let b = format!("b{i}");
        let cmd = format!(
            "{} | {}",
            p.fixture("identity", &a),
            p.fixture("identity", &b)
        );
        p.command(&cmd);
        let x = p.report(&a);
        let y = p.report(&b);
        assert_eq!(x[1], y[1], "pipeline common PGID");
        assert_eq!(x[1], x[2], "first target ran only after terminal transfer");
        assert_eq!(y[1], y[2], "last target ran only after terminal transfer");
        assert_ne!(x[1], p.shell.id() as i32);
        assert_eq!(
            unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
            p.shell.id() as i32
        );
        assert!(!alive(x[0]) && !alive(y[0]), "every member reaped");
    }
}

#[test]
fn external_command_survives_unlinked_shell_executable() {
    let mut p = Pty::interactive();
    fs::remove_file(p.home.path().join("mix")).unwrap();
    p.command(&p.fixture("identity", "after-unlink"));
    let child = p.report("after-unlink");
    assert_eq!(child[1], child[2]);
    assert!(!alive(child[0]));
}

#[test]
fn unmanaged_stream_stop_is_visible_to_outer_parent_and_resumes() {
    let mut p = Pty::interactive();
    p.send(&format!("{}\n", p.fixture("outer-shell", "outer")));
    p.report("outer");
    p.until(PROMPT);
    let pid_path = p.home.path().join("outer.child");
    wait_for(|| pid_path.exists());
    let nested: i32 = fs::read_to_string(pid_path).unwrap().parse().unwrap();
    p.jobs.push(nested);
    let report = p.home.path().join("stream");
    p.send(&format!(
        "run_stream([\"{}\", \"--exact\", \"fixture_process\", \"--nocapture\"], {{env: {{P0J_MODE: \"hold\", P0J_REPORT: \"{}\"}}}})\n",
        p.home.path().join("fixture").display(), report.display()
    ));
    let child = p.report("stream");
    assert_eq!(child[1], nested);
    p.send("\x1a");
    wait_for(|| p.home.path().join("outer.stopped").exists());
    wait_for(|| state(nested) != Some('T') && state(child[0]) != Some('T'));
    p.send("\x03");
    p.until(PROMPT);
    p.command("/bin/true");
    p.send("\x04");
    p.until(PROMPT);
}

#[test]
fn idle_prompt_ctrl_z_keeps_readline_usable() {
    // This session-leader group is orphaned: default SIGTSTP is discarded.
    // The interrupted editor read must retry rather than terminate the REPL.
    let mut p = Pty::interactive();
    p.send("\x1a");
    p.send("print(97531)\n");
    // Ctrl+Z resumes with a paste-enable + prompt repaint before processing
    // the queued command. That repaint is not command completion. Retain the
    // typeahead coverage and require actual output, not echoed source text.
    p.until("\r\n97531\r\n");
    p.until(PROMPT);
    p.exit();
}

#[test]
fn background_pipeline_is_immediate_and_sigint_is_foreground_only() {
    let mut p = Pty::interactive();
    p.command(&format!(
        "{} | {} &",
        p.fixture("hold", "a"),
        p.fixture("hold", "b")
    ));
    let a = p.report("a");
    let b = p.report("b");
    assert_eq!(a[1], b[1]);
    assert_ne!(a[1], a[2]);
    p.send(&format!("{}\n", p.fixture("hold", "fg")));
    let fg = p.report("fg");
    p.send("\x03");
    p.until(PROMPT);
    assert!(!alive(fg[0]));
    assert!(alive(a[0]) && alive(b[0]));
    let status = p.command("print($status)");
    assert!(status.contains("130"), "{status}");
    p.exit();
    wait_for(|| !alive(a[0]) && !alive(b[0]));
}

#[test]
fn stop_bg_fg_and_terminal_modes() {
    let mut p = Pty::interactive();
    p.send(&format!("{}\n", p.fixture("hold", "job")));
    let j = p.report("job");
    p.send("\x1a");
    assert!(p.until(PROMPT).contains("Stopped"));
    assert_eq!(state(j[0]), Some('T'));
    assert!(p.command("jobs").contains("Stopped"));
    p.command("bg");
    wait_for(|| state(j[0]) != Some('T'));
    assert_eq!(
        unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
        p.shell.id() as i32
    );
    p.send("fg\n");
    wait_for(|| unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) } == j[1]);
    p.send("\x03");
    p.until(PROMPT);
    p.command(&p.fixture("stop-modes", "modes"));
    p.report("modes");
    p.command("fg");
    assert!(p.home.path().join("modes.continued").exists());
    p.command(&p.fixture("raw", "raw"));
    let t = tty_modes(p.slave.as_raw_fd());
    // readline is raw again at the prompt; verify a subsequent child sees
    // restored canonical mode by stopping it after launch in another test.
    assert_ne!(t.c_oflag & libc::OPOST, 0);
    assert!(p.command("print(2468)").contains("2468"));
    p.command(&p.fixture("canonical", "cooked"));
    assert!(p.home.path().join("cooked.canonical").exists());
}

#[test]
fn monitor_reaps_without_a_prompt_iteration() {
    let mut p = Pty::interactive();
    p.command(&format!("{} &", p.fixture("hold", "background")));
    let bg = p.report("background");
    // While the evaluator is blocked waiting for another foreground child,
    // the independent monitor must reap this background child immediately.
    p.send(&format!("{}\n", p.fixture("hold", "foreground")));
    let fg = p.report("foreground");
    unsafe {
        libc::kill(bg[0], libc::SIGTERM);
    }
    wait_for(|| !alive(bg[0]));
    assert!(alive(fg[0]));
    p.send("\x03");
    p.until(PROMPT);
}

#[test]
fn close_reports_hup_ignoring_survivor_without_kill_escalation() {
    let mut p = Pty::interactive();
    p.command(&format!("{} &", p.fixture("ignore-hup", "survivor")));
    let j = p.report("survivor");
    let start = Instant::now();
    p.send("\x04");
    p.until("survived HUP/CONT grace");
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert!(
        start.elapsed() < LIMIT,
        "bounded close exceeded fixture deadline"
    );
    assert!(
        alive(j[0]),
        "close policy must not silently escalate to SIGKILL"
    );
}

#[test]
fn background_read_gets_sigttin_and_tostop_write_stops() {
    let mut p = Pty::interactive();
    p.command(&format!("{} &", p.fixture("read", "reader")));
    let r = p.report("reader");
    wait_for(|| state(r[0]) == Some('T'));
    assert!(p.command("jobs").contains("Stopped"));
    p.send("fg\n");
    wait_for(|| unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) } == r[1]);
    p.send("x\n");
    p.until(PROMPT);
    assert!(!alive(r[0]));
    // TOSTOP remains in the termios snapshot rustyline restores for launches.
    p.command("stty tostop");
    p.command(&format!("{} &", p.fixture("write", "writer")));
    // libtest may itself write before the fixture report; jobs state is the
    // authoritative observation for this case, not fixture startup.
    wait_for(|| p.command("jobs").contains("Stopped"));
    p.command("fg");
    p.command("stty -tostop");
}

#[test]
fn failed_later_exec_kills_and_reaps_pipeline() {
    let mut p = Pty::interactive();
    let line = format!(
        "{} | /nonexistent-p0j-executable",
        p.fixture("hold", "first")
    );
    let out = p.command(&line);
    assert!(
        out.contains("No such file") || out.contains("os error 2"),
        "{out}"
    );
    let jobs = p.command("jobs");
    assert!(
        !jobs.contains("pgid=") && !jobs.contains("[1]"),
        "failed launch remained in jobs: {jobs:?}"
    );
    // /proc's children list includes zombies; both stages must be gone.
    let task_dir = PathBuf::from(format!("/proc/{}/task", p.shell.id()));
    for task in fs::read_dir(task_dir).unwrap() {
        let children = fs::read_to_string(task.unwrap().path().join("children")).unwrap();
        assert!(
            children.trim().is_empty(),
            "unreaped launch member: {children}"
        );
    }
    // Failure before a later spawn (redirect open) exercises the other edge.
    p.command(&format!(
        "{} | cat > /nonexistent-p0j-directory/out",
        p.fixture("hold", "early")
    ));
    let jobs = p.command("jobs");
    assert!(
        !jobs.contains("pgid=") && !jobs.contains("[2]"),
        "failed redirect remained in jobs: {jobs:?}"
    );
    assert!(p.command("print(99)").contains("99"));
}

#[test]
fn nested_shell_foreground_and_parent_restoration() {
    let mut p = Pty::interactive();
    p.send(&format!("{}\n", env!("CARGO_BIN_EXE_mix")));
    p.until(PROMPT);
    let nested = unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) };
    assert_ne!(nested, p.shell.id() as i32);
    p.command(&p.fixture("identity", "nested-job"));
    let job = p.report("nested-job");
    assert_eq!(job[1], job[2]);
    assert_ne!(job[1], nested);
    p.send("\x04");
    p.until(PROMPT);
    assert_eq!(
        unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
        p.shell.id() as i32
    );
}

#[test]
fn background_nested_shell_waits_for_foreground_admission() {
    let mut p = Pty::interactive();
    p.command(&format!("{} &", env!("CARGO_BIN_EXE_mix")));
    wait_for(|| p.command("jobs").contains("Stopped"));
    p.send("fg\n");
    let entered = p.until(PROMPT);
    let nested = unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) };
    assert_ne!(
        nested,
        p.shell.id() as i32,
        "foreground response: {entered:?}"
    );
    p.send("\x04");
    p.until(PROMPT);
    assert_eq!(
        unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
        p.shell.id() as i32
    );
}

#[test]
fn noninteractive_ssh_style_command_never_takes_terminal_or_group() {
    // PTY allocated, controlling terminal present, yet explicit -c policy.
    let source = "print(pid()); run_stream([\"/bin/true\"])";
    let mut p = Pty::new(&["-c", source], false, true);
    let original = unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) };
    assert_eq!(original, p.shell.id() as i32);
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert!(p.shell.wait().unwrap().success());
    // Source guard complements process observations (which cannot detect an
    // otherwise invisible tcsetpgrp to the already-owning group).
    let source = include_str!("../src/main.rs");
    assert!(!source.contains("Controller::interactive"));
    let exec = include_str!("../src/exec.rs");
    assert!(
        exec.contains("execute_pipeline_with_policy(pipeline, &ExecutionPolicy::NonInteractive)")
    );
}

#[test]
fn captured_runners_keep_their_wait_owner_inside_an_interactive_session() {
    let mut p = Pty::interactive();
    p.command(&format!("{} &", p.fixture("hold", "background")));
    let background = p.report("background");
    for _ in 0..8 {
        for (runner, expected) in [
            (
                r#"run_argv(["/bin/printf", "CAPTURE-VALUE"])"#,
                "CAPTURE-VALUE",
            ),
            (
                r#"run_pipeline([["/bin/printf", "PIPE-VALUE"], ["/bin/cat"]])"#,
                "PIPE-VALUE",
            ),
        ] {
            let output = p.command(&format!(
                "$r = {runner}; print($r.ok); print($r.exit_code); print($r.stdout)"
            ));
            assert!(output.contains("\r\ntrue\r\n0\r\n"), "{output:?}");
            assert!(
                output.contains(&format!("\r\n{expected}\r\n")),
                "{output:?}"
            );
            assert_eq!(
                unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
                p.shell.id() as i32
            );
        }
    }
    assert!(alive(background[0]));
    let jobs = p.command("jobs");
    assert!(
        jobs.contains(&format!("pgid={}", background[1])),
        "{jobs:?}"
    );
    assert!(
        !jobs.contains("[2]"),
        "captured process became a job: {jobs:?}"
    );
    let output = p.command(&format!(
        r#"$r = run_argv(["{}", "--exact", "fixture_process", "--nocapture"], {{env: {{P0J_MODE: "signals", P0J_REPORT: "{}"}}}}); print($r.ok); print($r.stderr)"#,
        p.home.path().join("fixture").display(),
        p.home.path().join("signals").display()
    ));
    assert!(output.contains("\r\ntrue\r\n"), "{output:?}");
}

#[test]
fn redirected_stdin_and_no_controlling_terminal_do_not_initialise_jobs() {
    let mut p = Pty::new(&[], true, true);
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert!(p.shell.wait().unwrap().success());
    let mut no_tty = Pty::new(&[], false, false);
    no_tty.until(PROMPT);
    assert!(no_tty.command("bg").contains("unavailable"));
    no_tty.exit();
}

#[test]
fn source_shares_background_controller_and_hup_reaps_owned_jobs() {
    let mut p = Pty::interactive();
    let script = p.home.path().join("jobs.mix");
    fs::write(&script, format!("{} &\n", p.fixture("hold", "sourced"))).unwrap();
    p.command(&format!("source(\"{}\")", script.display()));
    let j = p.report("sourced");
    assert_ne!(j[1], p.shell.id() as i32);
    assert!(p.command("jobs").contains("Running"));
    unsafe {
        libc::kill(p.shell.id() as i32, libc::SIGHUP);
    }
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert_eq!(p.shell.wait().unwrap().code(), Some(129));
    assert!(!alive(j[0]));
}

#[test]
fn hup_restores_retained_slave_after_foreground_raw_leak() {
    let mut p = Pty::new(&[], false, true);
    let original = p.initial_modes;
    p.until(PROMPT);
    p.send(&format!("{}\n", p.fixture("raw-hold", "raw-hup")));
    let child = p.report("raw-hup");
    wait_for(|| p.home.path().join("raw-hup.raw").exists());
    assert_eq!(tty_modes(p.slave.as_raw_fd()).c_lflag & libc::ICANON, 0);
    assert_eq!(tty_modes(p.slave.as_raw_fd()).c_oflag & libc::OPOST, 0);
    unsafe {
        libc::kill(p.shell.id() as i32, libc::SIGHUP);
    }
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert_eq!(p.shell.wait().unwrap().code(), Some(129));
    let restored = tty_modes(p.slave.as_raw_fd());
    assert_eq!(restored.c_iflag, original.c_iflag);
    assert_eq!(restored.c_oflag, original.c_oflag);
    assert_eq!(restored.c_cflag, original.c_cflag);
    assert_eq!(restored.c_lflag, original.c_lflag);
    assert_eq!(restored.c_cc, original.c_cc);
    assert!(!alive(child[0]));
}

#[test]
fn cold_start_hup_keeps_repaired_modes_before_any_launch() {
    let mut p = Pty::spawn_config(&[], false, true, false, false, true);
    p.until("COLD-READY");
    unsafe {
        libc::kill(p.shell.id() as i32, libc::SIGHUP);
    }
    wait_for(|| p.shell.try_wait().unwrap().is_some());
    assert_eq!(p.shell.wait().unwrap().code(), Some(129));
    let restored = tty_modes(p.slave.as_raw_fd());
    assert_eq!(
        restored.c_oflag & (libc::OPOST | libc::ONLCR),
        libc::OPOST | libc::ONLCR
    );
    assert_eq!(restored.c_lflag, p.initial_modes.c_lflag);
}

#[test]
fn resize_reaches_silent_foreground_job() {
    let mut p = Pty::interactive();
    p.send(&format!("{}\n", p.fixture("resize-silent", "resize")));
    p.report("resize");
    wait_for(|| p.home.path().join("resize.ready").exists());
    let size = libc::winsize {
        ws_row: 43,
        ws_col: 121,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(p.slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    p.until(PROMPT);
    assert_eq!(
        fs::read_to_string(p.home.path().join("resize.resized")).unwrap(),
        "43 121"
    );
}

#[test]
fn managed_target_resets_inherited_quit_disposition_and_mask() {
    let mut p = Pty::spawn_with_signals(&[], false, true, false, true);
    p.until(PROMPT);
    p.command(&p.fixture("signals", "managed-signals"));
    let child = p.report("managed-signals");
    assert_eq!(child[1], child[2]);
    assert!(p.home.path().join("managed-signals.signals").exists());
}

#[test]
fn fixture_path_has_no_shell_metacharacters() {
    // The generated fixture argv is intentionally plain shell-classifier input.
    assert!(
        std::env::current_exe()
            .unwrap()
            .to_str()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    );
    assert!(Path::new(env!("CARGO_BIN_EXE_mix")).is_absolute());
}

#[test]
#[cfg(target_arch = "x86_64")]
fn stop_between_terminal_transfer_and_stage_release_aborts_launch() {
    if run_bounded_ptrace_case("stop_between_terminal_transfer_and_stage_release_aborts_launch") {
        return;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    // The tracer must be the spawning thread. Hand PTY I/O to this test while
    // it follows only shell threads, leaving trampoline signals untraced.
    let tracer = std::thread::spawn(move || {
        use std::collections::{BTreeMap, BTreeSet};
        let p = Pty::spawn(&[], false, true, true);
        let pid = p.shell.id() as i32;
        // The observer is outside the slave's controlling session. Linux
        // rejects tcgetpgrp(slave) there with ENOTTY; the master can query it.
        let tty = p.master.try_clone().unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFSTOPPED(status));
        let opts =
            libc::PTRACE_O_TRACECLONE | libc::PTRACE_O_TRACESYSGOOD | libc::PTRACE_O_EXITKILL;
        assert_eq!(
            unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, opts) },
            0
        );
        sender.send(p).unwrap();
        assert_eq!(unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, 0) }, 0);
        let mut tids = BTreeSet::from([pid]);
        let mut transfers = BTreeMap::new();
        let mut injected = false;
        while !tids.is_empty() {
            let tid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::__WNOTHREAD) };
            if tid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            assert!(tid > 0);
            if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                tids.remove(&tid);
                continue;
            }
            let sig = libc::WSTOPSIG(status);
            if status >> 16 == libc::PTRACE_EVENT_CLONE {
                let mut child: libc::c_ulong = 0;
                assert_eq!(
                    unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, tid, 0, &mut child) },
                    0
                );
                tids.insert(child as i32);
            }
            if sig == (libc::SIGTRAP | 0x80) && !injected {
                let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::ptrace(libc::PTRACE_GETREGS, tid, 0, &mut regs) },
                    0
                );
                if regs.orig_rax == libc::SYS_ioctl as u64 && regs.rsi == libc::TIOCSPGRP {
                    if regs.rax == (-i64::from(libc::ENOSYS)) as u64 {
                        let group =
                            unsafe { libc::ptrace(libc::PTRACE_PEEKDATA, tid, regs.rdx, 0) } as i32;
                        transfers.insert(tid, group);
                    } else if regs.rax == 0
                        && let Some(group) = transfers.remove(&tid)
                        && group != pid
                    {
                        // The ioctl has completed, but the evaluator is still
                        // ptrace-stopped before it can write the release byte.
                        assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, group);
                        assert_eq!(unsafe { libc::kill(-group, libc::SIGTSTP) }, 0);
                        wait_for(|| state(group) == Some('T'));
                        injected = true;
                    }
                }
            }
            let deliver = if [libc::SIGTRAP, libc::SIGTRAP | 0x80, libc::SIGSTOP].contains(&sig) {
                0
            } else {
                sig
            };
            assert_eq!(
                unsafe { libc::ptrace(libc::PTRACE_SYSCALL, tid, 0, deliver) },
                0
            );
        }
        assert!(injected, "did not exercise the handoff-to-exec window");
    });
    let mut p = receiver.recv_timeout(LIMIT).unwrap();
    p.until(PROMPT);
    let out = p.command(&p.fixture("hold", "never-exec"));
    assert!(
        out.contains("stopped before exec acknowledgement"),
        "{out:?}"
    );
    assert!(
        !p.home.path().join("never-exec").exists(),
        "target ran before injection"
    );
    assert_eq!(
        unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) },
        p.shell.id() as i32
    );
    assert!(!p.command("jobs").contains("pgid="));
    p.send("\x04");
    tracer.join().unwrap();
}

#[test]
#[cfg(target_arch = "x86_64")]
fn ssh_dash_c_has_zero_shell_group_or_terminal_handoff_syscalls() {
    if run_bounded_ptrace_case("ssh_dash_c_has_zero_shell_group_or_terminal_handoff_syscalls") {
        return;
    }
    use std::collections::BTreeSet;
    // This is an actual syscall audit of -c on an allocated controlling PTY,
    // not a test whose isatty guard makes the assertion vacuous. Follow shell
    // threads only; captured subprocess group setup is explicitly permitted.
    let p = Pty::spawn(
        &["-c", "print(run_argv([\"/bin/true\"]).exit_code)"],
        false,
        true,
        true,
    );
    let pid = p.shell.id() as i32;
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFSTOPPED(status));
    let opts = libc::PTRACE_O_TRACECLONE | libc::PTRACE_O_TRACESYSGOOD | libc::PTRACE_O_EXITKILL;
    // TRACEFORK would false-positive on run_argv's permitted child setpgid.
    // This audit checks TIOCSPGRP, not TCSETS/other termios mutation.
    assert_eq!(
        unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, opts) },
        0
    );
    assert_eq!(unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, 0) }, 0);
    let mut tids = BTreeSet::from([pid]);
    while !tids.is_empty() {
        // Blocking wait avoids polling every thread at every syscall.
        // WNOTHREAD keeps parallel libtest cases' children out of this audit.
        let tid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::__WNOTHREAD) };
        if tid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        assert!(tid > 0);
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            if tid == pid {
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
            }
            tids.remove(&tid);
            continue;
        }
        let sig = libc::WSTOPSIG(status);
        if status >> 16 == libc::PTRACE_EVENT_CLONE {
            let mut child: libc::c_ulong = 0;
            assert_eq!(
                unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, tid, 0, &mut child) },
                0
            );
            tids.insert(child as i32);
        }
        if sig == (libc::SIGTRAP | 0x80) {
            let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::ptrace(libc::PTRACE_GETREGS, tid, 0, &mut regs) },
                0
            );
            assert_ne!(
                regs.orig_rax,
                libc::SYS_setpgid as u64,
                "-c changed shell process grouping"
            );
            assert!(
                !(regs.orig_rax == libc::SYS_ioctl as u64 && regs.rsi == libc::TIOCSPGRP),
                "-c attempted terminal handoff"
            );
        }
        let deliver =
            if sig == libc::SIGTRAP || sig == (libc::SIGTRAP | 0x80) || sig == libc::SIGSTOP {
                0
            } else {
                sig
            };
        assert_eq!(
            unsafe { libc::ptrace(libc::PTRACE_SYSCALL, tid, 0, deliver) },
            0
        );
    }
}

/// Keep blocking waitpid and tracer.join inside an isolated libtest process.
/// A deadline checked only between waits cannot detect a wedged tracee. The
/// parent bounds the entire case and attributes timeout/failure to its name;
/// killing the tracer also activates PTRACE_O_EXITKILL for its tracees.
#[cfg(target_arch = "x86_64")]
fn run_bounded_ptrace_case(name: &str) -> bool {
    const CASE_LIMIT: Duration = Duration::from_secs(60);
    const CASE_ENV: &str = "P0J_PTRACE_CASE";
    if std::env::var(CASE_ENV).as_deref() == Ok(name) {
        return false;
    }
    // Files rather than pipes: diagnostic output must not wedge the child.
    let output = tempfile::NamedTempFile::new().unwrap();
    let log = output.reopen().unwrap();
    let mut command = Command::new("/proc/self/exe");
    command
        .args(["--exact", name, "--nocapture"])
        .env(CASE_ENV, name)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let deadline = Instant::now() + CASE_LIMIT;
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("{name}: start isolated case: {e}"));
    loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|e| panic!("{name}: wait: {e}"))
        {
            let diagnostics = fs::read_to_string(output.path()).unwrap_or_default();
            assert!(
                status.success(),
                "{name}: isolated ptrace case failed ({status}):\n{diagnostics}"
            );
            return true;
        }
        if Instant::now() >= deadline {
            // Only this case's session/group. Never join or use a blocking
            // reap on timeout: even a failed kill must not hang the test run.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let cleanup_deadline = Instant::now() + Duration::from_secs(1);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < cleanup_deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let diagnostics = fs::read_to_string(output.path()).unwrap_or_default();
            panic!("{name}: ptrace case exceeded 60-second deadline:\n{diagnostics}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
