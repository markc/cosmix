//! MCS-P1-C — lifecycle paths emit `container_change_set` rows.
//!
//! Spec: `_doc/planned/mailbox-changes-substrate.md`.
//!
//! Each test runs through the typed `SqliteSetTx` accessors (the
//! mailstore-discipline entry points) and reads back the
//! container_change_set table via a read-only `with_set_tx` probe.
//! The fields we pin per kind match the documented payload shapes
//! in `schema/v1_4.sql` — drift here surfaces against the schema
//! comment, not in a downstream JMAP consumer six months later.

use cosmix_mds::{ContainerAttrs, ContainerId, Error, Mds, SetId, SqliteCasMds};
use serde_json::Value;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
}

fn fresh_set(mds: &SqliteCasMds) -> SetId {
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    s
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::Value::Null,
    }
}

/// Read all rows from container_change_set for the given set, in
/// insertion order. Each row is returned as `(seq, container_id,
/// kind, payload_json)`. The read is performed inside a
/// `with_set_tx` body that returns Err — so the read tx rolls back
/// and we observe rows that earlier writers actually committed.
fn dump_change_set(mds: &SqliteCasMds, set: &SetId) -> Vec<(i64, String, String, Value)> {
    let mut out: Vec<(i64, String, String, Value)> = Vec::new();
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |h| {
        let mut stmt = h
            .tx()
            .prepare(
                "SELECT container_change_set_seq, container_id, kind, payload \
                 FROM container_change_set \
                 ORDER BY container_change_set_seq ASC",
            )
            .unwrap();
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (seq, cid, kind, payload) in rows {
            let v: Value = serde_json::from_str(&payload).unwrap();
            out.push((seq, cid, kind, v));
        }
        Err(Error::Other("probe rollback".into()))
    });
    out
}

#[test]
fn create_emits_container_created_row_with_full_path() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let (parent, child) = mds
        .with_set_tx(&set, |h| {
            let p = h.create_container_named(None, "Folders", &attrs())?;
            let c = h.create_container_named(Some(&p), "Work", &attrs())?;
            Ok((p, c))
        })
        .unwrap();

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2, "two creates → two CONTAINER_CREATED rows");

    let (_, cid0, kind0, p0) = &rows[0];
    assert_eq!(cid0, &parent.0.to_string());
    assert_eq!(kind0, "CONTAINER_CREATED");
    assert_eq!(p0["name"], "Folders");
    assert!(p0["parent_id"].is_null(), "root has no parent_id");
    assert_eq!(p0["full_path"], "Folders");

    let (_, cid1, kind1, p1) = &rows[1];
    assert_eq!(cid1, &child.0.to_string());
    assert_eq!(kind1, "CONTAINER_CREATED");
    assert_eq!(p1["name"], "Work");
    assert_eq!(p1["parent_id"], parent.0.to_string());
    assert_eq!(p1["full_path"], "Folders/Work");
}

#[test]
fn rename_only_name_emits_changed_props_name() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Old", &attrs()))
        .unwrap();

    mds.with_set_tx(&set, |h| h.rename_container(&cid, None, "New"))
        .unwrap();

    let rows = dump_change_set(&mds, &set);
    // CREATED then RENAMED.
    assert_eq!(rows.len(), 2);
    let (_, _, kind, payload) = &rows[1];
    assert_eq!(kind, "CONTAINER_RENAMED");
    assert_eq!(payload["old_name"], "Old");
    assert_eq!(payload["new_name"], "New");
    assert!(payload["old_parent_id"].is_null());
    assert!(payload["new_parent_id"].is_null());
    assert_eq!(payload["old_full_path"], "Old");
    assert_eq!(payload["new_full_path"], "New");
    let props: Vec<&str> = payload["changed_props"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(props, vec!["name"]);
}

#[test]
fn rename_only_parent_emits_changed_props_parent_id() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let (parent_a, parent_b, child) = mds
        .with_set_tx(&set, |h| {
            let a = h.create_container_named(None, "A", &attrs())?;
            let b = h.create_container_named(None, "B", &attrs())?;
            let c = h.create_container_named(Some(&a), "Child", &attrs())?;
            Ok((a, b, c))
        })
        .unwrap();

    mds.with_set_tx(&set, |h| {
        h.rename_container(&child, Some(&parent_b), "Child")
    })
    .unwrap();

    let rows = dump_change_set(&mds, &set);
    // 3 CREATED + 1 RENAMED.
    assert_eq!(rows.len(), 4);
    let (_, cid, kind, payload) = &rows[3];
    assert_eq!(cid, &child.0.to_string());
    assert_eq!(kind, "CONTAINER_RENAMED");
    assert_eq!(payload["old_parent_id"], parent_a.0.to_string());
    assert_eq!(payload["new_parent_id"], parent_b.0.to_string());
    assert_eq!(payload["old_full_path"], "A/Child");
    assert_eq!(payload["new_full_path"], "B/Child");
    let props: Vec<&str> = payload["changed_props"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(props, vec!["parentId"]);
}

