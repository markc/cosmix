//! JMAP VacationResponse methods (RFC 8621 §8).

use anyhow::Result;

use crate::db::{self, Db};
use super::types::*;

/// VacationResponse/get — returns the singleton vacation config.
pub async fn get(db: &Db, account_id: i32, _args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let vr = db::vacation::get(&db.conn, account_id).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "VacationResponse").await?;

    Ok(serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": [vr],
        "notFound": [],
    }))
}

/// VacationResponse/set — update the singleton vacation config.
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "VacationResponse").await?;

    let mut updated_map = std::collections::HashMap::new();

    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id, patch) in update {
            if id != "singleton" {
                continue;
            }

            // Load current, apply patch
            let mut vr = db::vacation::get(&db.conn, account_id).await?;

            if let Some(v) = patch.get("isEnabled").and_then(|v| v.as_bool()) {
                vr.is_enabled = v;
            }
            if let Some(v) = patch.get("fromDate") {
                vr.from_date = v.as_str().map(|s| s.to_string());
            }
            if let Some(v) = patch.get("toDate") {
                vr.to_date = v.as_str().map(|s| s.to_string());
            }
            if let Some(v) = patch.get("subject") {
                vr.subject = v.as_str().map(|s| s.to_string());
            }
            if let Some(v) = patch.get("textBody") {
                vr.text_body = v.as_str().map(|s| s.to_string());
            }
            if let Some(v) = patch.get("htmlBody") {
                vr.html_body = v.as_str().map(|s| s.to_string());
            }

            db::vacation::upsert(&db.conn, account_id, &vr).await?;

            let vr_id = uuid::Uuid::new_v4();
            db::changelog::record(&db.conn, account_id, "VacationResponse", vr_id, "updated").await?;
            updated_map.insert(id.clone(), serde_json::Value::Null);
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "VacationResponse").await?;

    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: None,
        updated: if updated_map.is_empty() { None } else { Some(updated_map.into_iter().collect()) },
        destroyed: None,
        not_created: None,
        not_updated: None,
        not_destroyed: None,
    };

    Ok(serde_json::to_value(resp)?)
}
