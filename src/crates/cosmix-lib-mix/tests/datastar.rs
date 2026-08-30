//! Datastar `ds_*` SSE builtin tests (Part A.2 of the Datastar webd
//! foundation plan). Each test drives Mix source through the public
//! `cosmix_mix::run` API and asserts the EXACT SSE wire bytes — the
//! `event:` line + `data:` lines + the mandatory `\n\n` terminator —
//! so a wire-format drift in the upstream `datastar` crate is caught
//! here rather than at the browser. The builtins FRAME content; they do
//! not escape it (that is the handler's `html_escape()` obligation), and
//! the verbatim-`<` test pins that contract.
//!
//! Raw-string note: Mix selectors carry `"#id"`, which closes an `r#"…"#`
//! literal early, so every source snippet uses `r##"…"##`.
#![cfg(feature = "datastar")]

use cosmix_mix::value::Value;

/// Run `source` and return the program's result value as a string.
async fn run_str(source: &str) -> String {
    match cosmix_mix::run(source).await {
        Ok(Value::String(ref s)) => s.clone(),
        Ok(other) => panic!("expected a string result, got {other:?}\nsource: {source}"),
        Err(e) => panic!("script failed: {e}\nsource: {source}"),
    }
}

/// Run `source` expecting a runtime error; return its message.
async fn run_err(source: &str) -> String {
    match cosmix_mix::run(source).await {
        Ok(v) => panic!("expected an error, got {v:?}\nsource: {source}"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn patch_elements_default_outer_id_targeted() {
    // No selector, default (outer) mode, no view-transition → only the
    // `elements` data line is emitted (selector/mode/useViewTransition are
    // all at their defaults and therefore omitted from the wire).
    let out = run_str(r##"ds_patch_elements("<div id=\"x\">hi</div>")"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-elements\ndata: elements <div id=\"x\">hi</div>\n\n"
    );
}

#[tokio::test]
async fn patch_elements_selector_inner_view_transition() {
    let out = run_str(
        r##"ds_patch_elements("<p>x</p>", {selector: "#content", mode: "inner", view_transition: true})"##,
    )
    .await;
    assert_eq!(
        out,
        "event: datastar-patch-elements\n\
         data: selector #content\n\
         data: mode inner\n\
         data: useViewTransition true\n\
         data: elements <p>x</p>\n\n"
    );
}

#[tokio::test]
async fn patch_elements_multiline_splits_elements_lines() {
    // Each line of the HTML becomes its own `data: elements` line — the
    // SSE framing requirement the SDK enforces.
    let out = run_str(r##"ds_patch_elements("<ul>\n<li>a</li>\n</ul>")"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-elements\n\
         data: elements <ul>\n\
         data: elements <li>a</li>\n\
         data: elements </ul>\n\n"
    );
}

#[tokio::test]
async fn patch_elements_remove_mode_uses_selector_no_html() {
    let out = run_str(r##"ds_patch_elements("", {selector: "#row-5", mode: "remove"})"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-elements\ndata: selector #row-5\ndata: mode remove\n\n"
    );
}

#[tokio::test]
async fn patch_elements_remove_without_selector_errors() {
    let e = run_err(r##"ds_patch_elements("<x/>", {mode: "remove"})"##).await;
    assert!(e.contains("requires a 'selector'"), "got: {e}");
}

#[tokio::test]
async fn patch_elements_unknown_mode_errors() {
    let e = run_err(r##"ds_patch_elements("<x/>", {mode: "sideways"})"##).await;
    assert!(e.contains("unknown mode 'sideways'"), "got: {e}");
}

#[tokio::test]
async fn patch_elements_does_not_escape_content() {
    // The builtin FRAMES, it does not sanitise: a raw `<` passes through
    // verbatim. Handlers MUST html_escape() untrusted content themselves.
    let out = run_str(r##"ds_patch_elements("<script>alert(1)</script>")"##).await;
    assert!(
        out.contains("data: elements <script>alert(1)</script>"),
        "got: {out}"
    );
}

#[tokio::test]
async fn patch_signals_from_map_encodes_json() {
    // IndexMap preserves insertion order; whole numbers stay integers.
    let out = run_str(r##"ds_patch_signals({count: 5, open: true})"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-signals\ndata: signals {\"count\":5,\"open\":true}\n\n"
    );
}

#[tokio::test]
async fn patch_signals_only_if_missing() {
    let out = run_str(r##"ds_patch_signals({theme: "dark"}, {only_if_missing: true})"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-signals\n\
         data: onlyIfMissing true\n\
         data: signals {\"theme\":\"dark\"}\n\n"
    );
}

#[tokio::test]
async fn patch_signals_accepts_verbatim_json_string() {
    let out = run_str(r##"ds_patch_signals("{\"a\":1}")"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-signals\ndata: signals {\"a\":1}\n\n"
    );
}

#[tokio::test]
async fn ds_sse_joins_a_list_of_events() {
    let out = run_str(
        r##"ds_sse([ds_patch_signals({a: 1}), ds_patch_elements("<i>x</i>", {selector: "#c", mode: "inner"})])"##,
    )
    .await;
    assert_eq!(
        out,
        "event: datastar-patch-signals\ndata: signals {\"a\":1}\n\n\
         event: datastar-patch-elements\ndata: selector #c\ndata: mode inner\ndata: elements <i>x</i>\n\n"
    );
}

#[tokio::test]
async fn ds_sse_single_event_passthrough() {
    let out = run_str(r##"ds_sse(ds_patch_elements("<b>1</b>"))"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-elements\ndata: elements <b>1</b>\n\n"
    );
}

#[tokio::test]
async fn ds_sse_rejects_non_string_list_element() {
    let e = run_err(r##"ds_sse([42])"##).await;
    assert!(e.contains("must be an SSE event string"), "got: {e}");
}

// --- SSE frame-injection guards (Codex MAJORs) ---

#[tokio::test]
async fn patch_elements_selector_rejects_line_terminators() {
    // A newline in a selector goes into a single un-split `data: selector`
    // line and could forge a second event — must fail closed.
    let e = run_err(r##"ds_patch_elements("<x/>", {selector: "#a\nevent: x"})"##).await;
    assert!(e.contains("must not contain a line terminator"), "got: {e}");
    let e2 = run_err(r##"ds_patch_elements("", {mode: "remove", selector: "#a\rINJ"})"##).await;
    assert!(
        e2.contains("must not contain a line terminator"),
        "got: {e2}"
    );
}

#[tokio::test]
async fn patch_elements_lone_cr_in_html_is_contained() {
    // A lone `\r` is normalised to `\n` so the SDK re-prefixes the break as a
    // second `data: elements` line — the injected text can NEVER appear as a
    // bare (un-prefixed) line, so no frame is forged. No raw `\r` survives.
    let out = run_str(r##"ds_patch_elements("<a>\rmalicious")"##).await;
    assert!(!out.contains('\r'), "raw CR survived: {out:?}");
    assert_eq!(
        out,
        "event: datastar-patch-elements\n\
         data: elements <a>\n\
         data: elements malicious\n\n"
    );
}

#[tokio::test]
async fn patch_signals_verbatim_lone_cr_is_contained() {
    // Same containment for a verbatim signals string: the `\r` becomes `\n`
    // and every line is re-prefixed with `data: signals`, so even a blank
    // line cannot become an SSE event separator.
    let out = run_str(r##"ds_patch_signals("{\"a\":1}\rINJ")"##).await;
    assert!(!out.contains('\r'), "raw CR survived: {out:?}");
    assert_eq!(
        out,
        "event: datastar-patch-signals\n\
         data: signals {\"a\":1}\n\
         data: signals INJ\n\n"
    );
}

#[tokio::test]
async fn patch_signals_map_with_newline_value_is_escaped_not_split() {
    // The encoded-map path is provably single-line: serde escapes the `\n`
    // inside the string value as `\\n`, so the wire stays one data line.
    let out = run_str(r##"ds_patch_signals({note: "a\nb"})"##).await;
    assert_eq!(
        out,
        "event: datastar-patch-signals\ndata: signals {\"note\":\"a\\nb\"}\n\n"
    );
}
