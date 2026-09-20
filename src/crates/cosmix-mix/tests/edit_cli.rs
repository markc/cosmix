//! `mix edit FILE OLD NEW` — the exact-match one-line edit.
//!
//! The reason this subcommand exists is `sed -i`'s two silent failure
//! modes, so those are what the gate asserts, and it asserts them on the
//! FILE BYTES as well as the exit code. A rc-only gate would pass an
//! implementation that returned 2 and still wrote the file, which is the
//! exact bug worth catching.
//!
//! Exit-code contract: 0 edited · 1 OLD absent, untouched · 2 OLD
//! ambiguous, untouched · 3 usage / I/O.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mix-edit-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create workdir");
    dir
}

fn seed(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, body).expect("seed file");
    p
}

fn edit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("edit")
        .args(args)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix edit")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn body(p: &Path) -> String {
    fs::read_to_string(p).expect("read back")
}

#[test]
fn unique_hit_edits_and_prints_the_diff() {
    let dir = workdir("unique");
    let p = seed(&dir, "one.txt", "alpha\nbeta\ngamma\n");
    let out = edit(&[p.to_str().unwrap(), "beta", "BETA"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "alpha\nBETA\ngamma\n");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains(":2"), "must name the line: {stdout:?}");
    assert!(stdout.contains("- beta"), "must show the old line: {stdout:?}");
    assert!(stdout.contains("+ BETA"), "must show the new line: {stdout:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// `sed -i` reports "0 substitutions" as exit 0 and edits nothing. That
/// is how three wrong files shipped in one session, so the absent needle
/// is a hard failure here.
#[test]
fn absent_needle_is_rc_1_and_leaves_the_file_alone() {
    let dir = workdir("absent");
    let p = seed(&dir, "absent.txt", "alpha\nbeta\n");
    let out = edit(&[p.to_str().unwrap(), "zeta", "ZETA"]);
    assert_eq!(code(&out), 1, "a missed needle must not be exit 0");
    assert_eq!(body(&p), "alpha\nbeta\n");
    let _ = fs::remove_dir_all(&dir);
}

/// `sed -i` edits EVERY match without being asked. An ambiguous needle
/// here writes nothing and names every line, so the caller can lengthen
/// OLD rather than discover the collateral edit later.
#[test]
fn two_hits_is_rc_2_untouched_and_names_both_lines() {
    let dir = workdir("ambig");
    let p = seed(&dir, "two.txt", "x = 1\ny = 0\nx = 1\n");
    let out = edit(&[p.to_str().unwrap(), "x = 1", "x = 2"]);
    assert_eq!(code(&out), 2);
    assert_eq!(body(&p), "x = 1\ny = 0\nx = 1\n", "must not write");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains(":1:"), "must name line 1: {stderr:?}");
    assert!(stderr.contains(":3:"), "must name line 3: {stderr:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn all_is_the_explicit_opt_in_to_every_occurrence() {
    let dir = workdir("all");
    let p = seed(&dir, "all.txt", "x = 1\ny = 0\nx = 1\n");
    let out = edit(&["--all", p.to_str().unwrap(), "x = 1", "x = 2"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "x = 2\ny = 0\nx = 2\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dry_run_matches_but_writes_nothing() {
    let dir = workdir("dry");
    let p = seed(&dir, "dry.txt", "alpha\nbeta\n");
    let out = edit(&["--dry-run", p.to_str().unwrap(), "beta", "BETA"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "alpha\nbeta\n");
    let _ = fs::remove_dir_all(&dir);
}

/// OLD is compared byte-for-byte. `a.c` must NOT match `abc` — the whole
/// point of not being a regex engine.
#[test]
fn old_is_literal_not_a_pattern() {
    let dir = workdir("literal");
    let p = seed(&dir, "regex.txt", "abc\n");
    let out = edit(&[p.to_str().unwrap(), "a.c", "X"]);
    assert_eq!(code(&out), 1, "'.' must be a literal dot");
    assert_eq!(body(&p), "abc\n");
    let _ = fs::remove_dir_all(&dir);
}

/// "aaa" contains "aa" twice by overlap but only once by the
/// non-overlapping scan the edit performs. Counting overlaps would
/// refuse an edit that was never ambiguous.
#[test]
fn overlapping_candidates_are_not_an_ambiguity() {
    let dir = workdir("overlap");
    let p = seed(&dir, "overlap.txt", "aaa\n");
    let out = edit(&[p.to_str().unwrap(), "aa", "b"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "ba\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn old_may_span_lines() {
    let dir = workdir("multi");
    let p = seed(&dir, "multi.txt", "one\ntwo\nthree\n");
    let out = edit(&[p.to_str().unwrap(), "one\ntwo", "ONE"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "ONE\nthree\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn double_dash_reaches_an_old_that_looks_like_a_flag() {
    let dir = workdir("dash");
    let p = seed(&dir, "dash.txt", "flag --old here\n");
    let out = edit(&["--", p.to_str().unwrap(), "--old", "--new"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "flag --new here\n");
    let _ = fs::remove_dir_all(&dir);
}

/// Usage and I/O failures are rc 3, kept OUT of the 1/2 band so a caller
/// can tell "your needle was wrong" from "I could not read the file".
#[test]
fn usage_and_io_failures_are_rc_3() {
    let dir = workdir("usage");
    let p = seed(&dir, "u.txt", "alpha\n");
    let path = p.to_str().unwrap();
    let missing = dir.join("nope.txt");

    assert_eq!(code(&edit(&[missing.to_str().unwrap(), "a", "b"])), 3);
    assert_eq!(code(&edit(&[path])), 3, "too few arguments");
    assert_eq!(code(&edit(&[path, "a"])), 3, "too few arguments");
    assert_eq!(code(&edit(&[path, "a", "b", "c"])), 3, "too many");
    assert_eq!(code(&edit(&[path, "alpha", "alpha"])), 3, "OLD == NEW");
    assert_eq!(code(&edit(&[path, "", "x"])), 3, "empty OLD");
    assert_eq!(code(&edit(&["--nope", path, "a", "b"])), 3, "bad flag");

    assert_eq!(body(&p), "alpha\n", "no usage error may write");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn non_utf8_input_is_refused_not_mangled() {
    let dir = workdir("binary");
    let p = dir.join("bin.dat");
    fs::write(&p, [0x68u8, 0x69, 0xff, 0xfe, 0x0a]).expect("seed binary");
    let out = edit(&[p.to_str().unwrap(), "hi", "HI"]);
    assert_eq!(code(&out), 3);
    assert_eq!(
        fs::read(&p).unwrap(),
        vec![0x68u8, 0x69, 0xff, 0xfe, 0x0a],
        "a refused binary file must be byte-identical"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The edit is a temp-file + rename, so it must carry the original mode
/// across. A script silently losing its executable bit mid-session is a
/// nasty way to learn that.
#[cfg(unix)]
#[test]
fn the_executable_bit_survives_the_edit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workdir("mode");
    let p = seed(&dir, "s.mix", "print(1)\n");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");

    let out = edit(&[p.to_str().unwrap(), "print(1)", "print(2)"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "print(2)\n");
    let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    let _ = fs::remove_dir_all(&dir);
}

/// The read follows a symlink, so the write must too. Renaming over the
/// link would replace it with a regular file and leave the real target
/// stale — a silent semantic change of exactly the kind this subcommand
/// exists to refuse.
#[cfg(unix)]
#[test]
fn editing_through_a_symlink_edits_the_target_and_keeps_the_link() {
    let dir = workdir("symlink");
    let target = seed(&dir, "real.conf", "port = 8080\n");
    let link = dir.join("link.conf");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let out = edit(&[link.to_str().unwrap(), "8080", "9090"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&target), "port = 9090\n", "the target must be edited");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link must survive as a link"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A multi-line OLD or NEW must report every line it touches. A
/// one-line report of a multi-line edit is a fragment that reads as the
/// whole truth.
#[test]
fn a_multi_line_edit_reports_every_line_it_touches() {
    let dir = workdir("diff");
    let p = seed(&dir, "d.txt", "one\ntwo\nthree\n");
    let out = edit(&[p.to_str().unwrap(), "one\ntwo", "A\nB\nC"]);
    assert_eq!(code(&out), 0);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    for want in ["- one", "- two", "+ A", "+ B", "+ C"] {
        assert!(stdout.contains(want), "missing {want:?} in {stdout:?}");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A match inside a longer line is reported as the WHOLE line, so the
/// diff is readable rather than a bare fragment.
#[test]
fn a_mid_line_match_reports_the_whole_line() {
    let dir = workdir("midline");
    let p = seed(&dir, "m.txt", "let alpha = 1;\n");
    let out = edit(&[p.to_str().unwrap(), "alpha", "beta"]);
    assert_eq!(code(&out), 0);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("- let alpha = 1;"), "{stdout:?}");
    assert!(stdout.contains("+ let beta = 1;"), "{stdout:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn help_is_a_success() {
    let out = edit(&["--help"]);
    assert_eq!(code(&out), 0, "--help is a request, not an error");
    assert!(String::from_utf8_lossy(&out.stdout).contains("mix edit"));
}

/// Multi-byte text on either side of the needle must not panic the
/// byte-offset slicing.
#[test]
fn multibyte_text_survives_the_edit() {
    let dir = workdir("utf8");
    let p = seed(&dir, "u.txt", "héllo → wörld\n");
    let out = edit(&[p.to_str().unwrap(), "→", "->"]);
    assert_eq!(code(&out), 0);
    assert_eq!(body(&p), "héllo -> wörld\n");
    let _ = fs::remove_dir_all(&dir);
}

/// No temp file may be left behind — not on the happy path, and not
/// after a refusal.
#[test]
fn no_temp_file_is_left_behind() {
    let dir = workdir("tmp");
    let p = seed(&dir, "t.txt", "a\nb\nb\n");
    assert_eq!(code(&edit(&[p.to_str().unwrap(), "a", "A"])), 0);
    assert_eq!(code(&edit(&[p.to_str().unwrap(), "b", "B"])), 2);
    assert_eq!(code(&edit(&[p.to_str().unwrap(), "zz", "Z"])), 1);

    let leftovers: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("mix-edit"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    let _ = fs::remove_dir_all(&dir);
}
