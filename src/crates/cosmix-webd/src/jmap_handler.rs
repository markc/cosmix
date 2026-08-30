//! `WebdJmapHandler` — the [`cosmix_mix::JmapHandler`] implementation that
//! backs the `jmap` builtin in an embedded Mix handler. It resolves JMAP
//! method calls over webd's HTTP client to the vhost's configured maild
//! upstream (the same upstream the `/jmap` reverse-proxy uses), so an SSR
//! handler can render contacts/calendar/mail server-side without the
//! script ever naming a host or holding credentials.
//!
//! Scope is enforced by webd, not the script: the handler is constructed
//! around exactly one vhost's `jmap_upstream`, so a Mix script can only
//! reach its own tenant's configured maild — `CapabilityClass::Jmap`
//! gates *whether* it may, this struct bounds *where* it can go.
//!
//! **Auth (Phase 2):** the `auth` field carries `Bearer <maild-token>`
//! unsealed from the `cosmix_session` cookie by `serve_static` (see
//! `main.rs`), so plain page *navigations* (which carry no
//! `Authorization`) authorize. `None` ⇒ no session ⇒ maild 401 ⇒ a
//! transport error here, which the SSR handler renders as a login
//! redirect. (Phase 1 forwarded the request's own `Authorization` header;
//! that path moved to the cookie in the auth-unification slice.)
//!
//! Forward-compatibility mirrors [`crate::db::WebdDbHandler`]: an
//! out-of-process untrusted worker would implement the same
//! `JmapHandler` trait by RPC-ing back to webd; the Mix script is
//! identical.

use cosmix_mix::json::{json_to_mix, mix_to_json};
use cosmix_mix::value::Value;
use cosmix_mix::{JmapCall, JmapFuture, JmapHandler, MixError, MixResult};

/// JMAP capability URNs maild advertises. Sent as the request `using`
/// set on every call so the single primitive can invoke any
/// contacts/calendars/mail method — including a cross-capability
/// batch — without sniffing the method prefix (which is brittle as
/// methods/capabilities grow). `accountId` is auth-derived by maild, so
/// `using` is pure capability declaration; the union is the least
/// surprising choice.
const JMAP_USING: &[&str] = &[
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:contacts",
    "urn:ietf:params:jmap:calendars",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

/// Hard cap on the upstream JMAP response we will buffer + parse. The
/// evaluator's collection caps only apply *after* JSON parse, so without
/// this a `jmap`-capable handler could force webd to buffer/parse an
/// unbounded response. Matches the `/jmap` proxy's 10 MiB request cap.
const MAX_JMAP_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// Per-request JMAP handler. Holds a reqwest client built fresh on this
/// request's own runtime (NOT a clone of the shared `node.http_client`,
/// which is bound to the main runtime — see `run_with_limits`), the
/// vhost's upstream base URL, and the forwarded request auth.
pub(crate) struct WebdJmapHandler {
    client: reqwest::Client,
    /// Upstream base URL (e.g. `https://127.0.0.1:8443`); `/jmap` is
    /// appended for the API endpoint.
    upstream: String,
    /// `Bearer <maild-token>` unsealed from the session cookie (Phase 2).
    /// `None` → the request is sent unauthenticated and maild replies 401,
    /// surfaced here as a transport error (an `Err`, not a method-level
    /// error triple) → the SSR handler renders a login redirect.
    auth: Option<String>,
}

impl WebdJmapHandler {
    pub(crate) fn new(client: reqwest::Client, upstream: String, auth: Option<String>) -> Self {
        Self {
            client,
            upstream,
            auth,
        }
    }
}

fn rt_err(msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        msg: msg.into(),
        span: None,
    }
}

