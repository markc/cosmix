//! T20 — Thunderbird MVP end-to-end smoke.
//!
//! Stands up the *real* maild binary surface end-to-end:
//!
//! 1. **Plain SMTP inbound** on 127.0.0.1:0 (port-25 equivalent) —
//!    drives a `MAIL FROM`/`RCPT TO`/`DATA` from an "external" peer
//!    so the message lands in the test account's INBOX via the
//!    production `inbound::deliver` path (MIME-parsed, with
//!    real ENVELOPE / BODYSTRUCTURE / blob_hash).
//! 2. **IMAPS** on 127.0.0.1:0 (port-993 equivalent, implicit TLS)
//!    — connects with a `tokio-rustls` client (verifier disabled
//!    because the server cert is a self-signed rcgen blob; this is
//!    a server-side test, not a CA-trust test) and walks the
//!    Thunderbird boot sequence: CAPABILITY → LOGIN → NAMESPACE →
//!    LIST → SELECT INBOX → UID SEARCH ALL → UID FETCH (UID FLAGS
//!    ENVELOPE BODYSTRUCTURE) → UID FETCH BODY.PEEK[] (split from
//!    the ENVELOPE fetch so subject / from assertions cannot be
//!    accidentally satisfied by the literal BODY payload) → FLAGS
//!    refetch → UID STORE +FLAGS (\Seen) → IDLE round-trip →
//!    LOGOUT.
//! 3. **SMTPS submission** on 127.0.0.1:0 (port-465 equivalent,
//!    implicit TLS) — same TLS client, drives EHLO → AUTH PLAIN →
//!    MAIL FROM (=auth identity) → RCPT TO (external) → DATA. The
//!    `inbound::deliver` path then enqueues the bytes in
//!    `smtp_queue` for the outbound delivery worker to relay on
//!    port 25 (the test does **not** carry the relay through to a
//!    fake-MX peer — that's a separate harness; asserting the
//!    queue row is the right terminal check for "submission
//!    landed").
//!
//! The substrate uses real `with_set_tx` boundaries, so the steady
//! state of every assertion in this file is "committed durably";
//! no manual sync/sleep. The poll loops that do exist (e.g. waiting
//! for an inbound message to show up via IMAP UID SEARCH) document
//! their SLA: SMTP 250 → UID SEARCH visibility within 2 s.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use base64::Engine;
use cosmix_maild::config::{Config, ListenSpec};
use cosmix_maild::runtime::{BuiltMaild, RuntimeOpts, build_runtime};
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::SetOpts;
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;
use rcgen::CertifiedKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf, split,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

const ACCOUNT_EMAIL: &str = "alice@e2e.test";
const ACCOUNT_PASSWORD: &str = "hunter2-tbird";

