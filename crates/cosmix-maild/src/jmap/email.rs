//! JMAP Email methods (RFC 8621).

use anyhow::Result;
use uuid::Uuid;

use crate::db::{self, Db};
use crate::filter::SpamFilter;
use super::types::*;

/// Email/get — returns email metadata and optionally body values.
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let fetch_text = args.get("fetchTextBodyValues").and_then(|v| v.as_bool()).unwrap_or(false);
    let fetch_html = args.get("fetchHTMLBodyValues").and_then(|v| v.as_bool()).unwrap_or(false);

    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let emails = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::email::get_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        let (ids, _) = db::email::query_ids(&db.conn, account_id, None, true, 0, 100).await?;

        db::email::get_by_ids(&db.conn, account_id, &ids).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    // If body values requested, load blobs and parse MIME
    let list: Vec<serde_json::Value> = if fetch_text || fetch_html {
        let mut result = Vec::new();
        for email in &emails {
            let mut val = serde_json::to_value(email)?;
            if let Ok(blob_uuid) = email.blob_id.parse::<uuid::Uuid>() {
                if let Ok(Some(blob_data)) = db::blob::load(&db.conn, &db.blob_dir, blob_uuid).await {
                    add_body_parts(&mut val, &blob_data, fetch_text, fetch_html);
                }
            }
            result.push(val);
        }
        result
    } else {
        emails.iter().map(|e| serde_json::to_value(e).unwrap()).collect()
    };

    let resp = serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": list,
        "notFound": [],
    });

    Ok(resp)
}

/// Parse a message and add textBody, htmlBody, bodyValues to the JSON response.
fn add_body_parts(val: &mut serde_json::Value, data: &[u8], fetch_text: bool, fetch_html: bool) {
    use mail_parser::{MessageParser, PartType};

    let parser = MessageParser::default();
    let Some(msg) = parser.parse(data) else { return };

    let mut body_values = serde_json::Map::new();
    let mut text_body = Vec::new();
    let mut html_body = Vec::new();
    let mut part_idx = 0u32;

    for part in msg.parts.iter() {
        let (is_text, is_html, body_text) = match &part.body {
            PartType::Text(text) => (true, false, text.as_ref().to_string()),
            PartType::Html(html) => (false, true, html.as_ref().to_string()),
            _ => continue,
        };

        if body_text.is_empty() { continue; }

        let pid = part_idx.to_string();
        let mime_type = if is_text { "text/plain" } else { "text/html" };

        if is_text {
            text_body.push(serde_json::json!({
                "partId": pid,
                "type": mime_type,
            }));
            if fetch_text {
                body_values.insert(pid.clone(), serde_json::json!({
                    "value": body_text,
                    "isEncodingProblem": false,
                    "isTruncated": false,
                }));
            }
        }

        if is_html {
            html_body.push(serde_json::json!({
                "partId": pid,
                "type": mime_type,
            }));
            if fetch_html {
                body_values.insert(pid.clone(), serde_json::json!({
                    "value": body_text,
                    "isEncodingProblem": false,
                    "isTruncated": false,
                }));
            }
        }

        part_idx += 1;
    }

    val["textBody"] = serde_json::Value::Array(text_body);
    val["htmlBody"] = serde_json::Value::Array(html_body);
    val["bodyValues"] = serde_json::Value::Object(body_values);
}

