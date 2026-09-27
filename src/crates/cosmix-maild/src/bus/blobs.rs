//! Reversible legacy migration on the daemon's existing MDS handle.

use crate::{
    db::Db,
    mailstore::{BlobId, SqliteMailStore, parse_hash},
};
use cosmix_client::IncomingCommand;
use cosmix_mds::{BlobHash, Mds, blob};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, io::Read, sync::Arc};

const MAX_ROWS: usize = 500;
// Bound declared source data, always allowing the first row even if oversized.
// A single legacy blob is streamed, never accumulated in memory.
const PAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    apply: bool,
    account_id: Option<i32>,
    #[serde(default)]
    cursor: i64,
    limit: Option<usize>,
}

#[derive(Default, Serialize)]
struct Counts {
    orphan: usize,
    planned: usize,
    migrated: usize,
    already_migrated: usize,
    missing: usize,
    corrupt: usize,
    conflicting: usize,
    failed: usize,
}

struct Row {
    account_exists: bool,
    cursor: i64,
    id: String,
    account: i32,
    hash: String,
    size: i64,
}

pub async fn dispatch(
    cmd: &IncomingCommand,
    db: &Db,
    ms: &Arc<SqliteMailStore>,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> (u8, String) {
    match super::try_resolve_args(cmd) {
        Ok(args) => migrate_admitted(db, ms, args, permit).await,
        Err(e) => error(format!("invalid_arguments: {e}")),
    }
}

fn error(message: String) -> (u8, String) {
    (10, json!({"error": message}).to_string())
}

/// Public for in-process conformance tests; production invokes this via Bus.
pub async fn migrate(db: &Db, ms: &Arc<SqliteMailStore>, args: Value) -> (u8, String) {
    let Ok(permit) = db.migration.clone().try_acquire_owned() else {
        return error("busy: migration already running".into());
    };
    migrate_admitted(db, ms, args, Arc::new(permit)).await
}

async fn migrate_admitted(
    db: &Db,
    ms: &Arc<SqliteMailStore>,
    args: Value,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> (u8, String) {
    let args = if args.is_null() { json!({}) } else { args };
    if !args.is_object() {
        return error("invalid_arguments: expected object".into());
    }
    let request: Request = match serde_json::from_value(args) {
        Ok(r) => r,
        Err(e) => return error(format!("invalid_arguments: {e}")),
    };
    let limit = request.limit.unwrap_or(MAX_ROWS);
    if request.cursor < 0
        || request.account_id.is_some_and(|id| id <= 0)
        || !(1..=MAX_ROWS).contains(&limit)
    {
        return error(
            "invalid_arguments: positive account_id, nonnegative cursor and limit 1..500 required"
                .into(),
        );
    }
    let db = db.clone();
    let ms = ms.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit; // cancellation cannot release a running blocking page
        page(&db, &ms, request, limit)
    })
    .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => error(format!("migration: {e}")),
        Err(e) => error(format!("migration: worker failed: {e}")),
    }
}