// Outbound recipient — a different domain than `hostname`/account
// so the relay-policy gate routes it onto the outbound queue
// instead of treating it as a local delivery.
const OUTBOUND_RCPT: &str = "external@remote.example";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imap_smtp_thunderbird_e2e_smoke() -> Result<()> {
    install_ring_provider();

    let tmp = TempDir::new()?;
    let (cert_path, key_path) = make_self_signed(tmp.path())?;

    let cfg = make_config(tmp.path(), &cert_path, &key_path);
    // Suppress the outbound delivery worker — `OUTBOUND_RCPT` lives
    // in the RFC-2606 `.example` namespace, so the production worker
    // would otherwise spin on MX lookups for `remote.example` until
    // the test process exits.
    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            disable_outbound_delivery: true,
            ..Default::default()
        },
    )
    .await?;

    let smtp_inbound_addr = built
        .smtp_handle
        .inbound_addrs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("smtp inbound did not bind"))?;
    let smtps_addr = built
        .smtp_handle
        .smtps_addrs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("smtps submission did not bind"))?;
    let imaps_addr = built
        .imap_handle
        .imaps_addrs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("imaps did not bind"))?;

    // Substrate path — the production `after_set` hook on
    // `maild.accounts` seeds the default mailbox layout (INBOX,
    // Sent, Trash, Spam, Drafts), so the IMAP session below sees a
    // fully-formed account.
    create_account(&built, ACCOUNT_EMAIL, ACCOUNT_PASSWORD).await?;

    let test_subject = "T20 Thunderbird e2e";
    let inbound_from = "sender@external.test";
    let inbound_body = "hello from the T20 smoke test\r\nsecond line of body\r\n";
    smtp_inbound_deliver(
        smtp_inbound_addr,
        inbound_from,
        ACCOUNT_EMAIL,
        test_subject,
        inbound_body,
    )
    .await?;

    let tls = test_tls_connector()?;

    // ----- IMAPS: the Thunderbird "fetch inbox" half -----
    imap_smoke(
        &tls,
        imaps_addr,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        test_subject,
        inbound_from,
    )
    .await?;

    // ----- SMTPS submission: the Thunderbird "send mail" half -----
    let out_subject = "T20 outbound submission";
    let out_body = "outbound body from authenticated submission\r\n";
    smtps_submit(
        &tls,
        smtps_addr,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        OUTBOUND_RCPT,
        out_subject,
        out_body,
    )
    .await?;

    // Negative path — RFC 6409 §6.1. An authenticated submission that
    // sets MAIL FROM to a different identity must be rejected with
    // 530. This was the gamma checklist §6 defect that landed in
    // production; the asserting test pins the fix.
    smtps_mailfrom_mismatch_rejected(
        &tls,
        smtps_addr,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "impostor@somewhere-else.test",
    )
    .await?;

    // Queue-row check — the submission `DATA` 250 means the
    // `inbound::deliver` path returned Ok, which means the queue
    // INSERT committed. Asserting the row exists is corroborating
    // evidence that the submission terminated in the relay queue
    // and not (for example) a half-completed local delivery.
    let entries = cosmix_maild::smtp::queue::list(&built.app_state.db.conn, 10).await?;
    let queued = entries
        .iter()
        .find(|e| e.from_addr == ACCOUNT_EMAIL && e.to_addrs.iter().any(|t| t == OUTBOUND_RCPT))
        .ok_or_else(|| {
            anyhow!("authenticated submission must enqueue an smtp_queue row; got {entries:?}")
        })?;

    // Queue-row content check (Codex T20 round-1 MINOR): the row must
    // reference a CAS blob, and the blob bytes must carry the Subject
    // and Message-ID the submission DATA flushed. This protects
    // against a future regression where the queue INSERT succeeds but
    // the CAS payload is empty / mis-routed.
    let blob_hash = queued
        .blob_hash
        .ok_or_else(|| anyhow!("queued row missing blob_hash: {queued:?}"))?;
    let mds = built.app_state.mailstore.mds().clone();
    let blob_bytes = tokio::task::spawn_blocking(move || {
        use cosmix_mds::store::Mds;
        mds.get_blob(&blob_hash)
    })
    .await??;
    let blob_text = String::from_utf8_lossy(&blob_bytes);
    assert!(
        blob_text.contains(out_subject),
        "queued blob must carry submission Subject {out_subject:?}: {blob_text}"
    );
    assert!(
        blob_text.contains("Message-ID: <t20-outbound-"),
        "queued blob must carry the submission Message-ID header: {blob_text}"
    );

    Ok(())
}

// ---------- TLS plumbing ----------

fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn make_self_signed(dir: &std::path::Path) -> Result<(String, String)> {
    let CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    let cert_path = dir.join("test-cert.pem");
    let key_path = dir.join("test-key.pem");
    std::fs::write(&cert_path, cert_pem)?;
    std::fs::write(&key_path, key_pem)?;
    Ok((
        cert_path.to_string_lossy().into_owned(),
        key_path.to_string_lossy().into_owned(),
    ))
}

#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

fn test_tls_connector() -> Result<TlsConnector> {
    let cc = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(cc)))
}

async fn tls_open(
    connector: &TlsConnector,
    addr: std::net::SocketAddr,
) -> Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost")
        .map_err(|e| anyhow!("ServerName::try_from(localhost): {e:?}"))?
        .to_owned();
    let stream = connector.connect(name, tcp).await?;
    Ok(stream)
}

// ---------- Account creation through the substrate ----------

