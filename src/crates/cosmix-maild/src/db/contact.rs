//! Contact and addressbook storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct AddressBook {
    pub id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
}

#[derive(Debug, Serialize)]
pub struct Contact {
    pub id: String,
    #[serde(rename = "addressBookId")]
    pub addressbook_id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    pub uid: String,
    /// Full JSContact Card object
    pub data: serde_json::Value,
    #[serde(rename = "fullName")]
    pub full_name: Option<String>,
    pub email: Option<String>,
    pub company: Option<String>,
    /// Primary phone, derived from `data` (JSContact phones / vCard TEL) —
    /// not a stored column; computed in `row_to_contact`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(rename = "updated")]
    pub updated_at: Option<String>,
}

fn row_to_book(row: &rusqlite::Row<'_>) -> rusqlite::Result<AddressBook> {
    Ok(AddressBook {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        sort_order: row.get(4)?,
    })
}

fn row_to_contact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    let data_json: String = row.get(4)?;
    let data: serde_json::Value = serde_json::from_str(&data_json).unwrap_or(serde_json::json!({}));
    let phone = cosmix_lib_davproto::vcard::primary_phone(&data);

    Ok(Contact {
        id: row.get(0)?,
        addressbook_id: row.get(1)?,
        account_id: row.get(2)?,
        uid: row.get(3)?,
        data,
        full_name: row.get(5)?,
        email: row.get(6)?,
        company: row.get(7)?,
        phone,
        updated_at: row.get(8)?,
    })
}

// -- AddressBook CRUD --

pub async fn get_all_books(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
) -> Result<Vec<AddressBook>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, name, description, sort_order \
             FROM addressbooks WHERE account_id = ?1 ORDER BY sort_order, name",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_book)?;
        let mut books = Vec::new();
        for row in rows {
            books.push(row?);
        }
        Ok(books)
    })
    .await?
}

pub async fn get_books_by_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    ids: &[Uuid],
) -> Result<Vec<AddressBook>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "SELECT id, account_id, name, description, sort_order \
             FROM addressbooks WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_book)?;
        let mut books = Vec::new();
        for row in rows {
            books.push(row?);
        }
        Ok(books)
    })
    .await?
}

pub async fn create_book(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    name: &str,
    description: Option<&str>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let name = name.to_string();
    let description = description.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO addressbooks (id, account_id, name, description) VALUES (?1, ?2, ?3, ?4)",
            params![id_str, account_id, name, description],
        )?;
        Ok(id)
    })
    .await?
}

pub async fn update_book(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let name = name.map(|s| s.to_string());
    let description = description.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if name.is_none() && description.is_none() {
            return Ok(true);
        }
        let changes = if let Some(n) = &name {
            if let Some(d) = &description {
                conn.execute(
                    "UPDATE addressbooks SET name = ?3, description = ?4 WHERE account_id = ?1 AND id = ?2",
                    params![account_id, id_str, n, d],
                )?
            } else {
                conn.execute(
                    "UPDATE addressbooks SET name = ?3 WHERE account_id = ?1 AND id = ?2",
                    params![account_id, id_str, n],
                )?
            }
        } else {
            conn.execute(
                "UPDATE addressbooks SET description = ?3 WHERE account_id = ?1 AND id = ?2",
                params![account_id, id_str, description],
            )?
        };
        Ok(changes > 0)
    }).await?
}

pub async fn delete_book(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM addressbooks WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    })
    .await?
}

pub async fn query_book_ids(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Uuid>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id FROM addressbooks WHERE account_id = ?1 ORDER BY sort_order, name",
        )?;
        let rows = stmt.query_map(params![account_id], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }
        Ok(ids)
    })
    .await?
}

// -- Contact CRUD --

pub async fn get_contacts_by_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    ids: &[Uuid],
) -> Result<Vec<Contact>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
             FROM contacts WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_contact)?;
        let mut contacts = Vec::new();
        for row in rows {
            contacts.push(row?);
        }
        Ok(contacts)
    }).await?
}

