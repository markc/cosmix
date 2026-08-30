//! End-to-end Phase 4 IMAP tests — COPY / MOVE / EXPUNGE (RFC 9051
//! §6.4.3 / §6.4.7, RFC 6851, RFC 4315 §2.1 + §3 COPYUID).
//!
//! Mirrors the `imap_phase4.rs` harness: in-process duplex socket, no
//! TLS, fresh sqlite Db + MailStore + Mds per test. Each test inserts
//! known messages into INBOX (occasionally Archive), drives
//! COPY/MOVE/EXPUNGE, and asserts the substrate side-effects line up
//! with the wire response.

use std::sync::Arc;

use anyhow::Result;
use cosmix_maild::db::Db;
use cosmix_maild::imap::config::ImapConfig;
use cosmix_maild::imap::session::{AccountSlots, handle_connection};
use cosmix_maild::mailstore::{
    EmailEnvelope, ListOpts, MailStore, MailboxRole, SortKey, SqliteMailStore,
};
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

fn archive_container(ms: &Arc<dyn MailStore>) -> cosmix_mds::ContainerId {
    ms.list_mailboxes(1)
        .expect("list_mailboxes")
        .into_iter()
        .find(|m| m.name == "Archive")
        .expect("Archive present")
        .id
}

fn list_archive(ms: &Arc<dyn MailStore>) -> Vec<cosmix_mds::ItemId> {
    let cid = archive_container(ms);
    ms.list_emails_in_mailbox(
        1,
        cid,
        ListOpts {
            sort_by: SortKey::Seq,
            limit: u32::MAX,
            offset: 0,
        },
    )
    .expect("list Archive")
    .into_iter()
    .map(|h| h.id)
    .collect()
}

fn list_inbox(ms: &Arc<dyn MailStore>) -> Vec<cosmix_mds::ItemId> {
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)
        .expect("inbox lookup")
        .expect("INBOX present");
    ms.list_emails_in_mailbox(
        1,
        inbox,
        ListOpts {
            sort_by: SortKey::Seq,
            limit: u32::MAX,
            offset: 0,
        },
    )
    .expect("list INBOX")
    .into_iter()
    .map(|h| h.id)
    .collect()
}

// ---------------------- COPY -----------------------

#[tokio::test]
async fn copy_emits_copyuid_and_leaves_source() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;
    insert_email(&ms, &mds, b"m3", "subj3", Flags(0), 1_700_000_002_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1:3 Archive\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(
        tagged.starts_with("a03 OK [COPYUID "),
        "expected COPYUID response code, got {tagged}"
    );
    assert!(tagged.contains("] COPY completed"), "{tagged}");

    // Source mailbox unchanged.
    assert_eq!(list_inbox(&ms).len(), 3);
    // Destination got all three items.
    assert_eq!(list_archive(&ms).len(), 3);
    Ok(())
}

#[tokio::test]
async fn uid_copy_emits_copyuid() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let u1 = insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;
    let _u2 = insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;
    let u3 = insert_email(&ms, &mds, b"m3", "subj3", Flags(0), 1_700_000_002_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    let cmd = format!("a03 UID COPY {u1},{u3} Archive\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK [COPYUID "), "{tagged}");
    assert!(tagged.contains("] COPY completed"), "{tagged}");

    assert_eq!(list_inbox(&ms).len(), 3);
    assert_eq!(list_archive(&ms).len(), 2);
    Ok(())
}

#[tokio::test]
async fn copy_empty_set_no_copyuid() -> Result<()> {
    // Sequence-set that resolves to zero rows (mailbox is empty).
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 UID COPY 99 Archive\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    assert!(
        !tagged.contains("[COPYUID"),
        "should not emit COPYUID: {tagged}"
    );
    assert_eq!(list_archive(&ms).len(), 0);
    Ok(())
}

