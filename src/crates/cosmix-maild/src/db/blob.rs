//! Blob storage (filesystem + DB metadata).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;
use uuid::Uuid;

/// Store a blob: write to filesystem, record in DB.
pub async fn store(
    conn: &Arc<Mutex<Connection>>,
    blob_dir: &Path,
    account_id: i32,
    data: &[u8],
) -> Result<Uuid> {
    let hash = blake3::hash(data).to_hex().to_string();
    let size = data.len() as i32;
    let data = data.to_vec();
    let blob_dir = blob_dir.to_path_buf();

    // Content-addressed filesystem path: {dir}/{hash[0:2]}/{hash[2:4]}/{hash}
    let sub_dir = blob_dir.join(&hash[..2]).join(&hash[2..4]);
    std::fs::create_dir_all(&sub_dir)?;
    let file_path = sub_dir.join(&hash);

    if !file_path.exists() {
        std::fs::write(&file_path, &data)?;
    }

    let conn = conn.clone();
    let hash_clone = hash;
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO blobs (id, account_id, size, hash) VALUES (?1, ?2, ?3, ?4)",
            params![id_str, account_id, size, hash_clone],
        )?;
        Ok(id)
    })
    .await?
}

/// Returns true iff a `blobs` row exists with both the given `id`
/// and `account_id`. Cheap ownership probe for paths that need to
/// gate on a UUID `blobId` *without* reading the bytes — e.g. the
/// `EmailSubmission/set` remote queueing branch, which only enqueues
/// the id and would otherwise let an SMTP worker downstream load the
/// bytes via the bare-`load` server-internal carveout.
///
/// Phase-6 deletion target: with Task 5.4c the only caller (the
/// JMAP submission gate) is gone — `MailStore::get_email` is
/// account-scoped, and CAS hashes are content-addressed, so there
/// is no UUID-smuggling vector left to defend against.
#[allow(dead_code)]
pub async fn id_owned_by_account(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    blob_id: Uuid,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = blob_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let row: rusqlite::Result<i32> = conn.query_row(
            "SELECT 1 FROM blobs WHERE id = ?1 AND account_id = ?2",
            params![id_str, account_id],
            |r| r.get(0),
        );
        match row {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// Returns true iff `account_id` has at least one `blobs` row binding
/// it to `hash_hex` (the 64-char lowercase-hex CAS hash).
///
/// Gate for the JMAP `/download/{accountId}/{blobId}` CAS branch. CAS
/// hashes are deduplicated across accounts; without this gate, any
/// authenticated user who learns a hash could fetch the bytes via the
/// shared mds CAS surface regardless of mailbox membership. A blob
/// becomes "owned" by an account when:
///
///   - JMAP `/upload/{accountId}` writes a `blobs` row via
///     `db::blob::store` (and, post-Task-3.3a, also a CAS staging
///     alias via `MailStore::create_blob_ref`), or
///   - SMTP inbound delivery records the blob into the destination
///     account via `db::blob::store` (`smtp/inbound.rs`).
///
/// Either path leaves a row visible to this query. Cross-account
/// ownership for the same CAS hash is tracked by separate rows; this
/// is intentional — deduplication happens at the filesystem layer,
/// not at the authorisation layer.
///
/// **Migration evolution.** Today every production write path that
/// produces a CAS hash also writes a `db::blob` row, so this gate
/// covers all blobs an account could legitimately request. When
/// Task 3.3a (`_doc/planned/jmap-mds-migration.md`) retires
/// `db::blob` in favor of mds-side staging aliases (and any future
/// `MailStore::create_email`-only path that bypasses `db::blob`
/// entirely), the gate must move to one of:
///
///   - dual-write `db::blob` for the duration of the migration so
///     this query keeps resolving, or
///   - reissue ownership against an mds-side index keyed by
///     `(account, blob_hash)` — e.g. `EmailRecord.blob_hash` joined
///     across the account's containers plus the staging alias set.
///
/// The C5 cold review surfaced this forward-looking gap; resolution
/// rides with whichever C-step actually moves an upload / import
/// path off `db::blob`.
pub async fn hash_owned_by_account(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    hash_hex: &str,
) -> Result<bool> {
    let conn = conn.clone();
    let hash = hash_hex.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let row: rusqlite::Result<i32> = conn.query_row(
            "SELECT 1 FROM blobs WHERE account_id = ?1 AND hash = ?2 LIMIT 1",
            params![account_id, hash],
            |r| r.get(0),
        );
        match row {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// Load blob data from filesystem.
///
/// Note: this lookup is keyed by `id` only and does NOT enforce
/// account ownership. Callers that serve bytes back to a
/// network-reachable client MUST use [`load_for_account`] instead.
///
/// The only legitimate remaining caller of this bare form is the
/// SMTP outbound queue worker (`smtp/delivery.rs`), which has no
/// per-caller account context and pulls queue rows that were
/// already gated at enqueue time (see
/// `EmailSubmission/set` in `jmap/submission.rs`, which validates
/// blob ownership via [`id_owned_by_account`] before enqueueing).
/// Adding new callers of `load` is a security regression — prefer
/// [`load_for_account`] or [`id_owned_by_account`].
pub async fn load(
    conn: &Arc<Mutex<Connection>>,
    blob_dir: &Path,
    blob_id: Uuid,
) -> Result<Option<Vec<u8>>> {
    let conn = conn.clone();
    let blob_id_str = blob_id.to_string();
    let blob_dir = blob_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT hash FROM blobs WHERE id = ?1",
            params![blob_id_str],
            |row| row.get(0),
        );

        let hash = match result {
            Ok(h) => h,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let file_path = blob_dir.join(&hash[..2]).join(&hash[2..4]).join(&hash);
        match std::fs::read(&file_path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// Account-scoped variant of [`load`]: requires the `(id, account_id)`
/// pair to match a row before returning bytes. Returns `Ok(None)` for
/// either "no such id" or "id exists but is not owned by this
/// account" — the two cases are deliberately collapsed so callers
/// uniformly emit a 404 and do not leak whether a foreign blob id
/// exists.
///
/// This is the safe lookup for the JMAP `/download/{accountId}/{blobId}`
/// HTTP surface; the bare [`load`] is reserved for internal paths
/// where the blob id was just minted by the server itself.
pub async fn load_for_account(
    conn: &Arc<Mutex<Connection>>,
    blob_dir: &Path,
    account_id: i32,
    blob_id: Uuid,
) -> Result<Option<Vec<u8>>> {
    let conn = conn.clone();
    let blob_id_str = blob_id.to_string();
    let blob_dir = blob_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT hash FROM blobs WHERE id = ?1 AND account_id = ?2",
            params![blob_id_str, account_id],
            |row| row.get(0),
        );

        let hash = match result {
            Ok(h) => h,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let file_path = blob_dir.join(&hash[..2]).join(&hash[2..4]).join(&hash);
        match std::fs::read(&file_path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE blobs (
                id TEXT PRIMARY KEY,
                account_id INTEGER NOT NULL,
                size INTEGER NOT NULL,
                hash TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[tokio::test]
    async fn store_then_hash_owned_returns_true_for_owner_false_for_other() {
        let conn = fresh_db();
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"abc";
        let id = store(&conn, dir.path(), 1, bytes).await.unwrap();
        let hash = blake3::hash(bytes).to_hex().to_string();

        assert!(hash_owned_by_account(&conn, 1, &hash).await.unwrap());
        // Cross-account: same hash, different account → no row →
        // gate denies. Closes the BLOCKER where any authenticated
        // user could fetch CAS bytes by hash.
        assert!(!hash_owned_by_account(&conn, 2, &hash).await.unwrap());
        // Nonexistent hash on legitimate account.
        assert!(
            !hash_owned_by_account(&conn, 1, &"f".repeat(64))
                .await
                .unwrap()
        );
        // Owner gets bytes back.
        let owner_data = load_for_account(&conn, dir.path(), 1, id).await.unwrap();
        assert_eq!(owner_data.as_deref(), Some(bytes.as_slice()));
        // Non-owner is collapsed into `None` (404 at the HTTP
        // layer) — never bytes, never a 403 that would leak
        // existence of a foreign id.
        let other_data = load_for_account(&conn, dir.path(), 2, id).await.unwrap();
        assert!(other_data.is_none());
    }

    #[tokio::test]
    async fn load_for_account_missing_id_returns_none() {
        let conn = fresh_db();
        let dir = tempfile::tempdir().unwrap();
        let missing = Uuid::new_v4();
        assert!(
            load_for_account(&conn, dir.path(), 1, missing)
                .await
                .unwrap()
                .is_none()
        );
    }
}
