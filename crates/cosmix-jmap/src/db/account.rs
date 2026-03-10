//! Account storage operations.

use anyhow::Result;
use sqlx::PgPool;

#[derive(Debug, sqlx::FromRow)]
pub struct Account {
    pub id: i32,
    pub email: String,
    pub password: String,
    pub name: Option<String>,
    pub quota: i64,
}

pub async fn get_by_email(pool: &PgPool, email: &str) -> Result<Option<Account>> {
    let row = sqlx::query_as::<_, Account>("SELECT id, email, password, name, quota FROM accounts WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn create(pool: &PgPool, email: &str, password_hash: &str, name: Option<&str>) -> Result<i32> {
    let row: (i32,) = sqlx::query_as(
        "INSERT INTO accounts (email, password, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(email)
    .bind(password_hash)
    .bind(name)
    .fetch_one(pool)
    .await?;

    // Create default mailboxes
    sqlx::query("SELECT create_default_mailboxes($1)")
        .bind(row.0)
        .execute(pool)
        .await?;

    Ok(row.0)
}

pub async fn list(pool: &PgPool) -> Result<Vec<Account>> {
    let rows = sqlx::query_as::<_, Account>("SELECT id, email, password, name, quota FROM accounts ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn delete(pool: &PgPool, email: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM accounts WHERE email = $1")
        .bind(email)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
