//! Per-domain outbound DKIM signing and HELO selection via fake MX.
//!
//! Multi-vhost wire e2e for the *outbound* leg: the `multi_vhost_e2e`
//! sibling covers SNI cert selection, the Phase 3 RCPT TO gate, and
//! the Phase 5a `maild.tls.reload` hot-swap on the *inbound* and
//! TLS-resolver surfaces; this test exercises the queue worker.
//!
//! Two sender domains (`alpha.test`, `beta.test`), each with its own
//! `[[dkim.domain]]` row pinned to a distinct selector
//! (`alphakey` / `betakey`) and its own `maild.domains` substrate row
//! pinned to a distinct `helo_identity` (`mail.alpha.test` /
//! `mail.beta.test`). One outbound queue entry per domain, addressed
//! to a remote recipient whose domain is wired into the
//! [`smtp::SmtpConfig::test_mx_overrides`] map so the queue worker
//! dials a fake MX on `127.0.0.1` instead of doing real DNS.
//!
//! Three observable invariants:
//!
//! 1. **Per-domain EHLO** — the fake MX session for alpha's mail must
//!    open with `EHLO mail.alpha.test`, beta's with `EHLO mail.beta.test`.
//!    Proves `sender_effective_host(HostKind::Helo)` consults the
//!    `maild.domains` row keyed on the *sender* domain, not the daemon
//!    `hostname`.
//!
//! 2. **Per-domain DKIM `d=`** — the DATA payload for alpha's mail
//!    must carry a `DKIM-Signature:` header with `d=alpha.test`, beta's
//!    with `d=beta.test`. Proves
//!    `MailAuthSigner::lookup_active(from_header_domain)` resolves to
//!    the correct per-domain signer at sign time.
//!
//! 3. **Per-domain DKIM `s=`** — same headers must carry distinct
//!    `s=` selectors (`alphakey` / `betakey`). Proves the per-row
//!    selector survives the parent-walk lookup.
//!
//! Bus wiring is disabled (`enable_bus: false`); the
//! `disable_outbound_delivery` flag stays *false* on purpose — this
//! test depends on the delivery worker running. Real DNS is never
//! consulted because the override map covers every recipient domain.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, anyhow};
use cosmix_maild::config::{Config, DkimDomainConfig, DkimSubsection, ListenSpec, TlsConfig};
use cosmix_maild::runtime::{BuiltMaild, RuntimeOpts, build_runtime};
use cosmix_mds::Mds;
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::SetOpts;
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Test-side time budget for waiting on the queue worker to dial the
/// fake MX. The delivery worker polls the queue every 30s, so the
/// first cycle can fire immediately or up to one full tick later.
const QUEUE_DELIVER_WAIT: Duration = Duration::from_secs(45);

