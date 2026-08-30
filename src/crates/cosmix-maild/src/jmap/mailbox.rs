//! JMAP Mailbox methods (RFC 8621).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use super::types::*;
use crate::db::Db;
use crate::mailstore::{
    self, DestroyPolicy, MailStore, MailboxRecord, MailboxRole, SqliteMailStore,
};
use cosmix_mds::ContainerId;

/// JMAP `UnsignedInt` bound (RFC 8621 §2; RFC 8620 §1.4) — `2^53 − 1`,
/// the largest non-negative integer JSON numbers carry losslessly. JMAP
/// servers MUST NOT emit larger values for `UnsignedInt`-typed fields.
const JMAP_UNSIGNED_INT_MAX: u64 = (1u64 << 53) - 1;

/// Saturate a `u64` count to the JMAP `UnsignedInt` range. The
/// `MailboxRecord` type doc places this narrowing on the response-shape
/// layer rather than the storage adapter; this is that layer.
fn jmap_unsigned(n: u64) -> u64 {
    n.min(JMAP_UNSIGNED_INT_MAX)
}

/// Project a [`MailboxRecord`] into the RFC 8621 §2 `Mailbox` JSON
/// shape.
///
/// `totalThreads` and `unreadThreads` are emitted as `0` until thread
/// aggregation lands on `MailboxRecord` — the legacy `db::mailbox`
/// path never surfaced these values either, so this preserves wire
/// parity. Wiring real counts is the responsibility of a later task
/// once `list_mailboxes` carries thread-level totals.
fn record_to_jmap(r: &MailboxRecord) -> serde_json::Value {
    let role = r.role.map(|x| x.as_jmap_str());
    let parent_id = r.parent.as_ref().map(|p| p.0.to_string());
    let mr = &r.my_rights;
    serde_json::json!({
        "id": r.id.0.to_string(),
        "name": r.name,
        "parentId": parent_id,
        "role": role,
        "sortOrder": r.sort_order,
        "totalEmails": jmap_unsigned(r.total_emails),
        "unreadEmails": jmap_unsigned(r.unread_emails),
        "totalThreads": 0,
        "unreadThreads": 0,
        "myRights": {
            "mayReadItems": mr.may_read_items,
            "mayAddItems": mr.may_add_items,
            "mayRemoveItems": mr.may_remove_items,
            "maySetSeen": mr.may_set_seen,
            "maySetKeywords": mr.may_set_keywords,
            "mayCreateChild": mr.may_create_child,
            "mayRename": mr.may_rename,
            "mayDelete": mr.may_delete,
            "maySubmit": mr.may_submit,
        },
        "isSubscribed": r.is_subscribed,
    })
}

/// Mailbox/get
///
/// Reads the mailbox list from `MailStore::list_mailboxes` and the
/// JMAP `state` token from `MailStore::mailbox_state` — the paired
/// `"<lifecycle_seq>:<set_change_seq>"` form documented in
/// `_doc/planned/mailbox-changes-substrate.md` §"State token". Phase 2
/// (MCS) closed the legacy `db::changelog` Mailbox bridge here.
pub async fn get(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let requested_ids: Option<Vec<String>> = args
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let ms = mailstore.clone();
    let all = tokio::task::spawn_blocking(move || ms.list_mailboxes(account_id)).await??;

    let (list, not_found) = if let Some(ids) = requested_ids {
        let by_id: std::collections::HashMap<Uuid, &MailboxRecord> =
            all.iter().map(|r| (r.id.0, r)).collect();
        let mut list = Vec::new();
        let mut not_found = Vec::new();
        for id_str in &ids {
            match id_str.parse::<Uuid>() {
                Ok(u) => match by_id.get(&u) {
                    Some(r) => list.push(record_to_jmap(r)),
                    None => not_found.push(id_str.clone()),
                },
                Err(_) => not_found.push(id_str.clone()),
            }
        }
        (list, not_found)
    } else {
        let list = all.iter().map(record_to_jmap).collect();
        (list, Vec::new())
    };

    // Paired token from the MCS Phase 2 substrate (see
    // `MailStore::mailbox_state` doc). `spawn_blocking` mirrors every
    // other mailstore call in this handler — the trait is sync, the
    // JMAP entry point is async.
    let ms = mailstore.clone();
    let state = tokio::task::spawn_blocking(move || ms.mailbox_state(account_id)).await??;

    let resp = serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": list,
        "notFound": not_found,
    });

    Ok(resp)
}

/// Mailbox/query
///
/// Phase A scope: filter and sort args are ignored. The handler returns
/// every mailbox in `(sortOrder, name)` order — matching the prior
/// `db::mailbox::query_ids` SQL ordering so client UI listings stay
/// stable across the migration. Adding RFC 8621 §2.3 filter/sort
/// support is a Phase B task; mailbox counts are small enough that
/// in-memory sorting over `list_mailboxes` is acceptable.
///
/// `query_state` derives from `MailStore::mailbox_state` (paired
/// `<lifecycle>:<set_change>` token) — MCS Phase 2 retired the legacy
/// `db::changelog` source.
pub async fn query(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    _args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let ms = mailstore.clone();
    let mut all = tokio::task::spawn_blocking(move || ms.list_mailboxes(account_id)).await??;
    // `(sortOrder, name)` ASC matches the prior `db::mailbox::query_ids`
    // SQL ordering. Rust `str::cmp` is byte-wise, which aligns with
    // SQLite's default BINARY collation on the MDS `container.name`
    // column — if a future schema revision attaches `COLLATE NOCASE`
    // or a locale-aware collation to that column, this comparator must
    // be revisited so the in-memory and SQL orderings do not diverge.
    all.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.name.cmp(&b.name))
    });

    let ms = mailstore.clone();
    let state = tokio::task::spawn_blocking(move || ms.mailbox_state(account_id)).await??;

    // Phase A scope: `_args` filter/sort are ignored, so `total` is
    // simply the listing length. When Phase B adds RFC 8621 §2.3
    // filter support, `total` MUST be recomputed after filtering — §5.5
    // requires `total` to equal the number of matching results, not the
    // unfiltered set size.
    let total = all.len() as u64;
    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        position: 0,
        ids: all.into_iter().map(|r| r.id.0.to_string()).collect(),
        total,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Mailbox/set
