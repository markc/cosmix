//! Regression guards against accidental CONDSTORE / QRESYNC leakage.
//!
//! The v1 capability subset deliberately omits CONDSTORE (RFC 7162)
//! and QRESYNC. Phase 6 will introduce them as a coherent unit;
//! shipping any of their wire shapes *before* `ENABLE CONDSTORE` is
//! advertised would tell a strict client we implicitly support
//! modseq semantics (RFC 7162 §3.1.2.2) — silently broken sync
//! follows. Per Codex's T19 scope decision (skip Phase 6 for the
//! Thunderbird MVP), these tests pin the no-leakage invariants so
//! a later partial CONDSTORE implementation cannot ship in
//! production without breaking this file.
//!
//! Coverage:
//! * CAPABILITY (pre-auth, greeting, post-LOGIN [CAPABILITY] code)
//!   must not advertise CONDSTORE / QRESYNC / ENABLE.
//! * SELECT / EXAMINE response must omit `* OK [HIGHESTMODSEQ n]`.
//! * STATUS must not return an untagged `HIGHESTMODSEQ` value.
//! * LIST-EXTENDED `RETURN (STATUS (HIGHESTMODSEQ))` must not leak
//!   an untagged STATUS line carrying a fabricated modseq.
//! * `SELECT INBOX (CONDSTORE)` / `(QRESYNC ...)` must reject BAD.
//! * `FETCH 1 MODSEQ` (data item) and `UID FETCH 1:* (FLAGS)
//!   (CHANGEDSINCE 0)` (modifier) must reject BAD; no untagged
//!   FETCH MODSEQ may appear.
//! * `UID STORE 1 (UNCHANGEDSINCE 0) +FLAGS (\Seen)` (modifier)
//!   and `UID STORE 1 +FLAGS.MODSEQ (\Seen)` (data-item-suffix)
//!   must reject without applying the flag mutation.
//! * `ENABLE CONDSTORE` must not flip a session flag — the full
//!   tagged exchange must omit `ENABLED`, `CONDSTORE`, `MODSEQ`,
//!   and the subsequent FETCH MODSEQ still rejects.
//! * IDLE must not emit `MODSEQ` on a FlagsChanged event — even
//!   though we drop FlagsChanged from the wire entirely, a future
//!   regression that started emitting `* N FETCH (... MODSEQ ...)`
//!   during IDLE would be a CONDSTORE leak.

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
use tokio::time::{Duration, timeout};

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

fn insert_one_email(ms: &Arc<dyn MailStore>, mds: &Arc<SqliteCasMds>) -> Result<()> {
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let hash = mds.put_blob(b"hello no-condstore")?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "no-condstore-1".into(),
        date: 1_700_000_000,
        message_id: Some("<no-condstore-1@example.com>".into()),
    };
    ms.create_email(
        1,
        inbox,
        hash,
        envelope,
        &[],
        Flags(0),
        Tags::new(),
        1_700_000_000_000,
    )?;
    Ok(())
}