/// Per-SMTP-line read budget inside the fake MX. Trips loud (panic
/// the handler task) if the daemon stops mid-session.
const WIRE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What the fake MX captured for one accepted session.
#[derive(Debug, Clone)]
struct CapturedSession {
    /// First `EHLO` argument observed on the wire (the daemon's
    /// claimed HELO host).
    ehlo_host: String,
    /// MAIL FROM envelope address.
    mail_from: String,
    /// Full DATA payload (headers + body) ending just before the
    /// terminating `\r\n.\r\n`.
    data: Vec<u8>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_vhost_outbound_dkim_and_helo() -> Result<()> {
    install_ring_provider();

    let tmp = TempDir::new()?;
    let dkim_root = tmp.path().join("dkim");
    std::fs::create_dir_all(&dkim_root)?;

    // Generate one RSA key per sender domain. `[[dkim.domain]]` rows
    // point at the per-domain PEM with the selector pinned in the
    // config so the wire-level `DKIM-Signature` carries the exact
    // `(d=, s=)` pair the assertions test.
    let alpha_key_path = write_rsa_key_pem(&dkim_root, "alpha")?;
    let beta_key_path = write_rsa_key_pem(&dkim_root, "beta")?;

    // Fake MX listens on 127.0.0.1:0; the resolved port goes into
    // `test_mx_overrides` so the queue worker dials it directly.
    let (mx_addr, mut captured_rx) = spawn_fake_mx().await?;
    let mut overrides: HashMap<String, SocketAddr> = HashMap::new();
    overrides.insert("remote-alpha.test".into(), mx_addr);
    overrides.insert("remote-beta.test".into(), mx_addr);

    let cfg = make_outbound_config(
        tmp.path(),
        vec![
            DkimDomainConfig {
                domain: "alpha.test".into(),
                selector: "alphakey".into(),
                algorithm: "rsa-sha256".into(),
                key_path: alpha_key_path,
                canonicalization: None,
                headers: None,
                active_for_signing: true,
                allow_body_length_tag: false,
            },
            DkimDomainConfig {
                domain: "beta.test".into(),
                selector: "betakey".into(),
                algorithm: "rsa-sha256".into(),
                key_path: beta_key_path,
                canonicalization: None,
                headers: None,
                active_for_signing: true,
                allow_body_length_tag: false,
            },
        ],
    );

    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            // Keep the delivery worker spawned — this test depends on
            // it polling the queue and dialing the fake MX through
            // the override map.
            disable_outbound_delivery: false,
            test_mx_overrides: overrides,
        },
    )
    .await?;

    // Seed the `maild.domains` rows that the sender path consults.
    // `helo_identity` is the first step of the
    // `sender_effective_host(HostKind::Helo)` fallback chain — pinning
    // it to a distinct value per row is what makes the per-domain
    // EHLO assertion non-trivial (`config.hostname` would mask a
    // regression that always falls through).
    seed_domain_row(&built, "alpha.test", "mail.alpha.test").await?;
    seed_domain_row(&built, "beta.test", "mail.beta.test").await?;

    // Enqueue one outbound message per sender domain. Bypass the
    // submission listener (which would need SMTPS + accounts) and
    // talk directly to the queue table the worker drains.
    enqueue_outbound_message(
        &built,
        "alice@alpha.test",
        "external@remote-alpha.test",
        &message_with_from("alice@alpha.test", "external@remote-alpha.test"),
    )
    .await?;
    enqueue_outbound_message(
        &built,
        "charlie@beta.test",
        "external@remote-beta.test",
        &message_with_from("charlie@beta.test", "external@remote-beta.test"),
    )
    .await?;

    // Collect both captured sessions. Order on the wire matches the
    // queue worker's iteration over `fetch_ready` rows — alpha row
    // was inserted first, so it should arrive first — but the
    // worker may interleave with multi-recipient grouping, so we
    // accept either order and demux by envelope sender.
    let mut got: BTreeMap<String, CapturedSession> = BTreeMap::new();
    for _ in 0..2 {
        let session = timeout(QUEUE_DELIVER_WAIT, captured_rx.recv())
            .await
            .map_err(|_| {
                anyhow!("queue worker did not dial fake MX within {QUEUE_DELIVER_WAIT:?}")
            })?
            .ok_or_else(|| anyhow!("fake MX channel closed before two sessions arrived"))?;
        got.insert(session.mail_from.clone(), session);
    }

    let alpha = got
        .remove("alice@alpha.test")
        .ok_or_else(|| anyhow!("fake MX did not see alpha sender; saw {:?}", got.keys()))?;
    let beta = got
        .remove("charlie@beta.test")
        .ok_or_else(|| anyhow!("fake MX did not see beta sender"))?;

    // -------- Invariant 1 — per-domain EHLO --------
    assert_eq!(
        alpha.ehlo_host, "mail.alpha.test",
        "alpha outbound must EHLO with its `helo_identity` substrate value"
    );
    assert_eq!(
        beta.ehlo_host, "mail.beta.test",
        "beta outbound must EHLO with its `helo_identity` substrate value"
    );

    // -------- Invariant 2 — per-domain DKIM d= --------
    let alpha_d = dkim_tag(&alpha.data, "d")
        .ok_or_else(|| anyhow!("alpha DATA missing DKIM-Signature d= tag"))?;
    let beta_d = dkim_tag(&beta.data, "d")
        .ok_or_else(|| anyhow!("beta DATA missing DKIM-Signature d= tag"))?;
    assert_eq!(
        alpha_d, "alpha.test",
        "alpha outbound DKIM signature must carry d=alpha.test"
    );
    assert_eq!(
        beta_d, "beta.test",
        "beta outbound DKIM signature must carry d=beta.test"
    );

    // -------- Invariant 3 — per-domain DKIM s= --------
    let alpha_s = dkim_tag(&alpha.data, "s")
        .ok_or_else(|| anyhow!("alpha DATA missing DKIM-Signature s= tag"))?;
    let beta_s = dkim_tag(&beta.data, "s")
        .ok_or_else(|| anyhow!("beta DATA missing DKIM-Signature s= tag"))?;
    assert_eq!(
        alpha_s, "alphakey",
        "alpha outbound DKIM signature must carry s=alphakey"
    );
    assert_eq!(
        beta_s, "betakey",
        "beta outbound DKIM signature must carry s=betakey"
    );

    Ok(())
}