fn page(
    db: &Db,
    ms: &SqliteMailStore,
    request: Request,
    limit: usize,
) -> anyhow::Result<(u8, String)> {
    let rows = {
        let conn = db
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("database lock: {e}"))?;
        if let Some(account) = request.account_id {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
                [account],
                |r| r.get(0),
            )?;
            anyhow::ensure!(exists, "account not found");
        }
        let mut query = conn.prepare(
            "SELECT rowid, id, account_id, hash, size, \
             EXISTS(SELECT 1 FROM accounts WHERE accounts.id = blobs.account_id) FROM blobs \
             WHERE rowid > ?1 AND (?2 IS NULL OR account_id = ?2) ORDER BY rowid LIMIT ?3",
        )?;
        query
            .query_map(
                params![request.cursor, request.account_id, (limit + 1) as i64],
                |r| {
                    Ok(Row {
                        account_exists: r.get(5)?,
                        cursor: r.get(0)?,
                        id: r.get(1)?,
                        account: r.get(2)?,
                        hash: r.get(3)?,
                        size: r.get(4)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut accounts: BTreeMap<i32, Counts> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut consumed = 0;
    let mut bytes = 0u64;
    let mut cursor = request.cursor;
    for (index, row) in rows.iter().take(limit).enumerate() {
        if index > 0 && row.size.max(0) as u64 > PAGE_BYTES.saturating_sub(bytes) {
            break;
        }
        let counts = accounts.entry(row.account).or_default();
        if !row.account_exists {
            counts.orphan += 1;
        } else {
            match migrate_row(db, ms, row, request.apply) {
                Ok(true) => counts.already_migrated += 1,
                Ok(false) if request.apply => counts.migrated += 1,
                Ok(false) => counts.planned += 1,
                Err(e) => {
                    let message = e.to_string();
                    if message.starts_with("missing:") {
                        counts.missing += 1;
                    } else if message.starts_with("corrupt:") {
                        counts.corrupt += 1;
                    } else if message.contains("conflicting:") {
                        counts.conflicting += 1;
                    } else {
                        counts.failed += 1;
                    }
                    // Bounded diagnostics; the full counts always remain available.
                    if failures.len() < 20 {
                        failures.push(json!({"cursor": row.cursor, "account_id": row.account,
                        "error": message.chars().take(256).collect::<String>()}));
                    }
                }
            }
        }
        bytes = bytes.saturating_add(row.size.max(0) as u64);
        consumed = index + 1;
        cursor = row.cursor;
    }
    let done = consumed == rows.len();
    let failed = accounts
        .values()
        .any(|c| c.missing + c.corrupt + c.conflicting + c.failed > 0);
    let mut reply = json!({"apply": request.apply, "done": done, "failed": failed,
        "next": if done { None } else { Some(cursor) }, "accounts": accounts, "errors": failures});
    if failed {
        reply["error"] = json!("migration: some rows failed; legacy data retained");
    }
    Ok((if failed { 5 } else { 0 }, reply.to_string()))
}

fn verify(mut file: File, expected: &BlobHash, size: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "corrupt: source is not a regular file"
    );
    anyhow::ensure!(
        file.metadata()?.len() == size,
        "corrupt: recorded size differs"
    );
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count = count.saturating_add(n as u64);
        anyhow::ensure!(count <= size, "corrupt: file grew during verification");
        hasher.update(&buf[..n]);
    }
    anyhow::ensure!(
        count == size && hasher.finalize().as_bytes() == &expected.0,
        "corrupt: hash or size differs from legacy row"
    );
    Ok(())
}

fn migrate_row(db: &Db, ms: &SqliteMailStore, row: &Row, apply: bool) -> anyhow::Result<bool> {
    let hash = parse_hash(&row.hash)
        .filter(|_| row.hash.bytes().all(|b| !b.is_ascii_uppercase()))
        .ok_or_else(|| anyhow::anyhow!("corrupt: invalid legacy hash"))?;
    let id = BlobId(
        uuid::Uuid::parse_str(&row.id)
            .map_err(|_| anyhow::anyhow!("corrupt: invalid legacy UUID"))?,
    );
    anyhow::ensure!(
        row.id == id.to_string(),
        "corrupt: noncanonical legacy UUID"
    );
    let size = u64::try_from(row.size).map_err(|_| anyhow::anyhow!("corrupt: negative size"))?;
    let source = blob::blob_path(&db.blob_dir, &hash);
    let file = File::open(&source).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("missing: legacy file")
        } else {
            anyhow::anyhow!("unreadable: legacy file: {e}")
        }
    })?;
    verify(file, &hash, size)?;
    let already = ms.legacy_ref_status(row.account, id, &hash)?;
    // Never overwrite a conflicting queue hash, including one whose legacy
    // UUID was successfully migrated by an earlier partial run.
    {
        let conn = db
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("database lock: {e}"))?;
        let conflict: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM smtp_queue WHERE blob_id = ?1 \
             AND blob_hash IS NOT NULL AND blob_hash != '' AND blob_hash != ?2)",
            params![row.id, row.hash],
            |r| r.get(0),
        )?;
        anyhow::ensure!(!conflict, "conflicting: queue hash differs");
    }
    if !apply {
        if already {
            verify(ms.mds().blob_file(&hash)?, &hash, size)?;
        }
        return Ok(already);
    }
    let landed = ms.mds().put_blob_path(&source, blob::PutMode::Copy)?;
    anyhow::ensure!(landed == hash, "corrupt: source changed during copy");
    // put_path's existing-CAS fast path does not rehash the destination.
    verify(ms.mds().blob_file(&hash)?, &hash, size)?;
    ms.import_legacy_ref(row.account, id, &hash, size)?;
    let conn = db
        .conn
        .lock()
        .map_err(|e| anyhow::anyhow!("database lock: {e}"))?;
    conn.execute(
        "UPDATE smtp_queue SET blob_hash = ?1 \
        WHERE blob_id = ?2 AND (blob_hash IS NULL OR blob_hash = '')",
        params![row.hash, row.id],
    )?;
    Ok(already)
}
