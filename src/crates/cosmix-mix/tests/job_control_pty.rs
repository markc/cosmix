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
        "raw" => {
            let fd = tty.as_ref().unwrap().as_raw_fd();
            let mut t = tty_modes(fd);
            unsafe {
                libc::cfmakeraw(&mut t);
            }
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) }, 0);
        }
        "canonical" => {
            let t = tty_modes(tty.as_ref().unwrap().as_raw_fd());
            assert_ne!(t.c_lflag & libc::ICANON, 0);
            assert_ne!(t.c_lflag & libc::ECHO, 0);
            fs::write(report.with_extension("canonical"), "yes").unwrap();
        }
        "exit" | "identity" => {}
        _ => panic!("unknown fixture mode"),
    }
}

struct Pty {
    master: File,
    slave: File,
    shell: Child,
    home: tempfile::TempDir,
    pending: String,
    jobs: Vec<i32>,
}
impl Pty {
    fn new(args: &[&str], redirected: bool, controlling: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join(".mixrc"),
            "fn prompt()\nreturn \"P0J> \"\nend\n",
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
        // Keep fixture FDs out of child exec; stdio clones are explicitly duped.
        unsafe {
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mix"));
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
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if controlling && libc::ioctl(1, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shell = cmd.spawn().unwrap();
        Self {
            master,
            slave,
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
            std::env::current_exe().unwrap().display()
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
    assert!(start.elapsed() < Duration::from_secs(3));
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
    assert!(p.command("jobs").contains(PROMPT));
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