/// Read a response body chunk-by-chunk with a hard cap (`MAX_JMAP_RESPONSE_BYTES`)
/// BEFORE parsing, so a hostile/huge upstream response can't force webd to
/// buffer + parse unbounded bytes (the evaluator's collection caps only
/// apply *after* parse). `what` labels the read site in errors (`"upstream"`
/// for the JMAP API, `"upload"` for the blob endpoint). Shared by
/// [`WebdJmapHandler::request`] and [`WebdJmapHandler::upload`].
async fn read_body_capped(mut resp: reqwest::Response, what: &str) -> MixResult<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| rt_err(format!("jmap {what} read failed: {e}")))?
    {
        if buf.len() + chunk.len() > MAX_JMAP_RESPONSE_BYTES {
            return Err(rt_err(format!(
                "jmap {what} response exceeds {MAX_JMAP_RESPONSE_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

impl JmapHandler for WebdJmapHandler {
    fn request<'a>(&'a self, calls: &'a [JmapCall]) -> JmapFuture<'a, MixResult<Value>> {
        Box::pin(async move {
            // Build the JMAP request envelope: one [method, args, callId]
            // triple per call, with the full capability `using` set.
            let method_calls: Vec<serde_json::Value> = calls
                .iter()
                .map(|c| {
                    // Fallible since cosmix-lib-mix 0.21.0: non-finite numbers
                    // have no JSON representation — a catchable jmap() error,
                    // never a silently-mangled request.
                    let args = mix_to_json(&c.args).map_err(|e| {
                        rt_err(format!("jmap {}: args not JSON-encodable: {e}", c.method))
                    })?;
                    Ok(serde_json::Value::Array(vec![
                        serde_json::Value::String(c.method.clone()),
                        args,
                        serde_json::Value::String(c.call_id.clone()),
                    ]))
                })
                .collect::<MixResult<_>>()?;
            let body = serde_json::json!({
                "using": JMAP_USING,
                "methodCalls": method_calls,
            });

            let url = format!("{}/jmap", self.upstream.trim_end_matches('/'));
            let mut req = self.client.post(&url).json(&body);
            if let Some(auth) = &self.auth {
                req = req.header(reqwest::header::AUTHORIZATION, auth);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| rt_err(format!("jmap upstream request failed: {e}")))?;
            let status = resp.status();
            // A non-2xx is a transport/auth failure (e.g. 401 when auth is
            // missing/invalid), distinct from a method-level JMAP error
            // (which arrives as an `["error", …]` triple inside a 200).
            if !status.is_success() {
                return Err(rt_err(format!(
                    "jmap upstream returned HTTP {}",
                    status.as_u16()
                )));
            }
            // Read the body with a hard byte cap BEFORE parsing (see
            // `read_body_capped`) so a hostile/huge upstream response can't
            // force webd to buffer + parse unbounded JSON.
            let buf = read_body_capped(resp, "upstream").await?;
            let json: serde_json::Value = serde_json::from_slice(&buf)
                .map_err(|e| rt_err(format!("jmap upstream returned malformed JSON: {e}")))?;
            let responses = json
                .get("methodResponses")
                .ok_or_else(|| rt_err("jmap response missing methodResponses"))?;
            let arr = responses
                .as_array()
                .ok_or_else(|| rt_err("jmap response methodResponses is not a list"))?;
            // Validate every methodResponse is a well-formed
            // [method, result, callId] triple (string method + callId). A
            // method-level error is a *valid* triple `["error", {…}, id]`
            // and stays inline (the builtin's batch form surfaces it); a
            // structurally malformed triple is a protocol failure (Err),
            // so garbage from a broken upstream never reaches the script
            // as a normal value.
            // Each triple is [method: string, payload: object, callId:
            // string] (RFC 8620 §3.4 — both a success result and an
            // `["error", {type…}, id]` payload are JSON objects). A
            // non-string method/callId or a non-object payload is a
            // structural protocol failure, not data to pass through.
            for (i, mr) in arr.iter().enumerate() {
                let triple = mr.as_array().filter(|t| t.len() == 3).ok_or_else(|| {
                    rt_err(format!(
                        "jmap response methodResponses[{i}] is not a 3-element triple"
                    ))
                })?;
                if !triple[0].is_string() || !triple[2].is_string() {
                    return Err(rt_err(format!(
                        "jmap response methodResponses[{i}] has a non-string method or callId"
                    )));
                }
                if !triple[1].is_object() {
                    return Err(rt_err(format!(
                        "jmap response methodResponses[{i}] payload is not a JSON object"
                    )));
                }
            }
            // Hand back the methodResponses as a Value::List of
            // [method, result, callId] triples; the `jmap()` builtin
            // applies the single-vs-batch unwrap + error policy.
            Ok(json_to_mix(responses.clone()))
        })
    }

    fn upload<'a>(
        &'a self,
        bytes: &'a [u8],
        content_type: &'a str,
    ) -> JmapFuture<'a, MixResult<Value>> {
        Box::pin(async move {
            // Defence-in-depth: a Content-Type carrying control bytes (CR/LF/NUL)
            // can't frame a valid HTTP header. The HTTP layer would reject it
            // anyway, but reject it here with a clean Err so a future caller that
            // passes a user-derived content_type gets a catchable error, never a
            // surprise build failure deep in reqwest. The current caller passes a
            // literal "message/rfc822", so this never trips today.
            if content_type.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return Err(rt_err(
                    "jmap upload: content_type contains control characters",
                ));
            }
            // POST the raw bytes to maild's blob-upload endpoint. maild
            // derives the account from the Bearer token and IGNORES the
            // URL's accountId path segment (`blob_upload`'s `Path(_acct_id)`),
            // so a fixed `_` placeholder is correct — no accountId discovery.
            let url = format!("{}/jmap/upload/_", self.upstream.trim_end_matches('/'));
            let mut req = self
                .client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes.to_vec());
            if let Some(auth) = &self.auth {
                req = req.header(reqwest::header::AUTHORIZATION, auth);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| rt_err(format!("jmap upload request failed: {e}")))?;
            let status = resp.status();
            // Non-2xx is a transport/auth failure (401 when the session
            // token is missing/expired/revoked), surfaced as an Err the
            // SSR handler renders as a login redirect — same discipline as
            // `request`. maild returns 201 Created on success.
            if !status.is_success() {
                return Err(rt_err(format!(
                    "jmap upload returned HTTP {}",
                    status.as_u16()
                )));
            }
            let buf = read_body_capped(resp, "upload").await?;
            let json: serde_json::Value = serde_json::from_slice(&buf)
                .map_err(|e| rt_err(format!("jmap upload returned malformed JSON: {e}")))?;
            // The upload response is `{accountId, blobId, type, size}`; the
            // builtin needs only the minted blobId (a string) to reference
            // from a subsequent `Email/set { create }`.
            let blob_id = json
                .get("blobId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| rt_err("jmap upload response missing a string blobId"))?;
            Ok(Value::String(blob_id.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Captured `(body, content_type)` from the stub upload endpoint.
    type CapturedUpload = Arc<Mutex<Option<(Vec<u8>, String)>>>;

    /// Spin a stub JMAP server on an ephemeral port. `canned` is the JSON
    /// body it returns from `/jmap`; the returned holder captures the
    /// request body it received so the test can assert the envelope.
    async fn stub_ok(canned: serde_json::Value) -> (String, Arc<Mutex<Option<serde_json::Value>>>) {
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/jmap",
            post(move |Json(body): Json<serde_json::Value>| {
                let cap = cap.clone();
                let canned = canned.clone();
                async move {
                    *cap.lock().await = Some(body);
                    Json(canned)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    fn map1(k: &str, v: Value) -> Value {
        let mut m = cosmix_mix::IndexMap::new();
        m.insert(k.to_string(), v);
        Value::map(m)
    }

    #[tokio::test]
    async fn builds_envelope_and_parses_methodresponses() {
        let canned = serde_json::json!({
            "methodResponses": [["Contact/query", {"ids": ["a", "b"], "total": 2}, "c0"]],
            "sessionState": "0",
        });
        let (base, captured) = stub_ok(canned).await;
        let h = WebdJmapHandler::new(
            reqwest::Client::new(),
            base,
            Some("Basic dXNlcjpwYXNz".to_string()),
        );
        let calls = vec![JmapCall {
            method: "Contact/query".to_string(),
            args: map1("filter", Value::Nil),
            call_id: "c0".to_string(),
        }];
        let out = h.request(&calls).await.expect("request succeeds");
        // Returns the methodResponses list (one triple here).
        match &out {
            Value::List(l) => assert_eq!(l.len(), 1, "one response triple"),
            o => panic!("expected a list, got {o:?}"),
        }
        // Envelope: the full 5-URN `using` set + our single methodCall.
        let body = captured.lock().await.clone().expect("server saw a body");
        assert_eq!(body["using"].as_array().unwrap().len(), JMAP_USING.len());
        assert_eq!(body["methodCalls"][0][0], "Contact/query");
        assert_eq!(body["methodCalls"][0][2], "c0");
    }

    #[tokio::test]
    async fn non_2xx_is_a_transport_error() {
        // A stub that 401s every request (auth missing/invalid).
        let app = Router::new().route("/jmap", post(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let h = WebdJmapHandler::new(reqwest::Client::new(), format!("http://{addr}"), None);
        let calls = vec![JmapCall {
            method: "Contact/query".to_string(),
            args: Value::map(cosmix_mix::IndexMap::new()),
            call_id: "c0".to_string(),
        }];
        let err = h
            .request(&calls)
            .await
            .expect_err("a 401 is a transport error");
        let msg = format!("{err}");
        assert!(msg.contains("HTTP 401"), "got: {msg}");
    }

    #[tokio::test]
    async fn upload_posts_bytes_and_returns_blobid() {
        // Stub maild's blob-upload endpoint: capture the body + the
        // Content-Type, return a maild-shaped 201 carrying a blobId.
        let captured: CapturedUpload = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/jmap/upload/_",
            post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let cap = cap.clone();
                    async move {
                        let ct = headers
                            .get(axum::http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let size = body.len();
                        *cap.lock().await = Some((body.to_vec(), ct));
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "accountId": "1",
                                "blobId": "Gblob-xyz",
                                "type": "application/octet-stream",
                                "size": size,
                            })),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let h = WebdJmapHandler::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            Some("Bearer t".to_string()),
        );
        let out = h
            .upload(b"From: a@b\r\n\r\nhi", "message/rfc822")
            .await
            .expect("upload succeeds");
        // The minted blobId comes back as a bare string.
        assert_eq!(out, Value::String("Gblob-xyz".into()));
        // The raw bytes + content-type reached the endpoint unaltered.
        let (body, ct) = captured
            .lock()
            .await
            .clone()
            .expect("server saw the upload");
        assert_eq!(body, b"From: a@b\r\n\r\nhi");
        assert_eq!(ct, "message/rfc822");
    }

    #[tokio::test]
    async fn upload_rejects_control_chars_in_content_type() {
        // A CRLF in content_type can't frame a header — rejected with a clean
        // Err before any network call (no upstream needed).
        let h = WebdJmapHandler::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_string(),
            None,
        );
        let err = h
            .upload(b"x", "message/rfc822\r\nX-Inject: 1")
            .await
            .expect_err("a control char in content_type is rejected");
        assert!(
            format!("{err}").contains("control characters"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn upload_non_2xx_is_a_transport_error() {
        // A 401 (missing/expired session) is a transport error (an Err),
        // surfaced by the SSR handler as a login redirect.
        let app = Router::new().route(
            "/jmap/upload/_",
            post(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let h = WebdJmapHandler::new(reqwest::Client::new(), format!("http://{addr}"), None);
        let err = h
            .upload(b"x", "message/rfc822")
            .await
            .expect_err("a 401 upload is a transport error");
        assert!(format!("{err}").contains("HTTP 401"), "got: {err}");
    }

    #[tokio::test]
    async fn malformed_triple_payload_is_a_protocol_error() {
        // A methodResponse whose payload is not a JSON object (here a bare
        // number) is a structural protocol failure, not data to pass on.
        let canned = serde_json::json!({
            "methodResponses": [["Contact/query", 123, "c0"]],
            "sessionState": "0",
        });
        let (base, _captured) = stub_ok(canned).await;
        let h = WebdJmapHandler::new(reqwest::Client::new(), base, None);
        let calls = vec![JmapCall {
            method: "Contact/query".to_string(),
            args: Value::map(cosmix_mix::IndexMap::new()),
            call_id: "c0".to_string(),
        }];
        let err = h
            .request(&calls)
            .await
            .expect_err("non-object payload rejected");
        assert!(format!("{err}").contains("not a JSON object"), "got: {err}");
    }
}
