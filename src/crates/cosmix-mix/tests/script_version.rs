//! `mix SCRIPT --version`, `script_version()` and lint note MIX-D3016 — the
//! Mix-script half of the fleet `--version` contract (Mark 2026-09-25: "ALL
//! binaries and mix script should emit a --version with build details").
//!
//! Every positive case is paired with a way it could have passed vacuously:
//! the script body prints a marker, and each version query asserts the marker
//! is ABSENT (the script did not run); the position-2 case asserts it is
//! PRESENT (the script did run and saw `--version`).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};

const RAN: &str = "SCRIPT-BODY-RAN";

fn mix() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mix"));
    c.env("MIX_STATS", "off");
    c
}

fn run(args: &[&str]) -> Output {
    mix().args(args).output().expect("run mix")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, body).unwrap();
    p
}

fn sha12(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..12]
        .to_string()
}

fn mix_part() -> String {
    format!("; mix {} (", env!("CARGO_PKG_VERSION"))
}

/// One line, exit 0, script not run, shape `name version (sha12, modified …; mix …)`.
fn assert_version_line(o: &Output, name: &str, version: &str, body: &str) -> String {
    assert!(o.status.success(), "exit {:?}, stderr {:?}", o.status, stderr(o));
    let out = stdout(o);
    assert!(!out.contains(RAN), "the script body ran on a version query: {out:?}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line: {out:?}");
    let line = lines[0];
    let prefix = format!("{name} {version} ({}, modified ", sha12(body));
    assert!(line.starts_with(&prefix), "{line:?} should start {prefix:?}");
    assert!(line.contains(&mix_part()), "{line:?} names the interpreter");
    assert!(line.ends_with("))"), "{line:?}");
    line.to_string()
}

const VERSIONED: &str = "#!/opt/cosmix/bin/mix\n-- deploy something\n-- version: 1.2.3\nprint(\"SCRIPT-BODY-RAN\")\n";
const UNVERSIONED: &str = "print(\"SCRIPT-BODY-RAN\")\n";

#[test]
fn versioned_script_answers_without_running() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "deploy.mix", VERSIONED);
    let line = assert_version_line(&run(&[p.to_str().unwrap(), "--version"]), "deploy.mix", "1.2.3", VERSIONED);
    // RFC 3339 UTC, seconds precision.
    let modified = line.split("modified ").nth(1).unwrap().split(';').next().unwrap();
    assert_eq!(modified.len(), 20, "{modified:?}");
    assert!(modified.ends_with('Z') && modified.as_bytes()[10] == b'T', "{modified:?}");
}

#[test]
fn short_flag_and_leading_interpreter_flags() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "deploy.mix", VERSIONED);
    let p = p.to_str().unwrap();
    assert_version_line(&run(&[p, "-V"]), "deploy.mix", "1.2.3", VERSIONED);
    assert_version_line(&run(&["--no-prelude", "--strict-arity", p, "--version"]), "deploy.mix", "1.2.3", VERSIONED);
}

#[test]
fn unversioned_script_says_so() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "plain.mix", UNVERSIONED);
    assert_version_line(&run(&[p.to_str().unwrap(), "--version"]), "plain.mix", "unversioned", UNVERSIONED);
}

#[test]
fn malformed_header_reads_as_unversioned() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- version: 1.2\nprint(\"SCRIPT-BODY-RAN\")\n";
    let p = write(d.path(), "typo.mix", body);
    assert_version_line(&run(&[p.to_str().unwrap(), "--version"]), "typo.mix", "unversioned", body);
}

