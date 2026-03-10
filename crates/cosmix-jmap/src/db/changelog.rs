//! JMAP change tracking (modification sequences).

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Record a change and return the new state (modseq).
pub async fn record(
    pool: &PgPool,
    account_id: i32,
    object_type: &str,
    object_id: Uuid,
    change_type: &str,
) -> Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO changelog (account_id, object_type, object_id, change_type) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(account_id)
    .bind(object_type)
    .bind(object_id)
    .bind(change_type)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Get the current state (highest modseq) for an object type.
pub async fn current_state(pool: &PgPool, account_id: i32, object_type: &str) -> Result<String> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COALESCE(MAX(id), 0) FROM changelog WHERE account_id = $1 AND object_type = $2",
    )
    .bind(account_id)
    .bind(object_type)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0.to_string()).unwrap_or_else(|| "0".into()))
}

/// Get changes since a given state.
pub async fn changes_since(
    pool: &PgPool,
    account_id: i32,
    object_type: &str,
    since_state: i64,
    max_changes: i64,
) -> Result<ChangesResult> {
    let rows = sqlx::query_as::<_, ChangeRow>(
        "SELECT object_id, change_type, id FROM changelog \
         WHERE account_id = $1 AND object_type = $2 AND id > $3 \
         ORDER BY id LIMIT $4",
    )
    .bind(account_id)
    .bind(object_type)
    .bind(since_state)
    .bind(max_changes + 1)
    .fetch_all(pool)
    .await?;

    let has_more = rows.len() as i64 > max_changes;
    let rows: Vec<ChangeRow> = rows.into_iter().take(max_changes as usize).collect();
    let new_state = rows.last().map(|r| r.id).unwrap_or(since_state);

    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut destroyed = Vec::new();

    // Deduplicate: if an object was created then updated, only report created.
    // If created then destroyed, report neither.
    use std::collections::HashMap;
    let mut seen: HashMap<Uuid, String> = HashMap::new();
    for row in &rows {
        seen.insert(row.object_id, row.change_type.clone());
    }

    for (id, change) in seen {
        let id_str = id.to_string();
        match change.as_str() {
            "created" => created.push(id_str),
            "updated" => updated.push(id_str),
            "destroyed" => destroyed.push(id_str),
            _ => {}
        }
    }

    Ok(ChangesResult {
        new_state: new_state.to_string(),
        has_more_changes: has_more,
        created,
        updated,
        destroyed,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct ChangeRow {
    object_id: Uuid,
    change_type: String,
    id: i64,
}

pub struct ChangesResult {
    pub new_state: String,
    pub has_more_changes: bool,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
}
