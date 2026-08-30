//! Exec stdio routing that only tells the truth OUT OF PROCESS.
//!
//! Two claims here are about the *real* file descriptors 1 and 2 of the Mix
//! process, and about a wall-clock deadline surviving a descendant that holds
//! a pipe open. An in-process test cannot see either: the evaluator's `print`
//! goes to a capture buffer, not fd 1, and a regression in the deadline path
//! HANGS the harness instead of failing it. A child process gives us both a
//! real pair of descriptors and a blast radius we can kill.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn script_path(label: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mix-exec-stdio-{label}-{}-{nonce}.mix",
        std::process::id()
    ))
}

/// Run `source` as a script and return `(stdout, stderr)`, failing rather than
/// hanging if it outlives `guard`.
fn run_script(label: &str, source: &str, guard: Duration) -> (String, String) {
    run_script_with_setup(label, source, guard, |_| {})
}

fn run_script_with_setup(
    label: &str,
    source: &str,
    guard: Duration,
    setup: impl FnOnce(&mut Command),
) -> (String, String) {
    run_script_with_setup_and_action(label, source, guard, setup, |_| {})
}

fn run_script_with_setup_and_action(
    label: &str,
    source: &str,
    guard: Duration,
    setup: impl FnOnce(&mut Command),
    after_spawn: impl FnOnce(&mut std::process::Child),
) -> (String, String) {
    let path = script_path(label);
    {
        let mut file = std::fs::File::create(&path).expect("write test script");
        file.write_all(source.as_bytes())
            .expect("write test script");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_mix"));
    command
        .arg(&path)
        .env("MIX_STATS", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup(&mut command);
    let mut child = command.spawn().expect("mix binary must run");
    after_spawn(&mut child);

    let deadline = Instant::now() + guard;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&path);
                panic!("`{label}` did not finish within {guard:?} — it wedged");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Leave fd 1 unread briefly so one live-tee writer owns the process stdout
/// lock while later abandoned readers queue behind it. Once reading starts,
/// collect output normally under the same outer watchdog as `run_script`.
fn run_script_after_stdout_stall(
    label: &str,
    source: &str,
    stall: Duration,
    guard: Duration,
    returned_marker: &std::path::Path,
) -> (String, String, bool) {
    let path = script_path(label);
    {
        let mut file = std::fs::File::create(&path).expect("write test script");
        file.write_all(source.as_bytes())
            .expect("write test script");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&path)
        .env("MIX_STATS", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mix binary must run");
    let mut child_stdout = child.stdout.take().expect("piped stdout");
    let mut child_stderr = child.stderr.take().expect("piped stderr");

    std::thread::sleep(stall);
    let returned_while_stdout_stalled = returned_marker.exists();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stdout.read_to_end(&mut bytes).expect("read stdout");
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stderr.read_to_end(&mut bytes).expect("read stderr");
        bytes
    });

    let deadline = Instant::now() + guard;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&path);
                panic!("`{label}` did not finish within {guard:?} — it wedged");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = child.wait();
    let stdout = stdout_reader.join().expect("stdout reader panicked");
    let stderr = stderr_reader.join().expect("stderr reader panicked");
    let _ = std::fs::remove_file(&path);
    let output = (
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        returned_while_stdout_stalled,
    );
    let _ = std::fs::remove_file(returned_marker);
    output
}

fn fifo(path: &std::path::Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).expect("fifo path contains NUL");
    // SAFETY: `path` is a valid, NUL-terminated pathname and mkfifo does not
    // retain the pointer.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

/// `stderr: "stdout"` means the child's stderr goes to the SELECTED stdout
/// destination, exactly as `2>&1` does. With `stdout: "inherit"` that is the
/// parent's stdout — not the parent's stderr, which is a different file
/// whenever the two are redirected apart (as they are here).
#[test]
fn stderr_stdout_merge_follows_inherited_stdout_not_parent_stderr() {
    let (stdout, stderr) = run_script(
        "merge-inherit",
        "$r = run_argv([\"sh\", \"-c\", \"printf child-out; printf child-err >&2\"], \
             {stdout: \"inherit\", stderr: \"stdout\"})\n\
         print(\"\\nok=\" .. $r.ok .. \" stderr=[\" .. $r.stderr .. \"]\")\n",
        Duration::from_secs(20),
    );
    assert!(
        stdout.contains("child-out") && stdout.contains("child-err"),
        "both child streams must land on the parent's stdout, got stdout={stdout:?}"
    );
    assert!(
        !stderr.contains("child-err"),
        "the merge sent child stderr to the parent's stderr, got stderr={stderr:?}"
    );
    assert!(stdout.contains("ok=true"), "stdout={stdout:?}");
}