pub async fn get_all_contacts(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    limit: i64,
) -> Result<Vec<Contact>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
             FROM contacts WHERE account_id = ?1 ORDER BY full_name LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![account_id, limit], row_to_contact)?;
        let mut contacts = Vec::new();
        for row in rows {
            contacts.push(row?);
        }
        Ok(contacts)
    }).await?
}

/// Fetch one contact by its DAV identity `(account, addressbook, uid)`.
/// The `UNIQUE(addressbook_id, uid)` constraint guarantees at most one row.
pub async fn get_contact_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Uuid,
    uid: &str,
) -> Result<Option<Contact>> {
    let conn = conn.clone();
    let addressbook_id = addressbook_id.to_string();
    let uid = uid.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
             FROM contacts WHERE account_id = ?1 AND addressbook_id = ?2 AND uid = ?3",
        )?;
        let mut rows = stmt.query_map(params![account_id, addressbook_id, uid], row_to_contact)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    })
    .await?
}

/// All contacts in one addressbook (DAV listing + addressbook-query),
/// bounded by `limit` (a DoS backstop — the caller logs if it's hit).
pub async fn list_contacts_in_book(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Uuid,
    limit: i64,
) -> Result<Vec<Contact>> {
    let conn = conn.clone();
    let addressbook_id = addressbook_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, addressbook_id, account_id, uid, data, full_name, email, company, updated_at \
             FROM contacts WHERE account_id = ?1 AND addressbook_id = ?2 ORDER BY full_name LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, addressbook_id, limit], row_to_contact)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await?
}

/// `sort_col` MUST be a caller-whitelisted column literal (`full_name` / `email`
/// / `company`) — it is interpolated into the SQL, so it must never carry client
/// input directly. `sort_asc` picks the direction; a stable `id` tiebreaker
/// keeps pagination deterministic when the sort key ties (e.g. many blank names).
// A flat query-parameter list; bundling into a struct buys nothing for
// the single jmap call site.
#[allow(clippy::too_many_arguments)]
pub async fn query_contact_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Option<Uuid>,
    text: Option<&str>,
    sort_col: &str,
    sort_asc: bool,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    let conn = conn.clone();
    let addressbook_id = addressbook_id.map(|u| u.to_string());
    let text = text.map(|s| format!("%{s}%"));
    let order_col = sort_col.to_string();
    let dir = if sort_asc { "ASC" } else { "DESC" };
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        let mut where_parts = vec!["account_id = ?1".to_string()];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));

        if let Some(ref ab_id) = addressbook_id {
            where_parts.push(format!("addressbook_id = ?{}", param_values.len() + 1));
            param_values.push(Box::new(ab_id.clone()));
        }
        if let Some(ref t) = text {
            let idx = param_values.len() + 1;
            where_parts.push(format!(
                "(full_name LIKE ?{idx} OR email LIKE ?{idx} OR company LIKE ?{idx})"
            ));
            param_values.push(Box::new(t.clone()));
        }

        let where_clause = where_parts.join(" AND ");

        // Count query first
        let count_sql = format!("SELECT COUNT(*) FROM contacts WHERE {where_clause}");
        let count_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
            .iter()
            .map(|b| &**b as &dyn rusqlite::types::ToSql)
            .collect();
        let total: i64 = conn.query_row(&count_sql, count_refs.as_slice(), |row| row.get(0))?;

        // Select query with sort + limit/offset. `order_col`/`dir` are
        // whitelisted literals (set by the caller, not client input); the page
        // bounds are bound parameters. SQLite requires LIMIT before OFFSET.
        let limit_idx = param_values.len() + 1;
        let offset_idx = param_values.len() + 2;
        let select_sql = format!(
            "SELECT id FROM contacts WHERE {where_clause} \
             ORDER BY {order_col} {dir}, id ASC LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
        );
        param_values.push(Box::new(limit));
        param_values.push(Box::new(position));
        let select_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
            .iter()
            .map(|b| &**b as &dyn rusqlite::types::ToSql)
            .collect();

        let mut stmt = conn.prepare(&select_sql)?;
        let rows = stmt.query_map(select_refs.as_slice(), |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }

        Ok((ids, total))
    })
    .await?
}

