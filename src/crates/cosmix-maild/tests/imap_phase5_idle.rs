//! End-to-end IMAP IDLE (RFC 2177) tests — Phase 5 T18.
//!
//! Mirrors the harness in `imap_phase4_copy_move_expunge.rs`: in-
//! process duplex socket, no TLS, fresh sqlite Db + MailStore + Mds
//! per test. Each test drives IDLE through the wire, triggers a
//! substrate-side mutation in a parallel task, and asserts the
//! untagged `EXISTS` / `EXPUNGE` lines arrive on the IDLE
//! connection.

use std::sync::Arc;
use std::time::Duration;

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
use tokio::time::timeout;

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
async fn idle_emits_continuation_then_tagged_ok_on_done() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        idle_status_interval: Duration::from_secs(60),
        ..Default::default()
    };
    let (client, _join) = spawn_session(db, ms.clone(), cfg).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 OK"), "expected tagged OK, got {resp}");
    assert!(resp.contains("IDLE terminated"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn idle_rejects_arguments() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE garbage\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 BAD"), "expected BAD, got {resp}");
    Ok(())
}

#[tokio::test]
async fn idle_rejected_pre_auth() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 IDLE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a01 BAD"), "expected BAD, got {resp}");
    Ok(())
}

#[tokio::test]
async fn idle_bad_line_in_place_of_done_closes_session() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    // RFC 2177 §3 — only DONE is legal during IDLE. Anything else
    // is a protocol violation; the server emits tagged BAD and
    // closes the session. Resuming dispatch from inside IDLE on a
    // possibly-desynced stream (literal-shaped input, partial
    // line) is not safe; terminal-close matches Cyrus/Dovecot.
    w.write_all(b"NOOP\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 BAD"), "expected BAD, got {resp}");

    // After the BAD the session must close: either the next read
    // returns Eof immediately, or the next write to the socket
    // fails because the peer hung up. Either is a valid signal of
    // terminal close.
    let _ = w.write_all(b"a04 NOOP\r\n").await; // may succeed (kernel buffer) or fail.
    let post = timeout(Duration::from_secs(2), read_line(&mut reader)).await?;
    match post {
        Ok(line) if line.is_empty() => {
            // Read of length 0 — clean EOF.
        }
        Ok(line) => panic!("expected EOF after IDLE BAD, got line: {line:?}"),
        Err(_) => {} // I/O error on closed socket — also a valid close.
    }
    Ok(())
}

#[tokio::test]
async fn idle_emits_exists_when_message_appended_to_selected_mailbox() -> Result<()> {
    // Drive the substrate from a parallel task: spawn the IDLE
    // session, enter IDLE, insert_email via the MailStore handle
    // shared with the test process, then expect the wire to carry
    // `* 1 EXISTS\r\n`.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    // Substrate write — fires `ItemAdded` on INBOX's notifier
    // channel, which the IDLE handler translates to `* N EXISTS`.
    insert_email(
        &ms,
        &mds,
        b"hello-idle",
        "idle-1",
        Flags(0),
        1_700_000_000_000,
    )?;

    let line = timeout(Duration::from_secs(5), read_line(&mut reader)).await??;
    assert_eq!(line, "* 1 EXISTS");

    // Clean DONE.
    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 OK"), "expected tagged OK, got {resp}");
    Ok(())
}

#[tokio::test]
async fn idle_emits_expunge_when_message_removed_from_selected_mailbox() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"first", "subj1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"second", "subj2", Flags(0), 1_700_000_001_000)?;
    insert_email(&ms, &mds, b"third", "subj3", Flags(0), 1_700_000_002_000)?;

    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    // Substrate write — delete the middle item (UID 2). The IDLE
    // handler resolves UID 2 → MSN 2 against the pre-snapshot.
    let items: Vec<cosmix_mds::ItemId> = {
        let inbox = ms.mailbox_by_role(1, MailboxRole::Inbox)?.expect("inbox");
        ms.list_emails_in_mailbox(
            1,
            inbox,
            ListOpts {
                sort_by: SortKey::Seq,
                limit: u32::MAX,
                offset: 0,
            },
        )?
        .into_iter()
        .map(|h| h.id)
        .collect()
    };
    let inbox = ms.mailbox_by_role(1, MailboxRole::Inbox)?.expect("inbox");
    ms.expunge_memberships(1, inbox, &[items[1]])?;

    let line = timeout(Duration::from_secs(5), read_line(&mut reader)).await??;
    assert_eq!(line, "* 2 EXPUNGE");

    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 OK"), "expected tagged OK, got {resp}");
    Ok(())
}

