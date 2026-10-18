//! MIME inspection and exports. Resolve account ownership before touching the
//! lane or consulting export bookkeeping; references do not grant mail access.

use crate::{
    attachments,
    blob_lane::{Discovery, Lane, Reference},
    db::{
        Db,
        references::{self, Key},
    },
    mailstore::{EmailRecord, MailStore, SqliteMailStore},
};
use cosmix_mds::{ItemId, blob};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageRequest {
    account_id: i32,
    email_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PartRequest {
    account_id: i32,
    email_id: Uuid,
    part: String,
    name: Option<String>,
}

fn owned(ms: &SqliteMailStore, account: i32, item: ItemId) -> Result<EmailRecord, String> {
    if account <= 0 {
        return Err("invalid_arguments: account_id must be positive".into());
    }
    ms.get_email(account, item).map_err(|e| {
        let reason = e.to_string();
        if reason.starts_with("email not found") || reason.starts_with("email envelope missing") {
            "not_found: message or part".into()
        } else {
            format!("unreadable: {reason}")
        }
    })
}

pub async fn dispatch(
    command: &str,
    args: Value,
    db: &Db,
    ms: &Arc<SqliteMailStore>,
    lane: &Lane,
    discovery: &impl Discovery,
) -> (u8, String) {
    match execute(command, args, db, ms, lane, discovery).await {
        Ok(value) => (0, value.to_string()),
        Err(error) => (10, json!({"error": error}).to_string()),
    }
}

async fn execute(
    command: &str,
    args: Value,
    db: &Db,
    ms: &Arc<SqliteMailStore>,
    lane: &Lane,
    discovery: &impl Discovery,
) -> Result<Value, String> {
    let (account, item, part, name) = if command == "maild.attachment.ref" {
        let req: PartRequest =
            serde_json::from_value(args).map_err(|e| format!("invalid_arguments: {e}"))?;
        if !attachments::valid_path(&req.part) {
            return Err("invalid_arguments: invalid part path".into());
        }
        (
            req.account_id,
            ItemId(req.email_id),
            Some(req.part),
            req.name,
        )
    } else {
        let req: MessageRequest =
            serde_json::from_value(args).map_err(|e| format!("invalid_arguments: {e}"))?;
        (req.account_id, ItemId(req.email_id), None, None)
    };
    let store = ms.clone();
    let record = tokio::task::spawn_blocking(move || owned(&store, account, item))
        .await
        .map_err(|e| format!("unreadable: message worker: {e}"))??;
    let item_string = item.0.to_string();
    let message_hash = blob::hex(&record.blob_hash);
    let key = Key {
        account,
        item: &item_string,
        message_hash: &message_hash,
        part: part.as_deref(),
    };
    let store = ms.clone();
    let selected = part.clone();
    let hash = record.blob_hash;
    let inspect = command != "maild.message.ref";
    let (bytes, structure) = tokio::task::spawn_blocking(move || {
        let bytes = attachments::read_message(&store, &hash).map_err(|e| e.to_string())?;
        let structure = if inspect {
            Some(
                attachments::inspect(&bytes, selected.as_deref(), false)
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        Ok::<_, String>((bytes, structure))
    })
    .await
    .map_err(|e| format!("unreadable: MIME worker: {e}"))??;

    if command == "maild.attachment.list" {
        let refs = references::blocking(db, &key, references::parts).await?;
        let parts: Vec<Value> = structure
            .ok_or("unreadable: missing MIME structure")?
            .parts
            .into_iter()
            .map(|p| {
                let mut value = json!({"part": p.path, "mime": p.mime, "size": p.size,
                "disposition": p.disposition, "is_attachment": p.attachment});
                if let Some(name) = p.name {
                    value["name"] = json!(name);
                }
                if let Some(cid) = p.cid {
                    value["content_id"] = json!(cid);
                }
                if p.undecodable {
                    value["undecodable"] = json!(true);
                } else if let Some(reference) = refs.get(&p.path) {
                    value["blob"] = json!(reference.blob);
                }
                value
            })
            .collect();
        return Ok(
            json!({"email_id": item_string, "blob": format!("b3:{message_hash}"), "parts": parts}),
        );
    }
    let (bytes, mime, name) = if let Some(path) = &part {
        drop(bytes); // retain only the selected decoded part across lane I/O
        let structure = structure.ok_or("unreadable: missing MIME structure")?;
        let p = structure
            .parts
            .into_iter()
            .find(|p| &p.path == path)
            .ok_or("not_found: message or part")?;
        (
            structure.extracted.ok_or("not_found: message or part")?,
            p.mime,
            name.or(p.name),
        )
    } else if command == "maild.message.ref" {
        (bytes, "message/rfc822".into(), None)
    } else {
        return Err("invalid_arguments: unknown attachment command".into());
    };
    let expected_blob = format!("b3:{}", blake3::hash(&bytes).to_hex());
    let expected_size = bytes.len() as u64;
    let reference = if let Some(reference) = references::blocking(db, &key, references::get).await?
    {
        if reference.blob != expected_blob || reference.size != expected_size {
            return Err("verify_failed: stored reference differs from exported bytes".into());
        }
        reference
    } else {
        let reference = lane
            .reference(
                discovery,
                &format!("maild:{account}"),
                bytes,
                &mime,
                name.as_deref(),
            )
            .await?;
        // A concurrent destroy/change must not turn a completed upload into an
        // acknowledgement for a different message. Its remote pin is retained
        // for reconciliation if this check or the following DB write fails.
        let store = ms.clone();
        let current = tokio::task::spawn_blocking(move || owned(&store, account, item))
            .await
            .map_err(|e| format!("unreadable: message worker: {e}"))??;
        if current.blob_hash != hash {
            return Err("not_found: message or part".into());
        }
        references::blocking(db, &key, move |db, key| {
            references::save(db, key, &reference)
        })
        .await?
    };
    reply(reference, item_string, part)
}

fn reply(reference: Reference, item: String, part: Option<String>) -> Result<Value, String> {
    let mut value = serde_json::to_value(reference)
        .map_err(|e| format!("unreadable: reference serialization: {e}"))?;
    value["email_id"] = json!(item);
    if let Some(part) = part {
        value["part"] = json!(part);
    }
    Ok(value)
}
