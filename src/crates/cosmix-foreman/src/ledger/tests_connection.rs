#[test]
fn every_production_reopen_refuses_a_repointed_same_project_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("ledger.db");
    let replacement_dir = temp.path().join("replacement");
    std::fs::create_dir(&replacement_dir).unwrap();
    let replacement = replacement_dir.join("ledger.db");

    let _primary = Ledger::open_with_create_for_project(
        &db,
        LedgerCreate::ParentsAndFile,
        Some(("same-project", "same-repository")),
    )
    .unwrap();
    // Prove one normal reopen does not turn the authority into a cached
    // one-shot check. This is the same public entry point used by existing
    // production callers, not a direct exercise of the authority helper.
    let first_lane = Ledger::open(&db).unwrap();
    let journal_mode: String = first_lane
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    drop(first_lane);

    // The replacement is a fully valid ledger with the exact same manifest
    // identity. Project binding therefore cannot distinguish it.
    drop(
        Ledger::open_with_create_for_project(
            &replacement,
            LedgerCreate::ParentsAndFile,
            Some(("same-project", "same-repository")),
        )
        .unwrap(),
    );
    std::fs::rename(&db, temp.path().join("primary-held.db")).unwrap();
    std::fs::rename(&replacement, &db).unwrap();

    let error = match Ledger::open(&db) {
        Ok(_) => panic!("a later reopen accepted a different same-project ledger object"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("refusing ledger reopen")
            && message.contains("device")
            && message.contains("inode"),
        "reopen must hard-refuse on the complete object identity: {message}"
    );
}

#[test]
fn reopen_checks_sqlites_fd_and_refuses_an_aba_path_rebind() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("ledger.db");
    let replacement = temp.path().join("replacement.db");
    let held_primary = temp.path().join("primary-held.db");

    let primary = Ledger::open_with_create_for_project(
        &db,
        LedgerCreate::ParentsAndFile,
        Some(("same-project", "same-repository")),
    )
    .unwrap();
    let reopen = primary.open_options();
    drop(
        Ledger::open_with_create_for_project(
            &replacement,
            LedgerCreate::ParentsAndFile,
            Some(("same-project", "same-repository")),
        )
        .unwrap(),
    );

    let error = match reopen.open_with(|path| {
        // A -> B while SQLite opens, then B -> A before the identity check.
        // Path metadata alone sees A on both sides; only fstat of SQLite's
        // actual main-database descriptor can prove it remained bound to B.
        std::fs::rename(path, &held_primary)?;
        std::fs::rename(&replacement, path)?;
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        std::fs::rename(path, &replacement)?;
        std::fs::rename(&held_primary, path)?;
        Ok(conn)
    }) {
        Ok(_) => panic!("ABA reopen accepted SQLite's replacement-ledger descriptor"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("refusing ledger reopen")
            && message.contains("SQLite opened device")
            && message.contains("inode"),
        "ABA reopen must report SQLite's actual object mismatch: {message}"
    );
}