// ---------- RSA key material ----------

fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Mail-auth's published RSA-2048 test key (also embedded in
/// `cosmix-maild-auth::signer::tests` and `crate::smtp::delivery`'s
/// `dkim_dispatch_tests`). Inline here so the test does not pull
/// `rsa` + `rand` into the cosmix-maild dev-dependency graph for a
/// single keypair. Two `[[dkim.domain]]` rows can legitimately share
/// the same key bytes — the `d=` and `s=` tags are pinned by the
/// per-row config, and the assertions verify those tags, not signing
/// crypto (covered by `cosmix-maild-auth` unit tests).
const RSA_TEST_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
MIIEowIBAAKCAQEAv9XYXG3uK95115mB4nJ37nGeNe2CrARm1agrbcnSk5oIaEfM\n\
ZLUR/X8gPzoiNHZcfMZEVR6bAytxUhc5EvZIZrjSuEEeny+fFd/cTvcm3cOUUbIa\n\
UmSACj0dL2/KwW0LyUaza9z9zor7I5XdIl1M53qVd5GI62XBB76FH+Q0bWPZNkT4\n\
NclzTLspD/MTpNCCPhySM4Kdg5CuDczTH4aNzyS0TqgXdtw6A4Sdsp97VXT9fkPW\n\
9rso3lrkpsl/9EQ1mR/DWK6PBmRfIuSFuqnLKY6v/z2hXHxF7IoojfZLa2kZr9Ae\n\
d4l9WheQOTA19k5r2BmlRw/W9CrgCBo0Sdj+KQIDAQABAoIBAFPChEi/OvnulReB\n\
ECQWhOUYuNKlFKQU++2YEvZJ4+bMn5UgnE7wfJ1pj2Pr9xlfALz+OMHNrjMxGbaV\n\
KzdrT2uCkYcf78XjnhuH9gKIiXDUv4L4N+P3u6w8yOx4bFgOS9IjS53yDOPM7SC5\n\
g6dIg5aigHaHlffqIuFFv4yQMI/+Ai+zBKxS7wRhxK/7nnAuo28fe5MEdp57ho9/\n\
AGlDNsdg9zCgjwhokwFE3+AaD+bkUFm4gQ1XjkUFrlmnQn8vDQ0i9toEWhCj+UPY\n\
iOKL63MJnr90MXTXWLHoFj99wBp//mYygbF9Lj8fa28/oa8LWp3Jhb7QeMgH46iv\n\
3aLHbTECgYEA5M2dAw+nyMw9vYlkMejhwObKYP8Mr/6zcGMLCalYvRJM5iUAM0JI\n\
H6sM6pV9/nv167cbKocj3xYPdtE7FPOn4132MLM8Ne1f8nPE64Qrcbj5WBXvLnU8\n\
hpWbwe2Z8h7UUMKx6q4F1/TXYkc3ScxYwfjM4mP/pLsAOgVzRSEEgrUCgYEA1qNQ\n\
xaQHNWZ1O8WuTnqWd5JSsic6iURAmUcLeFDZY2PWhVoaQ8L/xMQhDYs1FIbLWArW\n\
4Qq3Ibu8AbSejAKuaJz7Uf26PX+PYVUwAOO0qamCJ8d/qd6So7qWMDyAY2yXI39Y\n\
1nMqRjr7bkEsggAZao7BKqA7ZtmogjOusBT38iUCgYEA06agJ8TDoKvOMRZ26PRU\n\
YO0dKLzGL8eclcoI29cbj0rud7aiiMg3j5PbTuUat95TjsjDCIQaWrM9etvxm2AJ\n\
Xfn9Uu96MyhyKQWOk46f4YMKpMElkARDCPw8KRhx39dE77AqhLyWCz8iPndCXbH6\n\
KPTOEl4OjYOuof2Is9nnIkECgYBh948RdsnXhNlzm8nwhiGRmBbou+EK8D0v+O5y\n\
Tyy6IcKzgSnFzgZh8EdJ4EUtBk1f9SqY8wQdgIvSl3daXorusuA/TzkngsaV3YUY\n\
ktZOLlF7CKLrjOyPkMWmZKcROmpNyH1q/IvKHHfQnizLdXIkYd4nL5WNX0F7lE1i\n\
j1+QhQKBgB2lviBK7rJFwlFYdQUP1NAN2dKxMZk8uJS8JglHrM0+8nRI83HbTdEQ\n\
vB0ManEKBkbS4T5n+gRtdEqKSDmWDTXDlrBfcdCHNQLwYtBpOotCqQn/AmfjcPBl\n\
byAbwh4+HiZ5JISoRZpiZqy67aJNVoXmdtb/E9mi7ozzytpxMNql\n\
-----END RSA PRIVATE KEY-----\n";

