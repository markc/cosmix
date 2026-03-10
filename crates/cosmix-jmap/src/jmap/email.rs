//! JMAP Email methods (RFC 8621).

use anyhow::Result;
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// Email/get
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let emails = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::email::get_by_ids(&db.pool, account_id, &uuids).await?
    } else {
        // Return all (with reasonable limit)
        let (ids, _) = db::email::query_ids(&db.pool, account_id, None, true, 0, 100).await?;
        db::email::get_by_ids(&db.pool, account_id, &ids).await?
    };

    let state = db::changelog::current_state(&db.pool, account_id, "Email").await?;

    let resp = GetResponse {
        account_id: acct,
        state,
        list: emails,
        not_found: vec![],
    };

    Ok(serde_json::to_value(resp)?)
}

/// Email/query
pub async fn query(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    #[derive(Deserialize)]
    struct Filter {
        #[serde(rename = "inMailbox")]
        in_mailbox: Option<String>,
    }

    let filter: Option<Filter> = args.get("filter")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let mailbox_id = filter
        .and_then(|f| f.in_mailbox)
        .and_then(|s| s.parse::<Uuid>().ok());

    let position = args.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as i64;

    let (ids, total) = db::email::query_ids(&db.pool, account_id, mailbox_id, true, position, limit).await?;
    let state = db::changelog::current_state(&db.pool, account_id, "Email").await?;

    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        position: position as u64,
        ids: ids.into_iter().map(|u| u.to_string()).collect(),
        total: total as u64,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Email/set
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.pool, account_id, "Email").await?;

    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();
    let mut not_updated = std::collections::HashMap::new();
    let mut not_destroyed = std::collections::HashMap::new();

    // Handle updates (keywords, mailboxIds)
    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else {
                not_updated.insert(id_str.clone(), SetError {
                    error_type: "invalidArguments".into(),
                    description: Some("Invalid id".into()),
                });
                continue;
            };

            let mut changed = false;

            if let Some(keywords) = patch.get("keywords") {
                if db::email::update_keywords(&db.pool, account_id, id, keywords).await? {
                    changed = true;
                }
            }

            if let Some(mailbox_ids) = patch.get("mailboxIds") {
                if let Some(obj) = mailbox_ids.as_object() {
                    let mbox_uuids: Vec<Uuid> = obj.keys()
                        .filter_map(|k| k.parse().ok())
                        .collect();
                    if db::email::update_mailboxes(&db.pool, account_id, id, &mbox_uuids).await? {
                        changed = true;
                    }
                }
            }

            if changed {
                db::changelog::record(&db.pool, account_id, "Email", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    // Handle destroy
    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else { continue };
            let Ok(id) = id_str.parse::<Uuid>() else {
                not_destroyed.insert(id_str.to_string(), SetError {
                    error_type: "notFound".into(),
                    description: None,
                });
                continue;
            };

            if db::email::delete(&db.pool, account_id, id).await? {
                db::changelog::record(&db.pool, account_id, "Email", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            } else {
                not_destroyed.insert(id_str.to_string(), SetError {
                    error_type: "notFound".into(),
                    description: None,
                });
            }
        }
    }

    let new_state = db::changelog::current_state(&db.pool, account_id, "Email").await?;

    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: None,
        updated: if updated_map.is_empty() { None } else { Some(updated_map.into_iter().map(|(k, v)| (k, v)).collect()) },
        destroyed: if destroyed_list.is_empty() { None } else { Some(destroyed_list) },
        not_created: None,
        not_updated: if not_updated.is_empty() { None } else { Some(not_updated) },
        not_destroyed: if not_destroyed.is_empty() { None } else { Some(not_destroyed) },
    };

    Ok(serde_json::to_value(resp)?)
}

/// Email/changes
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);

    let result = db::changelog::changes_since(&db.pool, account_id, "Email", since_state, max).await?;

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