/// Atomically (under one DB lock) verify the addressbook, evaluate the DAV
/// precondition, and insert-or-update a contact by DAV identity. Closes
/// the read-check-write TOCTOU window for conditional PUTs.
pub async fn conditional_upsert_contact_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    meta: ContactMetadata<'_>,
    precond: &crate::db::DavPrecondition,
) -> Result<crate::db::UpsertOutcome> {
    use crate::db::UpsertOutcome;
    let ContactMetadata {
        full_name,
        email,
        company,
    } = meta;
    let conn = conn.clone();
    let ab = addressbook_id.to_string();
    let uid = uid.to_string();
    let data_json = serde_json::to_string(data)?;
    let full_name = full_name.map(|s| s.to_string());
    let email = email.map(|s| s.to_string());
    let company = company.map(|s| s.to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let precond = precond.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        let owns = conn
            .prepare("SELECT 1 FROM addressbooks WHERE id = ?1 AND account_id = ?2")?
            .query_map(params![ab, account_id], |_| Ok(()))?
            .next()
            .is_some();
        if !owns {
            return Ok(UpsertOutcome::CollectionMissing);
        }

        let existing = read_contact_etag_row(&conn, account_id, &ab, &uid)?;
        let current_etag = existing.as_ref().map(|(_, e)| e.clone());

        if !precond.allows(current_etag.as_deref(), existing.is_some()) {
            return Ok(UpsertOutcome::PreconditionFailed);
        }

        let tx = conn.unchecked_transaction()?;
        let outcome = match existing {
            Some((id_str, _)) => {
                let id = id_str.parse::<Uuid>()?;
                tx.execute(
                    "UPDATE contacts SET data = ?4, full_name = ?5, email = ?6, company = ?7, \
                     updated_at = ?8 WHERE account_id = ?1 AND addressbook_id = ?2 AND uid = ?3",
                    params![
                        account_id, ab, uid, data_json, full_name, email, company, now
                    ],
                )?;
                crate::db::changelog::record_tx(&tx, account_id, "Contact", id, "updated")?;
                UpsertOutcome::Updated(id)
            }
            None => {
                let id = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO contacts \
                     (id, account_id, addressbook_id, uid, data, full_name, email, company) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id.to_string(),
                        account_id,
                        ab,
                        uid,
                        data_json,
                        full_name,
                        email,
                        company
                    ],
                )?;
                crate::db::changelog::record_tx(&tx, account_id, "Contact", id, "created")?;
                UpsertOutcome::Created(id)
            }
        };
        tx.commit()?;
        Ok(outcome)
    })
    .await?
}

/// Atomically evaluate the precondition and delete a contact by DAV
/// identity, returning the outcome (with the deleted id for the changelog).
pub async fn conditional_delete_contact_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Uuid,
    uid: &str,
    precond: &crate::db::DavPrecondition,
) -> Result<crate::db::DeleteOutcome> {
    use crate::db::DeleteOutcome;
    let conn = conn.clone();
    let ab = addressbook_id.to_string();
    let uid = uid.to_string();
    let precond = precond.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let existing = read_contact_etag_row(&conn, account_id, &ab, &uid)?;
        let Some((id_str, current_etag)) = existing else {
            // Absent: an If-Match guard still applies (RFC 7232 → 412, not
            // 404), so evaluate it against the empty state first.
            return Ok(if precond.allows(None, false) {
                DeleteOutcome::NotFound
            } else {
                DeleteOutcome::PreconditionFailed
            });
        };
        if !precond.allows(Some(&current_etag), true) {
            return Ok(DeleteOutcome::PreconditionFailed);
        }
        let id = id_str.parse::<Uuid>()?;
        // Delete + tombstone atomically (see the calendar twin).
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM contacts WHERE account_id = ?1 AND addressbook_id = ?2 AND uid = ?3",
            params![account_id, ab, uid],
        )?;
        crate::db::tombstone::insert(&tx, account_id, "Contact", id, &ab, &uid)?;
        crate::db::changelog::record_tx(&tx, account_id, "Contact", id, "destroyed")?;
        tx.commit()?;
        Ok(DeleteOutcome::Deleted(id))
    })
    .await?
}

