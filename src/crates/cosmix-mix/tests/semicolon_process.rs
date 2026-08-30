//! Binary-level classifier and `-c` regression tests for Mix `;`.

use std::process::Command;
use std::{fs, path::PathBuf};

fn run(body: &str) -> (Option<i32>, String, String) {
    let mix = env!("CARGO_BIN_EXE_mix");
    let out = Command::new(mix)
        .arg("-c")
        .arg(body)
        .env("MIX_STATS", "off")
        .output()
        .expect("spawn mix");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn sigil_led_semicolon_sequence_is_mix() {
    let (code, stdout, stderr) = run("$a = 1; $b = 2; print($a + $b)");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "3\n");
}

#[test]
fn known_external_semicolon_list_stays_shell() {
    let (code, stdout, stderr) = run("printf one; printf two");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "onetwo");
}

#[test]
fn external_and_or_chains_stay_shell() {
    let (code, stdout, stderr) = run("/usr/bin/false || /usr/bin/printf fallback");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "fallback");

    let (code, stdout, stderr) = run("/usr/bin/printf left && /usr/bin/printf right");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "leftright");
}

#[test]
fn external_pipeline_stays_shell() {
    let (code, stdout, stderr) = run("/usr/bin/printf x | /usr/bin/tr x y");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "y");
}

#[test]
fn assignment_chains_surface_the_mix_parse_error_under_dash_c() {
    for operator in ["&&", "||"] {
        let (code, stdout, stderr) = run(&format!("$ok = true {operator} false"));
        assert_eq!(code, Some(1), "stderr={stderr}");
        assert!(stdout.is_empty(), "stdout={stdout:?}");
        assert!(
            stderr.contains(&format!("assignment cannot be chained with `{operator}`")),
            "stderr={stderr}"
        );
    }

    let (code, stdout, stderr) = run("print(\"GATE\") && $x = false || print(\"FALLBACK\")");
    assert_eq!(code, Some(1), "stderr={stderr}");
    assert!(
        stdout.is_empty(),
        "nothing may run before parse failure: {stdout:?}"
    );
    assert!(
        stderr.contains("assignment cannot be chained with `&&`"),
        "stderr={stderr}"
    );
}

/// A command-like head that is NOT on PATH reaches `classify_input`'s
/// last-resort `head_is_command_like` tie-break, a different shell-fallback
/// branch from the `$`/keyword path above. Without the typed-error guard there
/// the whole line reclassifies as a shell chain: the head fails "command not
/// found", `&&` short-circuits past the assignment, `||` runs the tail, and the
/// process exits 0 — the assignment silently vanishing into a green exit.
#[test]
fn unknown_command_like_head_does_not_shell_fallback_past_an_assignment_chain() {
    for head in ["nil", "zqxfoo"] {
        let (code, stdout, stderr) = run(&format!(
            "{head} && $x = false || /usr/bin/printf FALSE_GREEN"
        ));
        assert_eq!(code, Some(1), "{head}: stderr={stderr}");
        assert!(
            !stdout.contains("FALSE_GREEN"),
            "{head}: tail must not run: {stdout:?}"
        );
        assert!(
            stderr.contains("assignment cannot be chained with `&&`"),
            "{head}: stderr={stderr}"
        );
    }
}

#[test]
fn old_assignment_error_prose_in_source_text_does_not_block_shell_fallback() {
    let (code, stdout, stderr) =
        run("sh \"\" \"assignment cannot be chained with\"; /usr/bin/printf SHOULD_RUN");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "SHOULD_RUN");
}

#[test]
fn unknown_command_like_semicolon_list_stays_shell() {
    let (code, _stdout, stderr) = run("zqxfoo; zqxbar");
    assert_eq!(code, Some(127));
    assert!(stderr.contains("zqxfoo"), "{stderr}");
    assert!(stderr.contains("zqxbar"), "{stderr}");
}

#[test]
fn mix_and_shell_cannot_be_hybridized_by_semicolon() {
    let (code, stdout, stderr) = run("print(1); echo hi");
    assert_eq!(code, Some(1));
    assert!(
        stdout.is_empty(),
        "whole line must fail before print: {stdout:?}"
    );
    assert!(!stdout.contains("hi"));
    assert!(stderr.contains("Parse error"), "{stderr}");
}

