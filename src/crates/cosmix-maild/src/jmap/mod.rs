//! JMAP server (RFC 8620 + 8621).

pub mod calendar;
pub mod contact;
pub mod email;
pub mod mailbox;
pub mod references;
pub mod submission;
pub mod thread;
pub mod types;
pub mod vacation;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};

use cosmix_mds::Mds;

use crate::auth;
use crate::db::{self, Db};
use crate::mailstore::{MailStore, SqliteMailStore};
use types::*;

/// State change notification sent via broadcast channel.
#[derive(Clone, Debug)]
pub struct StateChange {
    pub account_id: i32,
    pub object_type: String,
    pub new_state: String,
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub base_url: String,
    pub classifier: Arc<cosmix_maild_bayesian::DefaultClassifier>,
    pub state_tx: tokio::sync::broadcast::Sender<StateChange>,
    /// MailStore adapter over `cosmix-mds`. Wired in Phase A
    /// (Task 0.3 of the JMAP→MDS migration plan); `Mailbox/get`
    /// (Task 2.1) is the first JMAP reader to consume it. Subsequent
    /// Phase 2 / 3 / 4 tasks migrate the remaining mail-domain
    /// methods (`Mailbox/changes`, `Mailbox/set`, `Email/*`,
    /// `Thread/*`) onto the same adapter.
    pub mailstore: Arc<SqliteMailStore>,
    /// Property-substrate runtime for `maild.domains`. Plumbed in
    /// Phase 3 commit 1 ahead of the `Identity/get` consumer that
    /// lands in commit 3. Commit 1 only stores the field; today's
    /// `Identity/get` continues to ignore it. See
    /// `_doc/planned/maild-multi-vhost-phase3.md`.
    pub domains_runtime: Arc<cosmix_props::Runtime>,
    /// Property-substrate runtime for `maild.aliases`. Used by
    /// `EmailSubmission/set` to resolve a local alias recipient before
    /// the local-vs-remote classification, so a JMAP client sending to
    /// an alias delivers locally instead of being queued outbound.
    pub aliases_runtime: Arc<cosmix_props::Runtime>,
    /// Per-account JMAP last-seen tracker for `maild.stats.online`.
    /// `touch`ed on every successful JMAP authentication so the stats
    /// surface can report "active on webmail in the last N min" — JMAP
    /// is stateless so there is no live connection to count. Shares one
    /// `Arc` with the Bus `StatsState`.
    pub jmap_activity: Arc<crate::bus::stats::JmapActivity>,
}

/// GET /.well-known/jmap — Session resource.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(account_id) = auth::authenticate(&state.db, &headers).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    };
    state.jmap_activity.touch(account_id);

    let acct = account_id.to_string();
    let mut capabilities = HashMap::new();
    capabilities.insert(
        CAPABILITY_CORE.into(),
        serde_json::json!({
            "maxSizeUpload": 50_000_000,
            "maxConcurrentUpload": 4,
            "maxSizeRequest": 10_000_000,
            "maxConcurrentRequests": 4,
            "maxCallsInRequest": 16,
            "maxObjectsInGet": 500,
            "maxObjectsInSet": 500,
        }),
    );
    capabilities.insert(CAPABILITY_MAIL.into(), serde_json::json!({}));
    capabilities.insert(CAPABILITY_SUBMISSION.into(), serde_json::json!({}));
    capabilities.insert(CAPABILITY_CALENDARS.into(), serde_json::json!({}));
    capabilities.insert(CAPABILITY_CONTACTS.into(), serde_json::json!({}));

    let mut account_caps = HashMap::new();
    account_caps.insert(CAPABILITY_MAIL.into(), serde_json::json!({}));
    account_caps.insert(CAPABILITY_SUBMISSION.into(), serde_json::json!({}));
    account_caps.insert(CAPABILITY_CALENDARS.into(), serde_json::json!({}));
    account_caps.insert(CAPABILITY_CONTACTS.into(), serde_json::json!({}));

    let mut accounts = HashMap::new();
    accounts.insert(
        acct.clone(),
        AccountInfo {
            name: format!("Account {acct}"),
            is_personal: true,
            is_read_only: false,
            account_capabilities: account_caps,
        },
    );

    let mut primary = HashMap::new();
    primary.insert(CAPABILITY_MAIL.into(), acct.clone());
    primary.insert(CAPABILITY_SUBMISSION.into(), acct.clone());
    primary.insert(CAPABILITY_CALENDARS.into(), acct.clone());
    primary.insert(CAPABILITY_CONTACTS.into(), acct);

    let state_val = "0".to_string();

    let session = Session {
        capabilities,
        accounts,
        primary_accounts: primary,
        username: String::new(),
        api_url: format!("{}/jmap", state.base_url),
        download_url: format!("{}/jmap/blob/{{blobId}}", state.base_url),
        upload_url: format!("{}/jmap/upload/{{accountId}}", state.base_url),
        event_source_url: format!("{}/jmap/eventsource", state.base_url),
        state: state_val,
    };

    Json(session).into_response()
}

