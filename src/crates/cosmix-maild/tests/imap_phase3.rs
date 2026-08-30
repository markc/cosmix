//! End-to-end Phase 3 IMAP tests — FETCH / UID FETCH metadata atoms.
//!
//! Uses the same in-process `handle_connection` harness as Phase 2
//! (tokio duplex; no TLS). Each test provisions a fresh sqlite Db +
//! MailStore + Mds, inserts known messages into INBOX, then drives
//! the wire to assert FETCH responses match RFC 9051 §6.4.5 / §6.4.8
//! shape for the T9 metadata atoms (FLAGS, UID, RFC822.SIZE,
//! INTERNALDATE).

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
async fn fetch_metadata_atoms_two_message_mailbox() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    // 2023-11-14 22:13:20 UTC, then 2023-11-14 22:13:21 UTC.
    let _uid1 = insert_email(
        &ms,
        &mds,
        b"body-1",
        "msg1",
        Flags(0b0001),
        1_700_000_000_000,
    )?;
    let _uid2 = insert_email(
        &ms,
        &mds,
        b"second-body-2",
        "msg2",
        Flags(0),
        1_700_000_001_000,
    )?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1:* (FLAGS UID RFC822.SIZE INTERNALDATE)\r\n")
        .await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    // Two untagged rows + tagged OK.
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[0].contains("* 1 FETCH (")
            && lines[0].contains("FLAGS (\\Seen)")
            && lines[0].contains("UID 1")
            && lines[0].contains("RFC822.SIZE 6")
            && lines[0].contains("INTERNALDATE \"14-Nov-2023 22:13:20 +0000\""),
        "row 1: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("* 2 FETCH (")
            && lines[1].contains("FLAGS ()")
            && lines[1].contains("UID 2")
            && lines[1].contains("RFC822.SIZE 13")
            && lines[1].contains("INTERNALDATE \"14-Nov-2023 22:13:21 +0000\""),
        "row 2: {}",
        lines[1]
    );
    assert!(lines[2].starts_with("a03 OK"), "{}", lines[2]);
    Ok(())
}

#[tokio::test]
async fn uid_fetch_implicit_uid_inclusion() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // Client asks for FLAGS only — server MUST also include UID
    // per RFC 9051 §6.4.8.
    w.write_all(b"a03 UID FETCH 1:* (FLAGS)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].contains("UID 1") && lines[0].contains("FLAGS ()"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("a03 OK") && lines[1].contains("UID FETCH"),
        "{}",
        lines[1]
    );
    Ok(())
}

#[tokio::test]
async fn fetch_empty_mailbox_emits_no_rows() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1:* (FLAGS UID)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 OK"), "{}", lines[0]);
    Ok(())
}

#[tokio::test]
async fn fetch_out_of_range_msn_returns_no_rows() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // Mailbox has 1 message; msn 5 is wholly above EXISTS.
    w.write_all(b"a03 FETCH 5 (FLAGS)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("a03 OK"), "{}", lines[0]);
    Ok(())
}

#[tokio::test]
async fn fetch_in_authenticated_state_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // No SELECT.
    w.write_all(b"a02 FETCH 1 (FLAGS)\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a02 BAD") && lines[0].to_lowercase().contains("no mailbox"),
        "{}",
        lines[0]
    );
    Ok(())
}

#[tokio::test]
async fn fetch_not_authenticated_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 FETCH 1 (FLAGS)\r\n").await?;
    let line = read_line(&mut reader).await?;
    assert!(line.starts_with("a01 BAD"), "{line}");
    Ok(())
}

#[tokio::test]
async fn fetch_rejects_zero_length_partial() -> Result<()> {
    // T12a: BODY[]<0.0> is rejected — RFC 9051 §9 partial length is
    // `nz-number` (must be > 0). Other malformed atoms still BAD too.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"x", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 BODY[]<0.0>\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a03 BAD") && lines[0].to_lowercase().contains("length"),
        "{}",
        lines[0]
    );
    Ok(())
}

