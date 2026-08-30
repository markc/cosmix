//! End-to-end Phase 2 IMAP tests — LIST / LSUB / NAMESPACE.
//!
//! Uses the same in-process `handle_connection` harness as Phase 1
//! (bypasses TLS via `tokio::io::duplex`) so we exercise the dispatch
//! arms, argument parsing, mailbox-set encoding, and wire output
//! without standing up the rustls pipeline.
//!
//! The test fixture provisions a small mailbox tree per account
//! directly through the `MailStore` trait:
//! ```text
//! INBOX (role=Inbox, subscribed)
//! ├── Folder1            (subscribed)
//! └── Folder1/Sub        (unsubscribed)
//! Sent  (role=Sent, subscribed)
//! Trash (role=Trash, unsubscribed)
//! ```
//! "INBOX" is the wire name regardless of stored name when
//! `role = Inbox` — the encoder folds the role-Inbox row to the
//! literal `INBOX` per RFC 9051 §5.1.

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

/// Reconnect-test fixture: build a per-test sqlite Db + MailStore,
/// provision the fixture mailbox tree on `account_id=1`, and return
/// the inner `Arc<SqliteCasMds>` so callers that need to drive
/// `put_blob` + `create_email` between IMAP sessions (T7) have a
/// direct handle.
async fn make_db_with_tree_and_mds(
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

    let inbox = mailstore.create_mailbox(1, "INBOX", None, attrs_for(Some("\\Inbox"), true))?;
    let folder1 = mailstore.create_mailbox(1, "Folder1", None, attrs_for(None, true))?;
    mailstore.create_mailbox(1, "Sub", Some(folder1), attrs_for(None, false))?;
    mailstore.create_mailbox(1, "Sent", None, attrs_for(Some("\\Sent"), true))?;
    mailstore.create_mailbox(1, "Trash", None, attrs_for(Some("\\Trash"), false))?;
    let _ = inbox;

    Ok((tmp, db, mds, mailstore))
}