async fn create_account(built: &BuiltMaild, email: &str, password: &str) -> Result<()> {
    let runtime = built.accounts_runtime();
    let hash = bcrypt::hash(password, 4).map_err(|e| anyhow!("bcrypt: {e}"))?;

    let mut m = BTreeMap::new();
    m.insert("email".into(), PropValue::String(email.into()));
    m.insert("password".into(), PropValue::String(hash));
    m.insert("name".into(), PropValue::String("T20 Test".into()));
    m.insert("quota".into(), PropValue::Int(0));
    m.insert("spam_enabled".into(), PropValue::Bool(false));
    m.insert("spam_threshold".into(), PropValue::Float(0.5));
    let value = PropValue::Object(m);

    let key = RecordKey::collection(cosmix_maild::props::accounts::namespace_name(), email);
    let opts = SetOpts {
        expected_version: Some(Version::zero()),
        merge: MergeMode::Replace,
        actor: Actor::operator("t20-test"),
        cause: Some("create test account".into()),
        ts_ms: 0,
    };
    runtime
        .set(key, value, opts)
        .await
        .map_err(|e| anyhow!("create_account: {e}"))?;
    Ok(())
}

fn make_config(root: &std::path::Path, cert_path: &str, key_path: &str) -> Config {
    let database_path = root.join("mail.db").to_string_lossy().into_owned();
    let blob_dir = root.join("blobs").to_string_lossy().into_owned();
    let mds_dir = root.join("mds").to_string_lossy().into_owned();
    let spam_db_dir = root.join("spam").to_string_lossy().into_owned();
    let rule_stats_dir = root.join("rules").to_string_lossy().into_owned();
    std::fs::create_dir_all(&blob_dir).expect("blob dir");
    std::fs::create_dir_all(&mds_dir).expect("mds dir");
    std::fs::create_dir_all(&spam_db_dir).expect("spam dir");

    Config {
        listen: "127.0.0.1:0".into(),
        base_url: "http://127.0.0.1".into(),
        database_path,
        blob_dir,
        mds_dir,
        hostname: "e2e.test".into(),
        smtp_inbound: Some(ListenSpec::Single("127.0.0.1:0".into())),
        require_starttls_inbound: Vec::new(),
        smtp_smtps: Some(ListenSpec::Single("127.0.0.1:0".into())),
        smtp_outbound_bind: Vec::new(),
        max_message_size: None,
        dkim_selector: None,
        dkim_private_key: None,
        dkim: Default::default(),
        tls_cert: Some(cert_path.to_string()),
        tls_key: Some(key_path.to_string()),
        tls: Default::default(),
        retention_operators: Vec::new(),
        bayesian_rebuild_operators: Vec::new(),
        vtoken_operators: Vec::new(),
        vtoken_delegated_peers: Vec::new(),
        tls_key_root: None,
        imap_imaps: Some(ListenSpec::Single("127.0.0.1:0".into())),
        imap_max_literal_bytes: None,
        imap_idle_status_interval_secs: Some(60),
        imap_pre_auth_timeout_secs: None,
        imap_max_auth_failures: None,
        imap_max_bad_commands_pre_auth: None,
        imap_max_bad_commands_post_auth: None,
        imap_max_concurrent_per_account: None,
        imap_advertise_capabilities: None,
        inbound_filter: None,
        spam_enabled: Some(false),
        spam_db_dir: Some(spam_db_dir),
        spam_baseline_db: None,
        spam_base_rate_prior: None,
        spam_base_rate_pseudocount: None,
        spam_base_rate_min: None,
        spam_base_rate_max: None,
        rules_pack_path: None,
        rule_stats_flush_interval_secs: None,
        rule_stats_dir: Some(rule_stats_dir),
    }
}

// ---------- Plain SMTP inbound (port-25 equivalent) ----------

async fn smtp_inbound_deliver(
    addr: std::net::SocketAddr,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    expect_smtp_code(&mut rd, b"220").await?;
    wr.write_all(b"EHLO sender.external.test\r\n").await?;
    consume_smtp_multiline(&mut rd, b"250").await?;
    wr.write_all(format!("MAIL FROM:<{from}>\r\n").as_bytes())
        .await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(format!("RCPT TO:<{to}>\r\n").as_bytes())
        .await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(b"DATA\r\n").await?;
    expect_smtp_code(&mut rd, b"354").await?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let payload = format!(
        "From: <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\n\
         Message-ID: <t20-inbound-{ts}@external.test>\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 8bit\r\n\r\n\
         {body}\r\n.\r\n",
    );
    wr.write_all(payload.as_bytes()).await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(b"QUIT\r\n").await?;
    let _ = expect_smtp_code(&mut rd, b"221").await;
    Ok(())
}