#[tokio::test]
async fn capability_omits_condstore_qresync_enable() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _greeting = read_line(&mut reader).await?;
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a01"),
    )
    .await??;
    let blob = lines.join("\n").to_uppercase();
    for forbidden in ["CONDSTORE", "QRESYNC"] {
        assert!(
            !blob.contains(forbidden),
            "CAPABILITY must not advertise {forbidden} until Phase 6: {blob}"
        );
    }
    // ENABLE is the gating capability for CONDSTORE / QRESYNC opt-in
    // (RFC 5161). Without ENABLE there is no way for a client to
    // turn on those semantics, and advertising it without those
    // extensions would surprise clients that infer support.
    assert!(
        !blob.contains(" ENABLE") && !blob.contains("\tENABLE"),
        "CAPABILITY should not advertise ENABLE in the v1 subset: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn select_response_omits_highestmodseq() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a02"),
    )
    .await??;
    let blob = lines.join("\n").to_uppercase();
    assert!(
        !blob.contains("HIGHESTMODSEQ"),
        "SELECT must not emit HIGHESTMODSEQ outside CONDSTORE: {blob}"
    );
    assert!(
        !blob.contains("MODSEQ"),
        "SELECT must not leak any MODSEQ token outside CONDSTORE: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn examine_response_omits_highestmodseq() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 EXAMINE INBOX\r\n").await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a02"),
    )
    .await??;
    let blob = lines.join("\n").to_uppercase();
    assert!(
        !blob.contains("HIGHESTMODSEQ"),
        "EXAMINE must not emit HIGHESTMODSEQ outside CONDSTORE: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn status_response_omits_highestmodseq_even_when_requested() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // Ask for HIGHESTMODSEQ explicitly. STATUS must either BAD-reject
    // the unsupported item, or silently omit it — but in NO case
    // emit an untagged `* STATUS ... (... HIGHESTMODSEQ <n> ...)`
    // line that would fabricate a modseq value to the client.
    w.write_all(b"a02 STATUS INBOX (MESSAGES HIGHESTMODSEQ)\r\n")
        .await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a02"),
    )
    .await??;
    let tagged_line = lines
        .iter()
        .rfind(|l| l.starts_with("a02 "))
        .expect("tagged response present")
        .to_uppercase();
    // Acceptable shapes: BAD/NO rejection (which may echo the keyword
    // in a human-readable error), or OK with a STATUS line that omits
    // the modseq. Reject the fabricated-modseq shape only.
    let leaks_modseq = lines.iter().any(|line| {
        let up = line.to_uppercase();
        up.starts_with("* STATUS") && up.contains("HIGHESTMODSEQ")
    });
    assert!(
        !leaks_modseq,
        "STATUS must not emit untagged HIGHESTMODSEQ outside CONDSTORE: {lines:?}"
    );
    assert!(
        tagged_line.starts_with("A02 BAD")
            || tagged_line.starts_with("A02 NO")
            || tagged_line.starts_with("A02 OK"),
        "unexpected STATUS tag: {tagged_line}"
    );
    Ok(())
}