/// Thin wrapper that drops the `mds` slot. Most tests don't need it,
/// so they keep their `let (_tmp, db, ms) = make_db_with_tree(...)`
/// shape unchanged. Single source of truth for the fixture tree.
async fn make_db_with_tree(
    email: &str,
    password: &str,
) -> Result<(TempDir, Db, Arc<dyn MailStore>)> {
    let (tmp, db, _mds, mailstore) = make_db_with_tree_and_mds(email, password).await?;
    Ok((tmp, db, mailstore))
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

/// Authenticate via LOGIN and drain through the tagged OK. Returns
/// the (reader, writer) split of the duplex stream so the caller can
/// continue issuing post-auth commands.
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

/// Read lines until one starts with `tag `; return the full block
/// including the tagged line.
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

#[tokio::test]
async fn namespace_returns_personal_with_slash_delimiter() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 NAMESPACE\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], r#"* NAMESPACE (("" "/")) NIL NIL"#);
    assert!(lines[1].starts_with("a02 OK NAMESPACE"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn namespace_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 NAMESPACE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn list_star_enumerates_all_mailboxes() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert!(lines.last().unwrap().starts_with("a02 OK"), "{lines:?}");

    let untagged: Vec<&String> = lines[..lines.len() - 1].iter().collect();
    let names: Vec<&str> = untagged
        .iter()
        .map(|l| l.split('"').nth(3).expect("name field"))
        .collect();
    assert!(names.contains(&"INBOX"), "missing INBOX in {names:?}");
    assert!(names.contains(&"Folder1"), "missing Folder1 in {names:?}");
    assert!(
        names.contains(&"Folder1/Sub"),
        "missing Folder1/Sub in {names:?}"
    );
    assert!(names.contains(&"Sent"), "missing Sent in {names:?}");
    assert!(names.contains(&"Trash"), "missing Trash in {names:?}");

    // INBOX line carries \Inbox and \HasChildren attrs (Folder1/Sub
    // hangs off something? No — Folder1 has \HasChildren. INBOX is
    // childless in this fixture, so \HasNoChildren).
    let inbox_line = untagged
        .iter()
        .find(|l| l.contains(r#""INBOX""#))
        .expect("INBOX line");
    assert!(inbox_line.contains(r"\Inbox"), "{inbox_line}");
    assert!(inbox_line.contains(r"\HasNoChildren"), "{inbox_line}");

    let folder1_line = untagged
        .iter()
        .find(|l| l.ends_with(r#""Folder1""#))
        .expect("Folder1 line");
    assert!(folder1_line.contains(r"\HasChildren"), "{folder1_line}");

    let sent_line = untagged
        .iter()
        .find(|l| l.contains(r#""Sent""#))
        .expect("Sent line");
    assert!(sent_line.contains(r"\Sent"), "{sent_line}");
    Ok(())
}

#[tokio::test]
async fn list_inbox_returns_only_inbox_row() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("* LIST "), "{}", lines[0]);
    assert!(lines[0].contains(r#""INBOX""#), "{}", lines[0]);
    assert!(lines[1].starts_with("a02 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn list_inbox_is_case_insensitive() -> Result<()> {
    // RFC 9051 §5.1: INBOX is case-insensitive in both resolution
    // and LIST pattern matching. `LIST "" inbox` must return the
    // role-Inbox row exactly the same way `LIST "" INBOX` does.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" inbox\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("* LIST "), "{}", lines[0]);
    assert!(lines[0].contains(r#""INBOX""#), "{}", lines[0]);
    assert!(lines[1].starts_with("a02 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn list_inbox_children_case_insensitive() -> Result<()> {
    // `LIST "" inbox/%` (lowercase) must match INBOX children just
    // like `LIST "" INBOX/%`.
    let tmp = tempfile::tempdir()?;
    let dbp = tmp.path().join("mail.db").to_string_lossy().into_owned();
    let blob = tmp.path().join("blobs").to_string_lossy().into_owned();
    let db = Db::connect(&dbp, &blob).await?;
    db.migrate().await?;
    let hash = bcrypt::hash("hunter2", 4)?;
    {
        let conn = db.conn.lock().expect("db lock");
        conn.execute(
            "INSERT INTO accounts (email, password, name) VALUES (?1, ?2, ?3)",
            params!["alice@example.com", hash, "Test"],
        )?;
    }
    let mds = Arc::new(cosmix_mds::SqliteCasMds::open(tmp.path().join("mds"))?);
    let ms: Arc<dyn MailStore> = Arc::new(SqliteMailStore::new(mds));
    let inbox = ms.create_mailbox(1, "INBOX", None, attrs_for(Some("\\Inbox"), true))?;
    ms.create_mailbox(1, "Receipts", Some(inbox), attrs_for(None, true))?;

    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" inbox/%\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    let untagged: Vec<&String> = lines[..lines.len() - 1].iter().collect();
    let names: Vec<&str> = untagged
        .iter()
        .map(|l| l.split('"').nth(3).expect("name field"))
        .collect();
    assert_eq!(names, vec!["INBOX/Receipts"], "{lines:?}");
    Ok(())
}

#[tokio::test]
async fn list_empty_pattern_returns_root_delimiter() -> Result<()> {
    // RFC 9051 §6.3.9 — `LIST "" ""` is the delimiter-probe shape;
    // server returns exactly one row carrying \Noselect, the root
    // delimiter, and an empty name.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" \"\"\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], r#"* LIST (\Noselect) "/" """#);
    assert!(lines[1].starts_with("a02 OK"), "{}", lines[1]);
    Ok(())
}

#[tokio::test]
async fn list_pattern_with_non_ascii_returns_bad() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // ä = 0xC3 0xA4 in UTF-8; the quoted-string parser keeps the
    // raw bytes, so the handler sees a non-ASCII pattern and BADs.
    w.write_all("a02 LIST \"\" \"Caf\u{00e9}/%\"\r\n".as_bytes())
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.contains("non-ASCII"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn capability_advertises_phase2_extensions() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.contains("NAMESPACE"), "{greeting}");
    assert!(greeting.contains("CHILDREN"), "{greeting}");
    assert!(greeting.contains("SPECIAL-USE"), "{greeting}");
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("NAMESPACE"), "{untagged}");
    assert!(untagged.contains("CHILDREN"), "{untagged}");
    assert!(untagged.contains("SPECIAL-USE"), "{untagged}");
    Ok(())
}

#[tokio::test]
async fn list_percent_excludes_nested_children() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" %\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    let untagged: Vec<&String> = lines[..lines.len() - 1].iter().collect();
    let names: Vec<&str> = untagged
        .iter()
        .map(|l| l.split('"').nth(3).expect("name field"))
        .collect();
    assert!(names.contains(&"INBOX"), "{names:?}");
    assert!(names.contains(&"Folder1"), "{names:?}");
    assert!(names.contains(&"Sent"), "{names:?}");
    assert!(names.contains(&"Trash"), "{names:?}");
    assert!(
        !names.contains(&"Folder1/Sub"),
        "% wildcard must not cross /: {names:?}"
    );
    Ok(())
}

#[tokio::test]
async fn list_rejects_non_empty_reference() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST INBOX *\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.contains("non-empty LIST reference"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn list_rejects_return_extension() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" * RETURN (SUBSCRIBED)\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn list_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 LIST \"\" *\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn lsub_returns_only_subscribed_rows() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LSUB \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert!(lines.last().unwrap().starts_with("a02 OK"), "{lines:?}");
    let untagged: Vec<&String> = lines[..lines.len() - 1].iter().collect();
    let names: Vec<&str> = untagged
        .iter()
        .map(|l| l.split('"').nth(3).expect("name field"))
        .collect();
    // Subscribed: INBOX, Folder1, Sent. Unsubscribed: Folder1/Sub, Trash.
    assert!(names.contains(&"INBOX"), "{names:?}");
    assert!(names.contains(&"Folder1"), "{names:?}");
    assert!(names.contains(&"Sent"), "{names:?}");
    assert!(!names.contains(&"Folder1/Sub"), "{names:?}");
    assert!(!names.contains(&"Trash"), "{names:?}");
    // Every row must be `* LSUB ...`, not `* LIST ...`.
    for line in &untagged {
        assert!(line.starts_with("* LSUB "), "{line}");
    }
    Ok(())
}

#[tokio::test]
async fn lsub_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 LSUB \"\" *\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

// ---------- T4: SELECT / EXAMINE / STATUS / CLOSE / UNSELECT ----------

#[tokio::test]
async fn select_inbox_emits_full_response_block() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    let tagged = lines.last().unwrap();
    assert!(tagged.starts_with("a02 OK [READ-WRITE]"), "{tagged}");

    let block = lines[..lines.len() - 1].to_vec().join("\n");
    // Mandatory untagged block — RFC 9051 §6.3.2.
    assert!(
        block.contains("* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)"),
        "{block}"
    );
    assert!(block.contains(" EXISTS"), "{block}");
    assert!(
        block.contains("* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)]"),
        "{block}"
    );
    assert!(block.contains("[UIDVALIDITY "), "{block}");
    // RFC 9051 §2.3.1.1: UIDVALIDITY is a 32-bit nz-number. The store's
    // seq_validity is epoch-ms (> u32::MAX); the wire value must be the
    // reduced 32-bit form or strict clients (dovecot imapc) reject the
    // mailbox with "UIDVALIDITY not received" (regression, 2026-08-25).
    let uidv = block
        .split("[UIDVALIDITY ")
        .nth(1)
        .and_then(|s| s.split(']').next())
        .expect("UIDVALIDITY code present");
    let uidv: u32 = uidv
        .parse()
        .unwrap_or_else(|e| panic!("UIDVALIDITY {uidv:?} is not a u32 nz-number: {e}\n{block}"));
    assert!(uidv > 0, "UIDVALIDITY must be non-zero\n{block}");
    assert!(block.contains("[UIDNEXT "), "{block}");
    assert!(block.contains("* LIST () \"/\" \"INBOX\""), "{block}");
    // Phase 2 deliberately omits these.
    assert!(!block.contains(" RECENT"), "{block}");
    assert!(!block.contains("HIGHESTMODSEQ"), "{block}");
    // PERMANENTFLAGS advertises `\*` — STORE/APPEND accept client keyword
    // creation (W2). The `* FLAGS` line (asserted above) still lists only
    // system flags, not `\*`.
    assert!(
        block.contains("PERMANENTFLAGS") && block.contains("\\*"),
        "{block}"
    );
    Ok(())
}

#[tokio::test]
async fn select_inbox_is_case_insensitive() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT inbox\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    let tagged = lines.last().unwrap();
    assert!(tagged.starts_with("a02 OK [READ-WRITE]"), "{tagged}");
    let block = lines.join("\n");
    // Canonical wire name flows through `* LIST`, regardless of client casing.
    assert!(block.contains("* LIST () \"/\" \"INBOX\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn examine_inbox_emits_read_only() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    let tagged = lines.last().unwrap();
    assert!(tagged.starts_with("a02 OK [READ-ONLY]"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn select_nonexistent_returns_no_nonexistent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT NoSuchFolder\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn failed_select_after_select_drops_to_authenticated() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // First, succeed on INBOX so the session is in Selected.
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert!(lines.last().unwrap().starts_with("a02 OK"), "{:?}", lines);

    // Now try a nonexistent mailbox: must fail AND drop the
    // session back to Authenticated (no selection).
    w.write_all(b"a03 SELECT NoSuchFolder\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 NO [NONEXISTENT]"), "{resp}");

    // CLOSE in Authenticated must fail "no mailbox selected" —
    // proving the failed SELECT implicitly closed the prior one.
    w.write_all(b"a04 CLOSE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a04 BAD"), "{resp}");
    assert!(resp.contains("no mailbox selected"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn close_after_select_returns_ok_and_unselects() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 CLOSE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 OK"), "{resp}");
    assert!(resp.contains("CLOSE"), "{resp}");

    // Second CLOSE without a prior SELECT must fail.
    w.write_all(b"a04 CLOSE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a04 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn unselect_after_select_returns_ok_and_unselects() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 UNSELECT\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 OK"), "{resp}");
    assert!(resp.contains("UNSELECT"), "{resp}");

    w.write_all(b"a04 UNSELECT\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a04 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn check_after_select_returns_ok_and_stays_selected() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 CHECK\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 OK"), "{resp}");
    assert!(resp.contains("CHECK"), "{resp}");

    // CHECK must leave the mailbox selected (unlike CLOSE/UNSELECT):
    // a following CLOSE should still succeed.
    w.write_all(b"a04 CLOSE\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a04 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn check_without_selected_mailbox_returns_bad() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;

    // No SELECT yet — CHECK requires Selected state.
    w.write_all(b"a02 CHECK\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.contains("no mailbox selected"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn check_rejects_arguments() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;

    w.write_all(b"a03 CHECK extra\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 BAD"), "{resp}");
    assert!(resp.contains("no arguments"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn select_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 SELECT INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn select_rejects_condstore_extension() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX (CONDSTORE)\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.contains("not supported"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn status_supported_items_round_trip() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 STATUS INBOX (MESSAGES UIDNEXT UIDVALIDITY UNSEEN)\r\n")
        .await?;
    let lines = read_until_tagged(&mut reader, "a02").await?;
    assert!(lines.last().unwrap().starts_with("a02 OK"), "{lines:?}");
    let untagged = &lines[0];
    assert!(untagged.starts_with("* STATUS \"INBOX\" ("), "{untagged}");
    assert!(untagged.contains("MESSAGES "), "{untagged}");
    assert!(untagged.contains("UIDNEXT "), "{untagged}");
    assert!(untagged.contains("UIDVALIDITY "), "{untagged}");
    assert!(untagged.contains("UNSEEN "), "{untagged}");
    Ok(())
}

#[tokio::test]
async fn status_rejects_unsupported_items() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // RECENT is a mandatory rev1 STATUS item: accepted as constant 0
    // (rev1 clients hard-require it; Thunderbird wedges on BAD).
    w.write_all(b"a02 STATUS INBOX (RECENT)\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("RECENT 0"), "{untagged}");
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    for (tag, item) in [
        ("a03", "HIGHESTMODSEQ"),
        ("a04", "DELETED"),
        ("a05", "SIZE"),
        ("a06", "BOGUS"),
    ] {
        let cmd = format!("{tag} STATUS INBOX ({item})\r\n");
        w.write_all(cmd.as_bytes()).await?;
        let resp = read_line(&mut reader).await?;
        assert!(resp.starts_with(&format!("{tag} BAD")), "{item}: {resp}");
        assert!(resp.contains("not supported"), "{item}: {resp}");
    }
    Ok(())
}

#[tokio::test]
async fn status_nonexistent_returns_no_nonexistent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 STATUS NoSuchFolder (MESSAGES)\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn status_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 STATUS INBOX (MESSAGES)\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn select_rejects_glued_condstore_paren_atom() -> Result<()> {
    // Codex MAJOR-1 from T4 cold review: `SELECT INBOX(CONDSTORE)`
    // without a space must NOT slip through the space-only tokenizer
    // as a single atom and produce NO [NONEXISTENT]. The atom path
    // must reject `(` / `)` / atom-specials and BAD it.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX(CONDSTORE)\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    assert!(resp.contains("atom-special"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn status_rejects_glued_paren_atom() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 STATUS INBOX(BOGUS) (MESSAGES)\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn close_rejects_trailing_arguments() -> Result<()> {
    // Codex MINOR-1: CLOSE/UNSELECT take no arguments. A trailing
    // token must BAD; do not silently ignore it.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 CLOSE garbage\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 BAD"), "{resp}");
    assert!(resp.contains("no arguments"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn unselect_rejects_trailing_arguments() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 UNSELECT bogus\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn capability_advertises_unselect() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.contains("UNSELECT"), "{greeting}");
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("UNSELECT"), "{untagged}");
    Ok(())
}

// =====================================================================
// T5 — CREATE / DELETE / RENAME / SUBSCRIBE / UNSUBSCRIBE
// =====================================================================

#[tokio::test]
async fn create_simple_mailbox_returns_ok() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Drafts\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    // LIST sees it on the wire.
    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(block.contains("\"Drafts\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn create_with_intermediate_segments_walks_top_down() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Archive/2025/Q1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(block.contains("\"Archive\""), "{block}");
    assert!(block.contains("\"Archive/2025\""), "{block}");
    assert!(block.contains("\"Archive/2025/Q1\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn create_inbox_is_idempotent_ok() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 CREATE inbox\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_trailing_slash_is_idempotent_for_existing_parent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Folder1 is in the fixture; trailing slash MUST succeed.
    w.write_all(b"a02 CREATE Folder1/\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_existing_without_trailing_slash_returns_alreadyexists() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Folder1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [ALREADYEXISTS]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_under_inbox_uses_role_inbox_as_parent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE INBOX/Receipts\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    w.write_all(b"a03 LIST \"\" INBOX/*\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(block.contains("\"INBOX/Receipts\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn create_rejects_empty_segment() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE \"foo//bar\"\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("empty"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_rejects_ampersand_segment() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE \"R&D\"\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("UTF-7"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_rejects_reserved_segment() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE __upload_staging__\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("reserved"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 CREATE Drafts\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_user_mailbox_returns_ok() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Folder1/Sub is a leaf with no role and no rows — destroyable.
    w.write_all(b"a02 DELETE Folder1/Sub\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_inbox_is_rejected_with_cannot() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 DELETE INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("INBOX"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_nonexistent_returns_nonexistent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 DELETE NoSuchFolder\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_mailbox_with_children_returns_cannot() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 DELETE Folder1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("child"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_role_mailbox_returns_cannot() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Sent has special-use \Sent.
    w.write_all(b"a02 DELETE Sent\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("role"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn delete_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 DELETE Folder1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_user_mailbox_returns_ok() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME Folder1/Sub MovedSub\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(block.contains("\"MovedSub\""), "{block}");
    assert!(!block.contains("\"Folder1/Sub\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn rename_inbox_is_rejected_with_cannot() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME INBOX OldMail\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    assert!(resp.contains("INBOX"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_to_inbox_is_rejected() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME Folder1 INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_nonexistent_source_returns_nonexistent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME NoSuchFolder Target\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_to_existing_target_returns_alreadyexists() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME Folder1 Sent\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [ALREADYEXISTS]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_with_missing_parent_returns_nonexistent_parent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Codex-locked: RENAME does NOT auto-create missing parents.
    w.write_all(b"a02 RENAME Folder1 NewParent/Moved\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    assert!(resp.contains("parent"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_rejects_trailing_slash_on_destination() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 RENAME Folder1 \"Moved/\"\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [CANNOT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn rename_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 RENAME a b\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn subscribe_toggles_visibility_in_lsub() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Folder1/Sub starts unsubscribed in the fixture.
    w.write_all(b"a02 SUBSCRIBE Folder1/Sub\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 LSUB \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(block.contains("\"Folder1/Sub\""), "{block}");
    Ok(())
}

#[tokio::test]
async fn unsubscribe_removes_from_lsub() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Folder1 starts subscribed in the fixture.
    w.write_all(b"a02 UNSUBSCRIBE Folder1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 LSUB \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    assert!(!block.contains("\"Folder1\"\r"), "{block}");
    // Note: simple substring "Folder1" still matches Folder1/Sub paths; tighten
    // the negative check by looking for the exact " \"Folder1\"" leaf line.
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("* LSUB ") && l.trim_end().ends_with("\"Folder1\"")),
        "Folder1 leaked back into LSUB: {lines:?}"
    );
    Ok(())
}

#[tokio::test]
async fn subscribe_inbox_returns_ok() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SUBSCRIBE INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 UNSUBSCRIBE INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a03 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn subscribe_nonexistent_returns_nonexistent() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SUBSCRIBE NoSuchFolder\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [NONEXISTENT]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn subscribe_requires_authentication() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 SUBSCRIBE Folder1\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

// =====================================================================
// T6 — CREATE with SPECIAL-USE (RFC 6154 §5.1)
// =====================================================================

#[tokio::test]
async fn create_with_use_drafts_emits_special_use_in_list() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Drafts (USE (\\Drafts))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    let drafts_line = lines
        .iter()
        .find(|l| l.contains("\"Drafts\""))
        .unwrap_or_else(|| panic!("no Drafts line in {block}"));
    assert!(
        drafts_line.contains("\\Drafts"),
        "Drafts line missing \\Drafts attribute: {drafts_line}"
    );
    Ok(())
}

#[tokio::test]
async fn create_with_unknown_use_returns_useattr() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Foo (USE (\\Bogus))\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_with_use_inbox_returns_useattr() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Foo (USE (\\Inbox))\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_with_multiple_use_flags_returns_useattr() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Foo (USE (\\Drafts \\Sent))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_with_use_only_applies_to_leaf_segment() -> Result<()> {
    // \Archive is absent from the fixture (which only seeds \Inbox and
    // \Sent), so this exercises the leaf-only USE attribute without
    // colliding with per-account role uniqueness.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Mail/OldStuff (USE (\\Archive))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    let mail_line = lines
        .iter()
        .find(|l| l.contains("\"Mail\""))
        .unwrap_or_else(|| panic!("no Mail line in {block}"));
    assert!(
        !mail_line.contains("\\Archive"),
        "intermediate Mail line should NOT carry \\Archive: {mail_line}"
    );
    let leaf_line = lines
        .iter()
        .find(|l| l.contains("\"Mail/OldStuff\""))
        .unwrap_or_else(|| panic!("no Mail/OldStuff line in {block}"));
    assert!(
        leaf_line.contains("\\Archive"),
        "leaf Mail/OldStuff line missing \\Archive: {leaf_line}"
    );
    Ok(())
}

#[tokio::test]
async fn create_with_duplicate_role_returns_useattr() -> Result<()> {
    // Fixture already provisions a Sent mailbox with \Sent. Creating
    // a second mailbox with the same SPECIAL-USE attribute would land
    // two role-holders in the same account; storage has no unique
    // index, so CREATE must reject pre-mutation.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE SecondSent (USE (\\Sent))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_inbox_idempotent_probe_with_use_returns_useattr() -> Result<()> {
    // `CREATE INBOX` is RFC 9051's idempotent existence probe. Adding
    // (USE (\Drafts)) would silently no-op because INBOX is
    // bootstrap-managed; return NO [USEATTR] rather than misleading OK.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE INBOX (USE (\\Drafts))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_existing_path_trailing_slash_with_use_returns_useattr() -> Result<()> {
    // Folder1 exists in the fixture with role=None. `CREATE Folder1/
    // (USE (\Drafts))` would idempotently OK while ignoring the
    // requested role; return NO [USEATTR] instead.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Folder1/ (USE (\\Drafts))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 NO [USEATTR]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_mailbox_storage_barrier_rejects_duplicate_role() -> Result<()> {
    // Storage-layer atomicity check: `MailStore::create_mailbox` runs
    // a role-uniqueness probe inside the same `with_set_tx` scope as
    // the create, so two direct calls with the same `special_use`
    // cannot both commit. This is the substrate barrier the IMAP
    // pre-walk snapshot scan defends from the application side; here
    // we exercise the barrier itself, bypassing the IMAP layer.
    let (_tmp, _db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    // First create lands.
    ms.create_mailbox(1, "Junk", None, attrs_for(Some("\\Junk"), true))?;
    // Second create with same role rejects via the in-tx barrier.
    let err = ms
        .create_mailbox(1, "SpamFolder", None, attrs_for(Some("\\Junk"), true))
        .expect_err("duplicate role must reject");
    assert!(
        err.to_string()
            .starts_with("mailbox special-use already in use"),
        "expected role-uniqueness barrier, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn create_with_lowercase_use_keyword_and_flag() -> Result<()> {
    // RFC 9051 §1.2: IMAP keywords are case-insensitive. Real clients
    // send `use (\drafts)`; we must accept those and canonicalise.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Drafts (use (\\drafts))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");

    // LIST should advertise the canonical \Drafts attribute.
    w.write_all(b"a03 LIST \"\" *\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let block = lines.join("\n");
    let drafts_line = lines
        .iter()
        .find(|l| l.contains("\"Drafts\""))
        .unwrap_or_else(|| panic!("no Drafts line in {block}"));
    assert!(
        drafts_line.contains("\\Drafts"),
        "Drafts line missing canonical \\Drafts: {drafts_line}"
    );
    Ok(())
}

#[tokio::test]
async fn create_without_use_is_unchanged() -> Result<()> {
    // Regression guard: the old single-arg parser shape must still work.
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE PlainFolder\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn create_with_non_use_parameter_returns_bad() -> Result<()> {
    let (_tmp, db, ms) = make_db_with_tree("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms.clone(), ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 CREATE Foo (RETURN (\\Drafts))\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    Ok(())
}

/// Parse the `<n>` out of `* OK [UIDVALIDITY <n>] ...` /
/// `* OK [UIDNEXT <n>] ...` lines in a SELECT response block.
/// Anchored to the start-of-line `* OK [<code> ` prefix so a future
/// response-code text that happens to contain the literal `[UIDNEXT…`
/// substring can't fool the parser.
fn parse_resp_code_uint(lines: &[String], code: &str) -> u64 {
    let prefix = format!("* OK [{code} ");
    let line = lines
        .iter()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing {code} OK response code in: {lines:?}"));
    let rest = &line[prefix.len()..];
    let end = rest
        .find(']')
        .unwrap_or_else(|| panic!("unterminated {code} response code: {line}"));
    rest[..end]
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{code} not numeric ({e}): {line}"))
}

/// Phase 2 acceptance gate: UIDVALIDITY is stable across reconnect
/// and UIDNEXT advances monotonically as messages are appended out of
/// band. Two IMAP sessions are spawned against the *same* on-disk Db
/// + mds; between SELECTs we land a single email into INBOX via the
/// MailStore (Phase 4 will replace this with IMAP APPEND, but the
/// invariant we're asserting is about storage-side identity, which
/// the MailStore path exercises just as faithfully).
#[tokio::test]
async fn select_inbox_uidvalidity_stable_uidnext_advances_across_reconnect() -> Result<()> {
    let (_tmp, db, mds, ms) = make_db_with_tree_and_mds("alice@example.com", "hunter2").await?;

    // Session 1: SELECT INBOX, capture UIDVALIDITY + UIDNEXT.
    let (uidvalidity_1, uidnext_1) = {
        let (client, _join) = spawn_session(db.clone(), ms.clone(), ImapConfig::default()).await;
        let (mut reader, mut w) = login(client).await?;
        w.write_all(b"a02 SELECT INBOX\r\n").await?;
        let lines = read_until_tagged(&mut reader, "a02").await?;
        assert!(
            lines.last().unwrap().starts_with("a02 OK [READ-WRITE]"),
            "{lines:?}"
        );
        (
            parse_resp_code_uint(&lines, "UIDVALIDITY"),
            parse_resp_code_uint(&lines, "UIDNEXT"),
        )
    };

    // Between sessions: insert one email into INBOX. The MailStore
    // path bumps `container.next_seq` (the IMAP UIDNEXT source) by
    // exactly 1 and assigns the new membership uid = old next_seq.
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX role missing after fixture seed");
    let hash = mds.put_blob(b"reconnect-fixture-message")?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "reconnect probe".into(),
        date: 1_700_000_000,
        message_id: Some("<reconnect-1@example.com>".into()),
    };
    let (_item, inserted_uid) = ms.create_email(
        1,
        inbox,
        hash,
        envelope,
        &[],
        Flags(0),
        Tags::new(),
        1_700_000_000_000,
    )?;
    // The inserted membership UID is the pre-insert UIDNEXT; the new
    // UIDNEXT must be one greater. Catches the storage-side projection
    // drifting from the wire-side projection.
    assert_eq!(
        inserted_uid.uid, uidnext_1,
        "inserted UID {} did not match pre-insert UIDNEXT {}",
        inserted_uid.uid, uidnext_1
    );

    // Session 2: fresh IMAP session against the same Db + mds.
    // UIDVALIDITY must be byte-identical; UIDNEXT must have advanced
    // by exactly one membership.
    let (uidvalidity_2, uidnext_2) = {
        let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
        let (mut reader, mut w) = login(client).await?;
        w.write_all(b"b02 SELECT INBOX\r\n").await?;
        let lines = read_until_tagged(&mut reader, "b02").await?;
        assert!(
            lines.last().unwrap().starts_with("b02 OK [READ-WRITE]"),
            "{lines:?}"
        );
        (
            parse_resp_code_uint(&lines, "UIDVALIDITY"),
            parse_resp_code_uint(&lines, "UIDNEXT"),
        )
    };

    assert_eq!(
        uidvalidity_1, uidvalidity_2,
        "UIDVALIDITY changed across reconnect ({uidvalidity_1} -> {uidvalidity_2}); \
         a client cache would invalidate even though the mailbox identity is stable"
    );
    assert_eq!(
        uidnext_2,
        uidnext_1 + 1,
        "UIDNEXT did not advance monotonically across reconnect \
         ({uidnext_1} -> {uidnext_2}); one membership was inserted between SELECTs"
    );
    Ok(())
}