///
/// All three branches (`create`, `update`, `destroy`) write through
/// the [`MailStore`] surface, whose substrate impl emits the
/// `container_change_set` lifecycle rows that `Mailbox/changes` reads
/// back. The MCS Phase 2 cutover removed the parallel
/// `db::changelog::record(..., "Mailbox", ...)` writes — the substrate
/// row IS the change-stream entry now.
pub async fn set(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let ms_old = mailstore.clone();
    let old_state = tokio::task::spawn_blocking(move || ms_old.mailbox_state(account_id)).await??;

    let mut created_map = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();
    let mut not_created: std::collections::HashMap<String, SetError> =
        std::collections::HashMap::new();
    let mut not_updated: std::collections::HashMap<String, SetError> =
        std::collections::HashMap::new();

    // Handle create
    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        // Snapshot the set of roles already in use so the per-iteration
        // RFC 8621 §2.5 role-uniqueness check is one hash lookup. Read
        // once per Mailbox/set call rather than per create iteration —
        // a concurrent `Mailbox/set` from another session could insert
        // a colliding role between snapshot and create, but JMAP is
        // typically single-client per account and MDS has no
        // `(account, special_use)` unique index to enforce strict
        // atomicity (out of scope here). The snapshot is augmented as
        // new roles get assigned within this same call so siblings
        // can't race each other.
        let ms_snapshot = mailstore.clone();
        let snapshot_records: Vec<MailboxRecord> =
            tokio::task::spawn_blocking(move || ms_snapshot.list_mailboxes(account_id)).await??;
        let mut roles_in_use: std::collections::HashSet<MailboxRole> =
            snapshot_records.iter().filter_map(|m| m.role).collect();

        for (client_id, obj) in create {
            // Required: `name`. RFC 8621 §2.5 — missing/blank name is
            // `invalidProperties`.
            let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
                not_created.insert(
                    client_id.clone(),
                    set_error_invalid_properties("name", "missing or non-string"),
                );
                continue;
            };
            if name.is_empty() {
                not_created.insert(
                    client_id.clone(),
                    set_error_invalid_properties("name", "must be non-empty"),
                );
                continue;
            }

            // Optional: `parentId`. Absent or JSON null = root mailbox;
            // present-and-non-null but un-parseable = invalidProperties.
            let parent_id = match obj.get("parentId") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => match s.parse::<Uuid>() {
                    Ok(u) => Some(ContainerId(u)),
                    Err(_) => {
                        not_created.insert(
                            client_id.clone(),
                            set_error_invalid_properties("parentId", "not a valid id"),
                        );
                        continue;
                    }
                },
                Some(_) => {
                    not_created.insert(
                        client_id.clone(),
                        set_error_invalid_properties("parentId", "must be a string or null"),
                    );
                    continue;
                }
            };

            // Optional: `role`. Absent or null = no role; unknown
            // wire-token = invalidProperties.
            let role = match obj.get("role") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => match MailboxRole::from_jmap_str(s) {
                    Some(r) => Some(r),
                    None => {
                        not_created.insert(
                            client_id.clone(),
                            set_error_invalid_properties("role", "unknown role"),
                        );
                        continue;
                    }
                },
                Some(_) => {
                    not_created.insert(
                        client_id.clone(),
                        set_error_invalid_properties("role", "must be a string or null"),
                    );
                    continue;
                }
            };

            // RFC 8621 §2.5: a role MUST be unique per account. Check
            // against the snapshot (plus roles assigned by prior
            // iterations of this same set call).
            if let Some(r) = role
                && roles_in_use.contains(&r)
            {
                not_created.insert(
                    client_id.clone(),
                    set_error_invalid_properties("role", "role already in use by another mailbox"),
                );
                continue;
            }

            // `sortOrder` is JMAP `UnsignedInt` (RFC 8621 §2; RFC 8620
            // §1.4) — 53-bit unsigned. The on-disk encoding narrows to
            // u32 (`jmap_sort_order_from_attrs` reads back via
            // `u32::try_from`); a value above `u32::MAX` would
            // silently round-trip to 0 if accepted, so reject it with
            // `invalidProperties` rather than corrupt the field.
            let sort_order = match obj.get("sortOrder") {
                None | Some(serde_json::Value::Null) => 0u32,
                Some(serde_json::Value::Number(n)) => match n.as_u64() {
                    Some(v) if v <= u32::MAX as u64 => v as u32,
                    _ => {
                        not_created.insert(
                            client_id.clone(),
                            set_error_invalid_properties(
                                "sortOrder",
                                "must be a non-negative integer not exceeding u32::MAX",
                            ),
                        );
                        continue;
                    }
                },
                Some(_) => {
                    not_created.insert(
                        client_id.clone(),
                        set_error_invalid_properties("sortOrder", "must be a non-negative integer"),
                    );
                    continue;
                }
            };
            let is_subscribed = obj
                .get("isSubscribed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let attrs = mailstore::jmap_attrs_for_create(role, sort_order, is_subscribed);
            let ms = mailstore.clone();
            let name_owned = name.to_string();
            // A panic from `create_mailbox` returns `JoinError` here.
            // RFC 8620 §5.3 requires per-object errors to land in
            // `notCreated` rather than failing the whole `Mailbox/set`
            // — propagating with `?` would orphan any creates from
            // earlier iterations that already committed to MDS +
            // changelog.
            let result = match tokio::task::spawn_blocking(move || {
                ms.create_mailbox(account_id, &name_owned, parent_id, attrs)
            })
            .await
            {
                Ok(r) => r,
                Err(join_err) => {
                    tracing::warn!(client_id = %client_id, error = %join_err, "create_mailbox panicked");
                    not_created.insert(
                        client_id.clone(),
                        SetError {
                            error_type: "serverFail".into(),
                            description: Some(format!("create task failed: {join_err}")),
                        },
                    );
                    continue;
                }
            };

            match result {
                Ok(id) => {
                    // Substrate `create_mailbox` already emitted the
                    // CONTAINER_CREATED row inside its `with_set_tx` —
                    // `Mailbox/changes` reads it back directly.
                    created_map.insert(
                        client_id.clone(),
                        serde_json::json!({ "id": id.0.to_string() }),
                    );
                    // Track the new role so a sibling create within
                    // the same Mailbox/set call can't re-assign it.
                    if let Some(r) = role {
                        roles_in_use.insert(r);
                    }
                }
                Err(e) => {
                    tracing::warn!(client_id = %client_id, error = %e, "Mailbox create failed");
                    not_created.insert(client_id.clone(), set_error_for_create(&e));
                }
            }
        }
    }

    // Handle update
    if let Some(update) = args.get("update").and_then(|v| v.as_object())
        && !update.is_empty()
    {
        // Snapshot the mailbox set so we can:
        //   * resolve the current name/parent when only one is patched
        //     (rename_mailbox needs both),
        //   * enforce JMAP `role` per-account uniqueness (RFC 8621 §2.5)
        //     against pre-existing rows plus updates that succeed
        //     earlier in the same Mailbox/set call.
        // Snapshot post-create so a `create+update` in the same call
        // sees the new mailbox if the update targets it.
        let ms_snapshot = mailstore.clone();
        let snapshot_records: Vec<MailboxRecord> =
            tokio::task::spawn_blocking(move || ms_snapshot.list_mailboxes(account_id)).await??;
        let mut by_id: std::collections::HashMap<Uuid, MailboxRecord> =
            snapshot_records.into_iter().map(|r| (r.id.0, r)).collect();
        let mut roles_in_use: std::collections::HashSet<MailboxRole> =
            by_id.values().filter_map(|m| m.role).collect();

        for (id_str, patch) in update {
            let Ok(id_uuid) = id_str.parse::<Uuid>() else {
                not_updated.insert(
                    id_str.clone(),
                    SetError {
                        error_type: "notFound".into(),
                        description: Some("malformed id".into()),
                    },
                );
                continue;
            };
            let id = ContainerId(id_uuid);
            let Some(current) = by_id.get(&id_uuid).cloned() else {
                not_updated.insert(
                    id_str.clone(),
                    SetError {
                        error_type: "notFound".into(),
                        description: None,
                    },
                );
                continue;
            };

            // Patch must be a JSON object per JMAP §5.3.
            let Some(patch_obj) = patch.as_object() else {
                not_updated.insert(
                    id_str.clone(),
                    set_error_invalid_properties("", "patch must be an object"),
                );
                continue;
            };

            // Parse each mutable field. Absent = "leave alone";
            // present-but-malformed = invalidProperties.
            let mut new_name: Option<String> = None;
            if let Some(v) = patch_obj.get("name") {
                match v.as_str() {
                    Some(s) if !s.is_empty() => new_name = Some(s.to_string()),
                    Some(_) => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties("name", "must be non-empty"),
                        );
                        continue;
                    }
                    None => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties("name", "must be a string"),
                        );
                        continue;
                    }
                }
            }

            // `parentId`: absent = leave alone; null = move to root;
            // string = parse as UUID.
            let mut new_parent_set = false;
            let mut new_parent: Option<ContainerId> = None;
            if let Some(v) = patch_obj.get("parentId") {
                new_parent_set = true;
                match v {
                    serde_json::Value::Null => new_parent = None,
                    serde_json::Value::String(s) => match s.parse::<Uuid>() {
                        Ok(u) => new_parent = Some(ContainerId(u)),
                        Err(_) => {
                            not_updated.insert(
                                id_str.clone(),
                                set_error_invalid_properties("parentId", "not a valid id"),
                            );
                            continue;
                        }
                    },
                    _ => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties("parentId", "must be a string or null"),
                        );
                        continue;
                    }
                }
            }

            // `role`: absent = leave alone; null = clear; string = role.
            // `Some(None)` semantics on the MailStore call.
            let mut new_role_patch: Option<Option<MailboxRole>> = None;
            if let Some(v) = patch_obj.get("role") {
                match v {
                    serde_json::Value::Null => new_role_patch = Some(None),
                    serde_json::Value::String(s) => match MailboxRole::from_jmap_str(s) {
                        Some(r) => new_role_patch = Some(Some(r)),
                        None => {
                            not_updated.insert(
                                id_str.clone(),
                                set_error_invalid_properties("role", "unknown role"),
                            );
                            continue;
                        }
                    },
                    _ => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties("role", "must be a string or null"),
                        );
                        continue;
                    }
                }
            }

            // `sortOrder`: same JMAP UnsignedInt narrowing as create.
            let mut new_sort_order: Option<u32> = None;
            if let Some(v) = patch_obj.get("sortOrder") {
                match v {
                    serde_json::Value::Null => new_sort_order = Some(0),
                    serde_json::Value::Number(n) => match n.as_u64() {
                        Some(x) if x <= u32::MAX as u64 => new_sort_order = Some(x as u32),
                        _ => {
                            not_updated.insert(
                                id_str.clone(),
                                set_error_invalid_properties(
                                    "sortOrder",
                                    "must be a non-negative integer not exceeding u32::MAX",
                                ),
                            );
                            continue;
                        }
                    },
                    _ => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties(
                                "sortOrder",
                                "must be a non-negative integer",
                            ),
                        );
                        continue;
                    }
                }
            }

            // `isSubscribed`: absent = leave alone; bool = set.
            let mut new_is_subscribed: Option<bool> = None;
            if let Some(v) = patch_obj.get("isSubscribed") {
                match v.as_bool() {
                    Some(b) => new_is_subscribed = Some(b),
                    None => {
                        not_updated.insert(
                            id_str.clone(),
                            set_error_invalid_properties("isSubscribed", "must be a boolean"),
                        );
                        continue;
                    }
                }
            }

            // Role-uniqueness check: only when the patch *sets* a non-
            // null role. Compare against the snapshot ∪ in-call
            // mutations, but exempt this same mailbox's existing role
            // (re-asserting your own role is a no-op, not a conflict).
            if let Some(Some(r)) = new_role_patch
                && Some(r) != current.role
                && roles_in_use.contains(&r)
            {
                not_updated.insert(
                    id_str.clone(),
                    set_error_invalid_properties("role", "role already in use by another mailbox"),
                );
                continue;
            }

            // Single combined call: rename/reparent + attrs go in one
            // `with_set_tx` so a concurrent destroy can't land between
            // a rename commit and an attrs commit (Sonnet review of
            // Task 2.4 — see `MailStore::update_mailbox` doc).
            let parent_arg: Option<Option<ContainerId>> = if new_parent_set {
                Some(new_parent)
            } else {
                None
            };
            let name_arg: Option<String> = new_name.clone();
            let role_arg = new_role_patch;
            let so_arg = new_sort_order;
            let sub_arg = new_is_subscribed;
            let ms = mailstore.clone();
            let update_result = match tokio::task::spawn_blocking(move || {
                ms.update_mailbox(
                    account_id,
                    id,
                    name_arg.as_deref(),
                    parent_arg,
                    role_arg,
                    so_arg,
                    sub_arg,
                )
            })
            .await
            {
                Ok(r) => r,
                Err(join_err) => {
                    tracing::warn!(id = %id_str, error = %join_err, "update_mailbox panicked");
                    not_updated.insert(
                        id_str.clone(),
                        SetError {
                            error_type: "serverFail".into(),
                            description: Some(format!("update task failed: {join_err}")),
                        },
                    );
                    continue;
                }
            };
            if let Err(e) = update_result {
                tracing::warn!(id = %id_str, error = %e, "Mailbox update failed");
                not_updated.insert(id_str.clone(), set_error_for_update(&e));
                continue;
            }
            let parent_changed = new_parent_set;

            // Substrate-driven lifecycle stream: the
            // `update_mailbox` / `rename_mailbox` calls already wrote
            // the matching CONTAINER_RENAMED / CONTAINER_ATTRS_CHANGED
            // row (and the substrate suppresses no-op diffs at the
            // `update_container_attrs` boundary). No-op patches
            // therefore do not surface a spurious `Mailbox/changes`
            // entry — the JMAP `updated` ack is still emitted per
            // RFC 8621 §2.5 (the id was processed).

            // Maintain `roles_in_use` and the snapshot map for any
            // sibling iterations that follow.
            let prev_role = current.role;
            let next_role = match new_role_patch {
                None => prev_role,
                Some(opt) => opt,
            };
            if prev_role != next_role {
                if let Some(p) = prev_role {
                    roles_in_use.remove(&p);
                }
                if let Some(n) = next_role {
                    roles_in_use.insert(n);
                }
            }
            let mut updated_record = current;
            if let Some(n) = &new_name {
                updated_record.name = n.clone();
            }
            if parent_changed {
                updated_record.parent = new_parent;
            }
            if let Some(opt) = new_role_patch {
                updated_record.role = opt;
            }
            if let Some(so) = new_sort_order {
                updated_record.sort_order = so;
            }
            if let Some(sub) = new_is_subscribed {
                updated_record.is_subscribed = sub;
            }
            by_id.insert(id_uuid, updated_record);

            updated_map.insert(id_str.clone(), serde_json::Value::Null);
        }
    }

    // Handle destroy. JMAP RFC 8621 §2.5: top-level boolean
    // `onDestroyRemoveEmails` (default false) toggles between
    // reject-non-empty (`DestroyPolicy::Reject`) and cascade-delete
    // (`DestroyPolicy::Delete`). The flag is per-call, not per-id.
    let mut not_destroyed: HashMap<String, SetError> = HashMap::new();
    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        let on_destroy_remove_emails = args
            .get("onDestroyRemoveEmails")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let policy = if on_destroy_remove_emails {
            DestroyPolicy::Delete
        } else {
            DestroyPolicy::Reject
        };
        for id_val in destroy {
            // Non-string id values (`null`, number, object) are
            // malformed JSON for `[Id]`. Reflect them in
            // `not_destroyed` using the JSON literal as the key —
            // mirrors the malformed-UUID arm so the client learns
            // that every requested id was processed exactly once
            // (Sonnet review of Task 2.5: silent `continue` here was
            // inconsistent with the malformed-UUID branch right
            // below).
            let id_str = match id_val.as_str() {
                Some(s) => s.to_string(),
                None => {
                    let key = id_val.to_string();
                    not_destroyed.insert(
                        key,
                        SetError {
                            error_type: "notFound".into(),
                            description: Some("id must be a string".into()),
                        },
                    );
                    continue;
                }
            };
            let Ok(id_uuid) = id_str.parse::<Uuid>() else {
                not_destroyed.insert(
                    id_str.clone(),
                    SetError {
                        error_type: "notFound".into(),
                        description: Some("malformed id".into()),
                    },
                );
                continue;
            };
            let id = ContainerId(id_uuid);
            let ms = mailstore.clone();
            let destroy_result = match tokio::task::spawn_blocking(move || {
                ms.destroy_mailbox(account_id, id, policy)
            })
            .await
            {
                Ok(r) => r,
                Err(join_err) => {
                    tracing::warn!(id = %id_str, error = %join_err, "destroy_mailbox panicked");
                    not_destroyed.insert(
                        id_str.clone(),
                        SetError {
                            error_type: "serverFail".into(),
                            description: Some(format!("destroy task failed: {join_err}")),
                        },
                    );
                    continue;
                }
            };
            match destroy_result {
                Ok(()) => {
                    // Substrate `destroy_mailbox` emitted the
                    // CONTAINER_DESTROYED lifecycle row inside its
                    // `with_set_tx`; nothing else to write here.
                    destroyed_list.push(id_str);
                }
                Err(e) => {
                    tracing::warn!(id = %id_str, error = %e, "Mailbox destroy failed");
                    not_destroyed.insert(id_str, set_error_for_destroy(&e));
                }
            }
        }
    }

    let ms_new = mailstore.clone();
    let new_state = tokio::task::spawn_blocking(move || ms_new.mailbox_state(account_id)).await??;

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
        not_created: if not_created.is_empty() {
            None
        } else {
            Some(not_created)
        },
        not_updated: if not_updated.is_empty() {
            None
        } else {
            Some(not_updated)
        },
        not_destroyed: if not_destroyed.is_empty() {
            None
        } else {
            Some(not_destroyed)
        },
    };

    Ok(serde_json::to_value(resp)?)
}