#[test]
fn rename_both_emits_changed_props_name_and_parent_id() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let (parent_a, parent_b, child) = mds
        .with_set_tx(&set, |h| {
            let a = h.create_container_named(None, "A", &attrs())?;
            let b = h.create_container_named(None, "B", &attrs())?;
            let c = h.create_container_named(Some(&a), "OldChild", &attrs())?;
            Ok((a, b, c))
        })
        .unwrap();

    mds.with_set_tx(&set, |h| {
        h.rename_container(&child, Some(&parent_b), "NewChild")
    })
    .unwrap();

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 4);
    let (_, _, _, payload) = &rows[3];
    let props: Vec<&str> = payload["changed_props"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(props, vec!["name", "parentId"]);
    assert_eq!(payload["old_full_path"], "A/OldChild");
    assert_eq!(payload["new_full_path"], "B/NewChild");
    // Confirm we did not write a row in the wrong order.
    let _ = parent_a;
    let _ = parent_b;
}

#[test]
fn noop_rename_does_not_emit_a_lifecycle_row() {
    // Caller passes the same name + same parent. The substrate
    // must not emit a lifecycle row; downstream
    // JMAP `Mailbox/changes` consumers would see a phantom event
    // otherwise.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Same", &attrs()))
        .unwrap();

    mds.with_set_tx(&set, |h| h.rename_container(&cid, None, "Same"))
        .unwrap();

    let rows = dump_change_set(&mds, &set);
    // Exactly one row: the CREATED row from the initial create.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "CONTAINER_CREATED");
}

#[test]
fn destroy_emits_container_destroyed_with_pre_delete_snapshot() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let (parent, child) = mds
        .with_set_tx(&set, |h| {
            let p = h.create_container_named(None, "Folders", &attrs())?;
            let c = h.create_container_named(Some(&p), "Work", &attrs())?;
            Ok((p, c))
        })
        .unwrap();

    mds.with_set_tx(&set, |h| h.delete_container(&child))
        .unwrap();

    let rows = dump_change_set(&mds, &set);
    // 2 CREATED + 1 DESTROYED.
    assert_eq!(rows.len(), 3);
    let (_, cid, kind, payload) = &rows[2];
    assert_eq!(cid, &child.0.to_string());
    assert_eq!(kind, "CONTAINER_DESTROYED");
    assert_eq!(payload["name"], "Work");
    // full_path snapshot is the path the container had *before*
    // the DELETE — ancestor renames after the fact cannot mutate
    // the snapshot.
    assert_eq!(payload["full_path"], "Folders/Work");
    let _ = parent;
}

#[test]
fn rollback_drops_create_lifecycle_row() {
    // `allocate_container_change_set` inherits the outer tx, so a
    // failed `with_set_tx` body must leave no row behind.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let res: cosmix_mds::Result<ContainerId> = mds.with_set_tx(&set, |h| {
        let c = h.create_container_named(None, "RolledBack", &attrs())?;
        Err::<ContainerId, _>(Error::Other("force rollback".into())).map(|_: ContainerId| c)
    });
    assert!(res.is_err());

    let rows = dump_change_set(&mds, &set);
    assert!(
        rows.is_empty(),
        "rollback should leave no lifecycle rows, got {rows:?}"
    );
}

#[test]
fn rollback_drops_rename_lifecycle_row() {
    // Commit a CREATE first (so a baseline row exists), then run a
    // RENAME inside a closure that returns Err. The CREATE row must
    // remain; the RENAME row must NOT appear.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Orig", &attrs()))
        .unwrap();
    let baseline = dump_change_set(&mds, &set);
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].2, "CONTAINER_CREATED");

    let res: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.rename_container(&cid, None, "Renamed")?;
        Err(Error::Other("force rollback".into()))
    });
    assert!(res.is_err());

    let rows = dump_change_set(&mds, &set);
    assert_eq!(
        rows.len(),
        1,
        "rolled-back rename must not leave a RENAMED row, got {rows:?}"
    );
    assert_eq!(rows[0].2, "CONTAINER_CREATED");
}

