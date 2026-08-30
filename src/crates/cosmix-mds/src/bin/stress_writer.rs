//! Internal test helper used by `tests/recovery.rs`.
//!
//! Opens an MDS root, creates one set + INBOX, and hammers
//! `put_blob` + `add_item` in a tight loop until killed. The parent
//! test sends SIGKILL at a random delay and then verifies the store
//! reopens cleanly via `rebuild_index`.
//!
//! Not part of the operator CLI. The `[[bin]]` entry is gated on
//! the internal `_stress-helper` feature (off by default), so a
//! plain `cargo build` / `cargo install` never produces it. If you
//! find yourself reaching for this binary outside the recovery
//! test, you're holding the wrong tool.

use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args
        .next()
        .expect("usage: cosmix-mds-stress-writer <root> <set-uuid>")
        .into();
    let set_uuid: uuid::Uuid = args
        .next()
        .expect("usage: cosmix-mds-stress-writer <root> <set-uuid>")
        .parse()
        .expect("set-uuid: not a valid UUID");

    let mds = SqliteCasMds::open(&root).expect("open store");
    let set = SetId(set_uuid);
    mds.create_set(&set).expect("create_set");
    let inbox = mds
        .create_container(
            &set,
            None,
            "INBOX",
            ContainerAttrs {
                special_use: None,
                subscribed: true,
                extra: serde_json::json!({}),
            },
        )
        .expect("create_container");

    // Ready signal: the parent waits for this line on the child's
    // stdout before starting its kill timer. This avoids killing the
    // child during open()/create_set() which would test SQLite WAL
    // recovery on a brand-new database — boring, and not the bug
    // class the harness is actually after.
    println!("READY");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let mut counter: u64 = 0;
    loop {
        let payload = format!("crash-stress-{counter:016}-{}", "y".repeat(256));
        let blob = mds.put_blob(payload.as_bytes()).expect("put_blob");
        let memberships = vec![Membership {
            container: inbox,
            flags: Flags(0),
            added_at: counter as i64 + 1,
        }];
        // SIGKILL can land mid-add; that's the whole point. We don't
        // care if add_item returns — the parent will reopen the
        // store and ask whether it survived.
        let _ = mds.add_item(&set, &blob, &memberships);
        counter = counter.wrapping_add(1);
    }
}
