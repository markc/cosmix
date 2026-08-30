//! End-to-end Phase 4 IMAP tests — STORE / UID STORE flag mutation.
//!
//! Mirrors the `imap_phase3.rs` harness: in-process duplex socket, no
//! TLS, fresh sqlite Db + MailStore + Mds per test. Each test inserts
//! known messages into INBOX, drives STORE / UID STORE, and asserts:
//!
//! 1. Untagged `* N FETCH (FLAGS (...) [UID n])` echo per touched
//!    message (unless `.SILENT`).
//! 2. Tagged `OK STORE completed` / `OK UID STORE completed`.
//! 3. The substrate observes the new flag set on subsequent FETCH.

use std::sync::Arc;

use anyhow::Result;
use cosmix_maild::db::Db;
use cosmix_maild::imap::config::ImapConfig;
use cosmix_maild::imap::session::{AccountSlots, handle_connection};
use cosmix_maild::mailstore::{EmailEnvelope, MailStore, MailboxRole, SqliteMailStore};
use cosmix_mds::types::ContainerAttrs;
use cosmix_mds::{Flags, Mds, SqliteCasMds, Tags};
use rusqlite::params;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn attrs_for(role: Option<&str>, subscribed: bool) -> ContainerAttrs {
    ContainerAttrs {
        special_use: role.map(|s| s.to_string()),
        subscribed,
        extra: serde_json::Value::Null,
    }
}

async fn make_fixture(
    email: &str,
    password: &str,
) -> Result<(TempDir, Db, Arc<SqliteCasMds>, Arc<dyn MailStore>)> {
    let tmp = tempfile::tempdir()?;
    let dbp = tmp.path().join("mail.db").to_string_lossy().into_owned();
    let blob = tmp.path().join("blobs").to_string_lossy().into_owned();
    let db = Db::connect(&dbp, &blob).await?;
    db.migrate().await?;
    let hash = bcrypt::hash(password, 4)?;
    {
        let conn = db.conn.lock().expect("db lock");
        conn.execute(
            "INSERT INTO accounts (email, password, name) VALUES (?1, ?2, ?3)",
            params![email, hash, "Test"],
        )?;
    }
    let mds_dir = tmp.path().join("mds");
    let mds = Arc::new(SqliteCasMds::open(mds_dir)?);
    let mailstore: Arc<dyn MailStore> = Arc::new(SqliteMailStore::new(Arc::clone(&mds)));
    mailstore.create_mailbox(1, "INBOX", None, attrs_for(Some("\\Inbox"), true))?;
    mailstore.create_mailbox(1, "Archive", None, attrs_for(Some("\\Archive"), true))?;
    Ok((tmp, db, mds, mailstore))
}

async fn spawn_session(
    db: Db,
    mailstore: Arc<dyn MailStore>,
    cfg: ImapConfig,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let slots = AccountSlots::new();
    let cfg = Arc::new(cfg);
    let peer = "127.0.0.1:0".parse().unwrap();
    let join = tokio::spawn(async move {
        handle_connection(
            server,
            peer,
            None,
            cfg,
            db,
            mailstore,
            slots,
            "localhost".to_string(),
        )
        .await
    });
    (client, join)
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<String> {
    let mut s = String::new();
    let n = reader.read_line(&mut s).await?;
    if n == 0 {
        return Err(anyhow::anyhow!("peer closed"));
    }
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

async fn login(
    client: tokio::io::DuplexStream,
) -> Result<(
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
)> {
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK"), "LOGIN failed: {resp}");
    Ok((reader, w))
}

async fn read_until_tagged<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    tag: &str,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    loop {
        let line = read_line(reader).await?;
        let is_tagged = line.starts_with(&format!("{tag} "));
        out.push(line);
        if is_tagged {
            return Ok(out);
        }
    }
}

fn insert_email(
    ms: &Arc<dyn MailStore>,
    mds: &Arc<SqliteCasMds>,
    body: &[u8],
    subject: &str,
    flags: Flags,
    received_at_ms: i64,
) -> Result<u32> {
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let hash = mds.put_blob(body)?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: subject.into(),
        date: received_at_ms / 1000,
        message_id: Some(format!("<{subject}@example.com>")),
    };
    let (_item, ins) = ms.create_email(
        1,
        inbox,
        hash,
        envelope,
        &[],
        flags,
        Tags::new(),
        received_at_ms,
    )?;
    Ok(ins.uid as u32)
}

