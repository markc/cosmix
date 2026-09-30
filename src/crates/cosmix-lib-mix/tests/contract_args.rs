//! A2 (TODO-mix 2026-09-24): contract argument TYPE checks — always-on
//! for the side-effect classes (fs/process/network/bus), strict-mode-only
//! for pure builtins in the first release. The 0.93-era side effects were
//! verified then: mkdir(42) created a directory named `42`,
//! write_file(99, "x") wrote a file named `99`.

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(src: &str, strict: bool) -> Result<String, String> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    if strict {
        eval.set_arity_mode(cosmix_mix::ArityMode::Strict);
    }
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

#[tokio::test]
async fn critical_class_types_gate_in_every_mode() {
    // exists(path: string) — FsRead — gates in BOTH modes: a number path
    // is a probe on the wrong target. The TYPE_MISMATCH code rides the
    // structured error (catch $m, $e → $e.code); the plain message names
    // the argument, the shape and the actual type.
    for strict in [false, true] {
        let err = run("print(exists(42))", strict)
            .await
            .expect_err("number path must raise");
        assert!(err.contains("must be string"), "mode {strict}: got: {err}");
        assert!(err.contains("got number"), "mode {strict}: got: {err}");
    }
}

#[tokio::test]
async fn pure_builtin_types_gate_only_under_strict_mode() {
    // len(v) — Pure — gates under strict mode only in this first
    // release; in compatible mode the gate stays silent and the call
    // falls through to len's own behavior (which refuses a number with
    // ITS message, not the contract gate's).
    let err = run("print(len(3))", true)
        .await
        .expect_err("strict mode gates pure types");
    assert!(err.contains("must be string | list | map | bytes | buffer"), "got: {err}");
    let err = run("print(len(3))", false)
        .await
        .expect_err("len(3) still fails — but on len's own terms");
    assert!(
        !err.contains("must be string | list | map | bytes | buffer"),
        "the contract gate must not fire in compatible mode: {err}"
    );
}

#[tokio::test]
async fn well_typed_calls_pass_in_both_modes() {
    for strict in [false, true] {
        run("print(exists(\".\"))", strict)
            .await
            .expect("string path passes");
        run("print(len([1, 2]))", strict)
            .await
            .expect("list arg passes");
    }
}