fn write_rsa_key_pem(dir: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let path = dir.join(format!("{name}.pem"));
    std::fs::write(&path, RSA_TEST_PEM)?;
    Ok(path)
}

// ---------- Fake MX listener ----------

/// Bind a fake MX on `127.0.0.1:0`, return its resolved address and a
/// channel that yields one [`CapturedSession`] per accepted connection.
/// The handler accepts plaintext SMTP (no STARTTLS advertised) and
/// captures the first `EHLO`, the `MAIL FROM` argument, and the DATA
/// payload through the terminating `\r\n.\r\n`. Anything else (e.g.
/// RCPT TO) is acknowledged with `250` so the daemon proceeds; RSET /
/// NOOP / second-EHLO are not handled because the daemon doesn't
/// issue them on the plaintext path.
async fn spawn_fake_mx() -> Result<(SocketAddr, mpsc::Receiver<CapturedSession>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = mpsc::channel::<CapturedSession>(8);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_fake_mx_session(stream, tx).await {
                            eprintln!("fake-mx session error: {e}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("fake-mx accept error: {e}");
                    break;
                }
            }
        }
    });

    Ok((addr, rx))
}

async fn handle_fake_mx_session(
    stream: TcpStream,
    tx: mpsc::Sender<CapturedSession>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    wr.write_all(b"220 fake-mx.test ESMTP ready\r\n").await?;
    wr.flush().await?;

    let mut ehlo_host = String::new();
    let mut mail_from = String::new();

    loop {
        let mut line = String::new();
        let n = timeout(WIRE_READ_TIMEOUT, rd.read_line(&mut line))
            .await
            .map_err(|_| anyhow!("fake-mx read_line timed out"))??;
        if n == 0 {
            // Client closed.
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        let upper = trimmed.to_ascii_uppercase();

        if let Some(rest) = upper.strip_prefix("EHLO ") {
            // Preserve case for the assertion; `upper` is only used
            // for verb matching.
            let host = trimmed[5..].trim().to_string();
            ehlo_host = host;
            let _ = rest;
            wr.write_all(b"250-fake-mx.test\r\n250 8BITMIME\r\n")
                .await?;
            wr.flush().await?;
        } else if upper.starts_with("HELO ") {
            ehlo_host = trimmed[5..].trim().to_string();
            wr.write_all(b"250 fake-mx.test\r\n").await?;
            wr.flush().await?;
        } else if upper.starts_with("MAIL FROM:") {
            // `MAIL FROM:<addr>` — extract between angle brackets.
            mail_from = extract_angle_addr(&trimmed);
            wr.write_all(b"250 2.1.0 OK\r\n").await?;
            wr.flush().await?;
        } else if upper.starts_with("RCPT TO:") {
            wr.write_all(b"250 2.1.5 OK\r\n").await?;
            wr.flush().await?;
        } else if upper == "DATA" {
            wr.write_all(b"354 Start mail input\r\n").await?;
            wr.flush().await?;
            // Read until "\r\n.\r\n". Easiest is byte-by-byte against
            // a small state machine — the daemon dot-stuffs lines
            // starting with `.`, so the only valid end-of-DATA marker
            // is a complete CRLF + dot + CRLF.
            let data = read_until_dot_crlf(&mut rd).await?;
            wr.write_all(b"250 2.0.0 Accepted\r\n").await?;
            wr.flush().await?;
            // Publish the captured session as soon as DATA closes;
            // the daemon will issue QUIT next but the assertions are
            // already complete.
            tx.send(CapturedSession {
                ehlo_host: std::mem::take(&mut ehlo_host),
                mail_from: std::mem::take(&mut mail_from),
                data,
            })
            .await
            .ok();
        } else if upper == "QUIT" {
            wr.write_all(b"221 2.0.0 Bye\r\n").await?;
            wr.flush().await?;
            break;
        } else if upper.starts_with("STARTTLS") {
            // Don't advertise; reject the verb so the daemon falls
            // through to plaintext.
            wr.write_all(b"502 5.5.1 STARTTLS not advertised\r\n")
                .await?;
            wr.flush().await?;
        } else {
            wr.write_all(b"250 2.0.0 OK\r\n").await?;
            wr.flush().await?;
        }
    }
    Ok(())
}

fn extract_angle_addr(line: &str) -> String {
    if let Some(start) = line.find('<')
        && let Some(end) = line[start + 1..].find('>')
    {
        return line[start + 1..start + 1 + end].to_string();
    }
    String::new()
}

/// Read the DATA body up to (but not including) the terminating
/// `\r\n.\r\n` sequence. Does NOT reverse dot-stuffing — any
/// production line that began with `.` arrives here with the leading
/// `.` still in place. The DKIM assertions inspect headers, where
/// dot-stuffing never applies, so the un-unstuffed body is fine for
/// this test. A reusing helper that asserts on body content would
/// need to strip leading `..` to `.` before comparing.
async fn read_until_dot_crlf<R: AsyncReadExt + Unpin>(rd: &mut R) -> Result<Vec<u8>> {
    let mut buf = Vec::<u8>::new();
    let mut byte = [0u8; 1];
    loop {
        let n = timeout(WIRE_READ_TIMEOUT, rd.read_exact(&mut byte))
            .await
            .map_err(|_| anyhow!("fake-mx DATA read timed out"))??;
        if n == 0 {
            return Err(anyhow!("fake-mx: connection closed mid-DATA"));
        }
        buf.push(byte[0]);
        // Match `\r\n.\r\n` at the tail.
        if buf.len() >= 5 && &buf[buf.len() - 5..] == b"\r\n.\r\n" {
            buf.truncate(buf.len() - 5);
            return Ok(buf);
        }
    }
}

// ---------- DKIM tag extraction ----------

/// Extract a single tag value (`d`, `s`, …) from the *first*
/// `DKIM-Signature:` header in `data`. Tolerates folded continuation
/// lines (a leading whitespace-prefixed line is appended to the
/// header). Returns `None` if no DKIM-Signature is present.
fn dkim_tag(data: &[u8], tag: &str) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?;
    let mut header_value = String::new();
    let mut in_dkim = false;
    for line in text.split("\r\n") {
        if line.is_empty() {
            // End of headers.
            break;
        }
        if in_dkim {
            if line.starts_with(' ') || line.starts_with('\t') {
                header_value.push_str(line.trim_start());
                continue;
            }
            // Next header started — stop here.
            break;
        }
        if let Some(rest) = line.strip_prefix("DKIM-Signature:") {
            in_dkim = true;
            header_value.push_str(rest.trim_start());
        }
    }
    if !in_dkim {
        return None;
    }
    for part in header_value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{tag}=")) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

// ---------- Substrate + queue seeding ----------

async fn seed_domain_row(built: &BuiltMaild, domain: &str, helo_identity: &str) -> Result<()> {
    let mut body = match cosmix_maild::props::domains::defaults_value(domain) {
        PropValue::Object(m) => m,
        _ => unreachable!("domains::defaults_value returns Object"),
    };
    body.insert(
        "helo_identity".into(),
        PropValue::String(helo_identity.into()),
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
                actor: Actor::operator("multi-vhost-outbound-e2e").expect("valid actor"),
                cause: Some(format!("seed sender domain {domain}")),
                ts_ms: 0,
            },
        )
        .await
        .map_err(|e| anyhow!("seed_domain_row({domain}): {e}"))?;
    Ok(())
}

