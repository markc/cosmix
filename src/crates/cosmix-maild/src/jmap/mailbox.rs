//! JMAP Mailbox methods (RFC 8621).

use anyhow::Result;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// Mailbox/get
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let mailboxes = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::mailbox::get_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        db::mailbox::get_all(&db.conn, account_id).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "Mailbox").await?;

    let resp = GetResponse {
        account_id: acct,
        state,
        list: mailboxes,
        not_found: vec![],
    };

    Ok(serde_json::to_value(resp)?)
}

/// Mailbox/query
pub async fn query(db: &Db, account_id: i32, _args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids = db::mailbox::query_ids(&db.conn, account_id).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "Mailbox").await?;

    let total = ids.len() as u64;
    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        position: 0,
        ids: ids.into_iter().map(|u| u.to_string()).collect(),
        total,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Mailbox/set
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "Mailbox").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();

    // Handle create
    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let parent_id = obj.get("parentId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Uuid>().ok());
            let role = obj.get("role").and_then(|v| v.as_str());

            match db::mailbox::create(&db.conn, account_id, name, parent_id, role).await {
                Ok(id) => {
                    db::changelog::record(&db.conn, account_id, "Mailbox", id, "created").await?;
                    created_map.insert(client_id.clone(), serde_json::json!({ "id": id.to_string() }));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Mailbox create failed");
                }
            }
        }
    }

    // Handle update
    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            let name = patch.get("name").and_then(|v| v.as_str());
            let sort_order = patch.get("sortOrder").and_then(|v| v.as_i64()).map(|v| v as i32);

            if db::mailbox::update(&db.conn, account_id, id, name, None, sort_order).await? {
                db::changelog::record(&db.conn, account_id, "Mailbox", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    // Handle destroy
    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else { continue };
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            if db::mailbox::delete(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "Mailbox", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "Mailbox").await?;

    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: if created_map.is_empty() { None } else { Some(created_map) },
        updated: if updated_map.is_empty() { None } else { Some(updated_map) },
        destroyed: if destroyed_list.is_empty() { None } else { Some(destroyed_list) },
        not_created: None,
        not_updated: None,
        not_destroyed: None,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Mailbox/changes
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);

    let result = db::changelog::changes_since(&db.conn, account_id, "Mailbox", since_state, max).await?;

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