// ---------- SMTPS submission (port-465 equivalent) ----------

async fn smtps_submit(
    connector: &TlsConnector,
    addr: std::net::SocketAddr,
    auth_user: &str,
    auth_pass: &str,
    rcpt_to: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let stream = tls_open(connector, addr).await?;
    let (rd, mut wr) = split(stream);
    let mut rd = BufReader::new(rd);

    expect_smtp_code_async(&mut rd, b"220").await?;
    wr.write_all(b"EHLO thunderbird.client.test\r\n").await?;
    let banner = read_smtp_multiline(&mut rd, b"250").await?;
    assert!(
        banner.iter().any(|l| l.contains("AUTH")),
        "submission EHLO must advertise AUTH after TLS: {banner:?}"
    );

    // AUTH PLAIN: \0user\0password, base64-encoded.
    let mut raw = Vec::new();
    raw.push(0);
    raw.extend_from_slice(auth_user.as_bytes());
    raw.push(0);
    raw.extend_from_slice(auth_pass.as_bytes());
    let token = base64::engine::general_purpose::STANDARD.encode(&raw);
    wr.write_all(format!("AUTH PLAIN {token}\r\n").as_bytes())
        .await?;
    expect_smtp_code_async(&mut rd, b"235").await?;

    // The envelope sender MUST match the authenticated identity —
    // production code enforces RFC 6409 §6.1 and 530s the mismatch.
    wr.write_all(format!("MAIL FROM:<{auth_user}>\r\n").as_bytes())
        .await?;
    expect_smtp_code_async(&mut rd, b"250").await?;
    wr.write_all(format!("RCPT TO:<{rcpt_to}>\r\n").as_bytes())
        .await?;
    expect_smtp_code_async(&mut rd, b"250").await?;
    wr.write_all(b"DATA\r\n").await?;
    expect_smtp_code_async(&mut rd, b"354").await?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let payload = format!(
        "From: <{auth_user}>\r\nTo: <{rcpt_to}>\r\nSubject: {subject}\r\n\
         Message-ID: <t20-outbound-{ts}@e2e.test>\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 8bit\r\n\r\n\
         {body}\r\n.\r\n",
    );
    wr.write_all(payload.as_bytes()).await?;
    expect_smtp_code_async(&mut rd, b"250").await?;
    wr.write_all(b"QUIT\r\n").await?;
    let _ = expect_smtp_code_async(&mut rd, b"221").await;
    Ok(())
}

/// Open an SMTPS submission session, AUTH, send a MAIL FROM whose
/// address does NOT match the authenticated identity, and assert the
/// server rejects with `530`. Asserts RFC 6409 §6.1 — the submission
/// identity binding the production checklist verifies on every
/// release.
async fn smtps_mailfrom_mismatch_rejected(
    connector: &TlsConnector,
    addr: std::net::SocketAddr,
    auth_user: &str,
    auth_pass: &str,
    impersonated_from: &str,
) -> Result<()> {
    let stream = tls_open(connector, addr).await?;
    let (rd, mut wr) = split(stream);
    let mut rd = BufReader::new(rd);

    expect_smtp_code_async(&mut rd, b"220").await?;
    wr.write_all(b"EHLO thunderbird.client.test\r\n").await?;
    let _ = read_smtp_multiline(&mut rd, b"250").await?;

    let mut raw = Vec::new();
    raw.push(0);
    raw.extend_from_slice(auth_user.as_bytes());
    raw.push(0);
    raw.extend_from_slice(auth_pass.as_bytes());
    let token = base64::engine::general_purpose::STANDARD.encode(&raw);
    wr.write_all(format!("AUTH PLAIN {token}\r\n").as_bytes())
        .await?;
    expect_smtp_code_async(&mut rd, b"235").await?;

    wr.write_all(format!("MAIL FROM:<{impersonated_from}>\r\n").as_bytes())
        .await?;
    let mut line = String::new();
    timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("read_line timed out waiting for MAIL FROM 530"))??;
    assert!(
        line.starts_with("530 5.7.1"),
        "submission must 530 5.7.1 when MAIL FROM != authed identity (got: {line:?})"
    );
    let _ = wr.write_all(b"QUIT\r\n").await;
    Ok(())
}

// ---------- IMAPS protocol body ----------