/// Build a minimal RFC 5322 message with the From and To headers the
/// DKIM signer + RCPT enumerator need. The body is non-empty so the
/// `bh=` body-hash tag is non-trivial.
fn message_with_from(from: &str, to: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\n\
         To: {to}\r\n\
         Subject: multi-vhost outbound e2e\r\n\
         Date: Tue, 26 May 2026 10:00:00 +1000\r\n\
         Message-ID: <test@multi-vhost-outbound-e2e>\r\n\
         \r\n\
         hello from {from}\r\n"
    )
    .into_bytes()
}

/// Enqueue a message directly via the CAS + queue tables (bypassing
/// SMTP submission, which would require an SMTPS listener +
/// authenticated session). The delivery worker picks up the row on
/// its next poll cycle.
async fn enqueue_outbound_message(
    built: &BuiltMaild,
    from: &str,
    to: &str,
    data: &[u8],
) -> Result<()> {
    let mds = built.app_state.mailstore.mds().clone();
    let bytes = data.to_vec();
    let hash = tokio::task::spawn_blocking(move || mds.put_blob(&bytes))
        .await
        .map_err(|e| anyhow!("put_blob join: {e}"))??;
    cosmix_maild::smtp::queue::enqueue(&built.app_state.db.conn, from, &[to.to_string()], hash)
        .await
        .map_err(|e| anyhow!("queue::enqueue: {e}"))?;
    Ok(())
}

