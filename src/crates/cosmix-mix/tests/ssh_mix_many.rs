//! `ssh_mix_many(hosts, source[, opts])` end to end, against a fake `ssh`.
//!
//! The fake is a shell script first on PATH. It finds the host after ssh's
//! `--`, refuses the host `down` the way a real ssh reports an unreachable
//! host (exit 255, message on stderr), and otherwise runs the mix under test
//! as `mix -` on the shipped stdin with `FAKE_HOST` set — so the whole real
//! path runs: planning, the bindings prefix, the worker pool, the
//! `ssh_result` map and per-host decode. Out-of-process for the reason
//! `which_executable.rs` gives: PATH must not be mutated in the test process.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const FAKE_SSH: &str = r#"#!/bin/sh
host=
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; host=$1; break; fi
  shift
done
if [ "$host" = "down" ]; then
  echo "ssh: connect to host down port 22: Connection refused" >&2
  exit 255
fi
FAKE_HOST=$host exec "$MIX_UNDER_TEST" -
"#;

struct Bed {
    dir: tempfile::TempDir,
}

impl Bed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let ssh = dir.path().join("ssh");
        fs::write(&ssh, FAKE_SSH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Bed { dir }
    }

    /// Run `program` under the mix being tested with the fake ssh first on
    /// PATH; returns (stdout, stderr, exit status success).
    fn run(&self, program: &str) -> (String, String, bool) {
        let path = format!("{}:/usr/bin:/bin", self.dir.path().display());
        let out = Command::new(env!("CARGO_BIN_EXE_mix"))
            .arg("-c")
            .arg(program)
            .env("PATH", path)
            .env("MIX_UNDER_TEST", env!("CARGO_BIN_EXE_mix"))
            .output()
            .expect("spawn mix");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    }
}

#[test]
fn one_unreachable_host_is_data_and_the_others_decode() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        r#"$r = ssh_mix_many(["alpha", "down", "gamma"], 'print(data_encode({host: env("FAKE_HOST"), n: $n + 1}))', {bindings: {n: 41}, decode: "data", timeout: 20})
print(join(keys($r), ","))
for each $h in ["alpha", "gamma"]
  $x = $r[$h]
  print($h .. " ok=" .. $x.ok .. " host=" .. $x.host .. " vhost=" .. $x.value.host .. " n=" .. $x.value.n)
end
$d = $r["down"]
print("down ok=" .. $d.ok .. " exit=" .. $d.exit_code .. " has_value=" .. has_key($d, "value") .. " refused=" .. (pos("Connection refused", $d.stderr) > 0))
"#,
    );
    assert!(ok, "the call must not raise: stdout={out} stderr={err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "alpha,down,gamma", "results keyed by host in INPUT order: {out}");
    assert_eq!(lines[1], "alpha ok=true host=alpha vhost=alpha n=42", "{out}");
    assert_eq!(lines[2], "gamma ok=true host=gamma vhost=gamma n=42", "{out}");
    assert_eq!(
        lines[3], "down ok=false exit=255 has_value=false refused=true",
        "{out}"
    );
}

#[test]
fn a_truncated_stdout_refuses_decode_for_that_host_only() {
    let bed = Bed::new();
    // `alpha` prints a long value that max_output truncates; `beta` a short
    // one that fits. ssh_mix would RAISE for alpha; the fan-out records the
    // refusal on alpha and still decodes beta.
    let (out, err, ok) = bed.run(
        r#"$src = 'if env("FAKE_HOST") == "alpha" then
  print(data_encode({count: 1234567890123}))
else
  print(data_encode({c: 1}))
end'
$r = ssh_mix_many(["alpha", "beta"], $src, {decode: "data", max_output: 12, timeout: 20})
$a = $r["alpha"]
print("alpha ok=" .. $a.ok .. " exit=" .. $a.exit_code .. " truncated=" .. $a.stdout_truncated .. " has_value=" .. has_key($a, "value") .. " says=" .. (pos("truncated", $a.decode_error) > 0))
$b = $r["beta"]
print("beta ok=" .. $b.ok .. " c=" .. $b.value.c .. " has_decode_error=" .. has_key($b, "decode_error"))
"#,
    );
    assert!(ok, "the call must not raise: stdout={out} stderr={err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0], "alpha ok=false exit=0 truncated=true has_value=false says=true",
        "{out}"
    );
    assert_eq!(lines[1], "beta ok=true c=1 has_decode_error=false", "{out}");
}

#[test]
fn unparseable_stdout_is_a_per_host_decode_error() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        r#"$r = ssh_mix_many(["alpha"], 'print("{not strict data")', {decode: "data", timeout: 20})
$a = $r["alpha"]
print("ok=" .. $a.ok .. " exit=" .. $a.exit_code .. " has_value=" .. has_key($a, "value") .. " has_err=" .. (length($a.decode_error) > 0))
"#,
    );
    assert!(ok, "the call must not raise: stdout={out} stderr={err}");
    assert_eq!(out.trim(), "ok=false exit=0 has_value=false has_err=true", "{out}");
}

#[test]
fn ssh_mix_itself_still_raises_on_truncated_decode() {
    // The refactor that split ssh_mix into plan + execute must not have
    // softened the single-host contract.
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        r#"try
  $r = ssh_mix("alpha", 'print(data_encode({count: 1234567890123}))', {decode: "data", max_output: 12, timeout: 20})
  print("no raise")
catch $e
  print("raised " .. (pos("truncated", $e) > 0))
end
"#,
    );
    assert!(ok, "stdout={out} stderr={err}");
    assert_eq!(out.trim(), "raised true", "{out}");
}

#[test]
fn hosts_run_concurrently_up_to_max() {
    let bed = Bed::new();
    // Four hosts that each sleep 1.5 s. Concurrently that is ~1.5 s; serially
    // (max: 1) it is at least 6 s. The bound is loose on purpose — a busy
    // build worker must not flake it — but no serial run can meet it.
    let (out, err, ok) = bed.run(
        r#"$t0 = monotonic()
$r = ssh_mix_many(["a", "b", "c", "d"], 'sleep(1.5)
print("done")', {max: 4, timeout: 30})
$el = monotonic() - $t0
$all = true
for each $h, $x in $r
  if !$x.ok then
    $all = false
  end
end
print("all_ok=" .. $all .. " fast=" .. ($el < 5.5))
"#,
    );
    assert!(ok, "stdout={out} stderr={err}");
    assert_eq!(out.trim(), "all_ok=true fast=true", "{out}");
}