#[test]
fn leading_comment_still_owns_the_whole_c_argument() {
    let (code, stdout, stderr) = run("# header; print(1)");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert!(stdout.is_empty());
}

#[test]
fn semicolon_terminates_mix_pipeline_tail() {
    let (code, stdout, stderr) = run("print(\"x\") | tr x y; print(\"z\")");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "y\nz\n");
}

#[test]
fn source_shell_fallback_still_executes_command_lists_with_arguments() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-semicolon-source-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, "printf source-one; printf source-two\n").unwrap();
    let source = format!("source {:?}", path.to_string_lossy());
    let (code, stdout, stderr) = run(&source);
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "source-onesource-two");
}

#[test]
fn sourced_assignment_chain_is_a_parse_error_not_a_shell_fallback() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-assignment-chain-source-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, "$x = true || /usr/bin/printf SOURCED_FALSE_GREEN\n").unwrap();
    let source = format!("source {:?}", path.to_string_lossy());
    let (code, stdout, stderr) = run(&source);
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(1), "stderr={stderr}");
    assert!(stdout.is_empty(), "shell fallback ran: {stdout:?}");
    assert!(
        stderr.contains("assignment cannot be chained with `||`"),
        "stderr={stderr}"
    );
}

#[test]
fn source_pure_mix_file_whole_parses_semicolon_statements() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-semicolon-pure-source-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, "$a = 20; $b = 22; print($a + $b)\n").unwrap();
    let source = format!("source {:?}", path.to_string_lossy());
    let (code, stdout, stderr) = run(&source);
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "42\n");
}

/// A `source`d line whose unknown command-like head carries a top-level `;`
/// hits the semicolon-list preservation guard, which classified it as shell
/// BEFORE consulting the parser. That let `zqxfoo; $x = false || cmd` run
/// `cmd` and source clean at exit 0 — the same false-green, one indirection
/// out. `mix -c` never had the hole because there the guard sits on the
/// parser's success arm.
#[test]
fn sourced_semicolon_list_does_not_shell_fallback_past_an_assignment_chain() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-semicolon-assign-chain-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, "zqxfoo; $x = false || /usr/bin/printf FALSE_GREEN\n").unwrap();
    let (code, stdout, stderr) = run(&format!("source {:?}", path.to_string_lossy()));
    let _ = fs::remove_file(&path);

    assert_eq!(code, Some(1), "stderr={stderr}");
    assert!(
        !stdout.contains("FALSE_GREEN"),
        "the `||` tail must not run: {stdout:?}"
    );
    assert!(
        stderr.contains("assignment cannot be chained with `||`"),
        "stderr={stderr}"
    );
}

/// The deliberate boundary of the assignment-chain rule, pinned because
/// `mix man syntax` now asserts it: a line Mix cannot parse AT ALL is
/// classified shell, so `$x = false` is three shell words, not an assignment — there is no Mix
/// assignment to discard, the failed `=` command is diagnosed on stderr,
/// and `||` fires. `bash` treats the same line the same way — it reports the
/// failed `=` command and runs the tail at exit 0. (The wording of the two
/// diagnostics differs; the semantics do not.)
///
/// This boundary is FORCED, not chosen: the typed assignment-chain error
/// cannot exist here. Ask the parser for `/usr/bin/true && $x = false ||
/// cmd` and it dies at `unexpected token Slash` — the head is not Mix, so
/// the parse never reaches the assignment. There is no Mix assignment being
/// discarded, only shell words. The plausible wrong fix is a text heuristic
/// (`work.contains(" = ")`); that mutation is what this test kills.
#[test]
fn shell_classified_heads_keep_bash_semantics_for_spaced_assignment_words() {
    // Both heads start with a path separator, so the Mix parse dies at
    // `unexpected token Slash` before reaching the assignment. A head that IS
    // parseable Mix (`cd /tmp`, `true`) rejects instead — see
    // `bare_path_head_that_is_also_valid_mix_rejects_an_assignment_chain`.
    for head in ["/usr/bin/true", "/usr/bin/env true"] {
        let (code, stdout, stderr) = run(&format!(
            "{head} && $x = false || /usr/bin/printf SHELL_TAIL"
        ));
        assert_eq!(code, Some(0), "{head}: stderr={stderr}");
        assert_eq!(
            stdout, "SHELL_TAIL",
            "{head}: tail must run under shell rules"
        );
        assert!(
            stderr.contains("=:"),
            "{head}: the failed `=` command must be diagnosed, not silent: {stderr:?}"
        );
    }
}

