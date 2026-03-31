//! JMAP Calendar + CalendarEvent methods (JSCalendar, RFC 8984).

use anyhow::Result;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// Calendar/get
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let calendars = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::calendar::get_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        db::calendar::get_all(&db.conn, account_id).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "Calendar").await?;
    let resp = GetResponse {
        account_id: acct,
        state,
        list: calendars,
        not_found: vec![],
    };
    Ok(serde_json::to_value(resp)?)
}

/// Calendar/query
pub async fn query(db: &Db, account_id: i32, _args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids = db::calendar::query_calendar_ids(&db.conn, account_id).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "Calendar").await?;
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

/// Calendar/set
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "Calendar").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();

    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let color = obj.get("color").and_then(|v| v.as_str());
            let description = obj.get("description").and_then(|v| v.as_str());
            let timezone = obj.get("timezone").and_then(|v| v.as_str());

            match db::calendar::create_calendar(&db.conn, account_id, name, color, description, timezone).await {
                Ok(id) => {
                    db::changelog::record(&db.conn, account_id, "Calendar", id, "created").await?;
                    created_map.insert(client_id.clone(), serde_json::json!({ "id": id.to_string() }));
                }
                Err(e) => tracing::warn!(error = %e, "Calendar create failed"),
            }
        }
    }

    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            if db::calendar::update_calendar(&db.conn, account_id, id, patch).await? {
                db::changelog::record(&db.conn, account_id, "Calendar", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else { continue };
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            if db::calendar::delete_calendar(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "Calendar", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "Calendar").await?;
    let resp = SetResponse {
        account_id: acct,
        old_state, new_state,
        created: if created_map.is_empty() { None } else { Some(created_map) },
        updated: if updated_map.is_empty() { None } else { Some(updated_map) },
        destroyed: if destroyed_list.is_empty() { None } else { Some(destroyed_list) },
        not_created: None, not_updated: None, not_destroyed: None,
    };
    Ok(serde_json::to_value(resp)?)
}

/// Calendar/changes
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState").and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);
    let result = db::changelog::changes_since(&db.conn, account_id, "Calendar", since_state, max).await?;
    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created, updated: result.updated, destroyed: result.destroyed,
    };
    Ok(serde_json::to_value(resp)?)
}

/// CalendarEvent/get
pub async fn event_get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let events = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::calendar::get_events_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        db::calendar::get_all_events(&db.conn, account_id, 100).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "CalendarEvent").await?;
    let resp = GetResponse {
        account_id: acct,
        state,
        list: events,
        not_found: vec![],
    };
    Ok(serde_json::to_value(resp)?)
}

/// CalendarEvent/query
pub async fn event_query(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let filter = args.get("filter");
    let calendar_id = filter
        .and_then(|f| f.get("calendarId"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok());
    let after = filter
        .and_then(|f| f.get("after"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
    let before = filter
        .and_then(|f| f.get("before"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
    let position = args.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as i64;

    let (ids, total) = db::calendar::query_event_ids(&db.conn, account_id, calendar_id, after, before, position, limit).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "CalendarEvent").await?;
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

/// CalendarEvent/set
pub async fn event_set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "CalendarEvent").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();

    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let calendar_id = obj.get("calendarId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Uuid>().ok());
            let Some(calendar_id) = calendar_id else { continue };

            let uid = obj.get("uid").and_then(|v| v.as_str())
                .unwrap_or(&Uuid::new_v4().to_string())
                .to_string();
            let title = obj.get("title").and_then(|v| v.as_str());
            let start_dt = obj.get("start").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
            let end_dt = obj.get("end").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

            // The full JSCalendar object is the data
            match db::calendar::create_event(&db.conn, account_id, calendar_id, &uid, obj, title, start_dt, end_dt).await {
                Ok(id) => {
                    db::changelog::record(&db.conn, account_id, "CalendarEvent", id, "created").await?;
                    created_map.insert(client_id.clone(), serde_json::json!({ "id": id.to_string(), "uid": uid }));
                }
                Err(e) => tracing::warn!(error = %e, "CalendarEvent create failed"),
            }
        }
    }

    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            let title = patch.get("title").and_then(|v| v.as_str());
            let start_dt = patch.get("start").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
            let end_dt = patch.get("end").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

            if db::calendar::update_event(&db.conn, account_id, id, patch, title, start_dt, end_dt).await? {
                db::changelog::record(&db.conn, account_id, "CalendarEvent", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else { continue };
            let Ok(id) = id_str.parse::<Uuid>() else { continue };
            if db::calendar::delete_event(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "CalendarEvent", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "CalendarEvent").await?;
    let resp = SetResponse {
        account_id: acct,
        old_state, new_state,
        created: if created_map.is_empty() { None } else { Some(created_map) },
        updated: if updated_map.is_empty() { None } else { Some(updated_map) },
        destroyed: if destroyed_list.is_empty() { None } else { Some(destroyed_list) },
        not_created: None, not_updated: None, not_destroyed: None,
    };
    Ok(serde_json::to_value(resp)?)
}

/// CalendarEvent/changes
pub async fn event_changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState").and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);
    let result = db::changelog::changes_since(&db.conn, account_id, "CalendarEvent", since_state, max).await?;
    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created, updated: result.updated, destroyed: result.destroyed,
    };
    Ok(serde_json::to_value(resp)?)
}
