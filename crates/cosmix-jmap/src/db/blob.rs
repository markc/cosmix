//! Blob storage (filesystem + DB metadata).

use anyhow::Result;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

/// Store a blob: write to filesystem, record in DB.
pub async fn store(pool: &PgPool, blob_dir: &Path, account_id: i32, data: &[u8]) -> Result<Uuid> {
    let hash = blake3::hash(data).to_hex().to_string();
    let size = data.len() as i32;

    // Content-addressed filesystem path: {dir}/{hash[0:2]}/{hash[2:4]}/{hash}
    let sub_dir = blob_dir.join(&hash[..2]).join(&hash[2..4]);
    std::fs::create_dir_all(&sub_dir)?;
    let file_path = sub_dir.join(&hash);

    if !file_path.exists() {
        std::fs::write(&file_path, data)?;
    }

    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO blobs (account_id, size, hash) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(account_id)
    .bind(size)
    .bind(&hash)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Load blob data from filesystem.
pub async fn load(pool: &PgPool, blob_dir: &Path, blob_id: Uuid) -> Result<Option<Vec<u8>>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT hash FROM blobs WHERE id = $1")
        .bind(blob_id)
        .fetch_optional(pool)
        .await?;

    let Some((hash,)) = row else {
        return Ok(None);
    };

    let file_path = blob_dir.join(&hash[..2]).join(&hash[2..4]).join(&hash);
    match std::fs::read(&file_path) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