#[test]
fn rollback_drops_destroy_lifecycle_row() {
    // CREATE then DESTROY inside a rolled-back tx. The CREATE row
    // (committed in its own tx) remains; the DESTROY row (in the
    // rolled-back tx) must NOT appear. The container row itself
    // also rolls back, so a follow-up DESTROY against the same id
    // must still succeed.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Doomed", &attrs()))
        .unwrap();

    let res: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.delete_container(&cid)?;
        Err(Error::Other("force rollback".into()))
    });
    assert!(res.is_err());

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 1, "rolled-back delete leaves no DESTROYED row");
    assert_eq!(rows[0].2, "CONTAINER_CREATED");

    // Confirm the container still exists (rollback restored it).
    mds.with_set_tx(&set, |h| h.delete_container(&cid)).unwrap();
    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].2, "CONTAINER_DESTROYED");
}

/// Build a `ContainerAttrs` patched against the default returned by
/// `attrs()`. `extra` is rendered as a JSON object so the
/// `"jmap_sort_order"` key has a stable container regardless of
/// whether the caller asked for a non-zero sortOrder.
fn patched(special_use: Option<&str>, sort_order: u32, subscribed: bool) -> ContainerAttrs {
    let mut extra = serde_json::Map::new();
    if sort_order != 0 {
        extra.insert("jmap_sort_order".into(), sort_order.into());
    }
    ContainerAttrs {
        special_use: special_use.map(str::to_string),
        subscribed,
        extra: serde_json::Value::Object(extra),
    }
}

#[test]
fn update_attrs_role_only_emits_changed_props_role() {
    // Patch only the `special_use` (JMAP `role`) field. The
    // substrate-level diff must emit `changed_props == ["role"]`
    // and place exactly that key in `old_values` / `new_values` —
    // never a phantom `sortOrder` / `isSubscribed` with identical
    // before/after.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    let new_attrs = patched(Some("\\Inbox"), 0, false);
    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert_eq!(res.changed_props, vec!["role"]);

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2, "create + attrs_changed only");
    assert_eq!(rows[1].2, "CONTAINER_ATTRS_CHANGED");
    let cp = rows[1]
        .3
        .get("changed_props")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(cp.len(), 1);
    assert_eq!(cp[0], "role");
    let ov = rows[1]
        .3
        .get("old_values")
        .and_then(|v| v.as_object())
        .unwrap();
    let nv = rows[1]
        .3
        .get("new_values")
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(ov.len(), 1);
    assert_eq!(nv.len(), 1);
    assert!(ov.get("role").unwrap().is_null());
    assert_eq!(nv.get("role").and_then(|v| v.as_str()), Some("\\Inbox"));
}

#[test]
fn update_attrs_sort_order_only_emits_changed_props_sort_order() {
    // sortOrder lives in `attrs.extra["jmap_sort_order"]`; absent
    // and `0` collapse to the same value (matches
    // `jmap_sort_order_from_attrs` in maild) so a write that flips
    // 0 → 0 must NOT emit a lifecycle row.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    let new_attrs = patched(None, 7, false);
    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert_eq!(res.changed_props, vec!["sortOrder"]);

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].2, "CONTAINER_ATTRS_CHANGED");
    let cp = rows[1]
        .3
        .get("changed_props")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(cp[0], "sortOrder");
    let ov = rows[1]
        .3
        .get("old_values")
        .and_then(|v| v.as_object())
        .unwrap();
    let nv = rows[1]
        .3
        .get("new_values")
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(ov.get("sortOrder").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(nv.get("sortOrder").and_then(|v| v.as_u64()), Some(7));
}

#[test]
fn update_attrs_is_subscribed_only_emits_changed_props_is_subscribed() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    let new_attrs = patched(None, 0, true);
    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert_eq!(res.changed_props, vec!["isSubscribed"]);

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].2, "CONTAINER_ATTRS_CHANGED");
    let cp = rows[1]
        .3
        .get("changed_props")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(cp[0], "isSubscribed");
    let ov = rows[1]
        .3
        .get("old_values")
        .and_then(|v| v.as_object())
        .unwrap();
    let nv = rows[1]
        .3
        .get("new_values")
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(
        ov.get("isSubscribed").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(nv.get("isSubscribed").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn update_attrs_multi_property_emits_combined_changed_props() {
    // All three JMAP-visible props flipped in one call. `old_values`
    // / `new_values` must list exactly the three keys in
    // `changed_props`, no extras.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    let new_attrs = patched(Some("\\Sent"), 42, true);
    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert_eq!(res.changed_props, vec!["role", "sortOrder", "isSubscribed"]);

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].2, "CONTAINER_ATTRS_CHANGED");
    let cp = rows[1]
        .3
        .get("changed_props")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(cp.len(), 3);
    let ov = rows[1]
        .3
        .get("old_values")
        .and_then(|v| v.as_object())
        .unwrap();
    let nv = rows[1]
        .3
        .get("new_values")
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(ov.len(), 3);
    assert_eq!(nv.len(), 3);
    assert!(ov.get("role").unwrap().is_null());
    assert_eq!(nv.get("role").and_then(|v| v.as_str()), Some("\\Sent"));
    assert_eq!(ov.get("sortOrder").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(nv.get("sortOrder").and_then(|v| v.as_u64()), Some(42));
    assert_eq!(
        ov.get("isSubscribed").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(nv.get("isSubscribed").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn update_attrs_noop_emits_no_row_and_returns_empty() {
    // Caller resends byte-identical attrs (the legacy code path
    // unconditionally UPDATE'd; the substrate now suppresses).
    // Empty `changed_props` is the documented no-op signal; the
    // lifecycle stream must contain only the CONTAINER_CREATED row.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let initial = patched(Some("\\Archive"), 5, true);
    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &initial))
        .unwrap();

    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &initial))
        .unwrap();
    assert!(res.changed_props.is_empty());

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 1, "only the create row, no attrs_changed");
    assert_eq!(rows[0].2, "CONTAINER_CREATED");
}

