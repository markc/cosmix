//! Phase 1a: container-lifecycle integration tests.
//!
//! Every test runs against a real on-disk SqliteCasMds rooted in a
//! tempdir. WAL files come and go alongside the per-set data.sqlite.

use cosmix_mds::{ContainerAttrs, ContainerId, Mds, SetId, SqliteCasMds};
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let dir = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(dir.path()).unwrap();
    (dir, mds)
}

fn fresh_set() -> SetId {
    SetId(uuid::Uuid::now_v7())
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

#[test]
fn create_and_list_set() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let sets = mds.list_sets().unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0], s);
}

#[test]
fn create_set_twice_is_error() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let err = mds.create_set(&s).unwrap_err();
    assert!(format!("{err}").contains("already exists"), "{err}");
}

#[test]
fn delete_set_removes_dir_and_listing() {
    let (dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let path = dir.path().join("containers").join(s.0.to_string());
    assert!(path.exists());
    mds.delete_set(&s).unwrap();
    assert!(!path.exists());
    assert!(mds.list_sets().unwrap().is_empty());
}

#[test]
fn create_root_container() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let listing = mds.list_containers(&s).unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].id, c);
    assert_eq!(listing[0].name, "INBOX");
    assert!(listing[0].parent.is_none());
}

#[test]
fn create_nested_container_under_existing_parent() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let parent = mds.create_container(&s, None, "Folders", attrs()).unwrap();
    let child = mds
        .create_container(&s, Some(&parent), "Work", attrs())
        .unwrap();
    let listing = mds.list_containers(&s).unwrap();
    assert_eq!(listing.len(), 2);
    let work = listing.iter().find(|c| c.id == child).unwrap();
    assert_eq!(work.parent, Some(parent));
}

#[test]
fn create_under_missing_parent_is_not_found() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let bogus = ContainerId(uuid::Uuid::now_v7());
    let err = mds
        .create_container(&s, Some(&bogus), "x", attrs())
        .unwrap_err();
    assert!(format!("{err}").contains("not found"), "{err}");
}

#[test]
fn duplicate_name_under_same_parent_is_error() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let err = mds
        .create_container(&s, None, "INBOX", attrs())
        .unwrap_err();
    assert!(format!("{err}").contains("already exists"), "{err}");
}

#[test]
fn rename_container_changes_name_and_parent() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let folders = mds.create_container(&s, None, "Folders", attrs()).unwrap();
    mds.rename_container(&s, &inbox, Some(&folders), "Archive")
        .unwrap();
    let listing = mds.list_containers(&s).unwrap();
    let renamed = listing.iter().find(|c| c.id == inbox).unwrap();
    assert_eq!(renamed.name, "Archive");
    assert_eq!(renamed.parent, Some(folders));
}

#[test]
fn rename_into_self_is_error() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let err = mds.rename_container(&s, &c, Some(&c), "x").unwrap_err();
    assert!(
        format!("{err}").contains("own parent") || format!("{err}").contains("cycle"),
        "{err}"
    );
}

#[test]
fn rename_forming_cycle_is_error() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    // a -> b -> c. Try to make a's parent c, which would form a cycle.
    let a = mds.create_container(&s, None, "a", attrs()).unwrap();
    let b = mds.create_container(&s, Some(&a), "b", attrs()).unwrap();
    let c = mds.create_container(&s, Some(&b), "c", attrs()).unwrap();
    let err = mds.rename_container(&s, &a, Some(&c), "a").unwrap_err();
    assert!(format!("{err}").contains("cycle"), "{err}");
}

#[test]
fn delete_empty_container_works() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    mds.delete_container(&s, &c).unwrap();
    assert!(mds.list_containers(&s).unwrap().is_empty());
}

#[test]
fn delete_container_with_children_is_restricted() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let parent = mds.create_container(&s, None, "p", attrs()).unwrap();
    let _child = mds
        .create_container(&s, Some(&parent), "c", attrs())
        .unwrap();
    let err = mds.delete_container(&s, &parent).unwrap_err();
    assert!(format!("{err}").contains("child"), "{err}");
}

#[test]
fn container_status_of_fresh_container_is_zeroed() {
    let (_dir, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let st = mds.container_status(&s, &c).unwrap();
    assert_eq!(st.exists, 0);
    assert_eq!(st.unread, 0);
    assert_eq!(st.next_seq.0, 1);
    assert_eq!(st.change_seq.0, 1);
    assert!(st.seq_validity > 0);
}

#[test]
fn reopening_root_rediscovers_sets_and_containers() {
    let dir = TempDir::new().unwrap();
    let s = fresh_set();
    {
        let mds = SqliteCasMds::open(dir.path()).unwrap();
        mds.create_set(&s).unwrap();
        mds.create_container(&s, None, "Persistent", attrs())
            .unwrap();
    }
    let mds = SqliteCasMds::open(dir.path()).unwrap();
    let sets = mds.list_sets().unwrap();
    assert_eq!(sets.len(), 1);
    let listing = mds.list_containers(&sets[0]).unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "Persistent");
}

#[test]
fn operations_on_unknown_set_error_cleanly() {
    let (_dir, mds) = open();
    let s = fresh_set();
    let err = mds.list_containers(&s).unwrap_err();
    assert!(format!("{err}").contains("set not found"), "{err}");
}
