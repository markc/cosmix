//! Multi-vhost wire e2e for SNI cert selection, the Phase 3 RCPT TO
//! gate, and the Phase 5a `maild.tls.reload` hot-swap.
//!
//! Stands up the *real* maild binary surface with two
//! `[[tls.identity]]` rows (`alpha.test`, `beta.test`) plus per-domain
//! `maild.domains` substrate rows, then exercises three observable
//! behaviours:
//!
//! 1. **SNI cert selection** — a TLS client connecting to the SMTPS
//!    submission port (and the IMAPS port) with SNI=`alpha.test` sees
//!    the alpha leaf cert; SNI=`beta.test` sees the beta leaf cert.
//!    Asserted on the raw DER bytes the *client* observes via
//!    `ClientConnection::peer_certificates()`, not on subject parsing,
//!    so a regression in `pick_for_sni` surfaces as a byte-equal
//!    mismatch.
//!
//! 2. **P3 RCPT TO gate** — plain SMTP inbound (port-25 equivalent)
//!    accepts `RCPT TO:<alice@alpha.test>` (substrate row +
//!    account-row both exist), and rejects
//!    `RCPT TO:<foo@nowhere.test>` with `550 5.1.2 No such domain
//!    here`. The latter is the substrate-authoritative leg of
//!    `crate::smtp::session::decide_rcpt_to` — the Phase 3 cutover
//!    rule (non-empty namespace + domain not in namespace = reject).
//!
//! 3. **`maild.tls.reload` hot-swap** — write `gamma.test` PEMs into
//!    the substrate `tls_root`, add a `maild.domains` row pointing at
//!    `tls_identity_name = "gamma.test"`, dispatch the reload Bus
//!    verb against the live [`BuiltMaild::tls_state`]. The next TLS
//!    connection with SNI=`gamma.test` MUST present the new gamma
//!    leaf cert; SNI=`alpha.test` MUST still present the alpha leaf
//!    cert (config rows survive every reload, substrate rows merge
//!    in on top).
//!
//! Out of scope for this commit (tracked as follow-ups):
//! - Per-domain outbound DKIM-Signature verification — DKIM signs in
//!   `smtp/delivery.rs` (queue worker), not `inbound.rs`. Asserting
//!   the queued blob carries a `DKIM-Signature` would test the wrong
//!   pipeline stage. Needs a fake-MX seam to exercise the relay path.
//! - Per-domain outbound HELO selection — same shape as DKIM; needs
//!   the same fake-MX seam.
//!
//! Bus wiring is disabled (`enable_bus: false`) so there is no broker
//! dependency. The `tls.reload` verb is dispatched directly against
//! the `TlsReloadState` clone preserved on `BuiltMaild`; production
//! goes through the broker, but both paths share the same Arc-backed
//! slot + cache so behaviour is observably identical.
//!
//! Outbound delivery is also disabled (`disable_outbound_delivery:
//! true`) — the test only drives plain SMTP inbound to alpha.test/
//! beta.test recipients, but the substrate seeds the domains so a
//! stray retry against a remote MX would otherwise spin the worker.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use cosmix_client::IncomingCommand;
use cosmix_maild::config::{Config, ListenSpec, TlsConfig, TlsIdentityConfig};
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