/// Build a JMAP `invalidProperties` SetError. RFC 8620 §5.3 allows an
/// optional `properties: [String]` array on `invalidProperties`; the
/// current `SetError` carries only `type` + `description`, so the
/// offending property name is inlined into the description until the
/// JMAP types grow that field.
fn set_error_invalid_properties(prop: &str, why: &str) -> SetError {
    SetError {
        error_type: "invalidProperties".into(),
        description: Some(format!("{prop}: {why}")),
    }
}

/// Map a `MailStore::create_mailbox` error into a JMAP setError per
/// RFC 8621 §2.5. Prefix-matches on the error vocabulary documented on
/// the `MailStore::create_mailbox` trait method:
///   * `"reserved name"` / `"mailbox invalid name"` →
///     `invalidProperties` (`name`)
///   * `"mailbox parent not found"` → `invalidProperties` (`parentId`)
///   * `"mailbox already exists"` → `invalidProperties` (`name`)
///   * `"mailbox special-use already in use"` →
///     `invalidProperties` (`role`) — the storage barrier (T6) catches
///     concurrent-peer wins that the pre-create snapshot scan missed.
///   * anything else → `serverFail` (with the error string preserved
///     in `description` for operator triage).
fn set_error_for_create(err: &anyhow::Error) -> SetError {
    let s = err.to_string();
    if s.starts_with("reserved name") {
        // Don't echo the internal sentinel back to the JMAP client.
        set_error_invalid_properties("name", "reserved name")
    } else if s.starts_with("mailbox invalid name") {
        set_error_invalid_properties("name", "invalid name")
    } else if s.starts_with("mailbox parent not found") {
        set_error_invalid_properties("parentId", "parent mailbox not found")
    } else if s.starts_with("mailbox already exists") {
        set_error_invalid_properties("name", "name already in use under this parent")
    } else if s.starts_with("mailbox special-use already in use") {
        set_error_invalid_properties("role", "role already in use by another mailbox")
    } else {
        SetError {
            error_type: "serverFail".into(),
            description: Some(s),
        }
    }
}