/// The other two unparseable-as-Mix shapes `mix man syntax` now names. A
/// shell redirect and an env prefix each break the Mix parse *before* the
/// assignment (`unexpected token Slash`, `expected end of statement, got
/// Assign`), so no assignment-chain error exists to raise and the line stays
/// a shell command list. Documented deliberately, because adding a redirect
/// to an otherwise-rejected line silently changes which language reads it.
#[test]
fn shapes_that_break_the_mix_parse_early_stay_shell_command_lists() {
    for line in [
        "zqxfoo > /dev/null; $x = false || /usr/bin/printf SHELL_TAIL",
        "FOO=bar export x = false || /usr/bin/printf SHELL_TAIL",
    ] {
        let (code, stdout, stderr) = run(line);
        assert_eq!(code, Some(0), "{line}: stderr={stderr}");
        assert_eq!(
            stdout, "SHELL_TAIL",
            "{line}: tail must run under shell rules"
        );
    }
}

/// A tight-hyphenated head (`cosmix-comp --nested`) is a shell discriminator
/// that fires BEFORE any Mix parse, so this line once ran its tail under `-c`
/// while the identical line in a sourced file was rejected. That asymmetry was
/// a bug, not a contract: the parser can type the assignment-chain error here,
/// and once it has, no head shape may reclassify the line as shell. Both modes
/// must now reject. The hyphen discriminator itself is unaffected — see
/// `tight_hyphenated_head_without_an_assignment_stays_shell`.
#[test]
fn tight_hyphenated_head_rejects_an_assignment_chain_in_both_modes() {
    let line = "zqx-foo && $x = false || /usr/bin/printf SHELL_TAIL";

    let (code, stdout, stderr) = run(line);
    assert_eq!(code, Some(1), "-c must reject: stderr={stderr}");
    assert!(!stdout.contains("SHELL_TAIL"), "-c tail ran: {stdout:?}");
    assert!(
        stderr.contains("assignment cannot be chained with `&&`"),
        "stderr={stderr}"
    );

    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-tight-hyphen-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, format!("{line}\n")).unwrap();
    let (code, stdout, stderr) = run(&format!("source {:?}", path.to_string_lossy()));
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(1), "sourced must reject: stderr={stderr}");
    assert!(
        !stdout.contains("SHELL_TAIL"),
        "sourced tail ran: {stdout:?}"
    );
    assert!(
        stderr.contains("assignment cannot be chained with `&&`"),
        "stderr={stderr}"
    );
}

/// The head that disproved "can Mix parse the line?" as the boundary. `true`
/// is BOTH a PATH executable and a valid Mix literal, so the shell-first
/// return fired while the parser could see and type the assignment-chain
/// error — `-c` ran the tail at exit 0 with the assignment silently gone.
/// This is the exact bug class the rule exists to kill, so it must reject
/// wherever a classifier is involved.
#[test]
fn bare_path_head_that_is_also_valid_mix_rejects_an_assignment_chain() {
    let line = "true && $x = false || /usr/bin/printf SHELL_TAIL";

    let (code, stdout, stderr) = run(line);
    assert_eq!(code, Some(1), "-c must reject: stderr={stderr}");
    assert!(!stdout.contains("SHELL_TAIL"), "-c tail ran: {stdout:?}");

    let path: PathBuf = std::env::temp_dir().join(format!(
        "mix-bare-path-head-{}-{}.mix",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, format!("{line}\n")).unwrap();
    let (code, stdout, stderr) = run(&format!("source {:?}", path.to_string_lossy()));
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(1), "sourced must reject: stderr={stderr}");
    assert!(
        !stdout.contains("SHELL_TAIL"),
        "sourced tail ran: {stdout:?}"
    );
}