/// POST /jmap — JMAP request endpoint.
pub async fn api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<JmapRequest>,
) -> impl IntoResponse {
    let Some(account_id) = auth::authenticate(&state.db, &headers).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    };
    state.jmap_activity.touch(account_id);

    let mut responses = Vec::new();

    for MethodCall(method, args, call_id) in &request.method_calls {
        // RFC 8620 §3.7: resolve `#`-prefixed ResultReferences against the
        // results produced earlier in this request, before the handler runs.
        let mut resolved_args = args.clone();
        if let Err(ref_err) = references::resolve_result_references(&mut resolved_args, &responses)
        {
            responses.push(MethodResponse(
                "error".into(),
                serde_json::to_value(JmapError {
                    error_type: ref_err.error_type,
                    description: ref_err.description,
                })
                .unwrap(),
                call_id.clone(),
            ));
            continue;
        }

        let result = dispatch(
            &state.db,
            &state.mailstore,
            &state.domains_runtime,
            &state.aliases_runtime,
            account_id,
            method,
            resolved_args,
            &state.classifier,
        )
        .await;

        match result {
            Ok(value) => {
                // Emit state change for mutating methods.
                //
                // Phase 6 (JMAP→MDS migration): the `Email` state-token
                // domain migrated to `MailStore::email_state` in Phase 3
                // (Tasks 3.3–3.5b) — `Email/set` and `Email/import` no
                // longer advance the legacy `db::changelog` "Email"
                // channel, so reading `current_state(_, "Email")` here
                // would yield a frozen value and EventSource clients
                // would never receive a post-mutation push (the
                // `changed[Email]` field of the SSE event would repeat
                // the pre-mutation state). All other types
                // (Mailbox / EmailSubmission / VacationResponse /
                // Identity / Calendar / Contact / ...) still flow
                // through `db::changelog` until their own migration
                // tasks land.
                if method.ends_with("/set") || method.ends_with("/import") {
                    let object_type = method.split('/').next().unwrap_or("");
                    let new_state_opt: Option<String> = if object_type == "Email" {
                        let ms = state.mailstore.clone();
                        match tokio::task::spawn_blocking(move || ms.email_state(account_id)).await
                        {
                            Ok(Ok(v)) => Some(v.to_string()),
                            _ => None,
                        }
                    } else {
                        db::changelog::current_state(&state.db.conn, account_id, object_type)
                            .await
                            .ok()
                    };
                    if let Some(new_state) = new_state_opt {
                        let _ = state.state_tx.send(StateChange {
                            account_id,
                            object_type: object_type.to_string(),
                            new_state,
                        });
                    }
                }
                responses.push(MethodResponse(method.clone(), value, call_id.clone()));
            }
            Err(e) => {
                // RFC 8620 §3.6.1: method-level errors emit
                // `["error", err, callId]` — the method name in the
                // triple is the literal `"error"`, NOT the requesting
                // method, otherwise clients route the error payload
                // into the method's success-type decoder.
                //
                // Handlers can opt into typed JMAP errors (e.g.
                // `cannotCalculateChanges`, `invalidArguments`) by
                // returning `Err(JmapMethodError { ... }.into())`;
                // the downcast below preserves their `error_type`
                // verbatim. Anything else falls through to the
                // catch-all `serverFail`.
                let err = if let Some(method_err) = e.downcast_ref::<JmapMethodError>() {
                    JmapError {
                        error_type: method_err.error_type.clone(),
                        description: method_err.description.clone(),
                    }
                } else {
                    JmapError {
                        error_type: "serverFail".into(),
                        description: Some(e.to_string()),
                    }
                };
                responses.push(MethodResponse(
                    "error".into(),
                    serde_json::to_value(err).unwrap(),
                    call_id.clone(),
                ));
            }
        }
    }

    let resp = JmapResponse {
        method_responses: responses,
        session_state: "0".into(),
    };

    Json(resp).into_response()
}

