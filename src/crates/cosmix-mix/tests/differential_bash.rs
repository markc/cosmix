//! P3 of the mix tokenizer fuzz/property corpus
//! (_doc/planned/mix-tokenizer-fuzz-corpus.md in the cosmix hub): a DIFFERENTIAL
//! test against bash. On the bash≡mix safe subset (no expansion, no Mix-only
//! divergences), Mix's shell-dispatch word-splitting and quote handling must
//! match bash exactly. Both shells run the same line through a `/usr/bin/printf
//! '[%s]'` argv-dumper, and the captured argv (stdout) must be byte-identical.
//!
//! The generator deliberately EXCLUDES every construct where Mix intentionally
//! differs from bash, so a failure here is a real unintended divergence, not a
//! known design choice:
//!   - `$` / `${...}` / `$(...)` (expansion; literal-vs-interpolated rules differ)
//!   - a backtick or backslash (escaping rules differ)
//!   - a leading `~` (HOME-dependent tilde expansion, nondeterministic across
//!     envs; both shells leave a double-quoted `~` literal — Mix does NOT expand
//!     it inside quotes)
//!   - unquoted glob `* ? [` (filename expansion, nondeterministic). Brace
//!     expansion is PURELY textual and deterministic, so it IS differential-
//!     tested — but only via the fixed cases below, not the random generator
//!     (a random alternative could contain a glob char and go nondeterministic)
//!   - the `sh` keyword and bare `&`/`;` as separators (covered structurally by P0)

use proptest::prelude::*;
use std::path::Path;
use std::process::Command;

const BASH: &str = "/bin/bash";
const PRINTF: &str = "/usr/bin/printf";

fn tools_present() -> bool {
    Path::new(BASH).exists() && Path::new(PRINTF).exists()
}

// Linux errno values (glibc and musl agree) for the two *transient* exec
// failures we tolerate. ETXTBSY: the binary is busy being written —
// `CARGO_BIN_EXE_mix` is the shared `target/release/mix`, so a concurrent
// build/link or CMM autodeploy rewriting it (the same shared-target race as
// `project_deploy_snapshot_shared_target_race`) makes exec fail momentarily.
// EAGAIN: fork/exec resource pressure under load.
const ETXTBSY: i32 = 26;
const EAGAIN: i32 = 11;

/// True for exec failures that are transient infrastructure races, not a bug.
fn is_transient_exec_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ETXTBSY) | Some(EAGAIN))
        || e.kind() == std::io::ErrorKind::Interrupted // EINTR
}

/// Spawn `program -c line` and capture stdout.
///
/// A normal *nonzero exit* is NOT an exec failure — it returns `Ok` with
/// captured (usually empty) stdout, so a real mix parse/word-splitting
/// divergence on a safe-subset line still surfaces as a byte mismatch and is
/// caught. Exec-launch errors are split two ways:
///   - Transient shared-target races (ETXTBSY/EAGAIN/EINTR): retried; if they
///     survive the whole budget we return `None` ("skip this case") rather than
///     let an infra race masquerade as a word-splitting divergence.
///   - Any OTHER error (ENOENT/EACCES/ENOEXEC/corrupt binary, …): a broken test
///     environment — we panic loudly. Silently skipping these would let the
///     suite pass green without ever comparing Mix output.
fn stdout_of(program: &str, line: &str) -> Option<Vec<u8>> {
    for attempt in 0..8 {
        match Command::new(program)
            .arg("-c")
            .arg(line)
            .env("MIX_STATS", "off")
            .output()
        {
            Ok(o) => return Some(o.stdout),
            Err(e) if is_transient_exec_error(&e) => {
                if attempt == 7 {
                    return None; // sustained shared-target race — skip, not a divergence
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => panic!("failed to exec {program:?} for line {line:?}: {e}"),
        }
    }
    None
}

/// Run `printf '[%s]' _S_ <words>` through bash and mix; return both stdouts and
/// the full line. The fixed `_S_` sentinel operand makes argc observable —
/// without it `printf '[%s]'` renders zero args and one EMPTY arg identically
/// (`[]`), so a bug dropping a lone `''`/`""` argv would slip through.
fn diff(words: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>, String) {
    let line = format!("{PRINTF} '[%s]' _S_ {words}");
    let mix = env!("CARGO_BIN_EXE_mix");
    (stdout_of(BASH, &line), stdout_of(mix, &line), line)
}

/// One segment of a shell word in the safe subset: a bare token, a single-quoted
/// token (all content literal in both shells), or a double-quoted token (content
/// excludes `$`, backtick, `\`, `"`, `!`, `~` — everything double quotes treat
/// differently between the two shells).
fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9_./@+]{1,8}",
        "[a-zA-Z0-9 _.,:;<>|&*?$+@/=!^%-]{0,8}".prop_map(|c| format!("'{c}'")),
        "[a-zA-Z0-9 _.,:;<>|&*?+@/=^%-]{0,8}".prop_map(|c| format!("\"{c}\"")),
    ]
}

