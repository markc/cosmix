//! Contact and addressbook storage operations.

use anyhow::Result;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AddressBook {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: i32,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Contact {
    pub id: Uuid,
    #[serde(rename = "addressBookId")]
    pub addressbook_id: Uuid,
    #[serde(skip)]
    pub account_id: i32,
    pub uid: String,
    /// Full JSContact Card object
    pub data: serde_json::Value,
    #[serde(rename = "fullName")]
    pub full_name: Option<String>,
    pub email: Option<String>,
    pub company: Option<String>,
    #[serde(rename = "updated")]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ── AddressBook CRUD ──

pub async fn get_all_books(pool: &PgPool, account_id: i32) -> Result<Vec<AddressBook>> {
    let rows = sqlx::query_as::<_, AddressBook>(
        "SELECT id, account_id, name, description, sort_order \
         FROM addressbooks WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_books_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<AddressBook>> {
    let rows = sqlx::query_as::<_, AddressBook>(
        "SELECT id, account_id, name, description, sort_order \
         FROM addressbooks WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn create_book(
    pool: &PgPool,
    account_id: i32,
    name: &str,
    description: Option<&str>,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO addressbooks (account_id, name, description) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update_book(
    pool: &PgPool,
    account_id: i32,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<bool> {
    if name.is_none() && description.is_none() {
        return Ok(true);
    }
    let result = if let Some(n) = name {
        if let Some(d) = description {
            sqlx::query("UPDATE addressbooks SET name = $3, description = $4 WHERE account_id = $1 AND id = $2")
                .bind(account_id).bind(id).bind(n).bind(d)
                .execute(pool).await?
        } else {
            sqlx::query("UPDATE addressbooks SET name = $3 WHERE account_id = $1 AND id = $2")
                .bind(account_id).bind(id).bind(n)
                .execute(pool).await?
        }
    } else {
        sqlx::query("UPDATE addressbooks SET description = $3 WHERE account_id = $1 AND id = $2")
            .bind(account_id).bind(id).bind(description)
            .execute(pool).await?
    };
    Ok(result.rows_affected() > 0)
}

pub async fn delete_book(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM addressbooks WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn query_book_ids(pool: &PgPool, account_id: i32) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM addressbooks WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

// ── Contact CRUD ──

pub async fn get_contacts_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<Contact>> {
    let rows = sqlx::query_as::<_, Contact>(
        "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
         FROM contacts WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_all_contacts(pool: &PgPool, account_id: i32, limit: i64) -> Result<Vec<Contact>> {
    let rows = sqlx::query_as::<_, Contact>(
        "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
         FROM contacts WHERE account_id = $1 ORDER BY full_name LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn query_contact_ids(
    pool: &PgPool,
    account_id: i32,
    addressbook_id: Option<Uuid>,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    let (ids, total): (Vec<(Uuid,)>, (i64,)) = if let Some(ab_id) = addressbook_id {
        let ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM contacts WHERE account_id = $1 AND addressbook_id = $2 \
             ORDER BY full_name OFFSET $3 LIMIT $4",
        )
        .bind(account_id)
        .bind(ab_id)
        .bind(position)
        .bind(limit)
        .fetch_all(pool)
        .await?;
        let total: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM contacts WHERE account_id = $1 AND addressbook_id = $2",
        )
        .bind(account_id)
        .bind(ab_id)
        .fetch_one(pool)
        .await?;
        (ids, total)
    } else {
        let ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM contacts WHERE account_id = $1 ORDER BY full_name OFFSET $2 LIMIT $3",
        )
        .bind(account_id)
        .bind(position)
        .bind(limit)
        .fetch_all(pool)
        .await?;
        let total: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM contacts WHERE account_id = $1",
        )
        .bind(account_id)
        .fetch_one(pool)
        .await?;
        (ids, total)
    };
    Ok((ids.into_iter().map(|r| r.0).collect(), total.0))
}

pub async fn create_contact(
    pool: &PgPool,
    account_id: i32,
    addressbook_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO contacts (account_id, addressbook_id, uid, data, full_name, email, company) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(account_id)
    .bind(addressbook_id)
    .bind(uid)
    .bind(data)
    .bind(full_name)
    .bind(email)
    .bind(company)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update_contact(
    pool: &PgPool,
    account_id: i32,
    id: Uuid,
    data: &serde_json::Value,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE contacts SET data = $3, full_name = $4, email = $5, company = $6, \
         updated_at = NOW() WHERE account_id = $1 AND id = $2",
    )
    .bind(account_id)
    .bind(id)
    .bind(data)
    .bind(full_name)
    .bind(email)
    .bind(company)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_contact(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM contacts WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