#[tokio::test]
async fn fetch_body_section_returns_section_bytes() -> Result<()> {
    // T12a: BODY.PEEK[HEADER.FIELDS (Subject From)] returns the
    // matching headers as an IMAP literal. PEEK suppresses the \Seen
    // side-effect — pinned by `fetch_body_no_peek_sets_seen` below.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let raw = b"Subject: hi\r\nFrom: a@b.test\r\nTo: c@d.test\r\n\r\nbody";
    insert_email(&ms, &mds, raw, "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 BODY.PEEK[HEADER.FIELDS (Subject From)]\r\n")
        .await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    // Literal payload "Subject: hi\r\nFrom: a@b.test\r\n\r\n" spans
    // additional lines after the `{N}` token; `read_until_tagged`
    // splits on CRLF so we just sanity-check the start of the row and
    // that the OK terminator arrived.
    let row = &lines[0];
    assert!(
        row.starts_with("* 1 FETCH (BODY[HEADER.FIELDS (Subject From)] {"),
        "{row}"
    );
    assert!(lines.last().unwrap().starts_with("a03 OK"), "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn fetch_body_peek_does_not_set_seen() -> Result<()> {
    // PEEK: \Seen MUST NOT be set as a side-effect of the FETCH.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"\r\nbody", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 BODY.PEEK[HEADER]\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a03").await?;

    w.write_all(b"a04 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    // \Seen MUST NOT appear in the FLAGS response after a BODY.PEEK.
    assert!(lines[0].contains("FLAGS ()"), "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn fetch_body_no_peek_sets_seen() -> Result<()> {
    // Non-PEEK BODY[...] MUST set \Seen as a side-effect (RFC 9051
    // §6.4.5). The FLAGS atom in the very same FETCH response should
    // reflect the new \Seen state.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"\r\nbody", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 (FLAGS BODY[HEADER])\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    // The FLAGS atom in this same response should already reflect
    // \Seen (the side-effect is applied before rendering).
    assert!(
        lines[0].contains("FLAGS (\\Seen)"),
        "expected \\Seen on first FETCH row, got: {lines:?}"
    );

    // Follow-up FETCH confirms the mailstore was actually updated.
    w.write_all(b"a04 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    assert!(lines[0].contains("FLAGS (\\Seen)"), "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn fetch_rfc822_header_alias_emits_legacy_wire_name() -> Result<()> {
    // T12a: RFC822.HEADER is the legacy alias for BODY.PEEK[HEADER]
    // and renders with the legacy wire name (no `BODY[]` brackets).
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let raw = b"Subject: hi\r\n\r\nbody";
    insert_email(&ms, &mds, raw, "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 RFC822.HEADER\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert!(
        lines[0].starts_with("* 1 FETCH (RFC822.HEADER {"),
        "{lines:?}"
    );
    assert!(lines.last().unwrap().starts_with("a03 OK"));

    // RFC822.HEADER MUST NOT set \Seen.
    w.write_all(b"a04 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    assert!(lines[0].contains("FLAGS ()"), "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn fetch_bodystructure_returns_parsed_structure() -> Result<()> {
    // T11b — FETCH BODYSTRUCTURE end-to-end. Pins the cross-layer
    // wiring; recursive-renderer behaviour is unit-tested in
    // `imap::op::fetch::tests::render_bodystructure_*`.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let raw = b"From: Alice <alice@x.test>\r\n\
                Subject: Hello\r\n\
                Content-Type: text/plain; charset=us-ascii\r\n\
                \r\n\
                hello\r\n";
    insert_email(&ms, &mds, raw, "self@x.test", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 BODYSTRUCTURE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    let row = &lines[0];
    assert!(row.starts_with("* 1 FETCH (BODYSTRUCTURE ("), "{row}");
    assert!(row.contains("\"TEXT\" \"PLAIN\""), "{row}");
    assert!(row.contains("\"CHARSET\" \"us-ascii\""), "{row}");
    assert!(row.contains("\"7BIT\""), "{row}");
    assert_eq!(lines[1], "a03 OK FETCH completed");
    Ok(())
}

#[tokio::test]
async fn fetch_envelope_returns_parsed_structure() -> Result<()> {
    // T11a — FETCH ENVELOPE end-to-end. The renderer's behaviour is
    // unit-tested in `imap::op::fetch::tests::render_envelope_*`; this
    // test pins the cross-layer wiring (mds blob load → mail_parser →
    // wire emission) and the per-row body load path.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let raw = b"From: Alice <alice@x.test>\r\n\
                To: Bob <bob@y.test>\r\n\
                Subject: Hello\r\n\
                Date: Wed, 14 Nov 2023 22:13:20 +0000\r\n\
                Message-ID: <self@x.test>\r\n\
                \r\n\
                body\r\n";
    insert_email(&ms, &mds, raw, "self@x.test", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 ENVELOPE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    let row = &lines[0];
    assert!(row.starts_with("* 1 FETCH (ENVELOPE ("), "{row}");
    assert!(row.contains("\"Wed, 14 Nov 2023 22:13:20 +0000\""), "{row}");
    assert!(row.contains("\"Hello\""), "{row}");
    assert!(
        row.contains("((\"Alice\" NIL \"alice\" \"x.test\"))"),
        "{row}"
    );
    assert!(row.contains("\"<self@x.test>\""), "{row}");
    assert_eq!(lines[1], "a03 OK FETCH completed");
    Ok(())
}

#[tokio::test]
async fn uid_fetch_unknown_inner_verb_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // After T15-T17 every RFC 9051 §6.4.8 UID inner verb
    // (FETCH/STORE/SEARCH/COPY/MOVE/EXPUNGE) is implemented; only an
    // unknown verb name should now produce the "UID inner command not
    // implemented" BAD path.
    w.write_all(b"a03 UID FROBNICATE 1\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a03 BAD") && lines[0].to_lowercase().contains("uid inner command"),
        "{}",
        lines[0]
    );
    Ok(())
}

#[tokio::test]
async fn fetch_single_atom_no_parens() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"x", "msg1", Flags(0b0001), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* 1 FETCH (FLAGS (\\Seen))");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

// ---------- T13a: SEARCH / UID SEARCH ----------

#[tokio::test]
async fn search_all_returns_every_msn() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"one", "msg1", Flags(0), 1_700_000_000_000)?;
    insert_email(&ms, &mds, b"two", "msg2", Flags(0), 1_700_000_001_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 SEARCH ALL\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* SEARCH 1 2");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn search_unseen_returns_messages_without_seen_bit() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(
        &ms,
        &mds,
        b"a",
        "seen",
        Flags(Flags::SEEN),
        1_700_000_000_000,
    )?;
    insert_email(&ms, &mds, b"b", "unseen", Flags(0), 1_700_000_001_000)?;
    insert_email(
        &ms,
        &mds,
        b"c",
        "seen2",
        Flags(Flags::SEEN),
        1_700_000_002_000,
    )?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 SEARCH UNSEEN\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* SEARCH 2");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn uid_search_emits_uids_not_msns() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let uid1 = insert_email(&ms, &mds, b"a", "msg1", Flags(0), 1_700_000_000_000)?;
    let uid2 = insert_email(&ms, &mds, b"b", "msg2", Flags(0), 1_700_000_001_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 UID SEARCH UID 1:*\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], format!("* SEARCH {uid1} {uid2}"));
    assert!(
        lines[1].starts_with("a03 OK") && lines[1].contains("UID SEARCH"),
        "{}",
        lines[1]
    );
    Ok(())
}

#[tokio::test]
async fn search_empty_match_emits_bare_search_line() -> Result<()> {
    // RFC 9051 §7.3.4 — no matches still produces an untagged
    // `* SEARCH` (with no numbers), followed by tagged OK.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(
        &ms,
        &mds,
        b"a",
        "seen",
        Flags(Flags::SEEN),
        1_700_000_000_000,
    )?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 SEARCH UNSEEN\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* SEARCH");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn search_not_deleted_excludes_deleted_messages() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"a", "live", Flags(0), 1_700_000_000_000)?;
    insert_email(
        &ms,
        &mds,
        b"b",
        "del",
        Flags(Flags::DELETED),
        1_700_000_001_000,
    )?;
    insert_email(&ms, &mds, b"c", "live2", Flags(0), 1_700_000_002_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 SEARCH NOT DELETED\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* SEARCH 1 3");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn search_implicit_and_combines_flag_predicates() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(
        &ms,
        &mds,
        b"a",
        "flagged-live",
        Flags(Flags::FLAGGED),
        1_700_000_000_000,
    )?;
    insert_email(
        &ms,
        &mds,
        b"b",
        "flagged-del",
        Flags(Flags::FLAGGED | Flags::DELETED),
        1_700_000_001_000,
    )?;
    insert_email(&ms, &mds, b"c", "plain", Flags(0), 1_700_000_002_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // Implicit AND: FLAGGED + UNDELETED → only msn 1.
    w.write_all(b"a03 SEARCH FLAGGED UNDELETED\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "* SEARCH 1");
    assert!(lines[1].starts_with("a03 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn search_deferred_subject_criterion_returns_bad() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_email(&ms, &mds, b"a", "msg1", Flags(0), 1_700_000_000_000)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    // T13b: header/body criteria deferred — must be BAD rather than
    // silently treated as ALL (state-of-truth divergence trap).
    w.write_all(b"a03 SEARCH SUBJECT hello\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a03 BAD") && lines[0].to_lowercase().contains("deferred"),
        "{}",
        lines[0]
    );
    Ok(())
}

#[tokio::test]
async fn search_no_mailbox_selected_returns_bad() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // No SELECT.
    w.write_all(b"a02 SEARCH ALL\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("a02 BAD") && lines[0].to_lowercase().contains("no mailbox"),
        "{}",
        lines[0]
    );
    Ok(())
}