async fn imap_smoke(
    connector: &TlsConnector,
    addr: std::net::SocketAddr,
    user: &str,
    password: &str,
    expected_subject: &str,
    expected_from: &str,
) -> Result<()> {
    let stream = tls_open(connector, addr).await?;
    let (rd, mut wr) = split(stream);
    let mut rd = BufReader::new(rd);

    // Greeting: `* OK [CAPABILITY ...] cosmix-maild IMAP ready`
    let greeting = read_imap_line(&mut rd).await?;
    assert!(
        greeting.starts_with("* OK"),
        "expected IMAP greeting starting with * OK, got {greeting:?}"
    );
    let upper = greeting.to_uppercase();
    assert!(
        upper.contains("[CAPABILITY") && upper.contains("IMAP4REV2"),
        "greeting must embed CAPABILITY with IMAP4rev2: {greeting}"
    );
    for forbidden in ["CONDSTORE", "QRESYNC", "MODSEQ"] {
        assert!(
            !upper.contains(forbidden),
            "greeting CAPABILITY must omit {forbidden} (Phase 6 not shipped): {greeting}"
        );
    }

    // CAPABILITY (pre-auth) — Thunderbird issues this before LOGIN.
    imap_send(&mut wr, "a01 CAPABILITY\r\n").await?;
    let cap_lines = imap_read_until_tagged(&mut rd, "a01").await?;
    let cap_blob = cap_lines.join("\n");
    assert!(
        cap_blob.to_uppercase().contains("AUTH=PLAIN")
            || cap_blob.to_uppercase().contains("AUTH=LOGIN"),
        "pre-auth CAPABILITY should advertise AUTH=PLAIN or AUTH=LOGIN: {cap_blob}"
    );
    assert!(
        cap_blob.contains(" IDLE"),
        "pre-auth CAPABILITY should advertise IDLE: {cap_blob}"
    );

    // LOGIN — Thunderbird uses LOGIN (not SASL AUTHENTICATE) by
    // default for username/password.
    imap_send(&mut wr, &format!("a02 LOGIN {user} {password}\r\n")).await?;
    let login_resp = read_imap_line(&mut rd).await?;
    assert!(
        login_resp.starts_with("a02 OK"),
        "LOGIN failed: {login_resp}"
    );

    // NAMESPACE — Thunderbird probes this to discover delimiter +
    // any shared/other-user namespaces.
    imap_send(&mut wr, "a03 NAMESPACE\r\n").await?;
    let ns_lines = imap_read_until_tagged(&mut rd, "a03").await?;
    let ns_blob = ns_lines.join("\n");
    assert!(
        ns_blob.contains("* NAMESPACE"),
        "NAMESPACE must return an untagged NAMESPACE line: {ns_blob}"
    );

    // LIST "" "*" — Thunderbird's folder-tree boot probe.
    imap_send(&mut wr, "a04 LIST \"\" \"*\"\r\n").await?;
    let list_lines = imap_read_until_tagged(&mut rd, "a04").await?;
    let list_blob = list_lines.join("\n");
    assert!(
        list_blob.contains("INBOX"),
        "LIST must include INBOX: {list_blob}"
    );

    // SELECT INBOX — bounded poll for EXISTS >= 1 so an SMTP DATA
    // 250 that committed under `with_set_tx` is visible to the
    // IMAP read path before we move to UID FETCH. SLA: 2 s.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut tag_n = 5u32;
    let last_select_blob;
    let exists = loop {
        let tag = format!("a{tag_n:02}");
        tag_n += 1;
        imap_send(&mut wr, &format!("{tag} SELECT INBOX\r\n")).await?;
        let lines = imap_read_until_tagged(&mut rd, &tag).await?;
        let select_blob = lines.join("\n");
        // The exists line is `* N EXISTS`.
        let exists = lines
            .iter()
            .find_map(|l| {
                l.strip_prefix("* ")
                    .and_then(|rest| rest.strip_suffix(" EXISTS"))
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .unwrap_or(0);
        if exists >= 1 {
            last_select_blob = select_blob;
            break exists;
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "SELECT INBOX never reached EXISTS>=1 within 2s; last response: {select_blob}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        last_select_blob.contains("[UIDVALIDITY "),
        "SELECT must emit UIDVALIDITY: {last_select_blob}"
    );
    assert!(
        last_select_blob.contains("[UIDNEXT "),
        "SELECT must emit UIDNEXT: {last_select_blob}"
    );
    assert_eq!(exists, 1, "expected exactly one message in INBOX");

    // UID SEARCH ALL — Thunderbird's "what UIDs exist" probe.
    let search_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(&mut wr, &format!("{search_tag} UID SEARCH ALL\r\n")).await?;
    let search_lines = imap_read_until_tagged(&mut rd, &search_tag).await?;
    let uid = search_lines
        .iter()
        .find_map(|l| l.strip_prefix("* SEARCH "))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("UID SEARCH ALL must return one UID, got {search_lines:?}"))?;

    // UID FETCH (ENVELOPE + BODYSTRUCTURE) — Thunderbird's
    // message-list summary. Subject/from must be visible in this
    // response by themselves; checking them after the literal BODY[]
    // payload was returned would let the verbatim RFC822 body satisfy
    // the substring (Codex T20 round-1 MAJOR).
    let env_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(
        &mut wr,
        &format!("{env_tag} UID FETCH {uid} (UID FLAGS ENVELOPE BODYSTRUCTURE)\r\n"),
    )
    .await?;
    let env_lines = imap_read_until_tagged(&mut rd, &env_tag).await?;
    let env_blob = env_lines.join("\n");
    assert!(
        env_blob.contains(expected_subject),
        "FETCH ENVELOPE must carry subject {expected_subject:?}: {env_blob}"
    );
    // ENVELOPE atom encodes addresses as RFC 9051 `address` structs:
    // `(personal NIL mailbox host)`. Assert the canonical
    // mailbox/host pairing rather than the raw `mailbox@host` form
    // (which would only appear in the literal BODY[] payload).
    let (from_local, from_host) = expected_from
        .split_once('@')
        .ok_or_else(|| anyhow!("expected_from {expected_from:?} must contain `@`"))?;
    let env_from_atom = format!("\"{from_local}\" \"{from_host}\"");
    assert!(
        env_blob.contains(&env_from_atom),
        "FETCH ENVELOPE must carry from address-struct {env_from_atom:?}: {env_blob}"
    );
    assert!(
        env_blob.contains("BODYSTRUCTURE"),
        "FETCH must include BODYSTRUCTURE: {env_blob}"
    );
    assert!(
        !env_blob.contains("BODY["),
        "ENVELOPE-only FETCH must not return BODY[*] literals: {env_blob}"
    );

    // UID FETCH BODY.PEEK[] — Thunderbird's preview-pane fetch. This
    // verifies the literal-payload shape and proves the `.PEEK` form
    // does not set `\Seen` (re-checked via a FLAGS refetch below).
    let body_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(
        &mut wr,
        &format!("{body_tag} UID FETCH {uid} BODY.PEEK[]\r\n"),
    )
    .await?;
    let body_lines = imap_read_until_tagged(&mut rd, &body_tag).await?;
    let body_blob = body_lines.join("\n");
    assert!(
        body_blob.contains("BODY[]"),
        "FETCH must include BODY[] payload: {body_blob}"
    );

    // BODY.PEEK[] must NOT have set \Seen — re-fetch FLAGS only.
    let flags_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(&mut wr, &format!("{flags_tag} UID FETCH {uid} FLAGS\r\n")).await?;
    let flags_lines = imap_read_until_tagged(&mut rd, &flags_tag).await?;
    let flags_blob = flags_lines.join("\n");
    assert!(
        !flags_blob.contains("\\Seen"),
        "BODY.PEEK[] must not set \\Seen: {flags_blob}"
    );

    // UID STORE +FLAGS (\Seen) — Thunderbird marks-as-read.
    let store_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(
        &mut wr,
        &format!("{store_tag} UID STORE {uid} +FLAGS (\\Seen)\r\n"),
    )
    .await?;
    let store_lines = imap_read_until_tagged(&mut rd, &store_tag).await?;
    let store_blob = store_lines.join("\n");
    assert!(
        store_blob.contains("\\Seen"),
        "UID STORE must echo FETCH with \\Seen: {store_blob}"
    );

    // IDLE — enter, wait for `+ idling`, then DONE. Thunderbird's
    // primary mechanism for inbox-tail updates.
    let idle_tag = format!("a{tag_n:02}");
    tag_n += 1;
    imap_send(&mut wr, &format!("{idle_tag} IDLE\r\n")).await?;
    let cont = timeout(Duration::from_secs(2), read_imap_line(&mut rd)).await??;
    assert_eq!(cont, "+ idling", "IDLE must emit `+ idling`: {cont}");
    imap_send(&mut wr, "DONE\r\n").await?;
    let idle_done = timeout(Duration::from_secs(2), read_imap_line(&mut rd)).await??;
    assert!(
        idle_done.starts_with(&format!("{idle_tag} OK")),
        "DONE must tag-complete the IDLE: {idle_done}"
    );

    // LOGOUT — clean session shutdown.
    let logout_tag = format!("a{tag_n:02}");
    imap_send(&mut wr, &format!("{logout_tag} LOGOUT\r\n")).await?;
    let logout_lines = imap_read_until_tagged(&mut rd, &logout_tag).await?;
    assert!(
        logout_lines.iter().any(|l| l.starts_with("* BYE")),
        "LOGOUT must emit `* BYE`: {logout_lines:?}"
    );

    Ok(())
}

// ---------- IMAP wire helpers (TLS streams) ----------

async fn imap_send(wr: &mut WriteHalf<TlsStream<TcpStream>>, line: &str) -> Result<()> {
    wr.write_all(line.as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}

/// Per-line wire-read budget. Every protocol step in this test is
/// either a single round-trip or a tight in-process broadcast hop; a
/// 5-second cap is two orders of magnitude over the expected p99 and
/// turns a regression-induced deadlock into a fast, command-tagged
/// failure instead of a CI watchdog kill (Codex T20 round-1 MINOR).
const WIRE_READ_TIMEOUT: Duration = Duration::from_secs(5);

async fn read_imap_line<R: AsyncRead + Unpin>(rd: &mut BufReader<R>) -> Result<String> {
    let mut s = String::new();
    let n = timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut s))
        .await
        .map_err(|_| anyhow!("IMAP read_line timed out after {WIRE_READ_TIMEOUT:?}"))??;
    if n == 0 {
        return Err(anyhow!("peer closed IMAP stream"));
    }
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

async fn imap_read_until_tagged<R: AsyncRead + Unpin>(
    rd: &mut BufReader<R>,
    tag: &str,
) -> Result<Vec<String>> {
    let prefix = format!("{tag} ");
    let mut out = Vec::new();
    loop {
        let line = read_imap_line(rd).await?;
        let is_tagged = line.starts_with(&prefix);
        out.push(line);
        if is_tagged {
            return Ok(out);
        }
    }
}

// ---------- SMTP wire helpers ----------

async fn expect_smtp_code<R: AsyncBufReadExt + Unpin>(rd: &mut R, code: &[u8]) -> Result<()> {
    let mut line = String::new();
    timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
        .await
        .map_err(|_| {
            anyhow!(
                "SMTP read_line timed out after {WIRE_READ_TIMEOUT:?} expecting {}",
                String::from_utf8_lossy(code)
            )
        })??;
    if !line.as_bytes().starts_with(code) {
        return Err(anyhow!(
            "SMTP expected {} got {:?}",
            String::from_utf8_lossy(code),
            line
        ));
    }
    Ok(())
}

async fn consume_smtp_multiline<R: AsyncBufReadExt + Unpin>(rd: &mut R, code: &[u8]) -> Result<()> {
    let _ = read_smtp_multiline(rd, code).await?;
    Ok(())
}

async fn read_smtp_multiline<R: AsyncBufReadExt + Unpin>(
    rd: &mut R,
    code: &[u8],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow!(
                    "SMTP multiline read_line timed out after {WIRE_READ_TIMEOUT:?} expecting {}",
                    String::from_utf8_lossy(code)
                )
            })??;
        let bytes = line.as_bytes();
        if !bytes.starts_with(code) {
            return Err(anyhow!(
                "SMTP expected {} got {:?}",
                String::from_utf8_lossy(code),
                line
            ));
        }
        let last = bytes.get(code.len()) == Some(&b' ');
        out.push(line.trim_end_matches(['\r', '\n']).to_string());
        if last {
            return Ok(out);
        }
    }
}

async fn expect_smtp_code_async<R>(rd: &mut BufReader<ReadHalf<R>>, code: &[u8]) -> Result<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
{
    expect_smtp_code(rd, code).await
}