/// Map a `MailStore::update_mailbox` error onto a JMAP `Mailbox/set`
/// update setError per RFC 8621 §2.5. Prefix vocabulary (union of the
/// rename + attrs surfaces, documented on the trait method):
///   * `"reserved name"` / `"mailbox invalid name"` →
///     `invalidProperties` (`name`)
///   * `"mailbox not found"` → `notFound` (the update target itself
///     vanished — `Mailbox/set update` against a missing id is
///     `notFound`, not `invalidProperties`)
///   * `"mailbox parent not found"` → `invalidProperties` (`parentId`)
///   * `"mailbox already exists"` → `invalidProperties` (`name`)
///   * `"mailbox cycle"` → `invalidProperties` (`parentId`) — RFC 8621
///     §2.5 lists "the parent must not form a cycle" under this filter
///   * everything else (decode/encode/SQL plumbing) → `serverFail`
fn set_error_for_update(err: &anyhow::Error) -> SetError {
    let s = err.to_string();
    if s.starts_with("reserved name") {
        set_error_invalid_properties("name", "reserved name")
    } else if s.starts_with("mailbox invalid name") {
        set_error_invalid_properties("name", "invalid name")
    } else if s.starts_with("mailbox not found") {
        SetError {
            error_type: "notFound".into(),
            description: None,
        }
    } else if s.starts_with("mailbox parent not found") {
        set_error_invalid_properties("parentId", "parent mailbox not found")
    } else if s.starts_with("mailbox already exists") {
        set_error_invalid_properties("name", "name already in use under this parent")
    } else if s.starts_with("mailbox cycle") {
        set_error_invalid_properties("parentId", "would form a parent cycle")
    } else {
        SetError {
            error_type: "serverFail".into(),
            description: Some(s),
        }
    }
}

