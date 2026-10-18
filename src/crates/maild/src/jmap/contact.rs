//! JMAP AddressBook + Contact methods (JSContact, RFC 9553).

use anyhow::Result;
use uuid::Uuid;

use super::types::*;
use crate::db::{self, Db};

/// AddressBook/get
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids: Option<Vec<String>> = args
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let books = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::contact::get_books_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        db::contact::get_all_books(&db.conn, account_id).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "AddressBook").await?;
    let resp = GetResponse {
        account_id: acct,
        state,
        list: books,
        not_found: vec![],
    };
    Ok(serde_json::to_value(resp)?)
}

/// AddressBook/query
pub async fn query(
    db: &Db,
    account_id: i32,
    _args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids = db::contact::query_book_ids(&db.conn, account_id).await?;
    let state = db::changelog::current_state(&db.conn, account_id, "AddressBook").await?;
    let total = ids.len() as u64;
    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        position: 0,
        ids: ids.into_iter().map(|u| u.to_string()).collect(),
        total,
    };
    Ok(serde_json::to_value(resp)?)
}

/// AddressBook/set
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "AddressBook").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();

    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled");
            let description = obj.get("description").and_then(|v| v.as_str());
            match db::contact::create_book(&db.conn, account_id, name, description).await {
                Ok(id) => {
                    db::changelog::record(&db.conn, account_id, "AddressBook", id, "created")
                        .await?;
                    created_map.insert(
                        client_id.clone(),
                        serde_json::json!({ "id": id.to_string() }),
                    );
                }
                Err(e) => tracing::warn!(error = %e, "AddressBook create failed"),
            }
        }
    }

    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else {
                continue;
            };
            let name = patch.get("name").and_then(|v| v.as_str());
            let description = patch.get("description").and_then(|v| v.as_str());
            if db::contact::update_book(&db.conn, account_id, id, name, description).await? {
                db::changelog::record(&db.conn, account_id, "AddressBook", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else {
                continue;
            };
            let Ok(id) = id_str.parse::<Uuid>() else {
                continue;
            };
            if db::contact::delete_book(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "AddressBook", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "AddressBook").await?;
    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: if created_map.is_empty() {
            None
        } else {
            Some(created_map)
        },
        updated: if updated_map.is_empty() {
            None
        } else {
            Some(updated_map)
        },
        destroyed: if destroyed_list.is_empty() {
            None
        } else {
            Some(destroyed_list)
        },
        not_created: None,
        not_updated: None,
        not_destroyed: None,
    };
    Ok(serde_json::to_value(resp)?)
}

/// AddressBook/changes
pub async fn changes(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args
        .get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let max = args
        .get("maxChanges")
        .and_then(|v| v.as_i64())
        .unwrap_or(500);
    let result =
        db::changelog::changes_since(&db.conn, account_id, "AddressBook", since_state, max).await?;
    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
        updated_properties: None,
    };
    Ok(serde_json::to_value(resp)?)
}

/// Contact/get
pub async fn contact_get(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ids: Option<Vec<String>> = args
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let contacts = if let Some(ids) = ids {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        db::contact::get_contacts_by_ids(&db.conn, account_id, &uuids).await?
    } else {
        db::contact::get_all_contacts(&db.conn, account_id, 100).await?
    };

    let state = db::changelog::current_state(&db.conn, account_id, "Contact").await?;
    let resp = GetResponse {
        account_id: acct,
        state,
        list: contacts,
        not_found: vec![],
    };
    Ok(serde_json::to_value(resp)?)
}

/// Map the optional JMAP `sort` (first comparator) to a whitelisted
/// `(column, ascending)` pair for `query_contact_ids`. Supported properties:
/// `name` (→ `full_name`), `email`, `company`. Anything else is the JMAP
/// `unsupportedSort` method error; absent/empty sort defaults to name ascending.
fn parse_contact_sort(sort: Option<&serde_json::Value>) -> Result<(&'static str, bool)> {
    let Some(first) = sort.and_then(|v| v.as_array()).and_then(|a| a.first()) else {
        return Ok(("full_name", true));
    };
    let prop = first.get("property").and_then(|v| v.as_str()).unwrap_or("");
    let asc = first
        .get("isAscending")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let col = match prop {
        "name" | "fullName" | "full_name" => "full_name",
        "email" => "email",
        "company" => "company",
        other => {
            return Err(JmapMethodError {
                error_type: "unsupportedSort".into(),
                description: Some(format!("cannot sort Contact by `{other}`")),
            }
            .into());
        }
    };
    Ok((col, asc))
}