#[tokio::test]
async fn copy_to_missing_mailbox_returns_trycreate() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1 NonExistent\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 NO [TRYCREATE]"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn copy_to_same_mailbox_rejected() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1 INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 NO"), "{tagged}");
    assert!(tagged.to_lowercase().contains("must differ"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn copy_in_examined_mailbox_allowed() -> Result<()> {
    // RFC 9051 §6.4.7 — COPY does not mutate the source, so reading
    // from a read-only EXAMINE'd mailbox is allowed.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1 Archive\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK [COPYUID "), "{tagged}");
    assert_eq!(list_archive(&ms).len(), 1);
    Ok(())
}

// ---------------------- EXPUNGE -----------------------

#[tokio::test]
async fn expunge_emits_descending_msns_and_only_for_deleted() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    // 3 messages: mark MSN 1 and MSN 3 as \Deleted, leave MSN 2 alone.
    insert_email(
        &ms,
        &mds,
        b"m1",
        "subj1",
        Flags(Flags::DELETED),
        1_700_000_000_000,
    )?;
    insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;
    insert_email(
        &ms,
        &mds,
        b"m3",
        "subj3",
        Flags(Flags::DELETED),
        1_700_000_002_000,
    )?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 EXPUNGE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let expunge_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("* ") && l.ends_with(" EXPUNGE"))
        .collect();
    assert_eq!(expunge_lines.len(), 2, "{lines:?}");
    assert_eq!(expunge_lines[0], "* 3 EXPUNGE", "must descend: {lines:?}");
    assert_eq!(expunge_lines[1], "* 1 EXPUNGE", "must descend: {lines:?}");
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    assert_eq!(list_inbox(&ms).len(), 1, "only MSN 2 survives");
    Ok(())
}

#[tokio::test]
async fn expunge_no_deleted_emits_only_tagged_ok() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 EXPUNGE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 OK"), "{lines:?}");
    assert_eq!(list_inbox(&ms).len(), 2);
    Ok(())
}

#[tokio::test]
async fn uid_expunge_filters_by_uid_set() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let u1 = insert_email(
        &ms,
        &mds,
        b"m1",
        "subj1",
        Flags(Flags::DELETED),
        1_700_000_000_000,
    )?;
    let _u2 = insert_email(
        &ms,
        &mds,
        b"m2",
        "subj2",
        Flags(Flags::DELETED),
        1_700_000_001_000,
    )?;
    let _u3 = insert_email(
        &ms,
        &mds,
        b"m3",
        "subj3",
        Flags(Flags::DELETED),
        1_700_000_002_000,
    )?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // Only u1 is in the supplied UID set; u2/u3 stay even though
    // they're also \Deleted (RFC 4315 §2.1).
    let cmd = format!("a03 UID EXPUNGE {u1}\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let expunge_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("* ") && l.ends_with(" EXPUNGE"))
        .collect();
    assert_eq!(expunge_lines.len(), 1, "{lines:?}");
    assert_eq!(expunge_lines[0], "* 1 EXPUNGE", "{lines:?}");
    assert_eq!(list_inbox(&ms).len(), 2);
    Ok(())
}

#[tokio::test]
async fn expunge_in_examined_mailbox_rejected() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(
        &ms,
        &mds,
        b"m1",
        "subj1",
        Flags(Flags::DELETED),
        1_700_000_000_000,
    )?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 EXPUNGE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(
        tagged.starts_with("a03 NO [CANNOT]") && tagged.to_lowercase().contains("read-only"),
        "{tagged}"
    );
    Ok(())
}

#[tokio::test]
async fn expunge_no_mailbox_selected_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXPUNGE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(
        resp.to_lowercase().contains("no mailbox selected"),
        "{resp}"
    );
    Ok(())
}

#[tokio::test]
async fn expunge_with_args_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 EXPUNGE 1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 BAD"), "{resp}");
    Ok(())
}

// ---------------------- MOVE -----------------------