#[test]
fn syntax_error_script_still_answers() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- version: 0.4.0\nif then ((( end end\nprint(\"SCRIPT-BODY-RAN\")\n";
    let p = write(d.path(), "broken.mix", body);
    let p = p.to_str().unwrap();
    // Control: the file really does not parse.
    let control = run(&["--check", p]);
    assert!(!control.status.success(), "control: the fixture must be a syntax error");
    assert_version_line(&run(&[p, "--version"]), "broken.mix", "0.4.0", body);
}

#[test]
fn version_in_position_two_belongs_to_the_script() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- version: 1.0.0\nprint(\"SCRIPT-BODY-RAN\")\nprint(\"argv:\" .. join(args(), \",\"))\n";
    let p = write(d.path(), "own.mix", body);
    let o = run(&[p.to_str().unwrap(), "x", "--version"]);
    assert!(o.status.success(), "{:?}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains(RAN), "the script must run: {out:?}");
    assert!(out.contains("argv:x,--version"), "{out:?}");
}

#[test]
fn serve_version_exits_before_serving() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- version: 0.2.0\non ping\n  reply(\"pong\")\nend\n";
    let p = write(d.path(), "citizen.mix", body);
    // No broker exists for this test; reaching the connect path would fail
    // or hang, so a clean one-line exit proves it stopped before serving.
    let o = mix()
        .args(["--serve", p.to_str().unwrap(), "--version"])
        .env("COSMIX_NODED_URL", "ws://127.0.0.1:9/ws")
        .output()
        .unwrap();
    assert_version_line(&o, "citizen.mix", "0.2.0", body);
}

#[test]
fn stdin_script_is_named_dash_with_no_mtime() {
    let body = "-- version: 2.0.0\nprint(\"SCRIPT-BODY-RAN\")\n";
    let mut child = mix()
        .args(["-", "--version"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{:?}", stderr(&o));
    let out = stdout(&o);
    assert!(!out.contains(RAN), "{out:?}");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    let expected = format!("- 2.0.0 ({}{}", sha12(body), mix_part());
    assert!(out.starts_with(&expected), "{out:?} should start {expected:?}");
    assert!(!out.contains("modified"), "{out:?}");
}

#[test]
fn unreadable_script_fails_like_running_it() {
    let o = run(&["/nonexistent/definitely-not-here.mix", "--version"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("definitely-not-here.mix"), "{:?}", stderr(&o));
    assert_eq!(stdout(&o), "");
}

/// Cold, like `mix --version`: a malformed session fd makes the native
/// session lane complain if it starts at all. Control first, so an
/// unconditionally-empty stderr cannot read as a pass.
#[test]
fn script_version_query_is_cold() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "deploy.mix", VERSIONED);
    let control = mix()
        .args(["--no-prelude", "-c", "print(1)"])
        .env("COSMIX_SESSION_FD", "99999")
        .output()
        .unwrap();
    assert!(stderr(&control).contains("mix native-session FAILED at"), "probe is dead");
    let o = mix()
        .args([p.to_str().unwrap(), "--version"])
        .env("COSMIX_SESSION_FD", "99999")
        .output()
        .unwrap();
    assert_eq!(stderr(&o), "", "a script version query started the session lane");
}

#[test]
fn builtin_matches_the_version_line() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- version: 3.1.4\n$v = script_version()\nprint($v.name .. \"|\" .. $v.version .. \"|\" .. $v.sha .. \"|\" .. len($v.sha256) .. \"|\" .. $v.mix.version .. \"|\" .. type($v.modified))\n";
    let p = write(d.path(), "self.mix", body);
    let o = run(&[p.to_str().unwrap()]);
    assert!(o.status.success(), "{:?}", stderr(&o));
    assert_eq!(
        stdout(&o).trim_end(),
        format!("self.mix|3.1.4|{}|64|{}|string", sha12(body), env!("CARGO_PKG_VERSION"))
    );

    let body = "print(type(script_version().version) .. \"|\" .. script_version().name)\n";
    let p = write(d.path(), "bare.mix", body);
    assert_eq!(stdout(&run(&[p.to_str().unwrap()])).trim_end(), "nil|bare.mix");
}

#[test]
fn builtin_is_nil_outside_a_script() {
    let o = run(&["-c", "print(script_version() == nil)"]);
    assert_eq!(stdout(&o).trim_end(), "true", "{:?}", stderr(&o));
}

#[test]
fn builtin_from_stdin_has_no_mtime() {
    let body = "-- version: 1.0.0\nprint(script_version().name .. \"|\" .. type(script_version().modified))\n";
    let mut child = mix()
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(stdout(&o).trim_end(), "-|nil");
}

// ── lint: MIX-D3016 ────────────────────────────────────────────────────

fn lint(args: &[&str]) -> (i32, String) {
    let o = mix().arg("lint").args(args).output().unwrap();
    (o.status.code().unwrap_or(-1), stdout(&o))
}

#[test]
fn lint_notes_an_unversioned_shebang_or_bin_script() {
    let d = tempfile::tempdir().unwrap();
    let shebang = write(d.path(), "tool.mix", "#!/opt/cosmix/bin/mix\nprint(1)\n");
    let in_bin = write(d.path(), "_bin/deploy.mix", "print(1)\n");
    for p in [&shebang, &in_bin] {
        let (code, out) = lint(&["--deny-warnings", p.to_str().unwrap()]);
        assert_eq!(code, 0, "a note never gates: {out}");
        assert!(out.contains("MIX-D3016 note:"), "{out}");
    }
}

#[test]
fn lint_is_silent_for_versioned_scripts_and_library_files() {
    let d = tempfile::tempdir().unwrap();
    let versioned = write(d.path(), "bin/ok.mix", "-- version: 0.1.0\nprint(1)\n");
    let library = write(d.path(), "lib/util.mix", "fn f()\n  return 1\nend\n");
    for p in [&versioned, &library] {
        let (code, out) = lint(&["--require-version", p.to_str().unwrap()]);
        assert_eq!(code, 0, "{out}");
        assert!(!out.contains("MIX-D3016"), "{out}");
    }
}

#[test]
fn require_version_promotes_to_a_warning() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "bin/tool.mix", "print(1)\n");
    let (code, out) = lint(&["--require-version", p.to_str().unwrap()]);
    assert_eq!(code, 0, "a warning alone does not fail: {out}");
    assert!(out.contains("MIX-D3016 warning:"), "{out}");
    let (code, out) = lint(&["--require-version", "--deny-warnings", p.to_str().unwrap()]);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn lint_names_a_malformed_header_line() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "bin/typo.mix", "-- a tool\n-- version: 1.2\nprint(1)\n");
    let (_, out) = lint(&[p.to_str().unwrap()]);
    assert!(out.contains("typo.mix:2: MIX-D3016 note:"), "{out}");
    assert!(out.contains("1.2"), "{out}");
}

// ── round 2: opt-out, -i, leading region, --json, symlink, lint scope ──

#[test]
fn opt_out_script_receives_its_own_version_flag() {
    let d = tempfile::tempdir().unwrap();
    let body = "#!/usr/bin/env mix\n-- version: 1.0.0\n-- version-flag: script\nprint(\"SCRIPT-BODY-RAN\")\nprint(\"argv:\" .. join(args(), \",\"))\n";
    let p = write(d.path(), "wrapper.mix", body);
    for flag in ["--version", "-V"] {
        let o = run(&[p.to_str().unwrap(), flag]);
        assert!(o.status.success(), "{:?}", stderr(&o));
        let out = stdout(&o);
        assert!(out.contains(RAN), "an opted-out script must run: {out:?}");
        assert!(out.contains(&format!("argv:{flag}")), "{out:?}");
    }
    // Control: the same file minus the opt-out line is answered for.
    let plain = body.replace("-- version-flag: script\n", "");
    let p2 = write(d.path(), "plain.mix", &plain);
    assert_version_line(&run(&[p2.to_str().unwrap(), "--version"]), "plain.mix", "1.0.0", &plain);
    // Serve has no argv to hand the flag to, so Mix still answers there.
    let o = run(&["--serve", p.to_str().unwrap(), "--version"]);
    assert_version_line(&o, "wrapper.mix", "1.0.0", body);
}

#[test]
fn opt_out_from_stdin_still_runs_the_bytes() {
    let body = "-- version: 1.0.0\n-- version-flag: script\nprint(\"stdin-argv:\" .. join(args(), \",\"))\n";
    let mut child = mix()
        .args(["-", "--version"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{:?}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), "stdin-argv:--version");
}

#[test]
fn interactive_flag_before_the_script_is_still_a_query() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "deploy.mix", VERSIONED);
    let o = mix()
        .args(["-i", p.to_str().unwrap(), "--version"])
        .env("HOME", d.path())
        .output()
        .unwrap();
    assert_version_line(&o, "deploy.mix", "1.2.3", VERSIONED);
}

