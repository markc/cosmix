//! End-to-end Phase 1 IMAP tests against an in-process session.
//!
//! Bypasses TLS by feeding `handle_connection` a `tokio::io::duplex`
//! — Phase 1's job is the IMAP protocol layer, not the rustls
//! pipeline (the SMTP TLS pipeline already covers that codepath and
//! `imap::server::start` reuses it byte-for-byte). The substrate
//! account-creation path is also bypassed; we insert directly into
//! the `accounts` table with a bcrypt hash, so the test exercises
//! the same `crate::auth::basic::verify` path the real LOGIN takes
//! without dragging the property runtime into the harness.

use std::sync::Arc;

use anyhow::Result;
use cosmix_maild::db::Db;
use cosmix_maild::imap::config::ImapConfig;
use cosmix_maild::imap::session::{AccountSlots, handle_connection};
use cosmix_maild::mailstore::{MailStore, SqliteMailStore};
use rusqlite::params;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Build a per-test sqlite Db + a fresh MailStore over an mds tempdir.
/// The MailStore is wired through to `handle_connection` so Phase 2
/// verbs that need it (LIST / LSUB / NAMESPACE) can run; Phase 1
/// tests do not exercise it.
async fn make_db_with_account(
    email: &str,
    password: &str,
) -> Result<(TempDir, Db, Arc<dyn MailStore>)> {
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
    let mds = Arc::new(cosmix_mds::SqliteCasMds::open(mds_dir)?);
    let mailstore: Arc<dyn MailStore> = Arc::new(SqliteMailStore::new(mds));
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

/// Read a single CRLF-terminated line. Strips the trailing `\r\n`.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<String> {
    let mut s = String::new();
    let n = reader.read_line(&mut s).await?;
    if n == 0 {
        return Err(anyhow::anyhow!("peer closed"));
    }
    if let Some(stripped) = s.strip_suffix("\r\n") {
        Ok(stripped.to_string())
    } else if let Some(stripped) = s.strip_suffix('\n') {
        Ok(stripped.to_string())
    } else {
        Ok(s)
    }
}

#[tokio::test]
async fn greeting_carries_capability_response_code() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, _w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.starts_with("* OK [CAPABILITY "), "{greeting}");
    assert!(greeting.contains("IMAP4rev2"));
    assert!(greeting.contains("AUTH=PLAIN"));
    assert!(greeting.contains("ID"));
    Ok(())
}

#[tokio::test]
async fn capability_command_lists_initial_set() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* CAPABILITY "), "{untagged}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a01 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn noop_returns_ok() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 NOOP\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn id_returns_server_block() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 ID NIL\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.starts_with("* ID "), "{untagged}");
    assert!(untagged.contains("cosmix-maild"));
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a01 OK"), "{tagged}");
    Ok(())
}

#[tokio::test]
async fn login_success_then_logout() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK [CAPABILITY "), "{resp}");
    assert!(resp.contains("authenticated"));
    w.write_all(b"a02 LOGOUT\r\n").await?;
    let bye = read_line(&mut reader).await?;
    assert!(bye.starts_with("* BYE "), "{bye}");
    let tagged = read_line(&mut reader).await?;
    assert!(tagged.starts_with("a02 OK"), "{tagged}");
    // Session task should exit cleanly.
    join.await??;
    Ok(())
}

#[tokio::test]
async fn login_failure_returns_authenticationfailed() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 LOGIN alice@example.com wrong\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 NO [AUTHENTICATIONFAILED]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn authenticate_plain_with_sasl_ir_succeeds() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    let raw = b"\x00alice@example.com\x00hunter2";
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
    let cmd = format!("a01 AUTHENTICATE PLAIN {b64}\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK [CAPABILITY "), "{resp}");
    Ok(())
}

#[tokio::test]
async fn authenticate_plain_continuation_path_succeeds() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 AUTHENTICATE PLAIN\r\n").await?;
    let cont = read_line(&mut reader).await?;
    assert!(cont.starts_with("+ "), "{cont}");
    let raw = b"\x00alice@example.com\x00hunter2";
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
    w.write_all(format!("{b64}\r\n").as_bytes()).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK [CAPABILITY "), "{resp}");
    Ok(())
}

