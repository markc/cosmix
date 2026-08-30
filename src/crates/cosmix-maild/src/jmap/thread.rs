//! JMAP Thread methods (RFC 8621 §4).

use std::sync::Arc;

use anyhow::Result;

use super::email::type_name_of_json;
use super::types::*;
use crate::db::{self, Db};
use crate::mailstore::{MailStore, SqliteMailStore, ThreadId};

/// Thread/get — returns thread objects with emailIds.
///
/// **Phase 4 / Task 4.2 status (current).** Data path runs on
/// `MailStore::list_thread{,s}` reading the `mail_threads` sidecar
/// (Task 4.1a populates it from JMAP create paths). Member ordering
/// is now `mail_envelopes.date_ts` ASC (RFC 5322 origination per RFC
/// 8621 §4.2), not `received_at` (delivery time) as the legacy
/// `db::thread::get_by_ids` did — see the `MailStore::list_thread`
/// doc-comment for the bucketed-fallback rules for items without an
/// envelope row. The substrate test
/// `list_thread_three_emails_same_chain_returned_in_date_order`
/// pins the new ordering contract.
///
/// **State token still flows through the legacy `"Thread"` changelog
/// channel.** This is deliberate — `Thread/changes` continues to read
/// `db::changelog::changes_since(... "Thread" ...)`, and the legacy
/// channel has never had a write-side bridge (legacy `db::email::create`
/// recorded as `"Email"`, not `"Thread"`), so `Thread/changes` returns
/// empty and `Thread/get.state` stays at `0` forever. Pinning both
/// reads to the same channel guarantees cross-method token
/// compatibility (a `Thread/get.state` round-tripped into
/// `Thread/changes.sinceState` will not surprise clients with
/// `cannotCalculateChanges` from a state advance the changelog never
/// recorded). A future task can add MailStore-side write bridging or
/// migrate both surfaces to `email_state` once `Thread/changes` gets
/// a real implementation; that work is deliberately deferred so
/// Task 4.2 stays a data-path-only migration.
///
/// **Migration window for SMTP-delivered mail.** Phase 5 has not
/// migrated `smtp/inbound.rs` yet, so SMTP-delivered emails land in
/// the legacy `db::email`/`db::thread` tables and are NOT surfaced by
/// this method until Phase 5 routes SMTP delivery through
/// `MailStore::create_email` (which writes `mail_threads`). Accepted
/// per the migration-plan ordering — Phase 4 ships before Phase 5,
/// and the regression closes when Phase 5 lands.
pub async fn get(
    db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    // RFC 8620 §3.7: a wrong-shape `ids` argument is `invalidArguments`,
    // not silently treated as absent (which would over-fetch the
    // unspecified-ids branch). Strict match — same pattern email::get
    // uses for `properties` (and the inverse fix for the legacy
    // `serde_json::from_value(...).ok()` swallow that hid e.g.
    // `{"ids": "bad"}` or `{"ids": [1, 2]}`).
    let ids: Option<Vec<String>> = match args.get("ids") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(arr)) => {
            let mut v = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    serde_json::Value::String(s) => v.push(s.clone()),
                    other => {
                        return Err(JmapMethodError {
                            error_type: "invalidArguments".into(),
                            description: Some(format!(
                                "ids[] entry not a string: {}",
                                type_name_of_json(other)
                            )),
                        }
                        .into());
                    }
                }
            }
            Some(v)
        }
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!("ids not an array: {}", type_name_of_json(other))),
            }
            .into());
        }
    };
    let ids_explicit = ids.is_some();

    // Resolve the request's thread-id list. Explicit `ids` are taken
    // verbatim — `mail_threads.thread_id` is `TEXT` and the format is
    // opaque (cosmix-mds v1.1 schema §6), so a non-UUID string is not
    // structurally invalid; it just won't match any rows. RFC 8620
    // §5.1 requires unknown ids surface in `notFound`, populated below.
    //
    // For `ids: null` (or absent), fall back to `list_threads` paginated
    // listing capped at 100 — matches the legacy `db::thread::query_ids`
    // LIMIT 100 contract preserved by `MailStore::list_threads`.
    let mailstore = mailstore.clone();
    let (list, not_found) = tokio::task::spawn_blocking(move || -> Result<_> {
        let request_threads: Vec<ThreadId> = if let Some(ids) = ids {
            ids.into_iter().map(ThreadId).collect()
        } else {
            mailstore.list_threads(account_id, 0, 100)?
        };

        let mut list: Vec<serde_json::Value> = Vec::with_capacity(request_threads.len());
        let mut not_found: Vec<String> = Vec::new();

        for tid in request_threads {
            let items = match mailstore.list_thread(account_id, tid.clone()) {
                Ok(v) => v,
                // An unprovisioned account has no observable threads;
                // explicit ids that hit it must surface in `notFound`,
                // not as a transport error. The no-ids branch already
                // returns `Vec::new()` for unprovisioned accounts via
                // `list_threads`'s `SetNotFound → empty` mapping, so
                // this only fires when `ids_explicit` and the set is
                // missing — exactly the RFC 8620 §5.1 unknown-id case.
                Err(e) if e.to_string().starts_with("mailbox not found") => Vec::new(),
                Err(e) => return Err(e),
            };

            if items.is_empty() {
                if ids_explicit {
                    not_found.push(tid.0);
                }
                continue;
            }

            list.push(serde_json::json!({
                "id": tid.0,
                "emailIds": items.iter().map(|i| i.0.to_string()).collect::<Vec<_>>(),
            }));
        }

        Ok((list, not_found))
    })
    .await??;

    // Deferred-by-design (Task 4.2): Thread state-token domain stays
    // on db::changelog for cross-method consistency with
    // Thread/changes; both surfaces will migrate together when a
    // MailStore-backed thread-projection stream lands.
    let state = db::changelog::current_state(&db.conn, account_id, "Thread").await?;

    Ok(serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": list,
        "notFound": not_found,
    }))
}

/// Thread/changes — returns thread changes since a given state.
///
/// **Phase 4 status:** Token domain remains the legacy `"Thread"`
/// changelog channel for cross-method consistency with `Thread/get`
/// (which also reads its `state` from this channel). The channel
/// has never had a write-side bridge — legacy `db::email::create`
/// recorded mutations as `"Email"`, not `"Thread"` — so this method
/// returns empty changes today, preserving the pre-migration
/// behaviour that clients are already coded against. Future work:
/// either bridge MailStore mail mutations into the legacy `"Thread"`
/// channel, or migrate both `Thread/get.state` and
/// `Thread/changes.sinceState` onto a fresh MailStore-backed surface
/// (e.g. `email_state` plus a thread-projection stream). Both
/// options are deliberately out of scope for Task 4.2.
pub async fn changes(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args
        .get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let max = args
        .get("maxChanges")
        .and_then(|v| v.as_i64())
        .unwrap_or(500);

    // Deferred-by-design (Task 4.2): see Thread/get for rationale.
    let result =
        db::changelog::changes_since(&db.conn, account_id, "Thread", since_state, max).await?;

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
        updated_properties: None,
    };

    Ok(serde_json::to_value(resp)?)
}
