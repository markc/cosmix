//! `run_argv`/`run_argv_must`/`run_parallel` env nil-removal regression.
//!
//! Contract: `env: {NAME: nil}` REMOVES an inherited NAME from the child
//! environment. The regression this pins: `parse_run_argv_opts` collected the
//! nil keys into `env_unset`, but the captured-process engine adapter dropped
//! them on the floor, so an inherited NAME survived into the child.
//!
//! The sentinel is planted on the outer `mix` child via `Command::env` only —
//! never on this test process, because `std::env::set_var`/`remove_var` would
//! race every other in-process getenv under the parallel test runner. The
//! probe chain is harness → mix (has SENTINEL) → run_argv child (must NOT have
//! it) → inner `mix -c` printing `env(SENTINEL, "REMOVED")`. Probing only the
//! unique sentinel name means no false match on an unrelated inherited
//! variable (the failure mode of the earlier DISPLAY-based test).

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SENTINEL: &str = "MIX_TEST_ENV_UNSET_SENTINEL";

#[test]
fn env_nil_removes_inherited_sentinel_from_run_argv_family() {
    let mix_bin = env!("CARGO_BIN_EXE_mix");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let script_path = std::env::temp_dir().join(format!(
        "mix-env-unset-regression-{}-{nonce}.mix",
        std::process::id()
    ));
    let probe = format!("print(env('{SENTINEL}', 'REMOVED'))");
    let source = format!(
        r#"$mix = {mix_bin:?}
$probe = {probe:?}

$r = run_argv([$mix, "-c", $probe], {{env: {{{sentinel}: nil}}}})
print("argv:" .. $r.ok .. ":" .. $r.stdout)

$m = run_argv_must([$mix, "-c", $probe], {{env: {{{sentinel}: nil}}}})
print("must:" .. $m)

$p = run_parallel([{{argv: [$mix, "-c", $probe], env: {{{sentinel}: nil}}}}])
print("parallel:" .. $p[0].ok .. ":" .. $p[0].stdout)

$q = run_pipeline([{{argv: [$mix, "-c", $probe], env: {{{sentinel}: nil}}}}])
print("pipeline:" .. $q.ok .. ":" .. $q.stdout)

$s = run_argv([$mix, "-c", $probe], {{clear_env: true, env: {{{sentinel}: "explicit"}}}})
print("clear-set:" .. $s.ok .. ":" .. $s.stdout)
"#,
        mix_bin = mix_bin,
        probe = probe,
        sentinel = SENTINEL,
    );
    {
        let mut file = std::fs::File::create(&script_path).expect("write test script");
        file.write_all(source.as_bytes())
            .expect("write test script");
    }

    let output = Command::new(mix_bin)
        .arg(&script_path)
        .env("MIX_STATS", "off")
        .env(SENTINEL, "inherited")
        .output()
        .expect("failed to spawn mix binary");
    let _ = std::fs::remove_file(&script_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "mix exited non-zero ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout,
        stderr
    );

    // Each print ends in the captured newline plus print's own, so filter
    // the empty interleave lines and compare the payloads verbatim.
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines,
        vec![
            "argv:true:REMOVED",
            "must:REMOVED",
            "parallel:true:REMOVED",
            "pipeline:true:REMOVED",
            "clear-set:true:explicit"
        ],
        "env nil-removal contract failed (inherited sentinel leaked)\nstderr:\n{}",
        stderr
    );
}