#[tokio::test]
async fn idle_keepalive_fires_every_interval() -> Result<()> {
    // Keepalive cadence — sub-second interval so the test stays
    // fast. The IDLE loop should emit `* OK Still here\r\n` after
    // each tick of `idle_status_interval`.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        idle_status_interval: Duration::from_millis(300),
        ..Default::default()
    };
    let (client, _join) = spawn_session(db, ms.clone(), cfg).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    let still_here = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        still_here.starts_with("* OK") && still_here.contains("Still here"),
        "expected keepalive, got {still_here}"
    );

    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 OK"), "expected tagged OK, got {resp}");
    Ok(())
}

#[tokio::test]
async fn capability_advertises_idle() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a01").await?;
    let cap_line = lines
        .iter()
        .find(|l| l.starts_with("* CAPABILITY"))
        .expect("CAPABILITY response");
    assert!(
        cap_line.contains(" IDLE"),
        "CAPABILITY must advertise IDLE: {cap_line}"
    );
    Ok(())
}

#[tokio::test]
async fn idle_in_authenticated_state_no_mailbox_emits_no_untagged_events() -> Result<()> {
    // RFC 2177 §3 — IDLE is valid in both AUTHENTICATED and
    // SELECTED states. In AUTHENTICATED-only (no mailbox selected)
    // the handler must NOT subscribe to anything and must NOT
    // emit any untagged mailbox-state event. Continuation + DONE +
    // tagged OK is the entire wire shape.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        idle_status_interval: Duration::from_secs(60),
        ..Default::default()
    };
    let (client, _join) = spawn_session(db, ms.clone(), cfg).await;
    let (mut reader, mut w) = login(client).await?;
    // Skip SELECT — we want to stay in AUTHENTICATED.

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    // Insert into INBOX while idling — this publishes ItemAdded
    // on the INBOX channel, but since we never SELECTed we must
    // not emit `* EXISTS` on this connection. The proof: the
    // next line after DONE is the tagged OK with no preceding
    // untagged response.
    let _uid = insert_email(
        &ms,
        &mds,
        b"untouched",
        "auth-idle",
        Flags(0),
        1_700_000_000_000,
    )?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a03 OK"),
        "expected tagged OK directly after DONE (no untagged), got {resp}"
    );
    assert!(resp.contains("IDLE terminated"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn idle_close_is_terminal_after_one_bad_line() -> Result<()> {
    // Under the post-T18 semantics, any protocol violation while
    // IDLE is "in progress" — non-DONE line, line-too-long, non-
    // UTF8 framing — is terminal. The handler emits tagged BAD,
    // returns `IdleOutcome::Close`, and the session loop returns.
    // The next read on the wire must observe EOF. This guards
    // against re-introduction of the "resume on BAD" behavior that
    // would leak stream-framing desync in the literal-shaped-input
    // case.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    w.write_all(b"BADLINE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 BAD"), "expected BAD, got {resp}");

    // Terminal: the next read sees EOF. Use a generous timeout and
    // accept either an Io error or an empty line (peer close).
    let post = timeout(Duration::from_secs(2), read_line(&mut reader)).await?;
    match post {
        Ok(line) if line.is_empty() => {}
        Ok(line) => panic!("expected EOF after IDLE BAD, got: {line:?}"),
        Err(_) => {}
    }
    Ok(())
}
