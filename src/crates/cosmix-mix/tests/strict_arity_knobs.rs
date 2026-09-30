//! A1 step 1 (TODO-mix strict-arity sweep): the strict mode now has three
//! operator-level knobs — `MIX_STRICT_ARITY=1` env, `$strict_arity = true`
//! in `~/.mixrc`, and `ssh_mix(…, {strict_arity: true})` — plus a warning
//! when a mix flag lands AFTER the source and is therefore a script
//! argument, not a flag. Out-of-process, like script_argv.rs: the contract
//! is about how the binary is invoked.

use std::process::Command;

fn mix_bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mix"));
    c.env("MIX_STATS", "off");
    c
}

#[test]
fn env_knob_turns_on_strict_arity() {
    // A user-function surplus argument is the A1 probe: `f(1, 2)` is the
    // compatible extra-ignored binding by default and ARITY_MISMATCH under
    // strict mode. (A builtin surplus would no longer serve — the A2
    // contract-type check makes e.g. remove(map, key) a TYPE_MISMATCH in
    // every mode.)
    let out = mix_bin()
        .env("MIX_STRICT_ARITY", "1")
        .args(["-c", "fn f($x) return 1 end\nprint(f(1, 2))"])
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "surplus-arity call must fail under the env knob");
    assert!(stderr.contains("ARITY_MISMATCH"), "got: {stderr}");
    // And without the knob the same call is the compatible extra-ignored
    // binding. The knob is inherited through the environment, so the
    // default arm must REMOVE it explicitly — a harness or fleet host
    // that already sets MIX_STRICT_ARITY would otherwise leak into this
    // assertion.
    let ok = mix_bin()
        .env_remove("MIX_STRICT_ARITY")
        .args(["-c", "fn f($x) return 1 end\nprint(f(1, 2))"])
        .output()
        .expect("run mix");
    assert!(
        ok.status.success(),
        "default mode keeps the compatible binding: {:?} / stderr: {}",
        ok.status.code(),
        String::from_utf8_lossy(&ok.stderr)
    );
}

#[test]
fn mixrc_strict_arity_variable_turns_on_strict_mode() {
    let dir = std::env::temp_dir().join(format!("mix-strict-rc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    std::fs::write(dir.join(".mixrc"), "$strict_arity = true\n").expect("write .mixrc");
    let out = mix_bin()
        .env("HOME", &dir)
        .env_remove("MIX_STRICT_ARITY")
        .args(["-ci", "fn f($x) return 1 end\nprint(f(1, 2))"])
        .output()
        .expect("run mix -ci");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "mixrc $strict_arity must apply: {stderr}");
    assert!(stderr.contains("ARITY_MISMATCH"), "got: {stderr}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn trailing_mix_flag_is_warned_not_silently_ignored() {
    let out = mix_bin()
        .args(["-c", "print(1)", "--strict-arity"])
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the code itself still runs");
    assert!(
        stderr.contains("script argument, not a flag"),
        "trailing flag must warn: {stderr}"
    );
}
