//! JMAP Thread methods (RFC 8621 §4).

use anyhow::Result;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// Thread/get — returns thread objects with emailIds.
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let threads = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::thread::get_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        // Return all threads (with pagination)
        let (ids, _) = db::thread::query_ids(&db.conn, account_id, 0, 100).await?;
        db::thread::get_by_ids(&db.conn, account_id, &ids).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "Thread").await?;

    let resp = serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": threads,
        "notFound": [],
    });

    Ok(resp)
}

/// Thread/changes — returns thread changes since a given state.
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);

    let result = db::changelog::changes_since(&db.conn, account_id, "Thread", since_state, max).await?;

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
    };

    Ok(serde_json::to_value(resp)?)
}
