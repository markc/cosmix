//! End-to-end guard: the `mix` binary must turn a runaway recursion
//! into a clean exit-1 Mix error, never a native stack overflow
//! (SIGABRT 134 / SIGSEGV 139). This exercises the recursion-depth cap
//! through the real binary on its real (main-thread, ~8 MB) stack at the
//! binary's configured limit, so a regression that lets the cap leak
//! shows up as a crashing subprocess here.

use std::process::Command;

#[test]
fn runaway_recursion_exits_cleanly_not_crash() {
    let mix = env!("CARGO_BIN_EXE_mix");
    let src = "function f($n)\n  return f($n + 1)\nend\nprint(f(0))\n";
    let out = Command::new(mix)
        .arg("-c")
        .arg(src)
        .env("MIX_STATS", "off")
        .output()
        .expect("spawn mix");

    let code = out.status.code();
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Exit 1 = clean Mix runtime error. 134 (SIGABRT) / 139 (SIGSEGV) /
    // None (killed by signal) would mean the cap leaked and the stack
    // overflowed.
    assert_eq!(
        code,
        Some(1),
        "expected clean exit 1, got {code:?} (134/139/None = stack overflow); stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("recursion depth exceeded"),
        "stderr:\n{stderr}"
    );
}
