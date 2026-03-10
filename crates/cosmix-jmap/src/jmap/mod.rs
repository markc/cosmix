//! JMAP server (RFC 8620 + 8621).

pub mod calendar;
pub mod contact;
pub mod email;
pub mod mailbox;
pub mod submission;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

use crate::auth;
use crate::db::{self, Db};
use types::*;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub base_url: String,
}

/// GET /.well-known/jmap — Session resource.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(account_id) = auth::basic::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    };

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
    let Some(account_id) = auth::basic::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    };

    let mut responses = Vec::new();

    for MethodCall(method, args, call_id) in &request.method_calls {
        let result = dispatch(&state.db, account_id, method, args.clone()).await;

        match result {
            Ok(value) => {
                responses.push(MethodResponse(method.clone(), value, call_id.clone()));
            }
            Err(e) => {
                let err = JmapError {
                    error_type: "serverFail".into(),
                    description: Some(e.to_string()),
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
async fn dispatch(
    db: &Db,
    account_id: i32,
    method: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    match method {
        "Core/echo" => Ok(args),

        "Mailbox/get" => mailbox::get(db, account_id, args).await,
        "Mailbox/query" => mailbox::query(db, account_id, args).await,
        "Mailbox/set" => mailbox::set(db, account_id, args).await,
        "Mailbox/changes" => mailbox::changes(db, account_id, args).await,

        "Email/get" => email::get(db, account_id, args).await,
        "Email/query" => email::query(db, account_id, args).await,
        "Email/set" => email::set(db, account_id, args).await,
        "Email/changes" => email::changes(db, account_id, args).await,

        "Identity/get" => submission::identity_get(db, account_id, args).await,
        "EmailSubmission/set" => submission::set(db, account_id, args).await,

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

        _ => {
            let err = JmapError::method_not_found(method);
            Ok(serde_json::to_value(err)?)
        }
    }
}

/// GET /jmap/blob/{blob_id} — Download a blob.
pub async fn blob_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(blob_id): Path<String>,
) -> impl IntoResponse {
    let Some(_account_id) = auth::basic::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };

    let Ok(id) = blob_id.parse::<uuid::Uuid>() else {
        return (StatusCode::BAD_REQUEST, "invalid blob id").into_response();
    };

    match db::blob::load(&state.db.pool, &state.db.blob_dir, id).await {
        Ok(Some(data)) => {
            (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                data,
            )
                .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "blob not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "blob load failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// POST /jmap/upload/{account_id} — Upload a blob.
pub async fn blob_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_acct_id): Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(account_id) = auth::basic::authenticate(&state.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    };

    match db::blob::store(&state.db.pool, &state.db.blob_dir, account_id, &body).await {
        Ok(blob_id) => {
            let resp = serde_json::json!({
                "accountId": account_id.to_string(),
                "blobId": blob_id.to_string(),
                "type": "application/octet-stream",
                "size": body.len(),
            });
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "blob upload failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "upload failed"}))).into_response()
        }
    }
}
