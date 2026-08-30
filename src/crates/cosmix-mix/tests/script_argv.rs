//! `args()` reports the script's arguments — and nothing else.
//!
//! It used to re-derive them from the process argv with a hardcoded
//! `skip(2)`, on the assumption that argv is always `[mix, script, args…]`.
//! Two invocations break that assumption, and both were wrong until 0.39.1:
//!
//!   mix --strict-arity script.mix a b   -> args() led with the script path
//!   mix -c '<code>' a b                 -> args() led with the program text
//!
//! `$1`, `$2`, … were right throughout, because they come from the argument
//! list the CLI parsed. This pins `args()` to that same list: the two must
//! agree, which is the property the skip-count could never hold.
//!
//! Out-of-process, like env_default.rs — the contract is about how the binary
//! is invoked, so it has to be invoked.

use std::process::Command;

fn script() -> String {
    format!(
        "{}/tests/scripts/script_argv.mix",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn run(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .output()
        .expect("failed to spawn mix binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "mix {:?} exited non-zero ({:?})\nstdout:\n{}\nstderr:\n{}",
        args,
        output.status.code(),
        stdout,
        stderr
    );
    stdout.trim().to_string()
}

#[test]
fn args_matches_positionals_however_the_script_is_reached() {
    let script = script();
    let expected = "args:alpha,beta\npositional:alpha,beta";

    assert_eq!(
        run(&[&script, "alpha", "beta"]),
        expected,
        "plain invocation"
    );
    assert_eq!(
        run(&["--strict-arity", &script, "alpha", "beta"]),
        expected,
        "a flag before the script must not shift args()"
    );
    assert_eq!(
        run(&["--no-prelude", &script, "alpha", "beta"]),
        expected,
        "any flag, not just --strict-arity"
    );
}

#[test]
fn dash_c_does_not_report_its_own_code_as_an_argument() {
    assert_eq!(
        run(&["-c", "print(join(args(), \",\"))", "alpha", "beta"]),
        "alpha,beta",
        "the program text is not an argument to the program"
    );
}

#[test]
fn no_script_arguments_is_an_empty_list() {
    // The old skip(2) made this depend on how the harness itself was invoked.
    assert_eq!(run(&["-c", "print(length(args()))"]), "0");
}