#[tokio::test]
async fn borrowed_write_payloads_propagate_filesystem_errors() {
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let scratch = std::env::temp_dir().join(format!("mix-payload-errors-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&scratch).expect("unique scratch directory");
    let directory = scratch.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let existing = scratch.join("existing");
    std::fs::write(&existing, b"preserved").unwrap();
    for strict in [false, true] {
        for builtin in ["write_file", "append_file"] {
            let src = format!("{builtin}({}, \"payload\")", serde_json::to_string(&directory.to_string_lossy()).unwrap());
            let err = run(&src, strict).await.expect_err("directory cannot accept file payload");
            assert!(err.contains(builtin), "filesystem error lost: {err}");
        }
        let src = format!("write_new({}, \"payload\", 0o600)", serde_json::to_string(&existing.to_string_lossy()).unwrap());
        let err = run(&src, strict).await.expect_err("existing file cannot be claimed");
        assert!(err.contains("write_new"), "filesystem error lost: {err}");
        assert_eq!(std::fs::read(&existing).unwrap(), b"preserved");
    }
    std::fs::remove_dir_all(scratch).unwrap();
}

#[tokio::test]
async fn write_payload_types_refuse_with_encode_hint_in_every_mode() {
    // A3a: write_file/append_file/write_new take string/bytes/buffer ONLY.
    // Every rejected payload — including explicit nil, which the gate
    // refuses like any other wrong type (the blanket nil skip is gone) —
    // raises a structured TYPE_MISMATCH whose message teaches encode-first,
    // in BOTH modes, before any file is created or touched. The hint is
    // declared on the contract (`hints[...]`) so the gate carries it too.
    let payloads: &[(&str, &str)] = &[
        ("nil", "nil"),
        ("42", "number"),
        ("true", "bool"),
        ("[1]", "list"),
        ("{}", "map"),
        ("fn($x) = $x", "function"),
    ];
    for strict in [false, true] {
        for (expr, what) in payloads {
            // Absent path: the refusal must not create a file.
            for (call, path) in [
                (
                    format!("write_file(\"/tmp/mix-a3a-wf\", {expr})"),
                    "/tmp/mix-a3a-wf",
                ),
                (
                    format!("append_file(\"/tmp/mix-a3a-af\", {expr})"),
                    "/tmp/mix-a3a-af",
                ),
                (
                    format!("write_new(\"/tmp/mix-a3a-wn\", {expr}, 0o600)"),
                    "/tmp/mix-a3a-wn",
                ),
            ] {
                std::fs::remove_file(path).ok();
                let err = run(&format!("try\n {call}\ncatch $m, $e\n print($e.code .. \" | \" .. $e.message)\nend"), strict).await.expect("catchable payload refusal");
                assert!(err.contains("TYPE_MISMATCH"), "mode {strict}, {call}: {err}");
                assert!(
                    err.contains("encode it first"),
                    "encode-first hint missing (mode {strict}, {call}, {what}): {err}"
                );
                assert!(
                    !std::path::Path::new(path).exists(),
                    "mode {strict}, {call}: a wrong payload must not create a file"
                );
            }
            // Existing path: the refusal must leave the contents alone
            // (for write_new the payload check runs before the O_EXCL
            // open, so the refusal is the payload's, not the path's).
            let live = "/tmp/mix-a3a-live";
            std::fs::write(live, "ORIGINAL").unwrap();
            for call in [
                format!("write_file({live:?}, {expr})"),
                format!("append_file({live:?}, {expr})"),
                format!("write_new({live:?}, {expr}, 0o600)"),
            ] {
                let err = run(&format!("try\n {call}\ncatch $m, $e\n print($e.code .. \" | \" .. $e.message)\nend"), strict).await.expect("catchable payload refusal");
                assert!(err.contains("TYPE_MISMATCH"), "mode {strict}, {call}: {err}");
                assert!(
                    err.contains("encode it first"),
                    "encode-first hint missing (mode {strict}, {call}, {what}): {err}"
                );
                assert_eq!(
                    std::fs::read_to_string(live).unwrap(),
                    "ORIGINAL",
                    "mode {strict}, {call}: existing contents changed"
                );
            }
            std::fs::remove_file(live).ok();
        }
    }
}

#[tokio::test]
async fn gate_errors_carry_argument_specific_guidance() {
    // A2 residual: the narrowed gate used to swallow the instructional
    // clauses that lived in the builtins behind a generic message. The
    // contract `hints[...]` seam restores them on the gate's own error.
    let err = run("kill(true)", false).await.expect_err("bool pid must raise");
    assert!(err.contains("entire group"), "kill pid hint: {err}");
    let err = run("kill(1234, \"SIGKILL\")", false)
        .await
        .expect_err("a signal name must raise");
    assert!(err.contains("signal number"), "kill signal hint: {err}");
    let err = run("hash_file(\"/dev/null\", {raw: true})", false)
        .await
        .expect_err("a map in the algo slot must raise");
    assert!(
        err.contains("options map placed one position early"),
        "hash_file algo hint: {err}"
    );
    let err = run("publish(\"t\", 42)", false)
        .await
        .expect_err("a non-string body must raise");
    assert!(err.contains("encode it first"), "publish body hint: {err}");
}

#[tokio::test]
async fn explicit_nil_sentinels_keep_working_in_every_mode() {
    // A6: these optional args treat an explicit nil as the omitted
    // sentinel at runtime, and the contracts declare it (`any_of(…, nil)`
    // in the registry). With the blanket nil skip REMOVED from the gate,
    // that declaration is the only thing that lets an explicit nil
    // through — this matrix is the runtime half of the registry
    // conformance test `declared_nil_sentinels_match_runtime_omission_…`.
    for strict in [false, true] {
        run("print(normalize(\"fi\", nil))", strict)
            .await
            .expect("nil form selects NFC");
        run("print(hash_file(\"/dev/null\", nil))", strict)
            .await
            .expect("nil algo selects sha256");
        run("print(exists(\"/\", nil))", strict)
            .await
            .expect("nil opts accepted");
        run("print(length(buffer(nil)))", strict)
            .await
            .expect("nil init = empty buffer");
        run("print(length(slice([1, 2, 3], 1, nil)))", strict)
            .await
            .expect("nil end = through the end");
        // Omission-family representatives beyond the original six:
        // fs (stat/walk/read_jsonl), process (run_argv), digest options
        // (hash_sha256 — pinned by scripts/digests.mix under the strict
        // default), and bytes_to_string (nil opts = strict decode).
        run("$s = stat(\"/\", nil)\nprint($s.is_dir)", strict)
            .await
            .expect("stat nil opts accepted");
        run("print(length(walk(\".\", nil)) > 0)", strict)
            .await
            .expect("walk nil opts accepted");
        run("print(hash_sha256(\"abc\", nil) == hash_sha256(\"abc\"))", strict)
            .await
            .expect("nil digest opts = hex, not a different encoding");
        run("print(bytes_to_string(string_to_bytes(\"ok\"), nil))", strict)
            .await
            .expect("nil opts = strict decode, ASCII passes");
        // Process opts nil must not start a service or hang: `true` exits
        // immediately and the result map is discarded with must_use intact
        // only because print() consumes it.
        run("$r = run_argv([\"true\"], nil)\nprint($r.ok)", strict)
            .await
            .expect("run_argv nil opts = defaults");
        // read_jsonl needs one real file: a temp file, nothing external.
        let tmp = std::env::temp_dir().join(format!(
            "mix-a6-jsonl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&tmp, "{\"a\": 1}\n").expect("temp jsonl");
        let src = format!(
            "print(read_jsonl({:?}, nil)[0].a)",
            tmp.to_string_lossy()
        );
        let out = run(&src, strict)
            .await
            .unwrap_or_else(|e| panic!("mode {strict}: read_jsonl nil opts must work: {e}"));
        assert!(out.contains('1'), "mode {strict}: {out}");
        std::fs::remove_file(&tmp).ok();
    }
}

#[tokio::test]
async fn required_nil_filesystem_targets_refuse_in_every_mode() {
    // A6 safety half: a nil in a REQUIRED string path slot must never be
    // coerced into a literal "nil" target. write_file(nil, …), mkdir(nil),
    // remove(nil) and remove_dir(nil) all raise a structured, catchable
    // TYPE_MISMATCH from the contract gate (FsWrite is a critical class —
    // checked in BOTH modes) BEFORE the builtin runs, so no file or
    // directory named "nil" is ever created or deleted. The process cwd
    // listing must be unchanged afterwards — the test never cleans up a
    // "nil" entry itself, so a real one elsewhere is never touched.
    let cwd_snapshot = || {
        let mut names: Vec<String> = std::fs::read_dir(".")
            .expect("cwd readable")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    for strict in [false, true] {
        let before = cwd_snapshot();
        for call in [
            "write_file(nil, \"x\")",
            "write_file(\"/tmp/mix-a6-wf\", nil)",
            "mkdir(nil)",
            "remove(nil)",
            "remove_dir(nil)",
        ] {
            let out = run(&format!(
                "try\n  {call}\ncatch $m, $e\n  print($e.code .. \" | \" .. $e.message)\nend\n"
            ), strict)
            .await
            .unwrap_or_else(|e| panic!("mode {strict}, {call}: must be catchable, got {e}"));
            assert!(
                out.contains("TYPE_MISMATCH"),
                "mode {strict}, {call}: structured refusal missing: {out:?}"
            );
            assert!(
                out.contains("got nil"),
                "mode {strict}, {call}: the message must name nil: {out:?}"
            );
        }
        assert_eq!(
            before,
            cwd_snapshot(),
            "mode {strict}: a refused nil target must leave the cwd untouched"
        );
    }
    // The /tmp witness from the payload call above must not exist either.
    std::fs::remove_file("/tmp/mix-a6-wf").ok();
}

#[tokio::test]
async fn forbidden_nil_is_consistent_in_both_modes() {
    // The explicit-nil refusals the BUILTIN owns stay mode-independent
    // even where the gate does not reach (pure builtins under compatible
    // mode): csv_parse's delim nil and bytes_to_string's {lossy: nil}
    // FIELD both raise structured TYPE_MISMATCH in BOTH modes. Under
    // strict mode the gate also joins in for csv_parse's delim, carrying
    // the contract's one-ASCII-byte hint on its own message.
    for strict in [false, true] {
        let out = run(
            "try\n  csv_parse(\"a,b\\n1,2\", nil)\ncatch $m, $e\n  print($e.code)\nend\n",
            strict,
        )
        .await
        .unwrap_or_else(|e| panic!("mode {strict}: csv nil delim must be catchable: {e}"));
        assert!(out.contains("TYPE_MISMATCH"), "mode {strict}: {out:?}");
        let out = run(
            "try\n  bytes_to_string(string_to_bytes(\"x\"), {lossy: nil})\ncatch $m, $e\n  print($e.code)\nend\n",
            strict,
        )
        .await
        .unwrap_or_else(|e| panic!("mode {strict}: lossy nil field must be catchable: {e}"));
        assert!(out.contains("TYPE_MISMATCH"), "mode {strict}: {out:?}");
    }
    // Strict mode names the declared delim hint on the gate's own refusal.
    let err = run("print(csv_parse(\"a,b\", nil))", true)
        .await
        .expect_err("strict gate refuses the nil delim");
    assert!(err.contains("ASCII"), "delim hint missing: {err}");
}

#[tokio::test]
async fn bool_option_fields_refuse_truthy_in_every_mode() {
    // A4: a string "false" is a wrong value for a boolean option, not a
    // truthiness. The gate only checks the options argument is a map (shallow),
    // so the builtin owns this refusal — and it is mode-independent.
    for strict in [false, true] {
        let err = run("print(exists(\".\", {follow_symlinks: \"false\"}))", strict)
            .await
            .expect_err("string bool option must raise");
        assert!(err.contains("must be a bool"), "mode {strict}: {err}");
    }
}
