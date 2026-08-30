//! The `JmapHandler` seam (`jmap`). Mirrors the host-seam pattern of
//! `DbHandler`: a stub handler is injected, the `jmap` builtin dispatches
//! to it in both its single-call and batch forms, and the
//! `CapabilityClass::Jmap` gate denies access when the policy withholds
//! it. The stub returns canned `methodResponses` so the test asserts both
//! dispatch (the calls reached the handler with the right method/args/
//! call_id) and the builtin's single-vs-batch unwrap + error policy.

use std::cell::RefCell;
use std::rc::Rc;

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{CapabilityClass, CategoryAllowList, IndexMap, JmapCall, JmapFuture, JmapHandler};

/// Records every batch it is handed and returns a canned `methodResponses`
/// list: one triple per call. `Bad/method` yields an `["error", …]`
/// triple (a JMAP method-level failure); everything else echoes
/// `[method, {method, args}, call_id]` so the test can assert the args
/// reached the handler.
#[derive(Default)]
struct StubJmap {
    batches: RefCell<Vec<Vec<(String, String)>>>,
    /// Every `upload(bytes, content_type)` it is handed, so the
    /// `jmap_upload` tests can assert the body + type reached the seam.
    uploads: RefCell<Vec<(Vec<u8>, String)>>,
}

impl JmapHandler for StubJmap {
    fn upload<'a>(
        &'a self,
        bytes: &'a [u8],
        content_type: &'a str,
    ) -> JmapFuture<'a, cosmix_mix::MixResult<Value>> {
        self.uploads
            .borrow_mut()
            .push((bytes.to_vec(), content_type.to_string()));
        // Canned blobId — the builtin returns it verbatim as a string.
        Box::pin(async move { Ok(Value::String("Gblob-1".to_string())) })
    }

    fn request<'a>(
        &'a self,
        calls: &'a [JmapCall],
    ) -> JmapFuture<'a, cosmix_mix::MixResult<Value>> {
        self.batches.borrow_mut().push(
            calls
                .iter()
                .map(|c| (c.method.clone(), c.call_id.clone()))
                .collect(),
        );
        let mut responses = Vec::with_capacity(calls.len());
        for c in calls {
            if c.method == "Bad/method" {
                let mut em = IndexMap::new();
                em.insert("type".to_string(), Value::String("unknownMethod".into()));
                responses.push(Value::list(vec![
                    Value::String("error".into()),
                    Value::map(em),
                    Value::String(c.call_id.clone()),
                ]));
            } else {
                let mut rm = IndexMap::new();
                rm.insert("method".to_string(), Value::String(c.method.clone()));
                rm.insert("args".to_string(), c.args.clone());
                responses.push(Value::list(vec![
                    Value::String(c.method.clone()),
                    Value::map(rm),
                    Value::String(c.call_id.clone()),
                ]));
            }
        }
        Box::pin(async move { Ok(Value::list(responses)) })
    }
}

async fn run_with(source: &str, configure: impl FnOnce(&mut Evaluator)) -> Result<Value, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout), Box::new(stderr));
    configure(&mut eval);
    eval.execute(&stmts).await.map_err(|e| e.to_string())
}

fn allow_jmap(eval: &mut Evaluator, j: Rc<StubJmap>) {
    eval.set_jmap_handler(j);
    eval.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Jmap])));
}

#[tokio::test]
async fn jmap_single_returns_the_one_result_map() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with("jmap(\"Contact/query\", { limit: 5 })\n", move |e| {
        allow_jmap(e, j2)
    })
    .await
    .expect("jmap allowed under the Jmap capability");
    // Single form unwraps methodResponses[0][1] → the echoed result map.
    let Value::Map(m) = &out else {
        panic!("single form returns the result map, got a non-map");
    };
    assert_eq!(
        m.get("method"),
        Some(&Value::String("Contact/query".into()))
    );
    // One batch of one call reached the handler, with the default call_id.
    let batches = j.batches.borrow();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0],
        vec![("Contact/query".to_string(), "c0".to_string())]
    );
}

#[tokio::test]
async fn jmap_single_omitted_args_send_empty_map() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with("jmap(\"Core/echo\")\n", move |e| allow_jmap(e, j2))
        .await
        .expect("args are optional in the single form");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    // Omitted args ⇒ an empty map reached the handler. (Compare via
    // destructure, not `==`: empty `Value::Map`s don't compare equal.)
    match m.get("args") {
        Some(Value::Map(a)) => assert!(a.is_empty(), "args should be an empty map"),
        other => panic!("expected an empty args map, got {other:?}"),
    }
}

#[tokio::test]
async fn jmap_single_raises_on_method_level_error() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with("jmap(\"Bad/method\", {})\n", move |e| allow_jmap(e, j2))
        .await
        .expect_err("single form raises on a method-level JMAP error");
    assert!(err.contains("jmap Bad/method error"), "got: {err}");
}

#[tokio::test]
async fn jmap_batch_returns_methodresponses_verbatim() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with(
        "jmap([[\"Contact/query\", { text: \"a\" }, \"q0\"], [\"Contact/get\", {}, \"g0\"]])\n",
        move |e| allow_jmap(e, j2),
    )
    .await
    .expect("batch form allowed");
    let Value::List(resps) = &out else {
        panic!("batch form returns the methodResponses list");
    };
    assert_eq!(resps.len(), 2, "one response triple per call");
    // The two calls reached the handler in order with their explicit ids.
    let batches = j.batches.borrow();
    assert_eq!(
        batches[0],
        vec![
            ("Contact/query".to_string(), "q0".to_string()),
            ("Contact/get".to_string(), "g0".to_string()),
        ]
    );
}