/// Map a `MailStore::destroy_mailbox` error onto a JMAP `Mailbox/set`
/// destroy setError per RFC 8621 §2.5. Prefix vocabulary documented on
/// the trait method:
///   * `"mailbox not found"` → `notFound`
///   * `"mailbox not empty"` → `mailboxHasEmail` (RFC 8621 §2.5)
///   * `"mailbox has children"` → `mailboxHasChild` (RFC 8621 §2.5)
///   * `"mailbox has role"` → `forbidden` (RFC 8621 §2.5 lets us
///     refuse destroy on any mailbox carrying a non-null
///     `attrs.special_use`; clients clear the role first)
///   * `"reserved container"` → `forbidden` (defensive: JMAP listings
///     filter `__upload_staging__`, so a client should never legitimately
///     hold its id)
///   * `"mailbox has no inbox"` / `"mailbox conflicting policy"` →
///     `serverFail` — these belong to `DestroyPolicy::MoveToInbox`
///     which is not exposed to JMAP clients in Phase A; reaching them
///     means storage drift, not a client error
fn set_error_for_destroy(err: &anyhow::Error) -> SetError {
    let s = err.to_string();
    if s.starts_with("mailbox not found") {
        SetError {
            error_type: "notFound".into(),
            description: None,
        }
    } else if s.starts_with("mailbox not empty") {
        SetError {
            error_type: "mailboxHasEmail".into(),
            description: None,
        }
    } else if s.starts_with("mailbox has children") {
        SetError {
            error_type: "mailboxHasChild".into(),
            description: None,
        }
    } else if s.starts_with("mailbox has role") {
        // RFC 8621 §2.5 lets implementations refuse to destroy
        // role mailboxes. JMAP has no dedicated error class for
        // this, so map to `forbidden` with a description that
        // tells the client to clear the role first.
        SetError {
            error_type: "forbidden".into(),
            description: Some(s),
        }
    } else if s.starts_with("reserved container") {
        SetError {
            error_type: "forbidden".into(),
            description: Some("reserved container".into()),
        }
    } else {
        SetError {
            error_type: "serverFail".into(),
            description: Some(s),
        }
    }
}