/// Dispatch a JMAP method call.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    domains_runtime: &Arc<cosmix_props::Runtime>,
    aliases_runtime: &Arc<cosmix_props::Runtime>,
    account_id: i32,
    method: &str,
    args: serde_json::Value,
    classifier: &Arc<cosmix_maild_bayesian::DefaultClassifier>,
) -> Result<serde_json::Value> {
    match method {
        "Core/echo" => Ok(args),

        "Mailbox/get" => mailbox::get(db, mailstore, account_id, args).await,
        "Mailbox/query" => mailbox::query(db, mailstore, account_id, args).await,
        "Mailbox/set" => mailbox::set(db, mailstore, account_id, args).await,
        "Mailbox/changes" => mailbox::changes(db, mailstore, account_id, args).await,

        "Email/get" => email::get(db, mailstore, account_id, args).await,
        "Email/query" => email::query(db, mailstore, account_id, args).await,
        "Email/queryChanges" => email::query_changes(db, mailstore, account_id, args).await,
        "Email/set" => email::set(db, mailstore, account_id, args, classifier).await,
        "Email/changes" => email::changes(mailstore, account_id, args).await,
        "Email/import" => email::import(db, mailstore, account_id, args).await,

        "Thread/get" => thread::get(db, mailstore, account_id, args).await,
        "Thread/changes" => thread::changes(db, account_id, args).await,

        "EmailSubmission/get" => submission::get(db, account_id, args).await,
        "EmailSubmission/changes" => submission::changes(db, account_id, args).await,

        "Identity/get" => {
            submission::identity_get(db, domains_runtime.clone(), account_id, args).await
        }
        "EmailSubmission/set" => {
            submission::set(db, mailstore, aliases_runtime, account_id, args).await
        }

        "Calendar/get" => calendar::get(db, account_id, args).await,
        "Calendar/query" => calendar::query(db, account_id, args).await,
        "Calendar/set" => calendar::set(db, account_id, args).await,
        "Calendar/changes" => calendar::changes(db, account_id, args).await,

        "CalendarEvent/get" => calendar::event_get(db, account_id, args).await,
        "CalendarEvent/query" => calendar::event_query(db, account_id, args).await,
        "CalendarEvent/set" => calendar::event_set(db, account_id, args).await,
        "CalendarEvent/changes" => calendar::event_changes(db, account_id, args).await,

        "AddressBook/get" => contact::get(db, account_id, args).await,
        "AddressBook/query" => contact::query(db, account_id, args).await,
        "AddressBook/set" => contact::set(db, account_id, args).await,
        "AddressBook/changes" => contact::changes(db, account_id, args).await,

        "Contact/get" => contact::contact_get(db, account_id, args).await,
        "Contact/query" => contact::contact_query(db, account_id, args).await,
        "Contact/set" => contact::contact_set(db, account_id, args).await,
        "Contact/changes" => contact::contact_changes(db, account_id, args).await,

        "VacationResponse/get" => vacation::get(db, account_id, args).await,
        "VacationResponse/set" => vacation::set(db, account_id, args).await,

        // RFC 8620 §3.6.1: an unknown method MUST be reported via the
        // `["error", {type:"unknownMethod"}, callId]` triple — emitting
        // `Ok(JmapError-shaped JSON)` (the previous implementation)
        // routes the payload into the `MethodResponse(method, ...)`
        // success-triple position, which clients decode as the
        // method's success type. The `Err(JmapMethodError)` path
        // routes through the `Err` arm in `api()` (see `mod.rs:163+`)
        // which produces the correct `["error", ...]` triple via
        // downcast. Do not regress this when adding new typed errors.
        _ => Err(JmapMethodError {
            error_type: "unknownMethod".into(),
            description: Some(format!("Unknown method: {method}")),
        }
        .into()),
    }
}