/// Email/query — supports RFC 8621 §4.4 FilterCondition properties.
pub async fn query(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let filter: Option<db::email::EmailFilter> = args.get("filter")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let position = args.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as i64;

    // Parse sort — array of {property, isAscending} objects
    let sort_desc = if let Some(sort) = args.get("sort").and_then(|v| v.as_array()) {
        if let Some(first) = sort.first() {
            // Default isAscending=false for receivedAt (most recent first)
            !first.get("isAscending").and_then(|v| v.as_bool()).unwrap_or(false)
        } else {
            true
        }
    } else {
        true
    };

    let (ids, total) = db::email::query_ids(
        &db.conn, account_id, filter.as_ref(), sort_desc, position, limit,
    ).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        position: position as u64,
        ids: ids.into_iter().map(|u| u.to_string()).collect(),
        total: total as u64,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Email/set — handles create (from uploaded blob), updates (keywords, mailboxIds with spam retraining), and destroy.
pub async fn set(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
    spam_filter: &SpamFilter,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut not_created = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();
    let mut not_updated = std::collections::HashMap::new();
    let mut not_destroyed = std::collections::HashMap::new();

    // Handle create — build email record from an uploaded blob
    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, entry) in create {
            match create_from_blob(db, account_id, entry).await {
                Ok(email_id) => {
                    db::changelog::record(&db.conn, account_id, "Email", email_id, "created").await?;
                    created_map.insert(client_id.clone(), serde_json::json!({ "id": email_id.to_string() }));
                }
                Err(e) => {
                    not_created.insert(client_id.clone(), SetError {
                        error_type: "invalidArguments".into(),
                        description: Some(e.to_string()),
                    });
                }
            }
        }
    }

    // Look up the Junk mailbox ID for spam retraining
    let junk_id = db::mailbox::get_by_role(&db.conn, account_id, "junk").await?;

    // Handle updates (keywords, mailboxIds)
    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else {
                not_updated.insert(id_str.clone(), SetError {
                    error_type: "invalidArguments".into(),
                    description: Some("Invalid id".into()),
                });
                continue;
            };

            let mut changed = false;

            if let Some(keywords) = patch.get("keywords") {
                if db::email::update_keywords(&db.conn, account_id, id, keywords).await? {
                    changed = true;
                }
            }

            if let Some(mailbox_ids) = patch.get("mailboxIds") {
                if let Some(obj) = mailbox_ids.as_object() {
                    let new_mbox_uuids: Vec<Uuid> = obj.keys()
                        .filter_map(|k| k.parse().ok())
                        .collect();

                    // Get old mailbox_ids + blob_id for retraining
                    if let Some(junk_id) = junk_id {
                        if let Ok(Some((old_mboxes, blob_id))) =
                            db::email::get_mailbox_and_blob(&db.conn, account_id, id).await
                        {
                            let was_in_junk = old_mboxes.contains(&junk_id);
                            let now_in_junk = new_mbox_uuids.contains(&junk_id);

                            if was_in_junk != now_in_junk {
                                // Retrain spamlite based on folder move
                                if let Ok(Some(blob_data)) = db::blob::load(&db.conn, &db.blob_dir, blob_id).await {
                                    if !was_in_junk && now_in_junk {
                                        // Moved TO Junk → train as spam
                                        if let Err(e) = spam_filter.train_spam(account_id, &blob_data) {
                                            tracing::warn!(error = %e, "Spam retrain failed");
                                        }
                                    } else {
                                        // Moved FROM Junk → train as good
                                        if let Err(e) = spam_filter.train_good(account_id, &blob_data) {
                                            tracing::warn!(error = %e, "Ham retrain failed");
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if db::email::update_mailboxes(&db.conn, account_id, id, &new_mbox_uuids).await? {
                        changed = true;
                    }
                }
            }

            if changed {
                db::changelog::record(&db.conn, account_id, "Email", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    // Handle destroy
    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else { continue };
            let Ok(id) = id_str.parse::<Uuid>() else {
                not_destroyed.insert(id_str.to_string(), SetError {
                    error_type: "notFound".into(),
                    description: None,
                });
                continue;
            };

            if db::email::delete(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "Email", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            } else {
                not_destroyed.insert(id_str.to_string(), SetError {
                    error_type: "notFound".into(),
                    description: None,
                });
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: if created_map.is_empty() { None } else { Some(created_map) },
        updated: if updated_map.is_empty() { None } else { Some(updated_map.into_iter().collect()) },
        destroyed: if destroyed_list.is_empty() { None } else { Some(destroyed_list) },
        not_created: if not_created.is_empty() { None } else { Some(not_created) },
        not_updated: if not_updated.is_empty() { None } else { Some(not_updated) },
        not_destroyed: if not_destroyed.is_empty() { None } else { Some(not_destroyed) },
    };

    Ok(serde_json::to_value(resp)?)
}

/// Create an email record from an uploaded blob (used by Email/set create).
/// Expects: blobId (required), mailboxIds (required object like {"uuid": true}), keywords (optional).
async fn create_from_blob(
    db: &Db,
    account_id: i32,
    entry: &serde_json::Value,
) -> Result<Uuid> {
    use mail_parser::{HeaderValue, MessageParser};

    // Required: blobId
    let blob_id_str = entry.get("blobId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("blobId is required"))?;
    let blob_id: Uuid = blob_id_str.parse()
        .map_err(|_| anyhow::anyhow!("Invalid blobId"))?;

    // Required: mailboxIds — object like {"uuid": true}
    let mailbox_ids_obj = entry.get("mailboxIds")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("mailboxIds is required"))?;
    let mailbox_ids: Vec<Uuid> = mailbox_ids_obj.keys()
        .filter_map(|k| k.parse().ok())
        .collect();
    if mailbox_ids.is_empty() {
        return Err(anyhow::anyhow!("mailboxIds must contain at least one valid mailbox ID"));
    }

    // Load blob data
    let blob_data = db::blob::load(&db.conn, &db.blob_dir, blob_id).await?
        .ok_or_else(|| anyhow::anyhow!("Blob not found: {blob_id}"))?;

    // Parse MIME message (same pattern as smtp/inbound.rs)
    let parser = MessageParser::default();
    let message = parser.parse(&blob_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse MIME message"))?;

    let subject = message.subject().map(|s| s.to_string());
    let message_id = message.message_id().map(|s| s.to_string());
    let date = message.date().map(|d| {
        chrono::DateTime::from_timestamp(d.to_timestamp(), 0)
            .unwrap_or_else(chrono::Utc::now)
    });
    let in_reply_to: Option<Vec<String>> = match message.in_reply_to() {
        HeaderValue::Text(s) => Some(vec![s.to_string()]),
        HeaderValue::TextList(list) => Some(list.iter().map(|s| s.to_string()).collect()),
        _ => None,
    };

    let from_addr = extract_addresses(message.from());
    let to_addr = extract_addresses(message.to());
    let cc_addr = extract_addresses(message.cc());
    let preview = message.body_preview(256).map(|s| s.to_string());
    let has_attachment = message.attachment_count() > 0;
    let size = blob_data.len() as i32;

    // Find or create thread
    let thread_id = db::thread::find_or_create(
        &db.conn,
        account_id,
        message_id.as_deref(),
        in_reply_to.as_deref(),
    ).await?;

    // Optional: keywords from request (e.g., {"$draft": true, "$seen": true})
    let keywords = entry.get("keywords");

    // Create email record
    let email_id = db::email::create(
        &db.conn,
        account_id,
        thread_id,
        &mailbox_ids,
        blob_id,
        size,
        message_id.as_deref(),
        in_reply_to.as_deref(),
        subject.as_deref(),
        from_addr.as_ref(),
        to_addr.as_ref(),
        cc_addr.as_ref(),
        date,
        preview.as_deref(),
        has_attachment,
        None, // spam_score — not applicable for client-created emails
        None, // spam_verdict
    ).await?;

    // Apply keywords if provided
    if let Some(keywords) = keywords {
        db::email::update_keywords(&db.conn, account_id, email_id, keywords).await?;
    }

    Ok(email_id)
}

/// Extract addresses from a mail-parser Address into JSON (shared with smtp/inbound).
fn extract_addresses(addr: Option<&mail_parser::Address<'_>>) -> Option<serde_json::Value> {
    let addr = addr?;
    match addr {
        mail_parser::Address::List(list) => {
            let addrs: Vec<serde_json::Value> = list
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.name.as_deref().unwrap_or(""),
                        "email": a.address.as_deref().unwrap_or("")
                    })
                })
                .collect();
            Some(serde_json::Value::Array(addrs))
        }
        mail_parser::Address::Group(groups) => {
            let addrs: Vec<serde_json::Value> = groups
                .iter()
                .flat_map(|g| g.addresses.iter())
                .map(|a| {
                    serde_json::json!({
                        "name": a.name.as_deref().unwrap_or(""),
                        "email": a.address.as_deref().unwrap_or("")
                    })
                })
                .collect();
            Some(serde_json::Value::Array(addrs))
        }
    }
}

/// Email/import (RFC 8621 §4.6) — import raw RFC 5322 messages.
pub async fn import(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut not_created = std::collections::HashMap::new();

    // "emails" is a map of client-id → {blobId, mailboxIds, keywords, receivedAt}
    if let Some(emails) = args.get("emails").and_then(|v| v.as_object()) {
        for (client_id, entry) in emails {
            match create_from_blob(db, account_id, entry).await {
                Ok(email_id) => {
                    db::changelog::record(&db.conn, account_id, "Email", email_id, "created").await?;
                    created_map.insert(client_id.clone(), serde_json::json!({
                        "id": email_id.to_string(),
                        "blobId": entry.get("blobId").and_then(|v| v.as_str()).unwrap_or(""),
                        "size": 0,
                    }));
                }
                Err(e) => {
                    not_created.insert(client_id.clone(), SetError {
                        error_type: "invalidArguments".into(),
                        description: Some(e.to_string()),
                    });
                }
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "Email").await?;

    let resp = serde_json::json!({
        "accountId": acct,
        "oldState": old_state,
        "newState": new_state,
        "created": created_map,
        "notCreated": not_created.into_iter().map(|(k, v)| (k, serde_json::to_value(v).unwrap())).collect::<std::collections::HashMap<_, _>>(),
    });

    Ok(resp)
}

/// Email/changes
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);

    let result = db::changelog::changes_since(&db.conn, account_id, "Email", since_state, max).await?;

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
    };

    Ok(serde_json::to_value(resp)?)
}