// ---------- Config ----------

fn make_outbound_config(root: &std::path::Path, dkim_domains: Vec<DkimDomainConfig>) -> Config {
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
        // Distinct from any sender domain so a regression that falls
        // through to `config.hostname` would surface as a wrong EHLO
        // host on *both* sessions instead of silently masking one.
        hostname: "outbound-e2e.test".into(),
        // Inbound disabled — this test only enqueues directly.
        smtp_inbound: None,
        require_starttls_inbound: Vec::new(),
        smtp_smtps: None,
        smtp_outbound_bind: Vec::new(),
        max_message_size: None,
        dkim_selector: None,
        dkim_private_key: None,
        dkim: DkimSubsection {
            domains: dkim_domains,
            key_root: None,
        },
        tls_cert: None,
        tls_key: None,
        tls: TlsConfig {
            identity: vec![],
            strict_sni: false,
        },
        retention_operators: Vec::new(),
        bayesian_rebuild_operators: Vec::new(),
        vtoken_operators: Vec::new(),
        vtoken_delegated_peers: Vec::new(),
        tls_key_root: None,
        imap_imaps: None,
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

/// `tokio::net::TcpListener::bind(("127.0.0.1", 0))` returns an
/// arbitrary ephemeral port — re-exported here as a compile-time
/// reminder that the fake-MX address is captured *post-bind*, not
/// hardcoded, so the test never races a real local SMTP server.
#[allow(dead_code)]
fn _bind_invariant() -> ListenSpec {
    ListenSpec::Single("127.0.0.1:0".into())
}