/// GET /jmap/blob/{blob_id} — Download a blob.
///
/// `blob_id` can be either:
///   - a legacy per-account UUID (issued by `db::blob::store` when
///     pre-migration uploads / inbound delivery created the row), or
///   - a 64-character lowercase-hex CAS `BlobHash` (the post-
///     migration form returned by `Email/get` — see
///     `jmap/email.rs:record_to_jmap`).
///
/// The handler tries UUID first; on parse failure it falls back to
/// `cosmix_mds::blob::from_hex` for the CAS form. This dual-path
/// matching is the bridge while the upload path migrates off
/// `db::blob` (Task 3.3a, `_doc/planned/jmap-mds-migration.md`);
/// once that lands, both upload and download share the mds CAS
/// surface and the legacy UUID branch can be retired.
pub async fn blob_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(blob_id): Path<String>,
) -> impl IntoResponse {
    let Some(account_id) = auth::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };

    if let Ok(id) = blob_id.parse::<uuid::Uuid>() {
        // UUID can be either a post-Task-3.3a JMAP `BlobId` alias
        // (resolves through MailStore via `blob_refs`) or a legacy
        // `db::blob` UUID (pre-migration uploads / SMTP inbound that
        // still write the legacy row). Try the MailStore path first;
        // fall through to the legacy lookup only on "not found" so a
        // legitimate alias-existence row never leaks into the legacy
        // fallback. Both lookups are account-scoped, so foreign ids
        // surface as 404 from whichever branch ends up resolving.
        let ms = state.mailstore.clone();
        let alias_id = crate::mailstore::BlobId(id);
        let alias =
            tokio::task::spawn_blocking(move || ms.resolve_blob_ref(account_id, alias_id)).await;
        match alias {
            Ok(Ok(crate::mailstore::BlobRefLookup::Found(hash))) => {
                let mds = state.mailstore.mds().clone();
                let blob = tokio::task::spawn_blocking(move || mds.get_blob(&hash)).await;
                return match blob {
                    Ok(Ok(data)) => (
                        StatusCode::OK,
                        [("content-type", "application/octet-stream")],
                        data,
                    )
                        .into_response(),
                    Ok(Err(e)) => {
                        // CAS read failed even though the alias and
                        // CAS-existence check passed inside the same
                        // transaction. Treat as a transient read
                        // error rather than 404: the alias was
                        // legitimately minted, so a 500 is the right
                        // signal for ops to investigate.
                        tracing::error!(blob_id = %blob_id, error = %e, "alias-resolved CAS fetch failed");
                        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
                    }
                    Err(join_err) => {
                        tracing::error!(error = %join_err, "blob fetch task panicked");
                        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
                    }
                };
            }
            Ok(Ok(crate::mailstore::BlobRefLookup::NotFound)) => {
                // Alias row absent — fall through to the legacy
                // db::blob lookup. This is the only case where
                // legacy-fallback is correct.
            }
            Ok(Ok(crate::mailstore::BlobRefLookup::Dangling)) => {
                // Alias row exists but the underlying CAS bytes are
                // gone (staging GC, core MDS GC, or operator
                // surgery). The client got this BlobId from us, so
                // legacy fallback would be incorrect — surface as
                // a real 404.
                tracing::warn!(blob_id = %blob_id, "alias resolved but CAS blob is dangling");
                return (StatusCode::NOT_FOUND, "blob not found").into_response();
            }
            Ok(Err(e)) => {
                // Internal error (SQLite, malformed hex). Do NOT
                // fall through to legacy — a 500 is the right
                // signal for ops to investigate.
                tracing::error!(blob_id = %blob_id, error = %e, "alias resolve internal error");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
            Err(join_err) => {
                tracing::error!(error = %join_err, "alias resolve task panicked");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        }

        // Legacy fallback: pre-migration `db::blob` UUID lookup.
        // Account-scoped: a row mismatch on `(id, account_id)` is
        // treated identically to "no such id" — both surface as 404
        // to avoid revealing whether someone else's blob exists.
        return match db::blob::load_for_account(&state.db.conn, &state.db.blob_dir, account_id, id)
            .await
        {
            Ok(Some(data)) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                data,
            )
                .into_response(),
            Ok(None) => (StatusCode::NOT_FOUND, "blob not found").into_response(),
            Err(e) => {
                tracing::error!(error = %e, "legacy blob load failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        };
    }

    let Some(hash) = cosmix_mds::blob::from_hex(&blob_id) else {
        return (StatusCode::BAD_REQUEST, "invalid blob id").into_response();
    };

    // Account ownership gate. CAS hashes are deduplicated across
    // accounts in the mds blob store, so a global `mds.get_blob(&hash)`
    // would let any authenticated user retrieve bytes by hash
    // regardless of mailbox membership. We require a `blobs` row
    // binding `(account_id, hash)` — written when this account either
    // uploaded the bytes (`/upload`) or received them via SMTP. A
    // missing row returns 404 (not 403) to avoid confirming that the
    // hash exists somewhere on the server.
    match db::blob::hash_owned_by_account(&state.db.conn, account_id, &blob_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "blob not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "CAS ownership lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }

    let mds = state.mailstore.mds().clone();
    let blob = tokio::task::spawn_blocking(move || mds.get_blob(&hash)).await;
    match blob {
        Ok(Ok(data)) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(Err(e)) => {
            // mds `get_blob` returns an error when the CAS file is
            // missing OR on IO error. The two cases are not
            // currently distinguished in the error type; logging
            // surfaces the underlying string for ops, while the
            // client sees a uniform 404 to avoid leaking storage
            // shape. Tighten to 500 vs 404 once the mds error type
            // splits the two.
            tracing::warn!(blob_id = %blob_id, error = %e, "CAS blob fetch failed");
            (StatusCode::NOT_FOUND, "blob not found").into_response()
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "blob fetch task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// POST /jmap/upload/{account_id} — Upload a blob.
///
/// Migrated to MailStore CAS in Task 3.3a:
///   1. `mds.put_blob(&body)` writes the bytes content-addressed,
///      returning a `BlobHash`. Idempotent on hash.
///   2. `mailstore.create_blob_ref` mints a JMAP `BlobId` alias
///      backed by a temp item in the per-account
///      `__upload_staging__` container with `expires_at = now + 1h`
///      (per `mailstore::expiry` v1.1 §1).
///
/// Migration-window dual-write: a `db::blob` row is also inserted so
/// the legacy `blob_download` CAS-hex gate (`hash_owned_by_account`)
/// keeps working unchanged. Per
/// `_doc/planned/jmap-mds-migration.md` §3.3a option (a), this is
/// the lower-risk path while the legacy `/download` UUID branch is
/// still serving traffic. The dual-write retires when the gate
/// migrates to an mds-side ownership oracle.
pub async fn blob_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_acct_id): Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(account_id) = auth::authenticate(&state.db, &headers).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    };

    let size = body.len();
    let bytes = body.to_vec();

    // Step 1: write to MDS CAS (idempotent on hash). Sync API; spawn_blocking.
    let mds = state.mailstore.mds().clone();
    let bytes_for_mds = bytes.clone();
    let blob_hash = match tokio::task::spawn_blocking(move || mds.put_blob(&bytes_for_mds)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "blob upload: put_blob failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "upload failed"})),
            )
                .into_response();
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "blob upload: put_blob task panicked");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "upload failed"})),
            )
                .into_response();
        }
    };

    // Step 2: mint the JMAP `BlobId` alias under the upload-staging
    // container with a 1-hour TTL (v1.1 §1). The expiry sweep will
    // reap the alias if the client never references it from
    // `Email/set { create }` / `Email/import`.
    let ms = state.mailstore.clone();
    let hash_for_ref = blob_hash;
    let blob_id = match tokio::task::spawn_blocking(move || {
        let expires_at = chrono::Utc::now().timestamp() + 3600;
        ms.create_blob_ref(account_id, hash_for_ref, size as u64, expires_at)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "blob upload: create_blob_ref failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "upload failed"})),
            )
                .into_response();
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "blob upload: create_blob_ref task panicked");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "upload failed"})),
            )
                .into_response();
        }
    };

    // Step 3: dual-write a `db::blob` row binding `(account_id,
    // hash)` so the legacy `/download` CAS-hex gate
    // (`db::blob::hash_owned_by_account`) keeps resolving for blobs
    // uploaded via this path. The UUID returned by `db::blob::store`
    // is intentionally discarded — it is not exposed to the client;
    // only the new MailStore `BlobId` is returned. This dual-write
    // retires when the `/download` gate migrates to an mds-side
    // ownership oracle (`_doc/planned/jmap-mds-migration.md` §3.3a
    // option (b)).
    if let Err(e) = db::blob::store(&state.db.conn, &state.db.blob_dir, account_id, &bytes).await {
        tracing::error!(error = %e, "blob upload: db::blob dual-write failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "upload failed"})),
        )
            .into_response();
    }

    let resp = serde_json::json!({
        "accountId": account_id.to_string(),
        "blobId": blob_id.to_string(),
        "type": "application/octet-stream",
        "size": size,
    });
    (StatusCode::CREATED, Json(resp)).into_response()
}

