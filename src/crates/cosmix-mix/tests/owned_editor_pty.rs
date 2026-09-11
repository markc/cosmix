//! Disposable real controlling-PTY tests. Fixtures execute in isolated libtest
//! processes so signal handlers never interfere with the parent test runner.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../src/editor/mod.rs"]
mod editor;

use editor::runtime::{CompletionSnapshot, Line, OwnedEditor};
use editor::{Generation, PromptProfile, Reply};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(15);
const PROMPT: &str = "OWNED> ";

#[test]
fn oversized_history_is_warned_and_never_rewritten() {
    let mut original = b"#V2\n".to_vec();
    original.extend("valid_record\n".repeat(1_400_000).as_bytes());
    let mut p = Pty::with_history(None, true, PROMPT, Some(&original));
    p.until("history saving disabled");
    p.prompt();
    p.command("print(101)");
    p.exit();
    assert_eq!(
        fs::read(p.home.path().join(".mix_history")).unwrap(),
        original
    );
}

fn modes(fd: i32) -> libc::termios {
    let mut t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
    t
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !condition() {
        assert!(Instant::now() < deadline, "condition deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn same_modes(a: libc::termios, b: libc::termios) {
    assert_eq!(a.c_iflag, b.c_iflag);
    assert_eq!(a.c_oflag, b.c_oflag);
    assert_eq!(a.c_cflag, b.c_cflag);
    assert_eq!(a.c_lflag, b.c_lflag);
    assert_eq!(a.c_cc, b.c_cc);
}

#[test]
fn fixture_editor() {
    let Ok(scenario) = std::env::var("OWNED_FIXTURE") else {
        return;
    };
    let original = modes(0);
    if scenario == "external-stop" {
        stop_supervisor(original);
        return;
    }
    let input = if scenario == "input-error" {
        fs::OpenOptions::new()
            .write(true)
            .open("/proc/self/fd/0")
            .unwrap()
    } else {
        unsafe { File::from_raw_fd(libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3)) }
    };
    let output = unsafe { File::from_raw_fd(libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3)) };
    let editor = OwnedEditor::start(input, output).unwrap();
    let g = Generation {
        session: 1,
        prompt: 1,
    };
    editor
        .begin(
            g,
            if scenario == "restricted" {
                PromptProfile::Restricted
            } else {
                PromptProfile::Primary(PROMPT.into())
            },
            CompletionSnapshot {
                variables: vec!["cycle_a".into(), "cycle_b".into()],
                commands: std::sync::Arc::new(vec!["secret_command".into()]),
                ..Default::default()
            },
            vec!["print(616)".into()],
        )
        .unwrap();
    if scenario == "input-error" {
        assert!(editor.readline().is_err());
        // A disconnected request channel is not the cleanup acknowledgement.
        editor.control.shutdown().unwrap();
        same_modes(original, modes(0));
        println!("CLEANUP-PASS");
        return;
    } else if scenario == "backpressure" {
        wait(|| editor.control.inspect().unwrap().text.len() == 2000);
        // Fill the same tty output queue explicitly to prove backpressure,
        // rather than assuming a particular emulator queue capacity.
        use std::os::unix::fs::OpenOptionsExt;
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/proc/self/fd/1")
            .unwrap();
        loop {
            match writer.write(&[b'x'; 4096]) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("fill tty: {e}"),
            }
        }
        let view = editor.control.inspect().unwrap();
        let start = Instant::now();
        assert!(matches!(
            editor.control.pause(g, view.revision).unwrap(),
            Reply::Suspended { .. }
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
        same_modes(original, modes(0));
        editor.control.shutdown().unwrap();
        fs::write(
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("backpressure-pass"),
            "ok",
        )
        .unwrap();
        return;
    } else if scenario == "restricted" {
        wait(|| editor.control.inspect().unwrap().revision >= 2);
        assert_eq!(editor.control.inspect().unwrap().text, "");
        println!("RESTRICTED-CHECKED");
    } else if scenario == "completion" {
        // The parent drives two Tab presses against a deterministic snapshot.
    } else if scenario == "admission" {
        let reply = editor
            .control
            .command(editor::Command::SuspendRequested {
                generation: g,
                edit_revision: 0,
            })
            .unwrap();
        assert!(matches!(reply, Reply::Suspended { .. }));
        same_modes(original, modes(0));
        assert!(
            editor
                .control
                .command(editor::Command::Resume {
                    generation: Generation { prompt: 2, ..g },
                    edit_revision: 0
                })
                .is_err()
        );
        editor
            .control
            .command(editor::Command::Resume {
                generation: g,
                edit_revision: 0,
            })
            .unwrap();
        println!("ADMISSION-RESUMED");
    } else {
        wait(|| {
            let v = editor.control.inspect().unwrap();
            if scenario == "paste" {
                v.paste && v.revision >= 9
            } else if scenario == "search" {
                v.search.as_deref() == Some("616") && v.text == "print(616)"
            } else {
                v.text == "draft界" && v.decoder_pending
            }
        });
        let view = editor.control.inspect().unwrap();
        let busy = editor
            .control
            .command(editor::Command::SuspendRequested {
                generation: g,
                edit_revision: view.revision,
            })
            .unwrap();
        assert!(matches!(busy, Reply::Busy { .. }));
        assert!(matches!(
            editor.control.pause(g, view.revision).unwrap(),
            Reply::Suspended { .. }
        ));
        same_modes(original, modes(0));
        println!("PAUSED");
        let mut child_input = String::new();
        std::io::stdin().read_line(&mut child_input).unwrap();
        assert_eq!(child_input, "child-only\n");
        let preserved = editor.control.inspect().unwrap();
        assert_eq!(preserved.text, view.text);
        assert_eq!(preserved.revision, view.revision);
        assert_eq!(preserved.decoder_pending, view.decoder_pending);
        assert_eq!(preserved.paste, view.paste);
        assert_eq!(preserved.search, view.search);
        assert!(
            editor
                .control
                .consume_reservation(g, view.revision)
                .is_err()
        );
        editor
            .control
            .command(editor::Command::Resume {
                generation: g,
                edit_revision: view.revision,
            })
            .unwrap();
    }
    let Line::Submitted(line) = editor.readline().unwrap() else {
        panic!("no submitted line");
    };
    assert_eq!(
        line,
        match scenario.as_str() {
            "paste" => "abcXYZ",
            "admission" => "accepted",
            "search" => "print(616)",
            "completion" => "$cycle_b",
            "restricted" => "jobs",
            _ => "draft界👩",
        }
    );
    same_modes(original, modes(0));
    editor.control.shutdown().unwrap();
    println!("FIXTURE-PASS");
}

fn stop_supervisor(original: libc::termios) {
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_mix"));
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let pid = child.id() as i32;
    let mut status = 0;
    // Background admission stops before any mode repair or readline setup.
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
        pid
    );
    assert!(libc::WIFSTOPPED(status));
    assert_eq!(libc::WSTOPSIG(status), libc::SIGTTIN);
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    fs::write(home.join("nested-pid"), pid.to_string()).unwrap();
    assert_eq!(unsafe { libc::tcsetpgrp(0, pid) }, 0);
    unsafe {
        libc::kill(pid, libc::SIGCONT);
    }
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
        pid
    );
    assert!(libc::WIFSTOPPED(status));
    assert_eq!(libc::WSTOPSIG(status), libc::SIGTSTP);
    same_modes(original, modes(0));
    assert_eq!(unsafe { libc::tcsetpgrp(0, libc::getpgrp()) }, 0);
    unsafe {
        libc::kill(pid, libc::SIGCONT);
    }
    // Give the resumed editor a chance to mishandle bg before checking modes.
    std::thread::sleep(Duration::from_millis(100));
    same_modes(original, modes(0));
    println!("BG-COOKED");
    wait(|| home.join("foreground-now").exists());
    assert_eq!(unsafe { libc::tcsetpgrp(0, pid) }, 0);
    unsafe {
        libc::kill(pid, libc::SIGCONT);
    }
    assert!(child.wait().unwrap().success());
    println!("SUPERVISOR-PASS");
}