#[tokio::test]
async fn store_not_authenticated_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    assert!(resp.to_uppercase().contains("AUTHENTICATED"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn store_in_authenticated_state_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // No SELECT — STORE must report "no mailbox selected".
    w.write_all(b"a02 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(
        resp.to_lowercase().contains("no mailbox selected"),
        "{resp}"
    );
    Ok(())
}

#[tokio::test]
async fn store_on_examine_selected_returns_no_cannot() -> Result<()> {
    // RFC 9051 §6.3.2 — EXAMINE selects the mailbox read-only. STORE
    // must refuse to mutate; the substrate must not see a write.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a03 NO [CANNOT]"),
        "expected NO [CANNOT], got: {}",
        lines[0]
    );

    // Substrate state must be unchanged.
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let emails = ms.list_emails_in_mailbox(
        1,
        inbox,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(emails[0].keywords, Flags(0), "EXAMINE+STORE leaked a write");
    Ok(())
}

#[tokio::test]
async fn store_add_seen_emits_fetch_echo_and_persists() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _uid1 = insert_email(&ms, &mds, b"body-1", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen))");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);

    // FETCH FLAGS confirms the new state survives the call.
    w.write_all(b"a04 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    assert!(
        lines.iter().any(|l| l.contains("FLAGS (\\Seen)")),
        "{lines:?}"
    );
    Ok(())
}

#[tokio::test]
async fn store_user_keyword_round_trips_normalised() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _uid1 = insert_email(&ms, &mds, b"body-1", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // STORE a system flag + a mixed-case user keyword in one op. The keyword
    // is normalised to NFC + lowercase ("Important" -> "important") and
    // rendered BARE (no leading `\`) after the system flags in the FETCH echo.
    w.write_all(b"a03 STORE 1 +FLAGS (\\Seen Important)\r\n")
        .await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(
        lines[0], "* 1 FETCH (FLAGS (\\Seen important))",
        "{lines:?}"
    );
    assert!(lines.last().unwrap().starts_with("a03 OK"), "{lines:?}");

    // SEARCH uses the same normalisation and per-membership tag storage as
    // STORE, so the mixed-case input above is immediately queryable.
    w.write_all(b"a04 SEARCH KEYWORD IMPORTANT\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    assert_eq!(lines[0], "* SEARCH 1", "{lines:?}");
    assert!(lines.last().unwrap().starts_with("a04 OK"), "{lines:?}");

    // It survives as a real FETCH FLAGS.
    w.write_all(b"a05 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a05").await?;
    assert!(
        lines.iter().any(|l| l.contains("FLAGS (\\Seen important)")),
        "{lines:?}"
    );

    // -FLAGS removes the keyword case-insensitively, leaving the system flag.
    w.write_all(b"a06 STORE 1 -FLAGS (IMPORTANT)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a06").await?;
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen))", "{lines:?}");

    // Regression: a registry `$`-keyword (Thunderbird's built-in tags are
    // `$label1`..`$label5`; "Work" = `$label2`) MUST be accepted, not rejected.
    // Rejecting these (the original `$`-reserved over-strictness) broke real
    // IMAP clients and violated the `PERMANENTFLAGS \*` promise.
    w.write_all(b"a07 STORE 1 +FLAGS ($label2)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a07").await?;
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen $label2))", "{lines:?}");
    assert!(lines.last().unwrap().starts_with("a07 OK"), "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn store_replace_clears_previous_system_flags() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(
        &ms,
        &mds,
        b"x",
        "msg1",
        Flags(Flags::SEEN | Flags::FLAGGED),
        1_700_000_000_000,
    )?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // Replace with `\Answered` only — both prior bits must be cleared.
    w.write_all(b"a03 STORE 1 FLAGS (\\Answered)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Answered))");
    Ok(())
}

