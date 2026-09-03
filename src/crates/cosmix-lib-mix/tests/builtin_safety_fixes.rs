//! Safety-fix regressions for builtins: substr slice-panic, sql_quote
//! backslash/NUL escaping, allocation caps (repeat/lpad/rpad/range),
//! and template's single-brace placeholder contract. Each test drives
//! Mix source through the public eval API (same harness as
//! tests/integration.rs) and asserts a clean value or a normal,
//! catchable Mix runtime error — never a panic.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

/// Parse + run `source`, returning Ok(stdout) or Err(error message).
async fn run_mix(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    run_mix(source)
        .await
        .unwrap_or_else(|e| panic!("script failed: {e}\nsource:\n{source}"))
        .trim_end()
        .to_string()
}

async fn run_err(source: &str) -> String {
    run_mix(source)
        .await
        .expect_err("script must return a runtime error, not succeed")
}

// ---------------------------------------------------------------------------
// FIX 1 — substr: no panic for ANY numeric args
// ---------------------------------------------------------------------------

/// The live-confirmed crash: a huge `len` used to wrap `start + len`
/// past usize::MAX into a start>end slice panic.
#[tokio::test]
async fn substr_huge_len_no_panic() {
    let out = run_ok(r#"print(substr("hello world abc foo", 10, 100000000000000000000))"#).await;
    assert_eq!(out, "d abc foo");
}

/// 2-arg form with start past the end used to underflow `chars.len() - start`.
#[tokio::test]
async fn substr_start_past_end_two_arg() {
    let out = run_ok(r#"print("[" .. substr("abc", 10) .. "]")"#).await;
    assert_eq!(out, "[]");
}

/// 3-arg form with start past the end clamps to empty.
#[tokio::test]
async fn substr_start_past_end_three_arg() {
    let out = run_ok(r#"print("[" .. substr("abc", 10, 5) .. "]")"#).await;
    assert_eq!(out, "[]");
}

/// Negative args saturate to 0 (start) / 0 (len) — never panic.
#[tokio::test]
async fn substr_negative_args() {
    let out = run_ok(r#"print(substr("abc", -5, 2))"#).await;
    assert_eq!(out, "ab");
    let out = run_ok(r#"print("[" .. substr("abc", 1, -2) .. "]")"#).await;
    assert_eq!(out, "[]");
}

/// Huge start AND huge len together (both saturate to usize::MAX).
#[tokio::test]
async fn substr_huge_start_and_len() {
    let out =
        run_ok(r#"print("[" .. substr("abc", 9999999999999999999999999999999999, 9999999999999999999999999999999999) .. "]")"#).await;
    assert_eq!(out, "[]");
}

/// Normal usage is unchanged.
#[tokio::test]
async fn substr_normal_unchanged() {
    let out = run_ok(r#"print(substr("hello world", 6, 5))"#).await;
    assert_eq!(out, "world");
    let out = run_ok(r#"print(substr("hello world", 6))"#).await;
    assert_eq!(out, "world");
}

// ---------------------------------------------------------------------------
// FIX 2 — sql_quote: backslash escaped, quotes doubled, NUL stripped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sql_quote_escapes_backslash_and_quote() {
    // Input a\b'c → a\\b''c (backslash escaped for MySQL/MariaDB
    // default mode, quote doubled).
    let out = run_ok(r#"print(sql_quote("a\\b'c"))"#).await;
    assert_eq!(out, r#"a\\b''c"#);
}

/// The classic MySQL bypass payload `\'` must come out fully neutral:
/// escaped backslash + doubled quote, leaving no live quote.
#[tokio::test]
async fn sql_quote_mysql_bypass_payload() {
    let out = run_ok(r#"print(sql_quote("\\' OR 1=1 --"))"#).await;
    assert_eq!(out, r#"\\'' OR 1=1 --"#);
}

#[tokio::test]
async fn sql_quote_strips_nul() {
    let out = run_ok(r#"print(sql_quote("a\u{0}b"))"#).await;
    assert_eq!(out, "ab");
}

#[tokio::test]
async fn sql_quote_plain_passthrough() {
    let out = run_ok(r#"print(sql_quote("it's 100% _done_"))"#).await;
    assert_eq!(out, "it''s 100% _done_");
}

// ---------------------------------------------------------------------------
// FIX 3 — allocation caps: repeat / lpad / rpad / range
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeat_over_cap_errors() {
    let err = run_err(r#"$x = repeat("x", 1000000000000)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
}

/// A count that saturates the f64→usize cast to usize::MAX must hit
/// the cap error too (checked_mul, no wrap).
#[tokio::test]
async fn repeat_astronomical_count_errors() {
    let err = run_err(r#"$x = repeat("ab", 999999999999999999999999999999999999)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
}

#[tokio::test]
async fn repeat_normal_and_edge_ok() {
    let out = run_ok(r#"print(repeat("ab", 3))"#).await;
    assert_eq!(out, "ababab");
    // Negative/zero counts → empty string, no error.
    let out = run_ok(r#"print("[" .. repeat("ab", -4) .. "]")"#).await;
    assert_eq!(out, "[]");
    // Empty string repeated a huge number of times is still empty (0 bytes).
    let out =
        run_ok(r#"print("[" .. repeat("", 999999999999999999999999999999999999) .. "]")"#).await;
    assert_eq!(out, "[]");
}

#[tokio::test]
async fn lpad_rpad_over_cap_error() {
    let err = run_err(r#"$x = lpad("x", 999999999999999999)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    let err = run_err(r#"$x = rpad("x", 999999999999999999)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    // The display-cell twins share the same hazard and cap.
    let err = run_err(r#"$x = lpad_w("x", 999999999999999999)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    let err = run_err(r#"$x = rpad_w("x", 999999999999999999)"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
}

/// A negative bound huge enough to SATURATE the f64→i64 cast to i64::MIN
/// must clamp like any other negative — `-(i64::MIN)` overflows (debug
/// panic, release wraparound), which is why every negative-index magnitude
/// goes through `neg_index_magnitude`. This pin exists because re-inlining
/// `(-n) as usize` at any of the take/drop sites passed the ENTIRE suite
/// while panicking on this one line (0.59.0 review round 2).
#[tokio::test]
async fn saturating_negative_bounds_clamp_without_panic() {
    let out = run_ok(r#"print(length(take([1,2,3], -10000000000000000000000000)))"#).await;
    assert_eq!(out, "3");
    let out = run_ok(r#"print(length(drop([1,2,3], -10000000000000000000000000)))"#).await;
    assert_eq!(out, "0");
    let out = run_ok(
        r#"print(length(slice([1,2,3], -10000000000000000000000000, 10000000000000000000000000)))"#,
    )
    .await;
    assert_eq!(out, "3");
    // String arms share the same negation sites.
    let out = run_ok(r#"print("[" .. take("abc", -10000000000000000000000000) .. "]")"#).await;
    assert_eq!(out, "[abc]");
    let out = run_ok(r#"print("[" .. drop("abc", -10000000000000000000000000) .. "]")"#).await;
    assert_eq!(out, "[]");
}

/// The cap bounds OUTPUT BYTES, not codepoint/cell width: a multibyte fill
/// at a width that passes the width-only guard used to build up to 4x the
/// advertised 256 MiB (found in the M-1 scope review, pre-existing on main).
#[tokio::test]
async fn pad_multibyte_fill_hits_byte_cap() {
    // 100M codepoints < the 268_435_456 width guard, but x4 bytes = 400 MB.
    let err = run_err(r#"$x = lpad("", 100000000, "🦀")"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    let err = run_err(r#"$x = rpad("", 100000000, "🦀")"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    // _w fills are 1 display cell but may still be multibyte: é is 2 bytes.
    let err = run_err(r#"$x = lpad_w("", 150000000, "é")"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    let err = run_err(r#"$x = rpad_w("", 150000000, "é")"#).await;
    assert!(err.contains("256 MiB cap"), "got: {err}");
    // Modest multibyte padding still works, and the original string rides
    // along untouched.
    let out = run_ok(r#"print(lpad("x", 5, "é"))"#).await;
    assert_eq!(out, "ééééx");
}

#[tokio::test]
async fn lpad_rpad_normal_ok() {
    let out = run_ok(r#"print(lpad("ab", 5) .. "|")"#).await;
    assert_eq!(out, "   ab|");
    let out = run_ok(r#"print(rpad("ab", 5) .. "|")"#).await;
    assert_eq!(out, "ab   |");
}

#[tokio::test]
async fn range_over_cap_errors() {
    let err = run_err(r#"$x = range(0, 1000000000000000000)"#).await;
    assert!(err.contains("cap 10000000"), "got: {err}");
    // Descending direction hits the same cap.
    let err = run_err(r#"$x = range(1000000000000000000, 0, -1)"#).await;
    assert!(err.contains("cap 10000000"), "got: {err}");
}

#[tokio::test]
async fn range_normal_ok() {
    let out = run_ok(r#"print(join(range(1, 5), ","))"#).await;
    assert_eq!(out, "1,2,3,4,5");
    let out = run_ok(r#"print(length(range(5, 1, -2)))"#).await;
    assert_eq!(out, "3");
    // Empty range (start past end) is fine, not an error.
    let out = run_ok(r#"print(length(range(5, 1)))"#).await;
    assert_eq!(out, "0");
    // Direction opposes step → empty, even when the gap is smaller than the
    // step (the count formula must not truncate a negative distance to 1).
    let out = run_ok(r#"print(length(range(5, 1, 10)))"#).await;
    assert_eq!(out, "0");
    let out = run_ok(r#"print(length(range(5, 4, 10)))"#).await;
    assert_eq!(out, "0");
    let out = run_ok(r#"print(length(range(1, 5, -10)))"#).await;
    assert_eq!(out, "0");
}

#[tokio::test]
async fn range_at_i64_extremes_no_overflow() {
    // Pre-0.59.0 a bound past i64 saturated on the `as i64` cast, so
    // range(1e30, 1e30) "succeeded" — length 1, but the ELEMENT was a
    // silently fabricated 9223372036854775807. The length-only assertion
    // here used to bless that corruption. Out-of-i64 bounds now refuse.
    let err =
        run_err(r#"$x = range(1000000000000000000000000000000, 1000000000000000000000000000000)"#)
            .await;
    // The message now NAMES the bound (0.69.x) rather than saying "the
    // declared range" and leaving the reader to guess which one — so this
    // asserts the stronger property: the refusal states what WAS allowed.
    assert!(
        err.contains("range(): argument 1")
            && err.contains("must be a whole number in")
            && err.contains("..="),
        "got: {err}"
    );
    let err = run_err(r#"$x = range(0, -1000000000000000000000000000000)"#).await;
    assert!(err.contains("range(): argument 2"), "got: {err}");
    // The stride-overflow regression this test was born for, kept at the
    // true extreme: the largest whole f64 below 2^63 is a VALID bound, and
    // the post-loop stride at that magnitude must not overflow i64 (debug
    // panic / release wraparound). One element, value intact.
    let out = run_ok(
        r#"$r = range(9223372036854774784, 9223372036854774784)
print(length($r) .. ":" .. $r[0])"#,
    )
    .await;
    assert_eq!(out, "1:9223372036854774784");
    let out =
        run_ok(r#"print(length(range(-9223372036854774784, -9223372036854774784, -1)))"#).await;
    assert_eq!(out, "1");
}

// ---------------------------------------------------------------------------
// FIX 6 — template substitutes single-brace {key} (docs were wrong, not code)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn template_single_brace_works() {
    let out = run_ok(r#"print(template("hi {name}, you are {age}", {name: "bob", age: 7}))"#).await;
    assert_eq!(out, "hi bob, you are 7");
}

// ---------------------------------------------------------------------------
// 2026-07 audit batch 2 — builtins quick wins:
// json_encode is LOUD on non-finite numbers, is_number/to_number reject the
// "inf"/"nan" spellings, template is single-pass (values cannot inject
// placeholders), sqlexec binds params TYPED (nil → SQL NULL).
// ---------------------------------------------------------------------------

#[cfg(feature = "json")]
#[tokio::test]
async fn json_encode_non_finite_errors_loudly() {
    // NaN/±inf have no JSON representation — error, never a silent 0.
    let err = run_err(r#"print(json_encode({v: sqrt(-1)}))"#).await;
    assert!(
        err.contains("json_encode") && err.contains("non-finite"),
        "got: {err}"
    );
    let err = run_err(r#"print(json_encode([ln(0)]))"#).await;
    assert!(err.contains("non-finite"), "got: {err}");
    // Finite values still encode; the whole-number path is intact.
    let out = run_ok(r#"print(json_encode({a: 1.5, b: 2}))"#).await;
    assert_eq!(out, r#"{"a":1.5,"b":2}"#);
    // The error is catchable like any runtime error.
    let out =
        run_ok("try\n  $x = json_encode(sqrt(-1))\ncatch $e\n  print(\"caught\")\nend\n").await;
    assert_eq!(out, "caught");
}

#[tokio::test]
async fn is_number_and_to_number_reject_non_finite_spellings() {
    for s in ["inf", "-inf", "Infinity", "nan", "NaN", "1e999"] {
        let out = run_ok(&format!(r#"print(is_number("{s}"))"#)).await;
        assert_eq!(out, "false", "is_number({s:?}) must be false");
        let out = run_ok(&format!(r#"print(to_number("{s}") == nil)"#)).await;
        assert_eq!(out, "true", "to_number({s:?}) must be nil");
    }
    // Real numeric strings (incl. whitespace-padded and exponent) still pass,
    // and is_number now agrees with to_number on trimming.
    for s in ["5", " 5.5 ", "1e6", "-0.25"] {
        let out = run_ok(&format!(r#"print(is_number("{s}"))"#)).await;
        assert_eq!(out, "true", "is_number({s:?}) must be true");
    }
    // A NaN VALUE (not string) is still a Number — math propagates.
    let out = run_ok("print(is_number(sqrt(-1)))").await;
    assert_eq!(out, "true");
}

#[tokio::test]
async fn template_values_cannot_inject_placeholders() {
    // A substituted VALUE containing "{secret}" is emitted verbatim — the old
    // per-key replace loop rescanned it and leaked any other map key.
    let out =
        run_ok(r#"print(template("hi {name}", {name: "{secret}", secret: "hunter2"}))"#).await;
    assert_eq!(out, "hi {secret}");
    // Unknown keys and unterminated braces stay literal; a nested `{` only
    // opens a new scan (the inner {y} still substitutes).
    let out = run_ok(r#"print(template("a {nope} b { c {x{y}", {y: "Y"}))"#).await;
    assert_eq!(out, "a {nope} b { c {xY");
    // Order independence: substitution no longer depends on map iteration
    // order (the old loop could substitute an injected key if it iterated
    // later). Same map, keys reversed — same output.
    let out =
        run_ok(r#"print(template("hi {name}", {secret: "hunter2", name: "{secret}"}))"#).await;
    assert_eq!(out, "hi {secret}");
}

#[cfg(all(feature = "sqlite", feature = "json"))]
#[tokio::test]
async fn sqlexec_binds_params_typed_not_text() {
    // nil → NULL (was the 3-char TEXT "nil"), whole number → INTEGER,
    // fractional → REAL, bool → INTEGER 0/1, string → TEXT.
    let src = r#"
$db = sqlopen(":memory:", "rw")
sqlexec($db, "CREATE TABLE t (a, b, c, d, e)")
sqlexec($db, "INSERT INTO t VALUES (?, ?, ?, ?, ?)", [nil, 42, 1.5, "x", true])
$r = sqlexec($db, "SELECT (a IS NULL) AS an, typeof(b) AS tb, typeof(c) AS tc, typeof(d) AS td, e FROM t")
print(json_encode($r[0]))
sqlclose($db)
"#;
    let out = run_ok(src).await;
    assert_eq!(
        out,
        r#"{"an":1,"e":1,"tb":"integer","tc":"real","td":"text"}"#
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlexec_rejects_unbindable_params() {
    let src = r#"
$db = sqlopen(":memory:", "rw")
sqlexec($db, "CREATE TABLE t (a)")
sqlexec($db, "INSERT INTO t VALUES (?)", [[1, 2]])
"#;
    let err = run_err(src).await;
    assert!(err.contains("cannot bind"), "got: {err}");
}

#[cfg(feature = "json")]
#[tokio::test]
async fn json_encode_i64_boundary_takes_real_path() {
    // 2^63 == i64::MAX as f64 (rounds UP one past i64::MAX): the integer
    // fast path must NOT claim it — `as` would saturate to i64::MAX and
    // silently emit 9223372036854775807. It encodes as a real instead.
    let out = run_ok(r#"print(json_encode(9223372036854775808))"#).await;
    assert_ne!(out, "9223372036854775807");
    assert!(
        out.contains("e") || out.contains("."),
        "2^63 must encode as a JSON real, got: {out}"
    );
    // The largest exactly-representable f64 BELOW 2^63 still takes the
    // integer path, as does -2^63 (exactly representable, inclusive bound).
    let out = run_ok(r#"print(json_encode(9223372036854774784))"#).await;
    assert_eq!(out, "9223372036854774784");
    let out = run_ok(r#"print(json_encode(0 - 9223372036854775808))"#).await;
    assert_eq!(out, "-9223372036854775808");
}

// ---------------------------------------------------------------------------
// 2026-07 audit batch 2 — run/run_rc timeouts: a hung child can no longer
// wedge the evaluator (login shell) forever; `{timeout: seconds}` bounds the
// call through the same PG-kill machinery as ssh_run.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_rc_timeout_kills_hung_child() {
    let started = std::time::Instant::now();
    let out = run_ok(
        r#"$r = run_rc("sleep 30", {timeout: 1})
print($r.rc .. " " .. $r.timed_out)"#,
    )
    .await;
    assert_eq!(out, "-1 true");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "timeout must bound the call, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn run_timeout_dies_catchably() {
    let out = run_ok(
        "try\n  $x = run(\"sleep 30\", {timeout: 1})\ncatch $e\n  print(\"caught: \" .. (\"\" .. $e))\nend\n",
    )
    .await;
    assert!(
        out.starts_with("caught:") && out.contains("timed out after 1s"),
        "got: {out}"
    );
}

#[tokio::test]
async fn run_rc_without_timeout_unchanged_shape() {
    // Default (no opts) keeps the historic no-deadline contract and the
    // {rc, stdout, stderr} keys, now plus timed_out/interrupted.
    let out = run_ok(
        r#"$r = run_rc("echo hi; echo err >&2; exit 3")
print($r.rc .. "|" .. $r.stdout .. "|" .. $r.stderr .. "|" .. $r.timed_out)"#,
    )
    .await;
    assert_eq!(out, "3|hi|err|false");
}

#[tokio::test]
async fn run_rejects_unknown_opt_keys() {
    let err = run_err(r#"run("true", {timout: 5})"#).await;
    assert!(err.contains("unknown opt"), "got: {err}");
}

#[tokio::test]
async fn run_signal_exit_reports_128_plus_sig() {
    let out = run_ok(
        r#"$r = run_rc("kill -TERM $$")
print($r.rc)"#,
    )
    .await;
    assert_eq!(out, "143");
}

#[tokio::test]
async fn run_rejects_surplus_args() {
    // A misplaced opts map must be a loud error, never silently ignored
    // (run("sleep 30", nil, {timeout: 1}) used to run unbounded).
    let err = run_err(r#"run("true", nil, {timeout: 1})"#).await;
    assert!(err.contains("at most 2"), "got: {err}");
    let err = run_err(r#"run_rc("true", nil, {timeout: 1})"#).await;
    assert!(err.contains("at most 2"), "got: {err}");
}

#[tokio::test]
async fn run_timeout_error_names_the_builtin() {
    // The shared opt parser must attribute the error to the calling
    // builtin, not "ssh_run:".
    let err = run_err(r#"run("true", {timeout: "soon"})"#).await;
    assert!(
        err.contains("run:") && !err.contains("ssh_run:"),
        "got: {err}"
    );
}

#[cfg(feature = "http")]
#[tokio::test]
async fn http_get_single_trailing_timeout_map_is_opts() {
    use std::io::Read;
    use std::net::TcpListener;
    // A server that accepts and then never responds: without the deadline
    // this wedges forever; {timeout: 1} in the HEADERS slot must be read as
    // the opts map, not sent as a `timeout` header.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hold = std::thread::spawn(move || {
        let mut held = Vec::new();
        // Accept a single connection and hold it open, draining nothing.
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            held.push(s);
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        drop(held);
    });
    let started = std::time::Instant::now();
    let out = run_ok(&format!(
        "$r = http_get(\"http://{addr}/\", {{timeout: 1}})\nprint($r.status .. \"|\" .. ($r.error != nil))"
    ))
    .await;
    assert_eq!(out, "0|true", "expected a timeout error result");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "deadline must bound the call, took {:?}",
        started.elapsed()
    );
    let _ = hold.join();
}

#[cfg(feature = "http")]
#[tokio::test]
async fn http_request_sole_trailing_timeout_map_is_opts_not_body() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    // The server records whether a body/`timeout` header arrived and
    // responds 200 — http_request("GET", url, {timeout: 1}) must send
    // NEITHER (the map is opts, not a stringified body).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        req
    });
    let out = run_ok(&format!(
        "$r = http_request(\"GET\", \"http://{addr}/\", {{timeout: 1}})\nprint($r.status)"
    ))
    .await;
    assert_eq!(out, "200");
    let req = server.join().unwrap();
    let head = req.to_lowercase();
    assert!(
        !head.contains("timeout:"),
        "the opts map must not become a header: {req}"
    );
    assert!(
        !req.contains("timeout") || !req.lines().last().unwrap_or("").contains("timeout"),
        "the opts map must not become a body: {req}"
    );
}

#[cfg(feature = "http")]
#[tokio::test]
async fn http_request_head_with_content_encoding_reports_status_not_transport_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    // A HEAD response mirrors the headers a GET would return — INCLUDING
    // `Content-Encoding: gzip` — but the server sends NO body. `ureq`
    // correctly forces a HEAD body to zero length (`std::io::empty()`), yet
    // still wraps that empty reader in a gzip decoder because of the
    // Content-Encoding header; draining it makes the decoder read a gzip
    // magic header that never arrives → `UnexpectedEof`. The old code
    // drained unconditionally and collapsed HEAD to `{status: 0, error:
    // "http: body read failed: unexpected end of file"}`. This is the exact
    // shape real servers (example.com, github.com) return — a plain
    // Content-Length HEAD does NOT reproduce it, because ureq intercepts the
    // body before any read. The fix skips the drain for HEAD entirely, so
    // the real status survives with an empty body.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        // `Content-Encoding: gzip` with an EMPTY body — the decoder-wrap
        // shape that used to EOF the drain. `Connection: close` closes after
        // the headers so the (wrapped) body read sees immediate EOF.
        let _ = s.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: 20\r\nConnection: close\r\n\r\n",
        );
        req
    });
    let out = run_ok(&format!(
        "$r = http_request(\"HEAD\", \"http://{addr}/\", {{timeout: 2}})\nprint($r.status .. \"|\" .. ($r.error == nil) .. \"|\" .. ($r.body == \"\"))"
    ))
    .await;
    assert_eq!(
        out, "200|true|true",
        "HEAD must report the real status with an empty body and no transport error"
    );
    let req = server.join().unwrap();
    assert!(
        req.starts_with("HEAD "),
        "server must have seen a HEAD request: {req}"
    );
}

// ---------------------------------------------------------------------------
// 2026-07 audit batch 2 — keyword lexemes as bare map keys / .field names:
// the reserved-word footgun ({label: 1} parse error, $m.to unreadable) is
// retired. `fn` stays quoted (lexes identically to `function`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn keywords_as_bare_map_keys_and_fields() {
    // Literal keys, field reads, field writes.
    let out = run_ok(
        "$m = {label: \"L\", to: \"dest\", on: true, parse: 2, in: 3, step: 4}\nprint($m.label .. \" \" .. $m.to .. \" \" .. $m.on .. \" \" .. $m.parse .. \" \" .. $m.in .. \" \" .. $m.step)",
    )
    .await;
    assert_eq!(out, "L dest true 2 3 4");
    let out = run_ok("$c = {a: 1}\n$c.to = \"wired\"\nprint($c.to)").await;
    assert_eq!(out, "wired");
    // Grammar keywords still work in their own positions right next to a
    // keyword field: `for … = $m.to to …` parses.
    let out = run_ok("$m = {to: 5}\nfor $i = $m.to to 7\n  $y = $i\nend\nprint($y)").await;
    assert_eq!(out, "7");
    // `fn` is the documented exception (same token as `function`).
    let err = run_err("print({fn: 1})").await;
    assert!(!err.is_empty());
}

#[tokio::test]
async fn keywords_as_strict_data_keys() {
    // data_parse must accept .conf.mix keys named to/in/on/label without
    // quoting (they round-trip through data_encode as barewords or quoted —
    // either way the PARSER must read them).
    let out = run_ok(
        "$v = data_parse(\"{to: 1, on: \\\"x\\\", label: [2]}\")\nprint($v.to .. \" \" .. $v.on .. \" \" .. $v.label[0])",
    )
    .await;
    assert_eq!(out, "1 x 2");
}

#[tokio::test]
async fn keywords_as_send_kwarg_keys_and_parse_delimiters() {
    // `parse … with` accepts a keyword lexeme as a literal delimiter.
    let out =
        run_ok("parse \"10 to 20\" with $a to $b\nprint(trim($a) .. \"|\" .. trim($b))").await;
    assert_eq!(out, "10|20");
    // send kwargs accept keyword keys (to=/label=): with no broker the send
    // degrades gracefully — the statement must PARSE (no parse error, and
    // execution reaches the print).
    let out = run_ok("send nosuchtarget someverb to=\"dest\" label=3\nprint(\"parsed\")").await;
    assert_eq!(out, "parsed");
}