/// Mailbox/changes — RFC 8621 §2.6.
///
/// Backed by `MailStore::mailbox_changes` against the MCS Phase 2
/// substrate (`_doc/planned/mailbox-changes-substrate.md`): the
/// `container_change_set` lifecycle stream paired with the
/// `set_change` count stream. `sinceState` is the
/// `"<lifecycle_seq>:<set_change_seq>"` token previously returned
/// from `Mailbox/get`, `Mailbox/query`, or a prior `Mailbox/changes`
/// call.
///
/// Strict coercion:
/// * Absent `sinceState` is rejected — `Mailbox/changes` is a
///   delta-from-known-state call per RFC 8620 §5.2, not a full sync.
///   Clients use `Mailbox/get` for initial state acquisition.
/// * Non-string `sinceState` (number, null, object) → `invalidArguments`.
/// * Malformed token (missing colon, non-decimal half) → `invalidArguments`.
/// * Token whose either half exceeds the current per-set head →
///   `cannotCalculateChanges`.
///
/// `maxChanges` is RFC 8620 §5.2 `PositiveInt`; absent / null defaults
/// to 500. `< 1` and non-numeric values are rejected as
/// `invalidArguments`. The substrate caps the lifecycle slice; count
/// synthesis runs only on the final (non-truncated) page.
pub async fn changes(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let since_state: String = match args.get("sinceState") {
        None | Some(serde_json::Value::Null) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(
                    "sinceState is required for Mailbox/changes (use Mailbox/get for \
                     initial state acquisition per RFC 8620 §5.2)"
                        .into(),
                ),
            }
            .into());
        }
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "sinceState must be a String per RFC 8620 §1.3; got {}",
                    super::email::type_name_of_json(other)
                )),
            }
            .into());
        }
    };

    let max: i64 = match args.get("maxChanges") {
        None | Some(serde_json::Value::Null) => 500,
        Some(serde_json::Value::Number(n)) => {
            let v = n.as_i64().ok_or_else(|| JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "maxChanges must fit an i64 (RFC 8620 §5.2 specifies PositiveInt); got {n}"
                )),
            })?;
            if v < 1 {
                return Err(JmapMethodError {
                    error_type: "invalidArguments".into(),
                    description: Some(format!(
                        "maxChanges is PositiveInt per RFC 8620 §5.2; got {v}"
                    )),
                }
                .into());
            }
            v
        }
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "maxChanges must be a Number per RFC 8620 §5.2; got {}",
                    super::email::type_name_of_json(other)
                )),
            }
            .into());
        }
    };

    let ms = mailstore.clone();
    let since_clone = since_state.clone();
    let max_usize = max as usize;
    let result = match tokio::task::spawn_blocking(move || {
        ms.mailbox_changes(account_id, &since_clone, max_usize)
    })
    .await?
    {
        Ok(r) => r,
        Err(e) => {
            // Trait error vocabulary → JMAP method error mapping:
            //   "invalid sinceState"   → invalidArguments
            //   "future since_state"   → cannotCalculateChanges
            //   "stale since_state"    → cannotCalculateChanges
            //     (MCS Phase 3-B — paired token half below its
            //     retention floor)
            // Anything else propagates as a generic anyhow error and
            // becomes `serverFail` at the dispatcher boundary.
            let msg = e.to_string();
            if msg.starts_with("invalid sinceState") {
                return Err(JmapMethodError {
                    error_type: "invalidArguments".into(),
                    description: Some(msg),
                }
                .into());
            }
            if msg.starts_with("future since_state") || msg.starts_with("stale since_state") {
                return Err(JmapMethodError {
                    error_type: "cannotCalculateChanges".into(),
                    description: Some(format!(
                        "sinceState {since_state:?} is not a valid prior Mailbox state ({msg})"
                    )),
                }
                .into());
            }
            return Err(e);
        }
    };

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state,
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result
            .created
            .into_iter()
            .map(|c| c.0.to_string())
            .collect(),
        updated: result
            .updated
            .into_iter()
            .map(|c| c.0.to_string())
            .collect(),
        destroyed: result
            .destroyed
            .into_iter()
            .map(|c| c.0.to_string())
            .collect(),
        // RFC 8621 §2.6: `updatedProperties` is required for
        // `Mailbox/changes`. Phase 2 emits `null` ("any may have
        // changed") uniformly per the conservative end of the
        // null-or-list contract. Substrate may later surface a
        // tighter intersection.
        updated_properties: result
            .updated_properties
            .map(|v| {
                serde_json::Value::Array(v.into_iter().map(serde_json::Value::String).collect())
            })
            .or(Some(serde_json::Value::Null)),
    };

    Ok(serde_json::to_value(resp)?)
}