/// The guard above must not cost the shell its head discriminators. A
/// tight-hyphenated head with no assignment still routes to the shell (it
/// would otherwise parse as the subtraction `"cosmix" - "comp"`), and a bare
/// PATH head still chains under shell rules.
///
/// The head must be a name that CANNOT exist on PATH. This test used to say
/// `cosmix-comp`, which stopped being hypothetical the day cosmix-comp was
/// installed to /opt/cosmix/bin: `cargo test --workspace` then launched the
/// real nested compositor on the developer's desktop, sat there for two
/// minutes, and failed with a wgpu `DeviceLost` instead of the expected 127.
/// What is under test is the *routing* of a tight-hyphenated head, and a name
/// nothing can resolve proves that strictly better than a real binary would.
#[test]
fn tight_hyphenated_head_without_an_assignment_stays_shell() {
    let head = "cosmix-comp-not-a-real-binary-8f3a1c";
    let (code, _stdout, stderr) = run(&format!("{head} --nested"));
    assert_eq!(
        code,
        Some(127),
        "must be a shell not-found: stderr={stderr}"
    );
    assert!(stderr.contains(head), "stderr={stderr}");

    let (code, stdout, stderr) = run("true && /usr/bin/printf CHAIN_OK");
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "CHAIN_OK");
}

/// The classifier probes every `&&`/`||` line with a *speculative* parse whose
/// `Ok` result it may discard. That probe must be diagnostic-silent. Before it
/// was, a line that went on to run as **shell** inherited an irrelevant Mix
/// deprecation warning, and a line that really was Mix got the warning
/// **twice** — once from the probe, once from the authoritative parse that
/// produces the executed statements. Over-silencing is the opposite failure,
/// so the last two cases pin that a genuine deprecation still reaches stderr
/// exactly once.
#[test]
fn speculative_classifier_probes_emit_no_parser_diagnostics() {
    let deprecations = |stderr: &str| stderr.matches("is deprecated").count();

    // Probed because of the `&&`, then routed to the shell by the head rules:
    // no Mix diagnostic may survive that round trip.
    let (_, _, stderr) = run("true && while false done");
    assert_eq!(deprecations(&stderr), 0, "stderr={stderr}");

    // Genuinely Mix, so it is parsed twice — and must still warn once.
    let (_, _, stderr) = run("nil && while false done");
    assert_eq!(deprecations(&stderr), 1, "stderr={stderr}");

    // No chain, so no probe: the ordinary deprecation path is untouched.
    let (_, _, stderr) = run("while false\ndone");
    assert_eq!(deprecations(&stderr), 1, "stderr={stderr}");
}

/// The `source` path parses a file up to three times: a provisional
/// whole-file attempt, a per-line routing probe, and the per-line parse that
/// actually executes. Only the parse that *wins* may report a deprecation. A
/// file that falls back to per-line handling used to print the same warning
/// three times; a file that parses whole must still print it once, which is
/// why the provisional parse buffers its diagnostics and flushes them on
/// success rather than simply going quiet.
#[test]
fn sourced_deprecations_are_reported_once_per_winning_parse() {
    let deprecations = |stderr: &str| stderr.matches("is deprecated").count();
    let write = |name: &str, body: &str| -> PathBuf {
        let path: PathBuf = std::env::temp_dir().join(format!(
            "mix-deprecation-{}-{}-{}.mix",
            name,
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, body).unwrap();
        path
    };

    // Falls back to per-line handling (the bareword path is not Mix), so the
    // provisional whole-file attempt is discarded along with its warning.
    let path = write("fallback", "while false done\n/usr/bin/true\n");
    let (_, _, stderr) = run(&format!("source {:?}", path.to_string_lossy()));
    let _ = fs::remove_file(&path);
    assert_eq!(deprecations(&stderr), 1, "stderr={stderr}");

    // Parses whole, so the provisional parse becomes the executed one and
    // must flush the deprecation it withheld.
    let path = write("whole", "while false\ndone\nprint(\"WHOLE\")\n");
    let (code, stdout, stderr) = run(&format!("source {:?}", path.to_string_lossy()));
    let _ = fs::remove_file(&path);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "WHOLE\n");
    assert_eq!(deprecations(&stderr), 1, "stderr={stderr}");
}
