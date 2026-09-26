//! Validated blobd references. No DB lock survives an HTTP await. Concurrent
//! exporters converge on one row; blobd's owner/hash pin is idempotent too.

use super::Db;
use crate::blob_lane::Reference;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

pub struct Key<'a> {
    pub account: i32,
    pub item: &'a str,
    pub message_hash: &'a str,
    pub part: Option<&'a str>,
}

fn error(e: impl std::fmt::Display) -> String {
    format!("unreadable: reference database: {e}")
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Reference> {
    Ok(Reference {
        blob: r.get(0)?,
        size: r.get(1)?,
        mime: r.get(2)?,
        name: r.get(3)?,
        origin: r.get(4)?,
    })
}

fn query(conn: &Connection, key: &Key<'_>) -> Result<Option<Reference>, String> {
    let result = if let Some(part) = key.part {
        conn.query_row("SELECT blob, size, mime, name, origin FROM attachment_refs WHERE account_id=?1 AND item_id=?2 AND message_hash=?3 AND part=?4",
            params![key.account, key.item, key.message_hash, part], row).optional()
    } else {
        conn.query_row("SELECT blob, size, mime, name, origin FROM message_refs WHERE account_id=?1 AND item_id=?2 AND message_hash=?3",
            params![key.account, key.item, key.message_hash], row).optional()
    }.map_err(error)?;
    if let Some(reference) = &result {
        reference.validate()?;
    }
    Ok(result)
}

pub fn get(db: &Db, key: &Key<'_>) -> Result<Option<Reference>, String> {
    let conn = db.conn.lock().map_err(error)?;
    query(&conn, key)
}

pub fn parts(db: &Db, key: &Key<'_>) -> Result<BTreeMap<String, Reference>, String> {
    let conn = db.conn.lock().map_err(error)?;
    let mut query = conn.prepare("SELECT blob, size, mime, name, origin, part FROM attachment_refs WHERE account_id=?1 AND item_id=?2 AND message_hash=?3").map_err(error)?;
    let rows = query
        .query_map(params![key.account, key.item, key.message_hash], |r| {
            Ok((r.get::<_, String>(5)?, row(r)?))
        })
        .map_err(error)?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(error)?;
    for reference in rows.values() {
        reference.validate()?;
    }
    Ok(rows)
}

/// Return the first committed canonical row, not the last caller's hints.
pub fn save(db: &Db, key: &Key<'_>, reference: &Reference) -> Result<Reference, String> {
    reference.validate()?;
    let size = i64::try_from(reference.size).map_err(error)?;
    let conn = db.conn.lock().map_err(error)?;
    let now = chrono::Utc::now().timestamp();
    if let Some(part) = key.part {
        conn.execute("INSERT INTO attachment_refs (account_id,item_id,message_hash,part,blob,size,mime,name,origin,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(account_id,item_id,message_hash,part) DO NOTHING",
            params![key.account, key.item, key.message_hash, part, reference.blob, size, reference.mime, reference.name, reference.origin, now])
    } else {
        conn.execute("INSERT INTO message_refs (account_id,item_id,message_hash,blob,size,mime,name,origin,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(account_id,item_id,message_hash) DO NOTHING",
            params![key.account, key.item, key.message_hash, reference.blob, size, reference.mime, reference.name, reference.origin, now])
    }.map_err(error)?;
    let saved = query(&conn, key)?.ok_or("unreadable: reference row disappeared")?;
    if saved.blob != reference.blob || saved.size != reference.size {
        return Err("verify_failed: stored reference differs from exported bytes".into());
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn additive_schema_and_reference_keys_preserve_first_writer_and_old_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO accounts(id,email,password) VALUES (1,'user@example.test','unused')",
            [],
        )
        .unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
            blob_dir: Default::default(),
        };
        let mut key = Key {
            account: 1,
            item: "item",
            message_hash: "message",
            part: Some("1.2"),
        };
        let reference = Reference {
            blob: format!("b3:{}", "a".repeat(64)),
            size: 5,
            mime: "text/plain".into(),
            name: Some("first".into()),
            origin: "alpha".into(),
        };
        assert_eq!(save(&db, &key, &reference).unwrap(), reference);
        let mut later = reference.clone();
        later.name = Some("later".into());
        assert_eq!(save(&db, &key, &later).unwrap(), reference);
        key.account = 2;
        assert!(get(&db, &key).unwrap().is_none());
        key.account = 1;
        key.message_hash = "changed";
        assert!(get(&db, &key).unwrap().is_none());
        assert!(parts(&db, &key).unwrap().is_empty());
        key.message_hash = "message";
        later.blob = format!("b3:{}", "b".repeat(64));
        assert!(
            save(&db, &key, &later)
                .unwrap_err()
                .starts_with("verify_failed:")
        );
        assert_eq!(get(&db, &key).unwrap(), Some(reference.clone()));
        key.part = None;
        assert!(get(&db, &key).unwrap().is_none());
        save(&db, &key, &reference).unwrap();
        // Account deletion retains export bookkeeping for later reconciliation.
        db.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM accounts WHERE id=1", [])
            .unwrap();
        assert_eq!(get(&db, &key).unwrap(), Some(reference));
    }
}