#[tokio::test]
async fn store_minus_flags_clears_named_bits_only() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(
        &ms,
        &mds,
        b"x",
        "msg1",
        Flags(Flags::SEEN | Flags::FLAGGED | Flags::DRAFT),
        1_700_000_000_000,
    )?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 -FLAGS (\\Flagged)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    // \Answered \Flagged \Deleted \Seen \Draft order — \Seen + \Draft remain.
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen \\Draft))");
    Ok(())
}

#[tokio::test]
async fn store_silent_suppresses_fetch_echo() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS.SILENT (\\Seen)\r\n")
        .await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    // Only the tagged OK — no untagged FETCH echo.
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 OK"), "{}", lines[0]);
    Ok(())
}

#[tokio::test]
async fn uid_store_echoes_uid_atom() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let uid1 = insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    let cmd = format!("a03 UID STORE {uid1} +FLAGS (\\Seen)\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    let expected = format!("* 1 FETCH (FLAGS (\\Seen) UID {uid1})");
    assert_eq!(lines[0], expected);
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn store_multi_message_range_touches_all() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"a", "m1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"b", "m2", Flags(0), 1_700_000_001_000)?;
    insert_email(&ms, &mds, b"c", "m3", Flags(0), 1_700_000_002_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1:3 +FLAGS (\\Seen)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    // 3 untagged FETCH echoes + 1 tagged OK
    assert_eq!(lines.len(), 4, "{lines:?}");
    for (i, line) in lines.iter().take(3).enumerate() {
        let msn = i + 1;
        let want = format!("* {msn} FETCH (FLAGS (\\Seen))");
        assert_eq!(line, &want, "row {i}");
    }
    assert!(lines[3].starts_with("a03 OK"), "{}", lines[3]);
    Ok(())
}

#[tokio::test]
async fn store_no_op_when_bits_already_set() -> Result<()> {
    // Idempotency: STORE +FLAGS (\Seen) on a message that already has
    // \Seen still emits the FETCH echo (RFC 9051 §6.4.6 — server SHOULD
    // emit FETCH on every successful STORE unless .SILENT) but does
    // not double-fire the notifier.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(
        &ms,
        &mds,
        b"x",
        "msg1",
        Flags(Flags::SEEN),
        1_700_000_000_000,
    )?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen))");
    Ok(())
}

#[tokio::test]
async fn store_out_of_range_msn_emits_no_rows_but_ok() -> Result<()> {
    // RFC 9051 §6.4.6 — a STORE referencing nonexistent message numbers
    // is not an error; the server emits no FETCH echoes and still
    // returns tagged OK.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1:5 +FLAGS (\\Seen)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 OK"), "{}", lines[0]);
    Ok(())
}

#[tokio::test]
async fn store_rejects_unknown_flag() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _ = insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS (\\Recent)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 BAD"), "{}", lines[0]);
    Ok(())
}