#[tokio::test]
async fn select_inbox_with_condstore_param_rejected() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX (CONDSTORE)\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a02 BAD"),
        "SELECT (CONDSTORE) must be rejected BAD: {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn select_inbox_with_qresync_param_rejected() -> Result<()> {
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    // RFC 7162 §3.2.5 QRESYNC parenthesised list — `(1 1)` is a
    // syntactically valid minimal `(uidvalidity modseq)` pair, so
    // a permissive parser could fall through to QRESYNC semantics
    // without realising the capability is unadvertised. The
    // bracketed param block must reject before any QRESYNC code
    // path is exercised.
    w.write_all(b"a02 SELECT INBOX (QRESYNC (1 1))\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a02 BAD"),
        "SELECT (QRESYNC ...) must be rejected BAD: {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn fetch_modseq_attribute_rejected() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 FETCH 1 MODSEQ\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a03 BAD"),
        "FETCH MODSEQ must reject BAD without CONDSTORE: {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn store_unchangedsince_rejected_without_applying_flag() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    // CONDSTORE UNCHANGEDSINCE modifier (RFC 7162 §3.1.3) — the
    // parser does not recognize a parenthesised modifier block, so
    // the BAD comes from "expected FLAGS/+FLAGS/-FLAGS" rather than
    // a CONDSTORE-specific error. Either is acceptable so long as
    // (a) the request is rejected and (b) the \Seen flag is NOT
    // applied to UID 1.
    w.write_all(b"a03 UID STORE 1 (UNCHANGEDSINCE 0) +FLAGS (\\Seen)\r\n")
        .await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a03 BAD") || resp.starts_with("a03 NO"),
        "UID STORE with UNCHANGEDSINCE must be rejected: {resp}"
    );
    // Verify the flag was not applied — re-fetch and check FLAGS.
    w.write_all(b"a04 UID FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    let blob = lines.join("\n");
    assert!(
        !blob.contains("\\Seen"),
        "STORE with UNCHANGEDSINCE must not apply the flag: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn enable_condstore_does_not_unlock_modseq_in_fetch() -> Result<()> {
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 ENABLE CONDSTORE\r\n").await?;
    // ENABLE is not in the v1 subset. Drain the FULL tagged exchange
    // (server is free to emit `* ENABLED ...` before the tag) and
    // assert no CONDSTORE/QRESYNC/MODSEQ token escaped — an untagged
    // `* ENABLED CONDSTORE` would be a wire-level leak even if the
    // tagged line said OK and SELECT/FETCH below stayed inert.
    let enable_lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a02"),
    )
    .await??;
    let enable_blob = enable_lines.join("\n").to_uppercase();
    for forbidden in ["ENABLED", "CONDSTORE", "QRESYNC", "MODSEQ"] {
        assert!(
            !enable_blob.contains(forbidden),
            "ENABLE exchange must not contain {forbidden} in the v1 subset: {enable_blob}"
        );
    }
    w.write_all(b"a03 SELECT INBOX\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a03").await?;
    let select_blob = lines.join("\n").to_uppercase();
    assert!(
        !select_blob.contains("HIGHESTMODSEQ"),
        "SELECT after `ENABLE CONDSTORE` must still omit HIGHESTMODSEQ: {select_blob}"
    );
    w.write_all(b"a04 FETCH 1 MODSEQ\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(
        resp.starts_with("a04 BAD"),
        "FETCH MODSEQ must still reject after `ENABLE CONDSTORE`: {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn greeting_and_login_capability_omit_condstore() -> Result<()> {
    // RFC 9051 §6.2.2 / §6.2.3 — both the greeting `* OK
    // [CAPABILITY ...]` banner and the tagged `<tag> OK
    // [CAPABILITY ...]` post-LOGIN response carry the capability
    // list. A regression that added CONDSTORE/QRESYNC to one but
    // not the other would still leak through whichever path a
    // particular client trusts; assert both.
    let (_tmp, db, _mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?.to_uppercase();
    for forbidden in ["CONDSTORE", "QRESYNC", "MODSEQ"] {
        assert!(
            !greeting.contains(forbidden),
            "greeting CAPABILITY code must omit {forbidden}: {greeting}"
        );
    }
    assert!(
        greeting.contains("[CAPABILITY"),
        "greeting must embed CAPABILITY code (sanity check on harness): {greeting}"
    );
    w.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let login_resp = read_line(&mut reader).await?.to_uppercase();
    assert!(
        login_resp.starts_with("A01 OK"),
        "LOGIN failed: {login_resp}"
    );
    assert!(
        login_resp.contains("[CAPABILITY"),
        "LOGIN OK must embed CAPABILITY code: {login_resp}"
    );
    for forbidden in ["CONDSTORE", "QRESYNC", "MODSEQ"] {
        assert!(
            !login_resp.contains(forbidden),
            "post-LOGIN CAPABILITY code must omit {forbidden}: {login_resp}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn uid_fetch_changedsince_modifier_rejected() -> Result<()> {
    // RFC 7162 §3.1.4 — `CHANGEDSINCE <mod-sequence>` is a FETCH
    // modifier that filters by MODSEQ. The data-item-only test
    // (`fetch_modseq_attribute_rejected`) doesn't cover this path,
    // and a future partial parser that handles the modifier syntax
    // without rejecting it would silently expose modseq semantics.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 UID FETCH 1:* (FLAGS) (CHANGEDSINCE 0)\r\n")
        .await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a03"),
    )
    .await??;
    let blob = lines.join("\n").to_uppercase();
    let leaks_modseq = lines.iter().any(|line| {
        let up = line.to_uppercase();
        up.starts_with("* ") && up.contains("FETCH") && up.contains("MODSEQ")
    });
    assert!(
        !leaks_modseq,
        "UID FETCH (CHANGEDSINCE ...) must not emit untagged FETCH MODSEQ: {lines:?}"
    );
    let tagged_line = lines
        .iter()
        .rfind(|l| l.starts_with("a03 "))
        .expect("tagged response present")
        .to_uppercase();
    assert!(
        tagged_line.starts_with("A03 BAD") || tagged_line.starts_with("A03 NO"),
        "UID FETCH (CHANGEDSINCE ...) must be rejected: {tagged_line} / full: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn uid_store_flags_modseq_modifier_rejected() -> Result<()> {
    // RFC 7162 §3.1.2.2 — clients that have ENABLEd CONDSTORE may
    // attach `.MODSEQ` to flag-attribute names (e.g.
    // `+FLAGS.MODSEQ`). Outside CONDSTORE this must reject without
    // either applying the flag OR emitting a FETCH echo carrying
    // a fabricated modseq. The `(UNCHANGEDSINCE ...)` test covers
    // the modifier path; this covers the data-item-suffix path.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 UID STORE 1 +FLAGS.MODSEQ (\\Seen)\r\n")
        .await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a03"),
    )
    .await??;
    let leaks_modseq = lines.iter().any(|line| {
        let up = line.to_uppercase();
        up.starts_with("* ") && up.contains("FETCH") && up.contains("MODSEQ")
    });
    assert!(
        !leaks_modseq,
        "UID STORE +FLAGS.MODSEQ must not emit FETCH MODSEQ: {lines:?}"
    );
    let tagged_line = lines
        .iter()
        .rfind(|l| l.starts_with("a03 "))
        .expect("tagged response present")
        .to_uppercase();
    assert!(
        tagged_line.starts_with("A03 BAD") || tagged_line.starts_with("A03 NO"),
        "UID STORE +FLAGS.MODSEQ must reject: {tagged_line}"
    );
    // Verify the flag was not applied — re-fetch and check FLAGS.
    w.write_all(b"a04 UID FETCH 1 FLAGS\r\n").await?;
    let lines = read_until_tagged(&mut reader, "a04").await?;
    let blob = lines.join("\n");
    assert!(
        !blob.contains("\\Seen"),
        "STORE +FLAGS.MODSEQ must not apply the flag: {blob}"
    );
    Ok(())
}

#[tokio::test]
async fn list_extended_return_status_highestmodseq_does_not_leak() -> Result<()> {
    // RFC 5819 + RFC 9051 §6.3.9 — LIST-EXTENDED `RETURN (STATUS
    // (...))` allows STATUS items to be merged with LIST output.
    // Even if the LIST-EXTENDED parser path eventually accepts the
    // syntax, it must not synthesise a HIGHESTMODSEQ value (the
    // implementation has no modseq counter to expose). Either
    // BAD-reject or emit a STATUS line that omits modseq — but
    // never both succeed AND emit a fabricated value.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let (client, _join) = spawn_session(db, ms, ImapConfig::default()).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 LIST \"\" INBOX RETURN (STATUS (MESSAGES HIGHESTMODSEQ))\r\n")
        .await?;
    let lines = timeout(
        Duration::from_secs(2),
        read_until_tagged(&mut reader, "a02"),
    )
    .await??;
    let leaks_modseq = lines.iter().any(|line| {
        let up = line.to_uppercase();
        up.starts_with("* STATUS") && up.contains("HIGHESTMODSEQ")
    });
    assert!(
        !leaks_modseq,
        "LIST RETURN (STATUS (HIGHESTMODSEQ)) must not emit untagged STATUS HIGHESTMODSEQ: {lines:?}"
    );
    Ok(())
}

#[tokio::test]
async fn idle_flag_change_does_not_emit_modseq() -> Result<()> {
    // FlagsChanged events on the substrate notifier currently turn
    // into wire silence — the IDLE handler intentionally drops them
    // because the v1 subset has no `* N FETCH (FLAGS (...))`
    // emission path. A regression that started emitting untagged
    // FETCH-on-flag-change would be a step toward CONDSTORE
    // semantics; this test pins the no-MODSEQ-leak shape.
    //
    // Strategy: enter IDLE, mutate flags (silently consumed), then
    // insert a second message (emits `* 2 EXISTS`). Reaching the
    // EXISTS line proves the FlagsChanged event was processed in
    // order before it; assert nothing carrying MODSEQ slipped out
    // between `+ idling` and the EXISTS.
    let (_tmp, db, mds, ms) = make_fixture("alice@example.com", "hunter2").await?;
    insert_one_email(&ms, &mds)?;
    let cfg = ImapConfig {
        idle_status_interval: Duration::from_secs(60),
        ..ImapConfig::default()
    };
    let (client, _join) = spawn_session(db, ms.clone(), cfg).await;
    let (mut reader, mut w) = login(client).await?;
    w.write_all(b"a02 SELECT INBOX\r\n").await?;
    let _ = read_until_tagged(&mut reader, "a02").await?;
    w.write_all(b"a03 IDLE\r\n").await?;
    let cont = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert_eq!(cont, "+ idling");

    // Mutate flags on the existing UID 1 — fires FlagsChanged on
    // INBOX's notifier channel.
    let inbox = ms
        .mailbox_by_role(1, MailboxRole::Inbox)?
        .expect("INBOX missing");
    let items = ms.list_emails_in_mailbox(
        1,
        inbox,
        cosmix_maild::mailstore::ListOpts {
            sort_by: cosmix_maild::mailstore::SortKey::Seq,
            limit: u32::MAX,
            offset: 0,
        },
    )?;
    let id1 = items[0].id;
    ms.add_flags_in_mailbox(1, id1, inbox, Flags(0x01))?; // \Seen

    // Drive an EXISTS by inserting a second email. The broadcast
    // channel preserves order, so by the time `* 2 EXISTS` reaches
    // the client, the FlagsChanged event has already been processed.
    let hash = mds.put_blob(b"hello-condstore-2")?;
    let envelope = EmailEnvelope {
        from: "ext@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "no-condstore-2".into(),
        date: 1_700_000_000,
        message_id: Some("<no-condstore-2@example.com>".into()),
    };
    ms.create_email(
        1,
        inbox,
        hash,
        envelope,
        &[],
        Flags(0),
        Tags::new(),
        1_700_000_001_000,
    )?;

    // Drain lines until the EXISTS marker. Anything we see before
    // that must not carry MODSEQ — and the EXISTS line itself must
    // be a bare `* N EXISTS`, not `* N EXISTS [HIGHESTMODSEQ ...]`.
    let mut idle_lines: Vec<String> = Vec::new();
    loop {
        let line = timeout(Duration::from_secs(5), read_line(&mut reader)).await??;
        idle_lines.push(line.clone());
        if line == "* 2 EXISTS" {
            break;
        }
        // Safety net — abort the loop if a `* 1 EXISTS` quirk
        // appears (shouldn't, but keeps the timeout cheap).
        if line.ends_with(" EXISTS") {
            break;
        }
    }
    for line in &idle_lines {
        let up = line.to_uppercase();
        assert!(
            !up.contains("MODSEQ"),
            "IDLE wire must not carry MODSEQ: {line} / full: {idle_lines:?}"
        );
        assert!(
            !up.contains("HIGHESTMODSEQ"),
            "IDLE wire must not carry HIGHESTMODSEQ: {line}"
        );
    }

    // Clean DONE.
    w.write_all(b"DONE\r\n").await?;
    let resp = timeout(Duration::from_secs(2), read_line(&mut reader)).await??;
    assert!(resp.starts_with("a03 OK"), "expected tagged OK, got {resp}");
    Ok(())
}