#[cfg(test)]
mod tests {
    //! `record_to_jmap` projection tests. The async `get` handler is
    //! covered indirectly: `MailStore::list_mailboxes` is exercised in
    //! `mailstore::tests`, and `record_to_jmap` is the only logic
    //! `get` adds on top — every other line is plumbing
    //! (`spawn_blocking`, ID parsing, `notFound` partition) tested
    //! end-to-end in the Phase 2.7 verification.
    use super::*;
    use crate::mailstore::{MailboxRights, MailboxRole};
    use cosmix_mds::{ContainerAttrs, ContainerId};

    fn rec(
        role: Option<MailboxRole>,
        sort: u32,
        sub: bool,
        rights: MailboxRights,
    ) -> MailboxRecord {
        MailboxRecord {
            id: ContainerId(uuid::Uuid::nil()),
            name: "Inbox".into(),
            parent: None,
            attrs: ContainerAttrs {
                special_use: None,
                subscribed: sub,
                extra: serde_json::json!({}),
            },
            total_emails: 5,
            unread_emails: 2,
            role,
            sort_order: sort,
            is_subscribed: sub,
            my_rights: rights,
            uid_validity: 0,
            highest_mod_seq: 0,
            uid_next: 1,
        }
    }

    #[test]
    fn record_to_jmap_emits_rfc8621_field_names() {
        let v = record_to_jmap(&rec(
            Some(MailboxRole::Inbox),
            7,
            true,
            MailboxRights::full(),
        ));
        assert_eq!(v["id"], uuid::Uuid::nil().to_string());
        assert_eq!(v["name"], "Inbox");
        assert_eq!(v["parentId"], serde_json::Value::Null);
        assert_eq!(v["role"], "inbox");
        assert_eq!(v["sortOrder"], 7);
        assert_eq!(v["totalEmails"], 5);
        assert_eq!(v["unreadEmails"], 2);
        assert_eq!(v["totalThreads"], 0);
        assert_eq!(v["unreadThreads"], 0);
        assert_eq!(v["isSubscribed"], true);
        let rights = &v["myRights"];
        assert_eq!(rights["mayReadItems"], true);
        assert_eq!(rights["mayAddItems"], true);
        assert_eq!(rights["mayRemoveItems"], true);
        assert_eq!(rights["maySetSeen"], true);
        assert_eq!(rights["maySetKeywords"], true);
        assert_eq!(rights["mayCreateChild"], true);
        assert_eq!(rights["mayRename"], true);
        assert_eq!(rights["mayDelete"], true);
        assert_eq!(rights["maySubmit"], true);
    }

    #[test]
    fn record_to_jmap_emits_null_role_when_none() {
        let v = record_to_jmap(&rec(None, 0, false, MailboxRights::full()));
        assert_eq!(v["role"], serde_json::Value::Null);
        assert_eq!(v["isSubscribed"], false);
    }

    #[test]
    fn record_to_jmap_serialises_parent_id_as_string() {
        let parent = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let mut r = rec(None, 0, false, MailboxRights::full());
        r.parent = Some(ContainerId(parent));
        let v = record_to_jmap(&r);
        assert_eq!(v["parentId"], parent.to_string());
    }