#[tokio::test]
async fn store_isolates_per_mailbox_membership() -> Result<()> {
    // Per-mailbox isolation invariant (`_doc/maild/imap.md` §5/§6):
    // \Seen STORE on INBOX must not propagate to the same email's
    // Archive membership.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _uid1 = insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;

    // Place a second membership of the same email in Archive.
    let archive = ms
        .mailbox_by_role(1, MailboxRole::Archive)?
        .expect("Archive missing");
    let inbox_emails = ms.list_emails_in_mailbox(
        1,
        ms.mailbox_by_role(1, MailboxRole::Inbox)?.unwrap(),
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    let item_id = inbox_emails[0].id;
    ms.add_memberships(1, item_id, &[archive])?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a03").await?;

    // Verify via the substrate: Archive membership still has Flags(0).
    let archive_emails = ms.list_emails_in_mailbox(
        1,
        archive,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(archive_emails.len(), 1, "{archive_emails:?}");
    assert_eq!(
        archive_emails[0].keywords,
        Flags(0),
        "STORE leaked across mailbox boundary"
    );

    // INBOX membership has \Seen.
    let inbox_emails = ms.list_emails_in_mailbox(
        1,
        ms.mailbox_by_role(1, MailboxRole::Inbox)?.unwrap(),
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(inbox_emails[0].keywords, Flags(Flags::SEEN));
    Ok(())
}

// ============================================================
// T14 — APPEND with LITERAL+ (RFC 9051 §6.3.12 / RFC 4315 / RFC 7888)
// ============================================================

/// A minimal RFC 5322 message body suitable for `mail_parser`.
const MSG: &[u8] =
    b"From: ext@example.com\r\nTo: alice@example.com\r\nSubject: hi\r\n\r\nhello world\r\n";

#[tokio::test]
async fn append_sync_literal_returns_appenduid() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    let cmd = format!("a02 APPEND INBOX {{{}}}\r\n", MSG.len());
    w.write_all(cmd.as_bytes()).await?;
    // Sync literal must elicit a continuation request.
    let cont = read_line(&mut reader).await?;
    assert!(cont.starts_with("+ "), "expected continuation, got {cont}");
    w.write_all(MSG).await?;
    w.write_all(b"\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(
        resp.starts_with("a02 OK [APPENDUID "),
        "expected APPENDUID, got {resp}"
    );
    assert!(resp.contains("] APPEND completed"), "{resp}");

    // Substrate observes the appended row in INBOX.
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let rows = ms.list_emails_in_mailbox(
        1,
        inbox,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    Ok(())
}

#[tokio::test]
async fn append_non_sync_literal_plus_no_continuation() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // LITERAL+ `{N+}` — body follows the CRLF immediately, no `+ Ready`
    // continuation should appear on the wire.
    let mut frame = format!("a02 APPEND INBOX {{{}+}}\r\n", MSG.len()).into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    // First line back must be the tagged OK, not a continuation.
    assert!(
        resp.starts_with("a02 OK [APPENDUID "),
        "expected tagged OK (not a continuation), got {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn append_with_flags_persists_them() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    let cmd = format!("a02 APPEND INBOX (\\Seen \\Draft) {{{}+}}\r\n", MSG.len());
    let mut frame = cmd.into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK [APPENDUID "), "{resp}");

    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let rows = ms.list_emails_in_mailbox(
        1,
        inbox,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].keywords, Flags(Flags::SEEN | Flags::DRAFT));
    Ok(())
}

#[tokio::test]
async fn append_with_internaldate_uses_supplied_timestamp() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // 2026-01-01 10:00:00 +0000 → 1767261600 secs.
    let cmd = format!(
        "a02 APPEND INBOX \"01-Jan-2026 10:00:00 +0000\" {{{}+}}\r\n",
        MSG.len()
    );
    let mut frame = cmd.into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK [APPENDUID "), "{resp}");

    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let rows = ms.list_emails_in_mailbox(
        1,
        inbox,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: 10,
            offset: 0,
        },
    )?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].received_at, 1_767_261_600_000);
    Ok(())
}