#[test]
fn update_attrs_noop_zero_sort_order_with_absent_key_is_suppressed() {
    // The `extra` key `"jmap_sort_order"` *absent* and *= 0* must
    // collapse to the same diff value — both decode to `0` via
    // `jmap_sort_order_from_extra`. A patch that writes "0" into a
    // previously-absent map (or removes the key from a 0-valued
    // map) must NOT emit a phantom `sortOrder` lifecycle row.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    // Initial: no sort_order key in extra (absent ≡ 0).
    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    // Patch: explicit `jmap_sort_order: 0`. Should still decode to
    // 0 and produce no diff.
    let mut extra = serde_json::Map::new();
    extra.insert("jmap_sort_order".into(), 0u32.into());
    let new_attrs = ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::Value::Object(extra),
    };

    let res = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert!(
        res.changed_props.is_empty(),
        "0 vs absent ≡ 0 must collapse"
    );

    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 1);
}

#[test]
fn update_attrs_missing_container_is_not_found_and_writes_no_row() {
    // The pre-UPDATE SELECT must surface ContainerNotFound for a
    // bogus id, and the closure's Err propagates up so the tx
    // rolls back — no lifecycle row, even if a sibling write had
    // staged one earlier in the same tx.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let bogus = ContainerId(uuid::Uuid::now_v7());

    let new_attrs = patched(Some("\\Inbox"), 0, false);
    let res: cosmix_mds::Result<_> =
        mds.with_set_tx(&set, |h| h.update_container_attrs(&bogus, &new_attrs));
    assert!(matches!(res, Err(Error::ContainerNotFound(_))), "{res:?}");

    let rows = dump_change_set(&mds, &set);
    assert!(rows.is_empty());
}

#[test]
fn update_attrs_rollback_drops_lifecycle_row() {
    // A `with_set_tx` body that returns Err after a successful
    // `update_container_attrs` call must roll back the lifecycle
    // row alongside the container UPDATE — neither the row nor the
    // mutated attrs survive.
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let cid = mds
        .with_set_tx(&set, |h| h.create_container_named(None, "Folder", &attrs()))
        .unwrap();

    let new_attrs = patched(Some("\\Inbox"), 9, true);
    let res: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.update_container_attrs(&cid, &new_attrs)?;
        Err(Error::Other("rollback probe".into()))
    });
    assert!(matches!(res, Err(Error::Other(_))));

    // Only the CONTAINER_CREATED row survives — no
    // CONTAINER_ATTRS_CHANGED.
    let rows = dump_change_set(&mds, &set);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "CONTAINER_CREATED");

    // And the container row itself is unchanged: a fresh
    // `update_container_attrs(&same_patch)` against the original
    // (default) attrs should see all three props as changing.
    let res2 = mds
        .with_set_tx(&set, |h| h.update_container_attrs(&cid, &new_attrs))
        .unwrap();
    assert_eq!(
        res2.changed_props,
        vec!["role", "sortOrder", "isSubscribed"]
    );
}

#[test]
fn delete_missing_container_is_not_found_and_writes_no_row() {
    // `delete_container` against a bogus id must surface
    // ContainerNotFound (from the pre-delete SELECT) and write
    // nothing to container_change_set — the closure's Err
    // propagates up and rolls the tx back.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let bogus = ContainerId(uuid::Uuid::now_v7());

    let res: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| h.delete_container(&bogus));
    assert!(matches!(res, Err(Error::ContainerNotFound(_))), "{res:?}");

    let rows = dump_change_set(&mds, &set);
    assert!(rows.is_empty());
}