    #[test]
    fn record_to_jmap_partial_rights_serialise_per_bit() {
        let mr = MailboxRights {
            may_read_items: true,
            may_add_items: false,
            may_remove_items: true,
            may_set_seen: false,
            may_set_keywords: true,
            may_create_child: false,
            may_rename: true,
            may_delete: false,
            may_submit: true,
        };
        let v = record_to_jmap(&rec(None, 0, false, mr));
        let rights = &v["myRights"];
        assert_eq!(rights["mayReadItems"], true);
        assert_eq!(rights["mayAddItems"], false);
        assert_eq!(rights["mayRemoveItems"], true);
        assert_eq!(rights["maySetSeen"], false);
        assert_eq!(rights["maySetKeywords"], true);
        assert_eq!(rights["mayCreateChild"], false);
        assert_eq!(rights["mayRename"], true);
        assert_eq!(rights["mayDelete"], false);
        assert_eq!(rights["maySubmit"], true);
    }

    #[test]
    fn record_to_jmap_saturates_counts_to_jmap_unsigned_int_max() {
        let mut r = rec(None, 0, false, MailboxRights::full());
        r.total_emails = u64::MAX;
        r.unread_emails = JMAP_UNSIGNED_INT_MAX + 1;
        let v = record_to_jmap(&r);
        assert_eq!(v["totalEmails"], JMAP_UNSIGNED_INT_MAX);
        assert_eq!(v["unreadEmails"], JMAP_UNSIGNED_INT_MAX);

        // Within-range counts pass through unchanged.
        r.total_emails = 12345;
        r.unread_emails = 67;
        let v = record_to_jmap(&r);
        assert_eq!(v["totalEmails"], 12345);
        assert_eq!(v["unreadEmails"], 67);
    }

    #[test]
    fn record_to_jmap_role_token_lowercase_for_all_variants() {
        for (role, expected) in [
            (MailboxRole::Inbox, "inbox"),
            (MailboxRole::Archive, "archive"),
            (MailboxRole::Drafts, "drafts"),
            (MailboxRole::Flagged, "flagged"),
            (MailboxRole::Important, "important"),
            (MailboxRole::Junk, "junk"),
            (MailboxRole::Sent, "sent"),
            (MailboxRole::Trash, "trash"),
            (MailboxRole::All, "all"),
        ] {
            let v = record_to_jmap(&rec(Some(role), 0, false, MailboxRights::full()));
            assert_eq!(v["role"], expected, "role variant {:?}", role);
        }
    }

    #[test]
    fn set_error_for_create_maps_known_prefixes() {
        let cases = [
            (
                anyhow::anyhow!("reserved name \"__upload_staging__\": collides"),
                "invalidProperties",
                "name",
            ),
            (
                anyhow::anyhow!("mailbox invalid name: must be non-empty"),
                "invalidProperties",
                "name",
            ),
            (
                anyhow::anyhow!("mailbox parent not found: bogus-id"),
                "invalidProperties",
                "parentId",
            ),
            (
                anyhow::anyhow!("mailbox already exists: Inbox"),
                "invalidProperties",
                "name",
            ),
            (
                anyhow::anyhow!("mailbox special-use already in use: \\Drafts"),
                "invalidProperties",
                "role",
            ),
        ];
        for (err, expected_type, expected_prop_in_desc) in cases {
            let se = set_error_for_create(&err);
            assert_eq!(se.error_type, expected_type, "error: {err}");
            assert!(
                se.description
                    .as_deref()
                    .unwrap_or("")
                    .starts_with(&format!("{expected_prop_in_desc}:")),
                "expected property prefix `{expected_prop_in_desc}:` in description \
                 for error {err}, got {:?}",
                se.description,
            );
        }
    }

    #[test]
    fn set_error_for_create_falls_back_to_server_fail() {
        let err = anyhow::anyhow!("rusqlite: database is locked");
        let se = set_error_for_create(&err);
        assert_eq!(se.error_type, "serverFail");
        assert!(
            se.description
                .as_deref()
                .unwrap()
                .contains("database is locked")
        );
    }

    #[test]
    fn jmap_attrs_for_create_round_trips_through_reader() {
        use crate::mailstore::{jmap_attrs_for_create, jmap_sort_order_from_attrs};

        // Role + sort_order + subscription all present.
        let a = jmap_attrs_for_create(Some(MailboxRole::Inbox), 7, true);
        assert_eq!(a.special_use.as_deref(), Some("\\Inbox"));
        assert!(a.subscribed);
        assert_eq!(jmap_sort_order_from_attrs(&a), 7);
        // sort_order=0 => key omitted, but reader default returns 0.
        let a0 = jmap_attrs_for_create(None, 0, false);
        assert_eq!(a0.special_use, None);
        assert!(!a0.subscribed);
        assert_eq!(a0.extra.as_object().map(|m| m.len()), Some(0));
        assert_eq!(jmap_sort_order_from_attrs(&a0), 0);
        // Boundary: u32::MAX round-trips losslessly.
        let a_max = jmap_attrs_for_create(None, u32::MAX, false);
        assert_eq!(jmap_sort_order_from_attrs(&a_max), u32::MAX);
    }

    #[test]
    fn role_jmap_str_round_trip() {
        for r in [
            MailboxRole::Inbox,
            MailboxRole::Archive,
            MailboxRole::Drafts,
            MailboxRole::Flagged,
            MailboxRole::Important,
            MailboxRole::Junk,
            MailboxRole::Sent,
            MailboxRole::Trash,
            MailboxRole::All,
        ] {
            assert_eq!(MailboxRole::from_jmap_str(r.as_jmap_str()), Some(r));
            // Special-use mapping symmetric.
            assert_eq!(MailboxRole::from_special_use(r.as_special_use()), Some(r));
        }
        assert_eq!(MailboxRole::from_jmap_str("unknown"), None);
        assert_eq!(MailboxRole::from_jmap_str("Inbox"), None); // case-sensitive
    }
}