#[tokio::test]
async fn append_to_missing_mailbox_returns_trycreate() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    let cmd = format!("a02 APPEND \"No Such Mailbox\" {{{}+}}\r\n", MSG.len());
    let mut frame = cmd.into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(
        resp.starts_with("a02 NO [TRYCREATE]"),
        "expected TRYCREATE, got {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn append_not_authenticated_returns_bad_and_stays_in_sync() -> Result<()> {
    // Pre-auth APPEND must be refused at the codec layer — the
    // session passes `accept_literals = false` when the connection
    // is not authenticated, so the body never lands in RAM (sync
    // form receives no continuation; non-sync form is drained byte
    // for byte). This is a DoS mitigation: with `LITERAL+`
    // advertised in the greeting, an attacker would otherwise be
    // able to push 64 MiB into memory before any account-scoped
    // limit applies.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;

    // LITERAL+ form — the body is drained by the codec.
    let mut frame = format!("a02 APPEND INBOX {{{}+}}\r\n", MSG.len()).into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "expected BAD, got {resp}");
    assert!(
        resp.to_lowercase().contains("literal"),
        "expected literal-refusal message, got {resp}"
    );

    // Stream sync — a follow-up command must parse cleanly.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn pre_auth_sync_literal_refused_without_continuation() -> Result<()> {
    // Sync literal pre-auth: the `+ Ready` continuation must *not*
    // be written. The client is waiting for it, so no body bytes
    // arrive — stream stays in sync.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;

    w.write_all(format!("a02 APPEND INBOX {{{}}}\r\n", MSG.len()).as_bytes())
        .await?;
    let resp = read_line(&mut reader).await?;
    // Must be a tagged BAD, not a `+ Ready` continuation.
    assert!(!resp.starts_with("+ "), "unexpected continuation: {resp}");
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.to_lowercase().contains("literal"), "{resp}");

    // Stream sync — no body was ever requested.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn append_sync_literal_too_large_no_continuation() -> Result<()> {
    let cfg = ImapConfig {
        max_literal_bytes: 16, // tiny cap
        ..ImapConfig::default()
    };
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, cfg).await;
    let (mut reader, mut w) = login(client).await?;

    // Declare 64 bytes against a 16-byte cap, sync form.
    w.write_all(b"a02 APPEND INBOX {64}\r\n").await?;
    let resp = read_line(&mut reader).await?;
    // Must be a tag-bearing rejection — no `+ Ready` line.
    assert!(!resp.starts_with("+ "), "unexpected continuation: {resp}");
    assert!(
        resp.contains("[TOOBIG]"),
        "expected NO [TOOBIG], got {resp}"
    );

    // Stream sync — next command must work without any body upload.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn append_non_sync_too_large_drains_body_and_stays_in_sync() -> Result<()> {
    let cfg = ImapConfig {
        max_literal_bytes: 16,
        ..ImapConfig::default()
    };
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, cfg).await;
    let (mut reader, mut w) = login(client).await?;

    // LITERAL+ with declared size > cap. The codec must drain the
    // 64-byte body + trailing CRLF so the next command lands cleanly.
    let body: Vec<u8> = vec![b'x'; 64];
    let mut frame = b"a02 APPEND INBOX {64+}\r\n".to_vec();
    frame.extend_from_slice(&body);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(
        resp.contains("[TOOBIG]"),
        "expected NO [TOOBIG], got {resp}"
    );

    // Next command must parse.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn append_rejects_unknown_flag() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    let cmd = format!("a02 APPEND INBOX (\\Bogus) {{{}+}}\r\n", MSG.len());
    let mut frame = cmd.into_bytes();
    frame.extend_from_slice(MSG);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(
        resp.to_lowercase().contains("unknown system flag"),
        "{resp}"
    );

    // Stream sync — the literal body was already consumed.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn append_parse_failure_returns_no_parse() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // Empty body — `mail_parser` returns None on the zero-byte input.
    w.write_all(b"a02 APPEND INBOX {0+}\r\n\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(
        resp.starts_with("a02 NO [PARSE]"),
        "expected NO [PARSE], got {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn capability_advertises_literal_plus_uidplus_and_move() -> Result<()> {
    // T14 shipped `LITERAL+` end-to-end. T15-T17 (this phase) ships
    // COPY / MOVE / UID EXPUNGE — `UIDPLUS` (RFC 4315) and `MOVE`
    // (RFC 6851) are now advertised in both the greeting and the
    // explicit CAPABILITY response.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.contains("LITERAL+"), "{greeting}");
    assert!(greeting.contains("UIDPLUS"), "{greeting}");
    assert!(greeting.contains("MOVE"), "{greeting}");
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("LITERAL+"), "{untagged}");
    assert!(untagged.contains("UIDPLUS"), "{untagged}");
    assert!(untagged.contains("MOVE"), "{untagged}");
    Ok(())
}

#[tokio::test]
async fn literal_on_non_append_verb_rejected_and_stays_in_sync() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // CAPABILITY does not accept a literal argument. Use LITERAL+ so
    // the codec doesn't gate on a continuation — the dispatch layer
    // should still refuse, after the body has been consumed.
    let body = b"junk";
    let mut frame = format!("a02 CAPABILITY {{{}+}}\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    frame.extend_from_slice(b"\r\n");
    w.write_all(&frame).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");

    // Sync invariant — next command parses.
    w.write_all(b"a03 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY"), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    Ok(())
}