#[tokio::test]
async fn move_emits_copyuid_then_expunge_descending() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;
    insert_email(&ms, &mds, b"m3", "subj3", Flags(0), 1_700_000_002_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 MOVE 1:3 Archive\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;

    // RFC 6851 §3.3: COPYUID first, then EXPUNGE rows in descending
    // MSN order, then tagged OK.
    let copyuid_idx = lines
        .iter()
        .position(|l| l.starts_with("* OK [COPYUID "))
        .expect("COPYUID present");
    let expunge_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("* ") && l.ends_with(" EXPUNGE"))
        .collect();
    assert_eq!(expunge_lines.len(), 3, "{lines:?}");
    assert_eq!(expunge_lines[0], "* 3 EXPUNGE", "{lines:?}");
    assert_eq!(expunge_lines[1], "* 2 EXPUNGE", "{lines:?}");
    assert_eq!(expunge_lines[2], "* 1 EXPUNGE", "{lines:?}");
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK"), "{tagged}");
    assert!(
        copyuid_idx
            < lines
                .iter()
                .position(|l| l.starts_with("* ") && l.ends_with(" EXPUNGE"))
                .expect("expunge"),
        "COPYUID must precede first EXPUNGE: {lines:?}"
    );

    assert_eq!(list_inbox(&ms).len(), 0, "INBOX must be empty after MOVE");
    assert_eq!(list_archive(&ms).len(), 3);
    Ok(())
}

#[tokio::test]
async fn uid_move_emits_copyuid() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let _u1 = insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;
    let u2 = insert_email(&ms, &mds, b"m2", "subj2", Flags(0), 1_700_000_001_000)?;
    let _u3 = insert_email(&ms, &mds, b"m3", "subj3", Flags(0), 1_700_000_002_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    let cmd = format!("a03 UID MOVE {u2} Archive\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert!(
        lines.iter().any(|l| l.starts_with("* OK [COPYUID ")),
        "{lines:?}"
    );
    let expunge_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("* ") && l.ends_with(" EXPUNGE"))
        .collect();
    assert_eq!(expunge_lines.len(), 1, "{lines:?}");
    assert_eq!(expunge_lines[0], "* 2 EXPUNGE", "{lines:?}");
    assert_eq!(list_inbox(&ms).len(), 2);
    assert_eq!(list_archive(&ms).len(), 1);
    Ok(())
}

#[tokio::test]
async fn move_to_missing_mailbox_returns_trycreate() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 MOVE 1 NoSuch\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 NO [TRYCREATE]"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn move_in_examined_mailbox_rejected() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 MOVE 1 Archive\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(
        tagged.starts_with("a03 NO [CANNOT]") && tagged.to_lowercase().contains("read-only"),
        "{tagged}"
    );
    Ok(())
}

// ---------------- read-only side-channel suppression ----------------