/// A word is 1..3 adjacent segments with no separator — `foo"bar"'baz'` must
/// concatenate into ONE argv entry identically in both shells.
fn word() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 1..4).prop_map(|segs| segs.concat())
}

fn show(b: &Option<Vec<u8>>) -> String {
    b.as_ref()
        .map(|x| String::from_utf8_lossy(x).into_owned())
        .unwrap_or_else(|| "<spawn failed>".into())
}

// Deterministic critical cases — empty args, quoted control operators/globs,
// adjacency, mixed quotes, preserved inner spaces — so these ALWAYS run
// regardless of the proptest random draw.
#[test]
fn differential_fixed_cases() {
    if !tools_present() {
        return;
    }
    let cases = [
        "''",
        "\"\"",
        "'' x",
        "a ''",
        "''''",
        "\"\"x\"\"",
        "'a b' \"c d\"",
        "'  spaced  '",
        "'a;b|c&d'",
        "\"x*?y\"",
        "'*?['",
        "'!bang'",
        "foo\"bar\"'baz'",
        "a'b'c",
        "a.b/c@d+e",
        // brace expansion (deterministic, no filesystem): alternation,
        // sequences (padding / step / reverse / 0-step / +sign / abs-step),
        // nesting, invalid-stays-literal, quoting, empties, cross products,
        // arg-position assignment shape
        "{a,b}",
        "x{a,b}y",
        "{a,b}{1..2}c",
        "{a,{b,c}d}",
        "a{b{c,d}}",
        "{x{a,b}",
        "{1..5}",
        "{5..1}",
        "{05..10}",
        "{-03..3..2}",
        "{a..e}",
        "{a..e..2}",
        "{z..x}",
        "{1..10..0}",
        "{+1..3}",
        "{a..b..-2}",
        "{}",
        "{a}",
        "{a..}",
        "{1...3}",
        "{1..2..3..4}",
        "{ab..cd}",
        "'{a,b}'",
        "\"{a,b}\"",
        "{a,'b c'}",
        "{a,\"b,c\"}",
        "{a,}",
        "{,x}",
        "x{a,}",
        "{a,\"\"}",
        "x={a,b}",
    ];
    for w in cases {
        let (b, m, line) = diff(w);
        // A None either side means the binary could not be exec'd after retries
        // (ETXTBSY from a concurrent rewrite of the shared target, etc.) — an
        // infra failure, not a parity divergence. Skip rather than false-fail.
        if b.is_none() || m.is_none() {
            continue;
        }
        assert_eq!(
            b,
            m,
            "argv differs for {line:?}: bash={} mix={}",
            show(&b),
            show(&m)
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 1..4 words after the sentinel: the bracketed argv must be identical from
    /// bash and from `mix -c`.
    #[test]
    fn word_splitting_and_quoting_match_bash(words in prop::collection::vec(word(), 1..5)) {
        if !tools_present() {
            return Ok(()); // portability: silently skip where bash/printf are absent
        }
        let (bash_out, mix_out, line) = diff(&words.join(" "));
        // Skip infra exec failures (see stdout_of): None either side is a
        // could-not-exec race, not a word-splitting divergence.
        if bash_out.is_none() || mix_out.is_none() {
            return Ok(());
        }
        prop_assert_eq!(
            &bash_out,
            &mix_out,
            "argv differs for line {:?}:\n  bash={}\n  mix ={}",
            line,
            show(&bash_out),
            show(&mix_out),
        );
    }
}
