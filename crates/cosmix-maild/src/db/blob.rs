//! Blob storage (filesystem + DB metadata).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;
use uuid::Uuid;

/// Store a blob: write to filesystem, record in DB.
pub async fn store(conn: &Arc<Mutex<Connection>>, blob_dir: &Path, account_id: i32, data: &[u8]) -> Result<Uuid> {
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
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO blobs (id, account_id, size, hash) VALUES (?1, ?2, ?3, ?4)",
            params![id_str, account_id, size, hash_clone],
        )?;
        Ok(id)
    }).await?
}

/// Load blob data from filesystem.
pub async fn load(conn: &Arc<Mutex<Connection>>, blob_dir: &Path, blob_id: Uuid) -> Result<Option<Vec<u8>>> {
    let conn = conn.clone();
    let blob_id_str = blob_id.to_string();
    let blob_dir = blob_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
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
    }).await?
}