#[test]
fn header_inside_a_heredoc_is_not_the_header() {
    let d = tempfile::tempdir().unwrap();
    let body = "-- makes scripts\n$gen = <<EOF\n-- version: 9.9.9\nprint(1)\nEOF\nprint(\"SCRIPT-BODY-RAN\")\n";
    let p = write(d.path(), "gen.mix", body);
    let p = p.to_str().unwrap();
    assert_version_line(&run(&[p, "--version"]), "gen.mix", "unversioned", body);
    // And lint still owes it a header (it is under bin/ here).
    let lp = write(d.path(), "bin/gen.mix", body);
    let (_, out) = lint(&[lp.to_str().unwrap()]);
    assert!(out.contains("MIX-D3016"), "{out}");
}

#[test]
fn json_form_is_the_builtin_map() {
    let d = tempfile::tempdir().unwrap();
    let p = write(d.path(), "deploy.mix", VERSIONED);
    let o = run(&[p.to_str().unwrap(), "--version", "--json"]);
    assert!(o.status.success(), "{:?}", stderr(&o));
    let out = stdout(&o);
    assert!(!out.contains(RAN), "{out:?}");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("one JSON object");
    let full: String = Sha256::digest(VERSIONED.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(v["name"], "deploy.mix");
    assert_eq!(v["version"], "1.2.3");
    assert_eq!(v["sha"], sha12(VERSIONED));
    assert_eq!(v["sha256"], full);
    assert!(v["modified"].as_str().is_some_and(|m| m.ends_with('Z')), "{v}");
    assert_eq!(v["mix"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(v["mix"]["dirty"].is_boolean(), "{v}");
    // Unversioned and stdin: nulls, not strings.
    let p = write(d.path(), "plain.mix", UNVERSIONED);
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&run(&[p.to_str().unwrap(), "-V", "--json"]))).unwrap();
    assert!(v["version"].is_null(), "{v}");
}

#[test]
fn symlink_reports_the_link_name_and_the_target_bytes() {
    let d = tempfile::tempdir().unwrap();
    let target = write(d.path(), "real-tool.mix", VERSIONED);
    let link = d.path().join("tool");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_version_line(&run(&[link.to_str().unwrap(), "--version"]), "tool", "1.2.3", VERSIONED);
}

#[test]
fn lint_gates_serve_citizens_and_script_directories() {
    let d = tempfile::tempdir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    // The real shipped files carry headers: silent even under --require-version.
    for rel in ["src/desktop/scripts/desktop-session.mix", "docs/build/gen-doc-pages.mix"] {
        let (code, out) = lint(&["--require-version", "--deny-warnings", root.join(rel).to_str().unwrap()]);
        assert_eq!(code, 0, "{rel}: {out}");
        assert!(!out.contains("MIX-D3016"), "{rel}: {out}");
    }
    // The same files with the header stripped are gated: desktop-session by
    // its `--serve` marker (no shebang, no script directory in this path),
    // gen-doc-pages by its `build/` directory.
    let strip = |rel: &str| {
        let src = std::fs::read_to_string(root.join(rel)).unwrap();
        src.lines()
            .filter(|l| !l.starts_with("-- version:"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
    };
    let session = write(d.path(), "flat/desktop-session.mix", &strip("src/desktop/scripts/desktop-session.mix"));
    let pages = write(d.path(), "build/gen-doc-pages.mix", &strip("docs/build/gen-doc-pages.mix"));
    for p in [&session, &pages] {
        let (code, out) = lint(&["--require-version", "--deny-warnings", p.to_str().unwrap()]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("MIX-D3016 warning:"), "{out}");
    }
    // A top-level `on` handler marks a citizen; `scripts/` counts as a
    // script directory; `scripts/lib/` is still a library.
    let citizen = write(d.path(), "flat/citizen.mix", "on ping\n  reply(0, \"pong\")\nend\n");
    let in_scripts = write(d.path(), "scripts/tool.mix", "print(1)\n");
    for p in [&citizen, &in_scripts] {
        let (_, out) = lint(&[p.to_str().unwrap()]);
        assert!(out.contains("MIX-D3016 note:"), "{}: {out}", p.display());
    }
    let library = write(d.path(), "scripts/lib/util.mix", "fn f()\n  return 1\nend\n");
    let plain = write(d.path(), "flat/util.mix", "fn f()\n  return 1\nend\n");
    for p in [&library, &plain] {
        let (_, out) = lint(&[p.to_str().unwrap()]);
        assert!(!out.contains("MIX-D3016"), "{}: {out}", p.display());
    }
}
