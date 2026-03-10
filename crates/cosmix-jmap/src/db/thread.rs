//! Thread storage operations.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Find or create a thread for a message based on in-reply-to / message-id.
pub async fn find_or_create(
    pool: &PgPool,
    account_id: i32,
    message_id: Option<&str>,
    in_reply_to: Option<&[String]>,
) -> Result<Uuid> {
    // Try to find an existing thread by in-reply-to
    if let Some(refs) = in_reply_to {
        for ref_id in refs {
            let row: Option<(Uuid,)> = sqlx::query_as(
                "SELECT thread_id FROM emails WHERE account_id = $1 AND message_id = $2 LIMIT 1",
            )
            .bind(account_id)
            .bind(ref_id)
            .fetch_optional(pool)
            .await?;
            if let Some((thread_id,)) = row {
                return Ok(thread_id);
            }
        }
    }

    // Try by subject threading (same subject, same message-id prefix)
    // For now, just create a new thread
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO threads (account_id) VALUES ($1) RETURNING id",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}