/// The deadline must also bound the stdin WRITER. `sh -c "sleep 5 <&0 >/dev/null 2>/dev/null & exit 0"`
/// exits immediately but leaves a descendant in the same process group holding
/// the stdin read end without reading it, so a payload larger than the pipe
/// buffer blocks the writer. Every direct child has exited, so the wait loop
/// finishes without timing out, and the join that follows used to be unbounded:
/// the call returned only when the descendant happened to exit — here ten times
/// past its 0.5 s deadline, reporting `ok: true, timed_out: false`. With a
/// descendant that sleeps for an hour, that is an hour-long wedge.
#[test]
fn stdin_writer_cannot_outlive_the_deadline() {
    let (stdout, stderr) = run_script(
        "stdin-writer-deadline",
        "$a = run_argv([\"sh\", \"-c\", \"sleep 5 <&0 >/dev/null 2>/dev/null & exit 0\"], \
             {stdin: repeat(\"x\", 1000000), timeout: 0.5})\n\
         print(\"argv \" .. $a.ok .. \" \" .. $a.timed_out)\n\
         $p = run_pipeline([{argv: [\"sh\", \"-c\", \"sleep 5 <&0 >/dev/null 2>/dev/null & exit 0\"], \
             stdin: repeat(\"x\", 1000000)}, [\"cat\"]], {timeout: 0.5})\n\
         print(\"pipeline \" .. $p.ok .. \" \" .. $p.timed_out)\n",
        Duration::from_secs(20),
    );
    assert_eq!(
        stdout, "argv false true\npipeline false true\n",
        "stderr={stderr:?}"
    );
}