/// Read `(id, strong_etag)` for one contact under the lock — same ETag
/// tuple GET hashes. `None` if absent.
fn read_contact_etag_row(
    conn: &Connection,
    account_id: i32,
    addressbook_id: &str,
    uid: &str,
) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, full_name, email, company, data FROM contacts \
         WHERE account_id = ?1 AND addressbook_id = ?2 AND uid = ?3",
    )?;
    let mut rows = stmt.query_map(params![account_id, addressbook_id, uid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    match rows.next() {
        Some(row) => {
            let (id, full_name, email, company, data_json) = row?;
            let data: serde_json::Value =
                serde_json::from_str(&data_json).unwrap_or(serde_json::json!({}));
            let etag = cosmix_lib_davproto::etag::for_contact(
                uid,
                full_name.as_deref(),
                email.as_deref(),
                company.as_deref(),
                &data,
            );
            Ok(Some((id, etag)))
        }
        None => Ok(None),
    }
}

/// Indexed contact metadata (extracted from JSContact payload).
pub struct ContactMetadata<'a> {
    pub full_name: Option<&'a str>,
    pub email: Option<&'a str>,
    pub company: Option<&'a str>,
}

pub async fn create_contact(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    addressbook_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    meta: ContactMetadata<'_>,
) -> Result<Uuid> {
    let ContactMetadata {
        full_name,
        email,
        company,
    } = meta;
    let conn = conn.clone();
    let ab_id_str = addressbook_id.to_string();
    let uid = uid.to_string();
    let data_json = serde_json::to_string(data)?;
    let full_name = full_name.map(|s| s.to_string());
    let email = email.map(|s| s.to_string());
    let company = company.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // Verify the addressbook belongs to this account (the FK is not
        // account-scoped) — no cross-account contact attachment.
        let owns = conn
            .prepare("SELECT 1 FROM addressbooks WHERE id = ?1 AND account_id = ?2")?
            .query_map(params![ab_id_str, account_id], |_| Ok(()))?
            .next()
            .is_some();
        if !owns {
            return Err(anyhow::anyhow!(
                "addressbook {ab_id_str} does not belong to account {account_id}"
            ));
        }
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO contacts (id, account_id, addressbook_id, uid, data, full_name, email, company) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id_str, account_id, ab_id_str, uid, data_json, full_name, email, company],
        )?;
        Ok(id)
    }).await?
}

pub async fn update_contact(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
    data: &serde_json::Value,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let data_json = serde_json::to_string(data)?;
    let full_name = full_name.map(|s| s.to_string());
    let email = email.map(|s| s.to_string());
    let company = company.map(|s| s.to_string());
    let now = chrono::Utc::now().to_rfc3339();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE contacts SET data = ?3, full_name = ?4, email = ?5, company = ?6, \
             updated_at = ?7 WHERE account_id = ?1 AND id = ?2",
            params![
                account_id, id_str, data_json, full_name, email, company, now
            ],
        )?;
        Ok(changes > 0)
    })
    .await?
}

pub async fn delete_contact(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // Capture (addressbook_id, uid) before deleting for the tombstone
        // (DAV sync needs it even for a JMAP-originated delete).
        let ids: Option<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT addressbook_id, uid FROM contacts WHERE account_id = ?1 AND id = ?2",
            )?;
            let mut rows = stmt.query_map(params![account_id, id_str], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            match rows.next() {
                Some(r) => Some(r?),
                None => None,
            }
        };
        let tx = conn.unchecked_transaction()?;
        let changes = tx.execute(
            "DELETE FROM contacts WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        if changes > 0
            && let Some((addressbook_id, uid)) = ids
        {
            crate::db::tombstone::insert(&tx, account_id, "Contact", id, &addressbook_id, &uid)?;
        }
        tx.commit()?;
        Ok(changes > 0)
    })
    .await?
}