#[test]
fn external_stop_is_cooked_and_bg_waits_for_foreground_before_resuming_draft() {
    let mut p = Pty::new(Some("external-stop"), true);
    p.prompt();
    p.send(b"print(731");
    p.until("print(731");
    let pid: i32 = fs::read_to_string(p.home.path().join("nested-pid"))
        .unwrap()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(pid, libc::SIGTSTP);
    }
    p.until("BG-COOKED");
    same_modes(p.original, modes(p.slave.as_raw_fd()));
    fs::write(p.home.path().join("foreground-now"), "go").unwrap();
    p.prompt();
    p.send(b")\n");
    assert!(p.prompt().contains("\r\n731\r\n"));
    p.send(b"\x04");
    p.until("SUPERVISOR-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn undrained_master_does_not_block_suspend_or_shutdown() {
    let mut p = Pty::new(Some("backpressure"), true);
    p.prompt();
    p.send(&[b'a'; 2000]);
    // Deliberately do not drain master, including after suspension.
    wait(|| p.home.path().join("backpressure-pass").exists());
    same_modes(p.original, modes(p.slave.as_raw_fd()));
    // Only after acknowledgement may the parent drain libtest's own output.
    p.until("test result:");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn input_failure_shutdown_waits_for_terminal_cleanup() {
    let mut p = Pty::new(Some("input-error"), true);
    p.prompt();
    p.send(b"x");
    p.until("CLEANUP-PASS");
    same_modes(p.original, modes(p.slave.as_raw_fd()));
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn wrapped_submission_moves_below_tail_from_home() {
    let mut p = Pty::new(None, true);
    p.prompt();
    let text = format!("print(\"{}\")", "a".repeat(100));
    p.send(text.as_bytes());
    p.until(&"a".repeat(40));
    p.send(b"\x01\n");
    let output = p.prompt();
    assert!(
        output.contains("\r\n\n\r\n"),
        "finish must move down two rows: {output:?}"
    );
    assert!(output.contains(&format!("{}\r\n", "a".repeat(100))));
    p.exit();
}

struct Pty {
    master: File,
    slave: File,
    original: libc::termios,
    child: Child,
    home: tempfile::TempDir,
    pending: Vec<u8>,
}
impl Pty {
    fn new(fixture: Option<&str>, owned: bool) -> Self {
        Self::configured(fixture, owned, PROMPT)
    }
    fn configured(fixture: Option<&str>, owned: bool, prompt: &str) -> Self {
        Self::with_history(fixture, owned, prompt, None)
    }
    fn with_history(
        fixture: Option<&str>,
        owned: bool,
        prompt: &str,
        history: Option<&[u8]>,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join(".mixrc"),
            format!("fn prompt()\nreturn \"{prompt}\"\nend\n"),
        )
        .unwrap();
        if let Some(history) = history {
            fs::write(home.path().join(".mix_history"), history).unwrap();
        }
        let mut m = -1;
        let mut s = -1;
        let size = libc::winsize {
            ws_row: 12,
            ws_col: 50,
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
        unsafe {
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let original = modes(s);
        let mut command = Command::new(if fixture.is_some() {
            std::env::current_exe().unwrap()
        } else {
            env!("CARGO_BIN_EXE_mix").into()
        });
        if let Some(scenario) = fixture {
            command
                .args(["--exact", "fixture_editor", "--nocapture"])
                .env("OWNED_FIXTURE", scenario);
        }
        command
            .env("HOME", home.path())
            .env("COSMIX", home.path())
            .env("COSMIX_SRC", home.path())
            .env("MIX_STATS", "off")
            .env("TERM", "xterm-256color")
            .env("MIX_EDITOR", if owned { "owned" } else { "rustyline" })
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            master,
            slave,
            original,
            child,
            home,
            pending: vec![],
        }
    }
    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }
    fn until(&mut self, marker: &str) -> String {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(i) = self
                .pending
                .windows(marker.len())
                .position(|w| w == marker.as_bytes())
            {
                return String::from_utf8_lossy(
                    &self.pending.drain(..i + marker.len()).collect::<Vec<_>>(),
                )
                .into_owned();
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {marker:?}: {:?}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut fd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
                continue;
            }
            let mut bytes = [0; 8192];
            let count = self.master.read(&mut bytes).unwrap();
            assert_ne!(count, 0);
            self.pending.extend_from_slice(&bytes[..count]);
            if self.pending.windows(4).any(|w| w == b"\x1b[6n") {
                self.send(b"\x1b[1;1R");
            }
        }
    }
    fn prompt(&mut self) -> String {
        let mut out = self.until("\x1b[?2004h");
        out.push_str(&self.until(PROMPT));
        editor::render::visible_prompt(&out)
    }
    fn command(&mut self, line: &str) -> String {
        self.send(format!("{line}\n").as_bytes());
        self.prompt()
    }
    fn exit(&mut self) {
        self.send(b"\x04");
        wait(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
        same_modes(self.original, modes(self.slave.as_raw_fd()));
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn type_echo_execute_unicode_history_multiline_and_eof() {
    let mut p = Pty::new(None, true);
    p.prompt();
    assert_eq!(modes(p.slave.as_raw_fd()).c_lflag & libc::ICANON, 0);
    // One backspace removes an entire family, not one codepoint.
    p.send("print(\"界👩‍👩‍👧‍👦".as_bytes());
    p.send(b"\x7f");
    p.send("e\u{301}\")\n".as_bytes());
    assert!(p.prompt().contains("\r\n界e\u{301}\r\n"));
    p.send(b"\x1b[A\n");
    assert!(p.prompt().contains("\r\n界e\u{301}\r\n"));
    p.send(b"if true then\n");
    p.until("\x1b[?2004h");
    p.until("  > ");
    p.send(b"print(4242)\n");
    p.until("\x1b[?2004h");
    p.until("  > ");
    p.send(b"end\n");
    assert!(p.prompt().contains("\r\n4242\r\n"));
    p.exit();
    let history = fs::read_to_string(p.home.path().join(".mix_history")).unwrap();
    let mut h = editor::history::History::default();
    h.load(&history);
    assert!(
        h.entries()
            .contains(&"if true then\nprint(4242)\nend".into())
    );
    assert_eq!(h.encode(), history);
}

#[test]
fn control_pause_preserves_draft_and_split_decoder_without_child_leak() {
    let mut p = Pty::new(Some("draft"), true);
    p.prompt();
    p.send("draft界".as_bytes());
    p.send(&"👩".as_bytes()[..2]);
    p.until("PAUSED");
    same_modes(p.original, modes(p.slave.as_raw_fd()));
    p.send(b"child-only\n");
    p.prompt();
    p.send(&"👩".as_bytes()[2..]);
    p.send(b"\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn control_pause_preserves_paste_and_admission_wakes_without_input() {
    let mut p = Pty::new(Some("paste"), true);
    p.prompt();
    p.send(b"\x1b[200~abc");
    p.until("PAUSED");
    p.send(b"child-only\n");
    p.prompt();
    p.send(b"XYZ\x1b[201~\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
    let mut p = Pty::new(Some("admission"), true);
    p.until("ADMISSION-RESUMED");
    p.send(b"accepted\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn silent_resize_redraw_and_hup_restores_modes_and_protocols() {
    let mut p = Pty::new(None, true);
    p.prompt();
    p.send(b"long_draft_for_resize");
    p.until("long_draft_for_resize");
    let size = libc::winsize {
        ws_row: 8,
        ws_col: 15,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(p.slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    p.until("\x1b[J");
    p.until("\r\n");
    unsafe {
        libc::kill(p.child.id() as i32, libc::SIGHUP);
    }
    p.until("\x1b[?2004l");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert_eq!(p.child.wait().unwrap().code(), Some(129));
    same_modes(p.original, modes(p.slave.as_raw_fd()));
}

#[test]
fn foreground_job_ctrl_z_fg_and_nested_editor_stop_preserve_draft() {
    let mut p = Pty::new(None, true);
    p.prompt();
    p.send(b"/bin/sleep 300\n");
    p.until("\x1b[?2004l");
    wait(|| unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) } != p.child.id() as i32);
    p.send(b"\x1a");
    p.until("Stopped");
    p.prompt();
    p.send(b"fg\n");
    p.until("\x1b[?2004l");
    wait(|| unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) } != p.child.id() as i32);
    p.send(b"\x03");
    p.prompt();
    // A nested shell group is not orphaned: Ctrl-Z must stop the editor itself.
    p.send(format!("{}\n", env!("CARGO_BIN_EXE_mix")).as_bytes());
    p.prompt();
    let nested = unsafe { libc::tcgetpgrp(p.master.as_raw_fd()) };
    assert_ne!(nested, p.child.id() as i32);
    p.send(b"print(73");
    p.until("print(73");
    p.send(b"\x1a");
    p.until("Stopped");
    p.prompt();
    p.send(b"fg\n");
    p.until("\x1b[?2004l");
    p.prompt();
    p.send(b")\n");
    assert!(p.prompt().contains("\r\n73\r\n"));
    p.send(b"\x04");
    p.prompt();
    p.exit();
}

#[test]
fn completion_snapshot_variables_paths_and_cycle() {
    let mut p = Pty::new(None, true);
    p.prompt();
    p.command("$owned_unique = 919");
    p.send(b"print( $owned_uni\t");
    p.until("print( $owned_unique");
    p.send(b")\n");
    assert!(p.prompt().contains("\r\n919\r\n"));
    let directory = p.home.path().join("unique_completion_dir");
    fs::create_dir(&directory).unwrap();
    p.send(format!("cd {}/unique_comp\t", p.home.path().display()).as_bytes());
    p.until("unique_completion_dir/");
    p.send(b"\n");
    p.prompt();
    assert!(p.command("pwd").contains(directory.to_str().unwrap()));
    p.exit();
}

#[test]
fn unselected_editor_uses_legacy_path() {
    let mut p = Pty::new(None, false);
    p.prompt();
    assert!(p.command("print(818)").contains("\r\n818\r\n"));
    p.exit();
}

#[test]
fn search_state_survives_control_suspend_and_resume() {
    let mut p = Pty::new(Some("search"), true);
    p.prompt();
    p.send(b"\x12616");
    p.until("PAUSED");
    p.send(b"child-only\n");
    p.prompt();
    // First Enter selects the search result; the second submits it.
    p.send(b"\n\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn completion_cycles_and_restricted_profile_cannot_read_ordinary_snapshots() {
    let mut p = Pty::new(Some("completion"), true);
    p.prompt();
    p.send(b"$cycle_\t");
    p.until("$cycle_a");
    p.send(b"\t");
    p.until("$cycle_b");
    p.send(b"\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());

    let mut p = Pty::new(Some("restricted"), true);
    p.until("jobs> ");
    p.send(b"\x1b[A\t");
    p.until("RESTRICTED-CHECKED");
    p.send(b"print(1)\n");
    p.until("\x07");
    p.send(b"\x15jobs\n");
    p.until("FIXTURE-PASS");
    wait(|| p.child.try_wait().unwrap().is_some());
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn coloured_prompt_and_paste_undo_yank_through_real_editor() {
    let mut p = Pty::configured(None, true, "\x1b[32mOWNED> \x1b[0m");
    p.until("\x1b[?2004h");
    p.until("\x1b[32m");
    p.until(PROMPT);
    p.send(b"\x1b[200~print(515)\x1b[201~");
    p.until("print(515)");
    p.send(b"\x1f"); // undo entire paste
    p.send(b"print(717)\x15\x19\n");
    assert!(p.prompt().contains("\r\n717\r\n"));
    p.exit();
}