fn count_retrain_outbox(mds: &Arc<SqliteCasMds>) -> i64 {
    // `mail_retrain_outbox` lives in the per-account MDS sidecar db.
    // Probe via `with_set_tx` and abort the tx (probes must not
    // commit) — same shape as `outbox_label_for` in
    // `mailstore::tests`.
    let set = cosmix_maild::mailstore::account_id_to_setid(1);
    let mut count: i64 = 0;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(&set, |tx| {
        count = tx
            .tx()
            .query_row("SELECT COUNT(*) FROM mail_retrain_outbox", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0);
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    count
}

#[tokio::test]
async fn copy_from_examined_junk_to_inbox_suppresses_retrain() -> Result<()> {
    // RFC 9051 §6.4.7 + substrate invariant: COPY out of an EXAMINE'd
    // source must not enqueue the junk-boundary retrain row — a
    // read-only mailbox is not allowed to side-channel the
    // classifier. We provision a per-account Junk container, drop a
    // message into it, then EXAMINE Junk and COPY out to INBOX.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    ms.create_mailbox(1, "Junk", None, attrs_for(Some("\\Junk"), true))?;
    let junk_cid = ms
        .list_mailboxes(1)?
        .into_iter()
        .find(|m| m.name == "Junk")
        .expect("Junk present")
        .id;
    // Hand-place the message in Junk directly so the test does not
    // depend on a MOVE INBOX→Junk (which would itself enqueue a
    // retrain row and pre-poison the count).
    let hash = mds.put_blob(b"m1")?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "junked".into(),
        date: 1_700_000_000,
        message_id: Some("<m1@example.com>".into()),
    };
    let (_item, _ins) = ms.create_email(
        1,
        junk_cid,
        hash,
        envelope,
        &[],
        Flags(0),
        Tags::new(),
        1_700_000_000_000,
    )?;
    assert_eq!(count_retrain_outbox(&mds), 0, "fixture must start clean");

    let (client, _join) = spawn_session(db.clone(), ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE Junk\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1 INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK [COPYUID "), "{tagged}");
    assert_eq!(
        count_retrain_outbox(&mds),
        0,
        "EXAMINE'd source must not enqueue retrain"
    );
    Ok(())
}

#[tokio::test]
async fn copy_from_selected_junk_to_inbox_enqueues_retrain() -> Result<()> {
    // Companion to the EXAMINE test above — under SELECT (read-write),
    // a Junk→INBOX COPY *does* enqueue a ham-retrain row. This pins
    // that the suppression is gated on read-only specifically, not
    // an unrelated short-circuit.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    ms.create_mailbox(1, "Junk", None, attrs_for(Some("\\Junk"), true))?;
    let junk_cid = ms
        .list_mailboxes(1)?
        .into_iter()
        .find(|m| m.name == "Junk")
        .expect("Junk present")
        .id;
    let hash = mds.put_blob(b"m1")?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "junked".into(),
        date: 1_700_000_000,
        message_id: Some("<m1@example.com>".into()),
    };
    let (_item, _ins) = ms.create_email(
        1,
        junk_cid,
        hash,
        envelope,
        &[],
        Flags(0),
        Tags::new(),
        1_700_000_000_000,
    )?;
    assert_eq!(count_retrain_outbox(&mds), 0, "fixture must start clean");

    let (client, _join) = spawn_session(db.clone(), ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT Junk\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 COPY 1 INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a03 OK [COPYUID "), "{tagged}");
    assert_eq!(
        count_retrain_outbox(&mds),
        1,
        "SELECT'd source crosses junk boundary → enqueue ham retrain"
    );
    Ok(())
}

#[tokio::test]
async fn copy_inherits_flags_from_src_not_sibling() -> Result<()> {
    // MAJOR 1 regression: `add_membership_in_tx` picked
    // `ORDER BY container_id LIMIT 1`, which under IMAP's
    // per-mailbox flag model could pick a sibling whose flags do not
    // match the source. With `add_membership_from`, the dest row
    // must inherit from the SOURCE mailbox specifically.
    //
    // Setup: create an item in INBOX (Flags(0)), COPY it to Archive
    // (so the item now has two memberships), STORE +FLAGS \Seen on
    // the Archive copy only (so Archive carries Flags::SEEN while
    // INBOX still carries Flags(0)), then COPY MSN 1 of INBOX to a
    // third container. The dest row must reflect INBOX's flags
    // (zero), not Archive's.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    ms.create_mailbox(1, "Scratch", None, attrs_for(None, true))?;
    insert_email(&ms, &mds, b"m1", "subj1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // INBOX → Archive
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 COPY 1 Archive\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a03").await?;

    // Set \Seen on Archive copy
    w.write_all(b"a04 SELECT Archive\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a04").await?;
    w.write_all(b"a05 STORE 1 +FLAGS (\\Seen)\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a05").await?;

    // INBOX → Scratch (source = INBOX, which is still Flags(0))
    w.write_all(b"a06 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a06").await?;
    w.write_all(b"a07 COPY 1 Scratch\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a07").await?;
    let tagged = lines.last().expect("tagged");
    assert!(tagged.starts_with("a07 OK [COPYUID "), "{tagged}");

    // Verify dest membership inherited Flags(0) from INBOX, not
    // Flags::SEEN from Archive.
    let scratch_cid = ms
        .list_mailboxes(1)?
        .into_iter()
        .find(|m| m.name == "Scratch")
        .expect("Scratch present")
        .id;
    let handles = ms.list_emails_in_mailbox(
        1,
        scratch_cid,
        ListOpts {
            sort_by: SortKey::Seq,
            limit: u32::MAX,
            offset: 0,
        },
    )?;
    assert_eq!(handles.len(), 1);
    assert_eq!(
        handles[0].keywords.0 & Flags::SEEN,
        0,
        "dest must inherit from INBOX (Flags 0), not Archive (Seen)"
    );
    Ok(())
}