const WIRE_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_vhost_sni_rcpt_and_reload() -> Result<()> {
    install_ring_provider();

    let tmp = TempDir::new()?;
    // Substrate `tls_root` and config identity PEMs share the same
    // directory. Substrate identities resolve PEMs at
    // `<tls_root>/<name>.{cert,key}.pem` strictly; config rows point
    // at absolute paths but we write them into the same dir for
    // symmetry. The `gamma.test` substrate identity added in Phase C
    // lands in the same tree.
    let tls_root = tmp.path().join("tls");
    std::fs::create_dir_all(&tls_root)?;

    // Two startup-config identities — config-managed; not declared in
    // the substrate's `maild.domains.tls_identity_name`.
    let alpha = make_identity(&tls_root, "alpha.test")?;
    let beta = make_identity(&tls_root, "beta.test")?;

    let cfg = make_multi_vhost_config(
        tmp.path(),
        &tls_root,
        vec![alpha.id_cfg.clone(), beta.id_cfg.clone()],
    );

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

    // Seed maild.domains rows for alpha + beta. `tls_identity_name`
    // stays NULL on these — the corresponding TLS identities come
    // from the `[[tls.identity]]` config rows. The Phase-3 RCPT gate
    // needs the substrate row to exist so the domain is recognised
    // as locally authoritative.
    seed_domain_row(&built, "alpha.test", None).await?;
    seed_domain_row(&built, "beta.test", None).await?;

    // Create one account per vhost, through the full substrate write
    // path (so `after_set` seeds the MDS set + default mailboxes).
    create_account(&built, "alice@alpha.test", "alpha-pass").await?;
    create_account(&built, "bob@beta.test", "beta-pass").await?;

    let tls = test_tls_connector()?;

    // -------- Phase A — SNI cert selection on SMTPS + IMAPS --------
    let leaf = peer_cert_via_sni(&tls, smtps_addr, "alpha.test").await?;
    assert_eq!(
        leaf, alpha.cert_der,
        "smtps with SNI=alpha.test must present alpha leaf cert"
    );
    let leaf = peer_cert_via_sni(&tls, smtps_addr, "beta.test").await?;
    assert_eq!(
        leaf, beta.cert_der,
        "smtps with SNI=beta.test must present beta leaf cert"
    );

    // The same resolver Arc is shared with the IMAPS listener — if
    // pick_for_sni regresses on either side, only one half of the
    // assertion will trip. Cover both surfaces so a SMTPS-only or
    // IMAPS-only regression is loud.
    let leaf = peer_cert_via_sni(&tls, imaps_addr, "alpha.test").await?;
    assert_eq!(
        leaf, alpha.cert_der,
        "imaps with SNI=alpha.test must present alpha leaf cert"
    );
    let leaf = peer_cert_via_sni(&tls, imaps_addr, "beta.test").await?;
    assert_eq!(
        leaf, beta.cert_der,
        "imaps with SNI=beta.test must present beta leaf cert"
    );

    // -------- Phase B — RCPT TO gate on plain SMTP inbound --------
    // Local vhost recipient — substrate row + account row both
    // present, expect 250 accept.
    smtp_rcpt_probe(
        smtp_inbound_addr,
        "external@external.test",
        "alice@alpha.test",
        b"250",
    )
    .await?;
    // Unknown vhost — namespace non-empty + recipient domain not
    // present → 550 5.1.2 (Phase-3 cutover rule).
    smtp_rcpt_probe(
        smtp_inbound_addr,
        "external@external.test",
        "foo@nowhere.test",
        b"550 5.1.2",
    )
    .await?;
    // Local vhost recipient whose mailbox does not exist —
    // substrate row present + account row missing → 550 5.1.1.
    smtp_rcpt_probe(
        smtp_inbound_addr,
        "external@external.test",
        "ghost@alpha.test",
        b"550 5.1.1",
    )
    .await?;

    // -------- Phase C — maild.tls.reload picks up substrate row --------
    let gamma = make_identity(&tls_root, "gamma.test")?;
    // Substrate-declared identity — `tls_identity_name = "gamma.test"`
    // is what the reload verb walks.
    seed_domain_row(&built, "gamma.test", Some("gamma.test")).await?;

    let cmd = IncomingCommand {
        from: String::new(),
        command: "maild.tls.reload".into(),
        id: None,
        args: serde_json::Value::Null,
        body: String::new(),
        headers: BTreeMap::new(),
    };
    let (rc, body) = cosmix_maild::bus::tls::dispatch("reload", &cmd, built.tls_state()).await;
    assert_eq!(rc, 0, "maild.tls.reload must succeed; body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let names: Vec<String> = json["identities"]
        .as_array()
        .ok_or_else(|| anyhow!("reload response missing identities[]: {body}"))?
        .iter()
        .map(|i| i["server_name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "gamma.test"),
        "reload identities must contain gamma.test; got {names:?}"
    );

    // The newly-added substrate identity must be presentable on both
    // listeners — Phase 5a wires the same resolver slot + cache into
    // SMTPS and IMAPS, so a listener-specific reload / cache-drift
    // regression would surface as one side updating while the other
    // serves the stale resolver.
    let leaf = peer_cert_via_sni(&tls, smtps_addr, "gamma.test").await?;
    assert_eq!(
        leaf, gamma.cert_der,
        "post-reload SMTPS SNI=gamma.test must present the new gamma leaf cert"
    );
    let leaf = peer_cert_via_sni(&tls, imaps_addr, "gamma.test").await?;
    assert_eq!(
        leaf, gamma.cert_der,
        "post-reload IMAPS SNI=gamma.test must present the new gamma leaf cert"
    );

    // Config-baseline identities survive the reload.
    let leaf = peer_cert_via_sni(&tls, smtps_addr, "alpha.test").await?;
    assert_eq!(
        leaf, alpha.cert_der,
        "post-reload SMTPS SNI=alpha.test must still present the alpha leaf cert"
    );
    let leaf = peer_cert_via_sni(&tls, smtps_addr, "beta.test").await?;
    assert_eq!(
        leaf, beta.cert_der,
        "post-reload SMTPS SNI=beta.test must still present the beta leaf cert"
    );

    Ok(())
}

// ---------- TLS plumbing ----------

fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// One generated SNI identity + its on-disk PEM pair + the leaf DER
/// captured for direct byte-comparison against the cert the server
/// presents at handshake.
struct Identity {
    id_cfg: TlsIdentityConfig,
    cert_der: Vec<u8>,
}

/// Generate a self-signed cert for `name`, write the PEM pair at
/// `<dir>/<name>.{cert,key}.pem` (the canonical layout the substrate
/// reload code expects), and return the resolved
/// [`TlsIdentityConfig`] plus the leaf DER bytes.
fn make_identity(dir: &std::path::Path, name: &str) -> Result<Identity> {
    let CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec![name.to_string()])?;
    let cert_path = dir.join(format!("{name}.cert.pem"));
    let key_path = dir.join(format!("{name}.key.pem"));
    std::fs::write(&cert_path, cert.pem())?;
    std::fs::write(&key_path, key_pair.serialize_pem())?;
    let cert_der: Vec<u8> = cert.der().to_vec();
    Ok(Identity {
        id_cfg: TlsIdentityConfig {
            server_name: name.into(),
            cert: cert_path.to_string_lossy().into_owned(),
            key: key_path.to_string_lossy().into_owned(),
            default: false,
            no_sni_fallback: false,
        },
        cert_der,
    })
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

/// Open a TLS handshake to `addr` with the SNI extension set to
/// `sni`, read the leaf cert the server presented, return its DER
/// bytes. The connection is dropped immediately after — we do not
/// drive any application protocol because the assertion is about the
/// resolver's `pick_for_sni` decision, not the listener's wire
/// protocol.
async fn peer_cert_via_sni(
    connector: &TlsConnector,
    addr: std::net::SocketAddr,
    sni: &str,
) -> Result<Vec<u8>> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from(sni.to_string())
        .map_err(|e| anyhow!("ServerName::try_from({sni:?}): {e:?}"))?;
    let stream = connector.connect(name, tcp).await?;
    let (_, conn) = stream.get_ref();
    let certs = conn
        .peer_certificates()
        .ok_or_else(|| anyhow!("TLS peer_certificates() returned None for SNI={sni:?}"))?;
    let leaf = certs
        .first()
        .ok_or_else(|| anyhow!("peer_certificates() empty for SNI={sni:?}"))?
        .as_ref()
        .to_vec();
    Ok(leaf)
}

// ---------- Substrate / account seeding ----------

async fn seed_domain_row(
    built: &BuiltMaild,
    domain: &str,
    tls_identity_name: Option<&str>,
) -> Result<()> {
    let mut body = match cosmix_maild::props::domains::defaults_value(domain) {
        PropValue::Object(m) => m,
        _ => unreachable!("domains::defaults_value returns Object"),
    };
    body.insert(
        "tls_identity_name".into(),
        match tls_identity_name {
            Some(name) => PropValue::String(name.into()),
            None => PropValue::Null,
        },
    );
    let key = RecordKey::collection(
        cosmix_maild::props::domains::namespace_name(),
        domain.to_string(),
    );
    built
        .app_state
        .domains_runtime
        .set(
            key,
            PropValue::Object(body),
            SetOpts {
                expected_version: Some(Version::zero()),
                merge: MergeMode::Replace,
                actor: Actor::operator("multi-vhost-e2e"),
                cause: Some(format!("seed domain {domain}")),
                ts_ms: 0,
            },
        )
        .await
        .map_err(|e| anyhow!("seed_domain_row({domain}): {e}"))?;
    Ok(())
}

async fn create_account(built: &BuiltMaild, email: &str, password: &str) -> Result<()> {
    let runtime = built.accounts_runtime();
    let hash = bcrypt::hash(password, 4).map_err(|e| anyhow!("bcrypt: {e}"))?;
    let mut m = BTreeMap::new();
    m.insert("email".into(), PropValue::String(email.into()));
    m.insert("password".into(), PropValue::String(hash));
    m.insert("name".into(), PropValue::String("Multi-vhost test".into()));
    m.insert("quota".into(), PropValue::Int(0));
    m.insert("spam_enabled".into(), PropValue::Bool(false));
    m.insert("spam_threshold".into(), PropValue::Float(0.5));
    let key = RecordKey::collection(cosmix_maild::props::accounts::namespace_name(), email);
    runtime
        .set(
            key,
            PropValue::Object(m),
            SetOpts {
                expected_version: Some(Version::zero()),
                merge: MergeMode::Replace,
                actor: Actor::operator("multi-vhost-e2e"),
                cause: Some(format!("create {email}")),
                ts_ms: 0,
            },
        )
        .await
        .map_err(|e| anyhow!("create_account({email}): {e}"))?;
    Ok(())
}

// ---------- Config ----------

fn make_multi_vhost_config(
    root: &std::path::Path,
    tls_root: &std::path::Path,
    identities: Vec<TlsIdentityConfig>,
) -> Config {
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
        // `hostname` is the EHLO greeting + the default-fallback
        // identity selector. Pick alpha so a no-SNI client (none of
        // the wire probes in this test) wouldn't shadow the explicit
        // SNI assertions if one slipped in.
        hostname: "alpha.test".into(),
        smtp_inbound: Some(ListenSpec::Single("127.0.0.1:0".into())),
        require_starttls_inbound: Vec::new(),
        smtp_smtps: Some(ListenSpec::Single("127.0.0.1:0".into())),
        smtp_outbound_bind: Vec::new(),
        max_message_size: None,
        dkim_selector: None,
        dkim_private_key: None,
        dkim: Default::default(),
        // Legacy single-pair MUST be empty when the identity list is
        // populated — `resolve_tls_identities()` prefers the list and
        // ignores legacy when both are set, but the parse-time check
        // half-rejects mixed shapes. Hold the half-set invariant.
        tls_cert: None,
        tls_key: None,
        retention_operators: Vec::new(),
        bayesian_rebuild_operators: Vec::new(),
        vtoken_operators: Vec::new(),
        vtoken_delegated_peers: Vec::new(),
        tls: TlsConfig {
            identity: identities,
            strict_sni: false,
        },
        tls_key_root: Some(tls_root.to_path_buf()),
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

// ---------- SMTP RCPT probe ----------

/// Open a plain SMTP (port-25 equivalent) session, EHLO, MAIL FROM,
/// then issue `RCPT TO:<rcpt>` and assert the server's response code
/// starts with `expected_prefix`. The session is QUIT'd as a
/// best-effort write — a 550 reject does NOT abort the session, so
/// the server is still in a state where QUIT is legal, but the
/// 221 response is not read or asserted (the wire assertion is the
/// RCPT response).
async fn smtp_rcpt_probe(
    addr: std::net::SocketAddr,
    mail_from: &str,
    rcpt_to: &str,
    expected_prefix: &[u8],
) -> Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    expect_smtp_code(&mut rd, b"220").await?;
    wr.write_all(b"EHLO probe.external.test\r\n").await?;
    consume_smtp_multiline(&mut rd, b"250").await?;
    wr.write_all(format!("MAIL FROM:<{mail_from}>\r\n").as_bytes())
        .await?;
    expect_smtp_code(&mut rd, b"250").await?;

    wr.write_all(format!("RCPT TO:<{rcpt_to}>\r\n").as_bytes())
        .await?;
    let mut line = String::new();
    timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("RCPT read_line timed out for {rcpt_to:?}"))??;
    if !line.as_bytes().starts_with(expected_prefix) {
        return Err(anyhow!(
            "RCPT TO:<{rcpt_to}> expected response prefix {:?}, got: {line:?}",
            String::from_utf8_lossy(expected_prefix)
        ));
    }
    let _ = wr.write_all(b"QUIT\r\n").await;
    Ok(())
}

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
    loop {
        let mut line = String::new();
        timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow!(
                    "SMTP multiline read_line timed out expecting {}",
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
        if bytes.get(code.len()) == Some(&b' ') {
            return Ok(());
        }
    }
}