/// EventSource query parameters (RFC 8620 §7.3).
#[derive(Debug, serde::Deserialize)]
pub struct EventSourceParams {
    /// Comma-separated list of "TypeName" to listen for, or "*" for all.
    pub types: Option<String>,
    /// If true, close the connection after the next event.
    #[serde(rename = "closeafter")]
    pub close_after: Option<String>,
    /// Client-requested ping interval in seconds.
    pub ping: Option<u64>,
}

/// GET /jmap/eventsource — Server-Sent Events for state changes (RFC 8620 §7.3).
pub async fn eventsource(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<EventSourceParams>,
) -> impl IntoResponse {
    let Some(account_id) = auth::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    state.jmap_activity.touch(account_id);

    let close_after_state = params.close_after.as_deref() == Some("state");
    let ping_secs = params.ping.unwrap_or(30).clamp(1, 300);

    // Parse which types to listen for
    let type_filter: Option<Vec<String>> = params
        .types
        .filter(|t| t != "*")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

    let mut rx = state.state_tx.subscribe();

    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(change) => {
                            if change.account_id != account_id {
                                continue;
                            }
                            if let Some(ref types) = type_filter
                                && !types.contains(&change.object_type) {
                                    continue;
                                }

                            let data = serde_json::json!({
                                "@type": "StateChange",
                                "changed": {
                                    change.account_id.to_string(): {
                                        &change.object_type: &change.new_state
                                    }
                                }
                            });

                            yield Ok::<_, std::convert::Infallible>(
                                Event::default()
                                    .event("state")
                                    .data(data.to_string())
                            );

                            if close_after_state {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(skipped = n, "EventSource client lagged");
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(ping_secs)) => {
                    yield Ok(Event::default().comment("ping"));
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(ping_secs)))
        .into_response()
}