#[tokio::test]
async fn jmap_batch_keeps_error_triples_inline() {
    // Batch form does NOT raise on a method-level error — it is part of the
    // protocol; the caller inspects the triple.
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with("jmap([[\"Bad/method\", {}, \"e0\"]])\n", move |e| {
        allow_jmap(e, j2)
    })
    .await
    .expect("batch form does not raise on a method-level error");
    let Value::List(resps) = &out else {
        panic!("expected a list")
    };
    let Value::List(triple) = &resps[0] else {
        panic!("expected a triple")
    };
    assert_eq!(triple[0], Value::String("error".into()));
}

#[tokio::test]
async fn jmap_denied_without_capability() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with("jmap(\"Contact/query\", {})\n", move |e| {
        e.set_jmap_handler(j2);
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])));
    })
    .await
    .expect_err("Jmap not in the allow-set must deny jmap");
    assert!(err.contains("capability denied"), "got: {err}");
    assert!(
        j.batches.borrow().is_empty(),
        "handler must NOT be called when denied"
    );
}

#[tokio::test]
async fn jmap_no_handler_is_clean_error_not_panic() {
    let err = run_with("jmap(\"Contact/query\", {})\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Jmap])));
    })
    .await
    .expect_err("no jmap handler must error");
    assert!(err.contains("jmap not available"), "got: {err}");
}

#[tokio::test]
async fn jmap_rejects_bad_arg_shapes() {
    let j = Rc::new(StubJmap::default());
    // A map as the first arg is neither a method name nor a call list.
    let j2 = j.clone();
    let err = run_with("jmap({ x: 1 })\n", move |e| allow_jmap(e, j2))
        .await
        .expect_err("a map first-arg is a usage error");
    assert!(err.contains("must be a method name"), "got: {err}");
    // A batch element that is not a 3-element list.
    let j3 = j.clone();
    let err2 = run_with("jmap([[\"Contact/query\", {}]])\n", move |e| {
        allow_jmap(e, j3)
    })
    .await
    .expect_err("a 2-element call is a usage error");
    assert!(err2.contains("3-element list"), "got: {err2}");
}

#[tokio::test]
async fn jmap_single_rejects_extra_args() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with("jmap(\"Contact/query\", {}, \"ignored\")\n", move |e| {
        allow_jmap(e, j2)
    })
    .await
    .expect_err("the single form takes (method, args) only");
    assert!(err.contains("single-call form takes"), "got: {err}");
    assert!(
        j.batches.borrow().is_empty(),
        "rejected before any host call"
    );
}

#[tokio::test]
async fn jmap_rejects_unjsonable_args() {
    // Bytes have no JSON form — reject rather than silently send null.
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with(
        "jmap(\"Blob/set\", { data: string_to_bytes(\"hi\") })\n",
        move |e| allow_jmap(e, j2),
    )
    .await
    .expect_err("a bytes-valued arg is rejected");
    assert!(err.contains("no JSON representation"), "got: {err}");
    assert!(
        j.batches.borrow().is_empty(),
        "rejected before any host call"
    );
}

// ── jmap_upload (the compose half of the seam) ──

#[tokio::test]
async fn jmap_upload_returns_blobid_and_records_body() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with(
        "jmap_upload(\"From: a@b\\r\\n\\r\\nhi\", \"message/rfc822\")\n",
        move |e| allow_jmap(e, j2),
    )
    .await
    .expect("jmap_upload allowed under the Jmap capability");
    assert_eq!(out, Value::String("Gblob-1".into()), "returns the blobId");
    let uploads = j.uploads.borrow();
    assert_eq!(uploads.len(), 1, "one upload reached the handler");
    assert_eq!(uploads[0].0, b"From: a@b\r\n\r\nhi", "raw RFC822 bytes");
    assert_eq!(uploads[0].1, "message/rfc822", "content type forwarded");
}

#[tokio::test]
async fn jmap_upload_defaults_content_type() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let out = run_with("jmap_upload(\"raw\")\n", move |e| allow_jmap(e, j2))
        .await
        .expect("content_type is optional");
    assert_eq!(out, Value::String("Gblob-1".into()));
    assert_eq!(
        j.uploads.borrow()[0].1,
        "application/octet-stream",
        "omitted content_type defaults to octet-stream"
    );
}

#[tokio::test]
async fn jmap_upload_rejects_non_body_arg() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with("jmap_upload({ x: 1 })\n", move |e| allow_jmap(e, j2))
        .await
        .expect_err("a map body is a usage error");
    assert!(err.contains("string or bytes body"), "got: {err}");
    assert!(
        j.uploads.borrow().is_empty(),
        "rejected before any host call"
    );
}

#[tokio::test]
async fn jmap_upload_denied_without_capability() {
    let j = Rc::new(StubJmap::default());
    let j2 = j.clone();
    let err = run_with("jmap_upload(\"raw\", \"message/rfc822\")\n", move |e| {
        e.set_jmap_handler(j2);
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])));
    })
    .await
    .expect_err("Jmap not in the allow-set must deny jmap_upload");
    assert!(err.contains("capability denied"), "got: {err}");
    assert!(
        j.uploads.borrow().is_empty(),
        "handler must NOT be called when denied"
    );
}

#[tokio::test]
async fn jmap_upload_no_handler_is_clean_error_not_panic() {
    let err = run_with("jmap_upload(\"raw\")\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Jmap])));
    })
    .await
    .expect_err("no jmap handler must error");
    assert!(err.contains("jmap_upload not available"), "got: {err}");
}