/// Contact/query
pub async fn contact_query(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let filter = args.get("filter");
    let addressbook_id = filter
        .and_then(|f| f.get("addressBookId"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok());
    let text = filter.and_then(|f| f.get("text")).and_then(|v| v.as_str());
    let position = args.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as i64;
    let (sort_col, sort_asc) = parse_contact_sort(args.get("sort"))?;

    let (ids, total) = db::contact::query_contact_ids(
        &db.conn,
        account_id,
        addressbook_id,
        text,
        sort_col,
        sort_asc,
        position,
        limit,
    )
    .await?;
    let state = db::changelog::current_state(&db.conn, account_id, "Contact").await?;
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

/// Contact/set
pub async fn contact_set(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "Contact").await?;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();

    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let addressbook_id = obj
                .get("addressBookId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Uuid>().ok());
            let Some(addressbook_id) = addressbook_id else {
                continue;
            };

            let uid = obj
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or(&Uuid::new_v4().to_string())
                .to_string();

            // Extract denormalized fields from JSContact Card
            let full_name = obj
                .get("name")
                .and_then(|v| v.get("full"))
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("fullName").and_then(|v| v.as_str()));
            let email = obj
                .get("emails")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("address"))
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("email").and_then(|v| v.as_str()));
            let company = obj
                .get("organizations")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("company").and_then(|v| v.as_str()));
            let phone = obj
                .get("phones")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("number"))
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("phone").and_then(|v| v.as_str()));

            // Normalise the stored data so primary_phone() finds the number
            // structurally and contact_to_vcf emits a TEL (preserving any
            // existing phones map).
            let mut data = obj.clone();
            if let Some(p) = phone {
                cosmix_lib_davproto::vcard::set_primary_phone(&mut data, p);
            }

            match db::contact::create_contact(
                &db.conn,
                account_id,
                addressbook_id,
                &uid,
                &data,
                db::contact::ContactMetadata {
                    full_name,
                    email,
                    company,
                },
            )
            .await
            {
                Ok(id) => {
                    db::changelog::record(&db.conn, account_id, "Contact", id, "created").await?;
                    created_map.insert(
                        client_id.clone(),
                        serde_json::json!({ "id": id.to_string(), "uid": uid }),
                    );
                }
                Err(e) => tracing::warn!(error = %e, "Contact create failed"),
            }
        }
    }

    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(id) = id_str.parse::<Uuid>() else {
                continue;
            };
            // Values this patch actually names (None = "not changed").
            let patch_full = patch
                .get("name")
                .and_then(|v| v.get("full"))
                .and_then(|v| v.as_str())
                .or_else(|| patch.get("fullName").and_then(|v| v.as_str()));
            let patch_email = patch
                .get("emails")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("address"))
                .and_then(|v| v.as_str())
                .or_else(|| patch.get("email").and_then(|v| v.as_str()));
            let patch_company = patch
                .get("organizations")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| patch.get("company").and_then(|v| v.as_str()));
            let patch_phone = patch
                .get("phones")
                .and_then(|v| v.as_object())
                .and_then(|m| m.values().next())
                .and_then(|v| v.get("number"))
                .and_then(|v| v.as_str())
                .or_else(|| patch.get("phone").and_then(|v| v.as_str()));

            // MERGE, don't replace. Load the existing row so an unspecified
            // field keeps its value — both the indexed columns (else a
            // name-only edit would null out email/company) AND the rich
            // vCard (phones, X-props the structured model doesn't carry).
            let existing = db::contact::get_contacts_by_ids(&db.conn, account_id, &[id]).await?;
            let Some(cur) = existing.first() else {
                continue; // no such contact → nothing to update
            };
            let full_name = patch_full.or(cur.full_name.as_deref());
            let email = patch_email.or(cur.email.as_deref());
            let company = patch_company.or(cur.company.as_deref());

            let mut data = cur.data.clone();
            if let Some(fname) = patch_full {
                data["name"] = serde_json::json!({ "full": fname });
            }
            if let Some(p) = patch_phone {
                cosmix_lib_davproto::vcard::set_primary_phone(&mut data, p);
            }
            // Patch only the changed lines of the verbatim vCard, keeping
            // its other lines, so DAV clients see the edit too.
            if let Some(raw) = data
                .get(cosmix_lib_davproto::vcard::RAW_VCARD_KEY)
                .and_then(|v| v.as_str())
            {
                let patched = cosmix_lib_davproto::vcard::patch_vcf(
                    raw,
                    patch_full,
                    patch_email,
                    patch_company,
                    patch_phone,
                );
                data[cosmix_lib_davproto::vcard::RAW_VCARD_KEY] =
                    serde_json::Value::String(patched);
            }
            if db::contact::update_contact(
                &db.conn, account_id, id, &data, full_name, email, company,
            )
            .await?
            {
                db::changelog::record(&db.conn, account_id, "Contact", id, "updated").await?;
                updated_map.insert(id_str.clone(), serde_json::Value::Null);
            }
        }
    }

    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else {
                continue;
            };
            let Ok(id) = id_str.parse::<Uuid>() else {
                continue;
            };
            if db::contact::delete_contact(&db.conn, account_id, id).await? {
                db::changelog::record(&db.conn, account_id, "Contact", id, "destroyed").await?;
                destroyed_list.push(id_str.to_string());
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "Contact").await?;
    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: if created_map.is_empty() {
            None
        } else {
            Some(created_map)
        },
        updated: if updated_map.is_empty() {
            None
        } else {
            Some(updated_map)
        },
        destroyed: if destroyed_list.is_empty() {
            None
        } else {
            Some(destroyed_list)
        },
        not_created: None,
        not_updated: None,
        not_destroyed: None,
    };
    Ok(serde_json::to_value(resp)?)
}

/// Contact/changes
pub async fn contact_changes(
    db: &Db,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args
        .get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let max = args
        .get("maxChanges")
        .and_then(|v| v.as_i64())
        .unwrap_or(500);
    let result =
        db::changelog::changes_since(&db.conn, account_id, "Contact", since_state, max).await?;
    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
        updated_properties: None,
    };
    Ok(serde_json::to_value(resp)?)
}