#[tokio::test]
async fn authenticate_login_with_sasl_ir_skips_username_challenge() -> Result<()> {
    // Regression test for the Codex MAJOR finding: AUTHENTICATE LOGIN
    // with a SASL-IR initial response was previously re-prompting for
    // the username, so a compliant client would send the password
    // bytes when the server expected base64(username). After the fix
    // the SASL-IR stands in for the first continuation and the server
    // proceeds directly to the password challenge.
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    let user_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"alice@example.com",
    );
    let cmd = format!("a01 AUTHENTICATE LOGIN {user_b64}\r\n");
    w.write_all(cmd.as_bytes()).await?;
    // Server should reply directly with the "Password:" challenge —
    // NOT a "Username:" challenge and NOT a NO line.
    let cont = read_line(&mut reader).await?;
    assert!(cont.starts_with("+ "), "{cont}");
    let challenge_b64 = cont.trim_start_matches("+ ").trim();
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, challenge_b64)?;
    assert_eq!(decoded, b"Password:", "unexpected challenge {decoded:?}");
    let pass_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hunter2");
    w.write_all(format!("{pass_b64}\r\n").as_bytes()).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK [CAPABILITY "), "{resp}");
    Ok(())
}

#[tokio::test]
async fn capability_advertises_literal_plus() -> Result<()> {
    // Phase 4 T14 — `read_command` honours RFC 7888 `LITERAL+`
    // non-synchronizing literals end-to-end, so the codec can
    // advertise the capability. Both the greeting's response code
    // and the untagged `* CAPABILITY` reply carry it.
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.contains("LITERAL+"), "{greeting}");
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("LITERAL+"), "{untagged}");
    Ok(())
}

#[tokio::test]
async fn capability_override_keeps_literal_plus_filters_unsafe_caps() -> Result<()> {
    // Operator-override path: after T14, `LITERAL+` is legitimate
    // and survives the filter. Caps that the codec still cannot
    // honour (`LITERAL-` — RFC 7888 size-capped sibling, not
    // implemented) plus smuggled multi-token entries (`FOO LITERAL+`)
    // are still dropped on both wire surfaces.
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        advertise_capabilities: Some(vec![
            "IMAP4rev2".to_string(),
            "LITERAL+".to_string(),
            "AUTH=PLAIN".to_string(),
            " literal- ".to_string(),
            "FOO LITERAL+".to_string(),
        ]),
        ..ImapConfig::default()
    };
    let (client, _join) = spawn_session(db, mailstore, cfg).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let greeting = read_line(&mut reader).await?;
    assert!(greeting.contains("LITERAL+"), "{greeting}");
    assert!(
        !greeting.to_ascii_uppercase().contains("LITERAL-"),
        "{greeting}"
    );
    assert!(!greeting.contains("FOO"), "{greeting}");
    assert!(greeting.contains("IMAP4rev2"), "{greeting}");
    assert!(greeting.contains("AUTH=PLAIN"), "{greeting}");
    w.write_all(b"a01 CAPABILITY\r\n").await?;
    let untagged = read_line(&mut reader).await?;
    assert!(untagged.contains("LITERAL+"), "{untagged}");
    assert!(
        !untagged.to_ascii_uppercase().contains("LITERAL-"),
        "{untagged}"
    );
    assert!(!untagged.contains("FOO"), "{untagged}");
    assert!(untagged.contains("IMAP4rev2"), "{untagged}");
    assert!(untagged.contains("AUTH=PLAIN"), "{untagged}");
    Ok(())
}

#[tokio::test]
async fn authenticate_login_two_step_succeeds() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 AUTHENTICATE LOGIN\r\n").await?;
    let cont1 = read_line(&mut reader).await?;
    assert!(cont1.starts_with("+ "));
    let user_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"alice@example.com",
    );
    w.write_all(format!("{user_b64}\r\n").as_bytes()).await?;
    let cont2 = read_line(&mut reader).await?;
    assert!(cont2.starts_with("+ "));
    let pass_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hunter2");
    w.write_all(format!("{pass_b64}\r\n").as_bytes()).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 OK [CAPABILITY "), "{resp}");
    Ok(())
}

#[tokio::test]
async fn authenticate_plain_bad_creds_returns_no() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    let raw = b"\x00alice@example.com\x00wrong";
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
    let cmd = format!("a01 AUTHENTICATE PLAIN {b64}\r\n");
    w.write_all(cmd.as_bytes()).await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 NO [AUTHENTICATIONFAILED]"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn authenticate_oauthbearer_stub_returns_no() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    // Send a non-empty SASL-IR so we skip the empty-challenge round-trip.
    w.write_all(b"a01 AUTHENTICATE OAUTHBEARER ignored\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 NO [AUTHENTICATIONFAILED]"), "{resp}");
    assert!(resp.contains("OAUTHBEARER"));
    Ok(())
}