/// Every pipe needed by a pipeline must exist before stage zero can run.  A
/// deliberately small descriptor limit makes a later pipe allocation fail;
/// the witness catches the old incremental setup even though the returned
/// PIPELINE_STDIO value has an empty `.stages` list.
#[test]
fn pipeline_pipe_setup_failure_cannot_run_an_earlier_stage() {
    let witness = script_path("pipeline-preflight-witness");
    let mut stages = vec![format!("[\"sh\", \"-c\", \"touch {}\"]", witness.display())];
    stages.extend((0..80).map(|_| "[\"cat\"]".to_string()));
    let source = format!(
        "$r = run_pipeline([{}])\nprint($r.error_code .. \" \" .. length($r.stages))\n",
        stages.join(", ")
    );
    let (stdout, stderr) = run_script_with_setup(
        "pipeline-preflight",
        &source,
        Duration::from_secs(5),
        |command| {
            // SAFETY: pre_exec runs in the child and setrlimit is async-signal
            // safe.  The hard limit is only reduced for the disposable Mix
            // subprocess.
            unsafe {
                command.pre_exec(|| {
                    let limit = libc::rlimit {
                        rlim_cur: 48,
                        rlim_max: 48,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        },
    );
    assert_eq!(stdout, "PIPELINE_STDIO 0\n", "stderr={stderr:?}");
    assert!(
        !witness.exists(),
        "stage zero ran before all pipeline pipes were created"
    );
}

/// FIFO opens must participate in the deadline. Read-side opens preserve their
/// wait-for-writer semantics but the caller abandons that wait at its deadline;
/// write-side opens with no reader fail promptly rather than blocking.
#[test]
fn fifo_route_opening_is_bounded_for_argv_and_pipeline() {
    let input = script_path("route-input-fifo");
    let output = script_path("route-output-fifo");
    fifo(&input);
    fifo(&output);
    let source = format!(
        "$a = run_argv([\"cat\"], {{stdin: {{file: {:?}}}, timeout: 0.2}})\n\
         print(\"argv-in \" .. $a.error_code)\n\
         $p = run_pipeline([{{argv: [\"cat\"], stdin: {{file: {:?}}}}}], {{timeout: 0.2}})\n\
         print(\"pipeline-in \" .. $p.error_code)\n\
         $b = run_argv([\"true\"], {{stdout: {{file: {:?}}}, timeout: 0.2}})\n\
         print(\"argv-out \" .. $b.error_code)\n\
         $q = run_pipeline([{{argv: [\"true\"], stdout: {{file: {:?}}}}}], {{timeout: 0.2}})\n\
         print(\"pipeline-out \" .. $q.error_code)\n",
        input.to_string_lossy(),
        input.to_string_lossy(),
        output.to_string_lossy(),
        output.to_string_lossy(),
    );
    let (stdout, stderr) = run_script("fifo-route-deadline", &source, Duration::from_secs(4));
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    assert_eq!(
        stdout,
        "argv-in PROCESS_STDIO\npipeline-in PIPELINE_STDIO\nargv-out PROCESS_STDIO\npipeline-out PIPELINE_STDIO\n",
        "stderr={stderr:?}"
    );
}

fn fifo_eof_ack_case(label: &str, pipeline: bool) {
    let route = script_path(&format!("{label}-fifo"));
    let ack = script_path(&format!("{label}-ack"));
    fifo(&route);
    let mut reader = Command::new("sh")
        .args([
            "-c",
            &format!(
                "cat {} >/dev/null; touch {}",
                route.display(),
                ack.display()
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn FIFO reader");
    std::thread::sleep(Duration::from_millis(50));

    let command = format!(
        "printf payload; exec 1>&-; while [ ! -e {} ]; do sleep 0.01; done",
        ack.display()
    );
    let source = if pipeline {
        format!(
            "$r = run_pipeline([{{argv: [\"sh\", \"-c\", {:?}], stdout: {{file: {:?}}}}}], {{timeout: 0.5}})\nprint($r.ok .. \" \" .. $r.timed_out)\n",
            command,
            route.to_string_lossy()
        )
    } else {
        format!(
            "$r = run_argv([\"sh\", \"-c\", {:?}], {{stdout: {{file: {:?}}}, timeout: 0.5}})\nprint($r.ok .. \" \" .. $r.timed_out)\n",
            command,
            route.to_string_lossy()
        )
    };
    let (stdout, stderr) = run_script(label, &source, Duration::from_secs(3));
    if reader.try_wait().expect("poll FIFO reader").is_none() {
        let _ = reader.kill();
    }
    let _ = reader.wait();
    let _ = std::fs::remove_file(&route);
    let _ = std::fs::remove_file(&ack);
    assert_eq!(stdout, "true false\n", "stderr={stderr:?}");
}

/// The parent's truncate-only clone must be dropped before spawn, otherwise
/// it remains a FIFO writer after the child closes fd 1 and suppresses EOF.
#[test]
fn truncate_clone_does_not_retain_fifo_writer() {
    fifo_eof_ack_case("argv-fifo-eof", false);
    fifo_eof_ack_case("pipeline-fifo-eof", true);
}

#[test]
fn argv_timeout_abandons_capture_held_by_detached_descendant() {
    let (stdout, stderr) = run_script(
        "argv-detached-capture",
        "$r = run_argv([\"sh\", \"-c\", \"setsid sh -c 'sleep 5' & sleep 5\"], {timeout: 0.5})\n\
         print($r.timed_out .. \" \" .. ($r.duration_ms < 1500) .. \" \" .. $r.stdout_truncated .. \" \" .. $r.stderr_truncated)\n",
        Duration::from_secs(3),
    );
    assert_eq!(stdout, "true true true true\n", "stderr={stderr:?}");
}

/// `timeout: 0` means there is no deadline, so a descendant which still owns
/// the capture pipe gets to produce its output before the call returns.  The
/// outer watchdog turns an accidental unbounded wait into a test failure.
#[test]
fn argv_without_deadline_waits_for_capture_eof() {
    let (stdout, stderr) = run_script(
        "argv-no-deadline-capture",
        "$r = run_argv([\"sh\", \"-c\", \"sh -c 'sleep 0.4; echo LATE' & exit 0\"], {timeout: 0})\n\
         print($r.stdout)\n\
         print($r.stdout_truncated)\n",
        Duration::from_secs(3),
    );
    assert_eq!(stdout, "LATE\n\nfalse\n", "stderr={stderr:?}");
}

/// Once a deadline abandons a capture reader, that dead call must never tee
/// later descendant output into the Mix process's real fd 1.
#[test]
fn abandoned_stream_reader_stops_teeing_after_return() {
    let (stdout, stderr) = run_script(
        "argv-abandoned-live-tee",
        "$r = run_argv([\"sh\", \"-c\", \"setsid sh -c 'sleep 0.4; echo LATE' & sleep 5\"], \
             {timeout: 0.1, stream: true})\n\
         print(\"AFTER \" .. $r.stdout_truncated)\n\
         run_argv([\"sleep\", \"0.6\"], {stdout: \"null\", stderr: \"null\"})\n\
         print(\"DONE\")\n",
        Duration::from_secs(3),
    );
    assert_eq!(stdout, "AFTER true\nDONE\n", "stderr={stderr:?}");
}

/// The enabled check and real-fd write must be one abandonment-critical
/// section. The first call fills fd 1 while holding Rust's stdout lock; eight
/// later drain workers then race abandonment while queued for that lock. The
/// side-file marker proves whether those calls returned before the harness
/// released the lock; any queued writer at that point can only write after its
/// `run_argv` call has returned.
#[test]
fn abandoned_in_flight_stream_write_cannot_follow_return() {
    let returned_marker = script_path("argv-in-flight-returned");
    let mut source = String::from(
        "run_argv([\"sh\", \"-c\", \"dd if=/dev/zero bs=131072 count=1 2>/dev/null; setsid sleep 2 & sleep 5\"], \
             {timeout: 0.01, stream: true})\n",
    );
    for _ in 0..8 {
        source.push_str(
            "run_argv([\"sh\", \"-c\", \"printf LATE; setsid sleep 2 & sleep 5\"], \
                 {timeout: 0.01, stream: true})\n",
        );
    }
    source.push_str(&format!(
        "write_file({:?}, \"returned\")\n\
         run_argv([\"printf\", \"AFTER\\n\"], {{stdout: \"inherit\", stderr: \"null\"}})\n\
         run_argv([\"sleep\", \"0.3\"], {{stdout: \"null\", stderr: \"null\"}})\n\
         run_argv([\"printf\", \"DONE\\n\"], {{stdout: \"inherit\", stderr: \"null\"}})\n",
        returned_marker.to_string_lossy()
    ));

    let (stdout, stderr, returned_while_stdout_stalled) = run_script_after_stdout_stall(
        "argv-in-flight-live-tee",
        &source,
        Duration::from_millis(2500),
        Duration::from_secs(5),
        &returned_marker,
    );
    assert!(
        !returned_while_stdout_stalled,
        "run_argv returned while an enabled tee write was still queued on stdout"
    );
    let after = stdout
        .split_once("AFTER\n")
        .unwrap_or_else(|| panic!("missing AFTER marker: stdout={stdout:?}, stderr={stderr:?}"))
        .1;
    assert!(
        after.ends_with("DONE\n"),
        "stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        !after.contains("LATE"),
        "an abandoned in-flight tee wrote after return: stdout={stdout:?}, stderr={stderr:?}"
    );
}

/// Once the timed open has resolved the original FIFO inode, replacing the
/// pathname must not redirect its wake-up. The watchdog converts either a wake
/// on the replacement FIFO or a lost wake of the original reader into failure.
#[test]
fn input_fifo_wake_survives_path_replacement() {
    let input = script_path("route-input-renamed-fifo");
    let original = script_path("route-input-original-fifo");
    fifo(&input);
    let source = format!(
        "$r = run_argv([\"true\"], {{stdin: {{file: {:?}}}, timeout: 0.3}})\n\
         print($r.error_code)\n",
        input.to_string_lossy()
    );
    let (stdout, stderr) = run_script_with_setup_and_action(
        "fifo-route-path-replacement",
        &source,
        Duration::from_secs(2),
        |_| {},
        |_| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::rename(&input, &original).expect("rename original FIFO");
            fifo(&input);
        },
    );
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&original);
    assert_eq!(stdout, "PROCESS_STDIO\n", "stderr={stderr:?}");
}

/// A timed-out no-writer FIFO open must not strand one native worker per
/// call.  Read `/proc` from a short-lived child so the count describes the
/// long-lived Mix process under test, not this Rust harness.
#[cfg(target_os = "linux")]
#[test]
fn timed_out_input_fifo_opens_do_not_leak_threads() {
    let input = script_path("route-input-thread-count-fifo");
    fifo(&input);
    let count =
        "run_argv([\"sh\", \"-c\", \"awk '/^Threads:/ {print $2}' /proc/$PPID/status\"]).stdout";
    let mut source = format!("$before = {count}\n");
    for _ in 0..24 {
        source.push_str(&format!(
            "run_argv([\"true\"], {{stdin: {{file: {:?}}}, timeout: 0.02}})\n",
            input.to_string_lossy()
        ));
    }
    source.push_str(&format!(
        "$after = {count}\nprint($before)\nprint($after)\n"
    ));

    let (stdout, stderr) = run_script("fifo-route-thread-count", &source, Duration::from_secs(4));
    let _ = std::fs::remove_file(&input);
    let counts: Vec<usize> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.parse().expect("numeric /proc thread count"))
        .collect();
    assert_eq!(counts.len(), 2, "stdout={stdout:?}, stderr={stderr:?}");
    assert!(
        counts[1] <= counts[0] + 3,
        "timed-out FIFO opens leaked threads: before={}, after={}, stderr={stderr:?}",
        counts[0],
        counts[1]
    );
}

/// An explicit no-deadline call owns its stdin-data writer until the last
/// holder closes the read end. Detaching the writer makes every quick-returning
/// call leave one blocked native thread behind; short-lived holders keep this
/// regression bounded while making the growth deterministic.
#[cfg(target_os = "linux")]
#[test]
fn no_deadline_stdin_writers_are_joined() {
    let count = "to_number(trim(run_argv([\"sh\", \"-c\", \"awk '/^Threads:/ {print $2}' /proc/$PPID/status\"]).stdout))";
    let mut source = format!("$before = {count}\n");
    for _ in 0..4 {
        source.push_str(
            "run_argv([\"sh\", \"-c\", \"setsid sh -c 'sleep 0.15' <&0 >/dev/null 2>/dev/null & exit 0\"], \
                 {stdin: repeat(\"x\", 1000000), stdout: \"null\", stderr: \"null\", timeout: 0})\n",
        );
    }
    for _ in 0..4 {
        source.push_str(
            "run_pipeline([{argv: [\"sh\", \"-c\", \"setsid sh -c 'sleep 0.15' <&0 >/dev/null 2>/dev/null & exit 0\"], \
                 stdin: repeat(\"x\", 1000000), stdout: \"null\", stderr: \"null\"}], {timeout: 0})\n",
        );
    }
    source.push_str(&format!(
        "$after = {count}\nprint(\"\" .. ($after - $before))\n"
    ));

    let (stdout, stderr) = run_script(
        "no-deadline-stdin-writer-threads",
        &source,
        Duration::from_secs(4),
    );
    let growth: isize = stdout
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("numeric thread growth: stdout={stdout:?}, stderr={stderr:?}"));
    assert!(
        growth <= 1,
        "no-deadline stdin writers leaked threads: growth={growth}, stderr={stderr:?}"
    );
}

#[test]
fn pipeline_timeout_abandons_capture_held_by_detached_descendant() {
    let (stdout, stderr) = run_script(
        "pipeline-detached-capture",
        "$r = run_pipeline([[\"sh\", \"-c\", \"setsid sh -c 'sleep 5' & sleep 5\"], [\"cat\"]], {timeout: 0.5})\n\
         print($r.timed_out .. \" \" .. ($r.duration_ms < 1500) .. \" \" .. $r.stderr_truncated)\n",
        Duration::from_secs(3),
    );
    assert_eq!(stdout, "true true true\n", "stderr={stderr:?}");
}