#[tokio::test]
async fn starttls_command_is_rejected() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 STARTTLS\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn login_in_authenticated_state_is_bad() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let _ok = read_line(&mut reader).await?;
    w.write_all(b"a02 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a02 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn unimplemented_verb_returns_bad() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let (client, _join) = spawn_session(db, mailstore, ImapConfig::default()).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 SELECT INBOX\r\n").await?;
    let resp = read_line(&mut reader).await?;
    assert!(resp.starts_with("a01 BAD"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn bad_command_cap_pre_auth_closes_connection() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        max_bad_commands_pre_auth: 2,
        ..ImapConfig::default()
    };
    let (client, join) = spawn_session(db, mailstore, cfg).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    // Two bad commands → second triggers the BYE [CLIENTBUG].
    w.write_all(b"a01 FROBNICATE\r\n").await?;
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a02 FROBNICATE\r\n").await?;
    // Second response then BYE then close.
    let _bad = read_line(&mut reader).await?;
    let bye = read_line(&mut reader).await?;
    assert!(bye.contains("BYE"), "{bye}");
    join.await??;
    Ok(())
}

#[tokio::test]
async fn auth_failure_cap_closes_connection() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let cfg = ImapConfig {
        max_auth_failures: 2,
        ..ImapConfig::default()
    };
    let (client, join) = spawn_session(db, mailstore, cfg).await;
    let (r, mut w) = tokio::io::split(client);
    let mut reader = BufReader::new(r);
    let _ = read_line(&mut reader).await?;
    w.write_all(b"a01 LOGIN alice@example.com wrong\r\n")
        .await?;
    let _no1 = read_line(&mut reader).await?;
    w.write_all(b"a02 LOGIN alice@example.com wrong\r\n")
        .await?;
    let _no2 = read_line(&mut reader).await?;
    let bye = read_line(&mut reader).await?;
    assert!(bye.contains("BYE"), "{bye}");
    join.await??;
    Ok(())
}

#[tokio::test]
async fn per_account_concurrency_cap_rejects_extras() -> Result<()> {
    let (_tmp, db, mailstore) = make_db_with_account("alice@example.com", "hunter2").await?;
    let cfg = Arc::new(ImapConfig {
        max_concurrent_per_account: 1,
        ..ImapConfig::default()
    });
    let slots = AccountSlots::new();

    // First session — log in and hold open.
    let (c1, s1) = tokio::io::duplex(64 * 1024);
    let db1 = db.clone();
    let cfg1 = cfg.clone();
    let slots1 = slots.clone();
    let ms1 = mailstore.clone();
    let peer1 = "127.0.0.1:0".parse().unwrap();
    let join1 = tokio::spawn(async move {
        handle_connection(
            s1,
            peer1,
            None,
            cfg1,
            db1,
            ms1,
            slots1,
            "localhost".to_string(),
        )
        .await
    });
    let (r1, mut w1) = tokio::io::split(c1);
    let mut reader1 = BufReader::new(r1);
    let _ = read_line(&mut reader1).await?;
    w1.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let resp = read_line(&mut reader1).await?;
    assert!(resp.starts_with("a01 OK"), "{resp}");

    // Second session — second LOGIN should hit the INUSE cap.
    let (c2, s2) = tokio::io::duplex(64 * 1024);
    let db2 = db.clone();
    let cfg2 = cfg.clone();
    let slots2 = slots.clone();
    let ms2 = mailstore.clone();
    let peer2 = "127.0.0.1:0".parse().unwrap();
    let join2 = tokio::spawn(async move {
        handle_connection(
            s2,
            peer2,
            None,
            cfg2,
            db2,
            ms2,
            slots2,
            "localhost".to_string(),
        )
        .await
    });
    let (r2, mut w2) = tokio::io::split(c2);
    let mut reader2 = BufReader::new(r2);
    let _ = read_line(&mut reader2).await?;
    w2.write_all(b"a01 LOGIN alice@example.com hunter2\r\n")
        .await?;
    let resp2 = read_line(&mut reader2).await?;
    assert!(resp2.starts_with("a01 NO [INUSE]"), "{resp2}");
    let bye = read_line(&mut reader2).await?;
    assert!(bye.contains("BYE"), "{bye}");
    join2.await??;

    // Tear down session 1 so per-account count returns to zero.
    drop(w1);
    drop(reader1);
    let _ = join1.await?;
    Ok(())
}
