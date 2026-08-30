//! JMAP→MDS Phase 7 end-to-end smoke test (plan Task 7.1–7.3).
//!
//! Stands up an in-process maild via `cosmix_maild::runtime::build_runtime`
//! against an ephemeral tempdir + random localhost ports, creates the
//! `e2e@e2e.test` account through the substrate so the production
//! `after_set` hook seeds the default mailboxes, then walks the 8-step
//! smoke flow from `_doc/planned/jmap-mds-migration.md` Task 7.1
//! (SMTP DATA → Email/query → Email/get → Email/set seen →
//! Email/changes → Mailbox/get → Email/set destroy → Email/get notFound).
//!
//! Codex pre-impl consult additions baked in:
//! - SMTP DATA → JMAP visibility is asserted via bounded poll (2s/25ms,
//!   timeout-as-failure), not a fixed `sleep`. The `250` reply already
//!   means `with_set_tx` committed; the poll asserts that the JMAP
//!   read path observes the commit promptly.
//! - After destroy: `Email/changes` includes the id in `destroyed`,
//!   `Mailbox/get` Inbox is `totalEmails: 0` *and* `unreadEmails: 0`.
//! - Bus broker is disabled (`enable_bus = false`) — the broker is
//!   absent in the test env and a failing connect would otherwise
//!   spam logs.
//!
//! Run: `cargo test --release -p cosmix-maild --test jmap_mds_e2e`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use base64::Engine;
use cosmix_maild::config::{Config, ListenSpec};
use cosmix_maild::runtime::{RuntimeOpts, build_runtime};
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::SetOpts;
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const ACCOUNT_EMAIL: &str = "e2e@e2e.test";
const ACCOUNT_PASSWORD: &str = "e2epw1234";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jmap_mds_e2e_smoke() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let cfg = make_config(root);

    // Bring up the full daemon (DB, MDS, mailstore, props substrate,
    // SMTP listener) sans Bus broker. SMTP binds to 127.0.0.1:0; the
    // resolved port comes back in `smtp_handle.inbound_addrs[0]`.
    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            disable_outbound_delivery: false,
            ..Default::default()
        },
    )
    .await?;
    let smtp_addr = built
        .smtp_handle
        .inbound_addrs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("smtp inbound did not bind"))?;

    // JMAP HTTP listener on 127.0.0.1:0; we drive `axum::serve` in a
    // detached task so the test thread can speak both SMTP and JMAP.
    let jmap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let jmap_addr = jmap_listener.local_addr()?;
    let router = built.router.clone();
    let _serve_task = tokio::spawn(async move {
        let _ = axum::serve(jmap_listener, router).await;
    });
    let base_url = format!("http://{jmap_addr}");

    // Create the account via the production substrate path. `after_set`
    // creates the MDS set and seeds the default mailboxes synchronously
    // before the call returns, so the JMAP surface sees a fully-formed
    // account when `set` resolves Ok.
    create_account(&built, ACCOUNT_EMAIL, ACCOUNT_PASSWORD).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // (1) Initial state + Inbox id via Mailbox/get.
    let (inbox_id, initial_email_state) =
        resolve_inbox_and_state(&client, &base_url, ACCOUNT_EMAIL, ACCOUNT_PASSWORD).await?;

    // (1) SMTP DATA injection. `mail_send_raw` returns when the server
    // replies 250 to `.<CRLF>`; per maild's `inbound::deliver`, that
    // 250 is sent only after `mailstore.create_email` has committed
    // through `with_set_tx`. No fixed sleep needed for sync.
    let subject = "Phase 7 e2e smoke";
    let body = "hello from the e2e smoke test\r\nsecond line\r\n";
    mail_send_raw(
        smtp_addr,
        "sender@external.test",
        ACCOUNT_EMAIL,
        subject,
        body,
    )
    .await?;

    // (2) Bounded poll of Email/query inMailbox:Inbox. Codex: 250
    // means the commit is durable, so this should resolve on the
    // first iteration in the steady-state — the poll loop is the
    // SLA (250 → visible within 2s) being asserted, not a workaround
    // for known async lag.
    let email_id = wait_for_email_id(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        &inbox_id,
        Duration::from_secs(2),
        Duration::from_millis(25),
    )
    .await?;

    // (3) Email/get with fetchTextBodyValues.
    let got = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/get",
        json!({
            "accountId": "1",
            "ids": [email_id],
            "properties": ["subject", "from", "to", "textBody", "bodyValues", "keywords"],
            "fetchTextBodyValues": true,
        }),
    )
    .await?;
    let got_list = got["list"]
        .as_array()
        .ok_or_else(|| anyhow!("Email/get response missing list: {got}"))?;
    let email = got_list
        .first()
        .ok_or_else(|| anyhow!("Email/get list empty: {got}"))?;
    assert_eq!(
        email["subject"].as_str(),
        Some(subject),
        "Email/get subject mismatch: {email}",
    );
    let body_values = email["bodyValues"]
        .as_object()
        .ok_or_else(|| anyhow!("Email/get response missing bodyValues: {email}"))?;
    let first_body_value = body_values
        .values()
        .next()
        .ok_or_else(|| anyhow!("Email/get bodyValues empty: {email}"))?;
    let body_str = first_body_value["value"]
        .as_str()
        .ok_or_else(|| anyhow!("Email/get bodyValues.value not a string: {first_body_value}"))?;
    assert!(
        body_str.contains("hello from the e2e smoke test"),
        "Email/get textBody mismatch: {body_str:?}",
    );
    assert!(
        !email["keywords"]
            .as_object()
            .map(|m| m.contains_key("$seen"))
            .unwrap_or(false),
        "expected $seen absent at pre-update time: {email}",
    );

    // (4) Email/set update keywords/$seen:true. JMAP keyword update
    // pointer syntax is `keywords/$seen` per RFC 8621 §4.1.2.
    let pre_seen_state = email_state(&client, &base_url, ACCOUNT_EMAIL, ACCOUNT_PASSWORD).await?;
    let set_seen = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/set",
        json!({
            "accountId": "1",
            "update": {
                email_id.as_str(): { "keywords/$seen": true }
            }
        }),
    )
    .await?;
    let updated = set_seen["updated"]
        .as_object()
        .ok_or_else(|| anyhow!("Email/set response missing updated: {set_seen}"))?;
    assert!(
        updated.contains_key(&email_id),
        "Email/set updated map missing id {email_id}: {set_seen}",
    );

    // (5) Email/changes since pre-update state — id must appear in `updated`.
    let changes_after_seen = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/changes",
        json!({ "accountId": "1", "sinceState": pre_seen_state }),
    )
    .await?;
    let updated_ids = changes_after_seen["updated"]
        .as_array()
        .ok_or_else(|| anyhow!("Email/changes response missing updated: {changes_after_seen}"))?;
    assert!(
        updated_ids.iter().any(|v| v.as_str() == Some(&email_id)),
        "Email/changes updated did not contain id {email_id}: {changes_after_seen}",
    );

    // (6) Mailbox/get → unreadEmails:0 (also assert totalEmails:1 here as
    // a guard against false-positive counts).
    let mbox_get = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Mailbox/get",
        json!({ "accountId": "1", "ids": [inbox_id] }),
    )
    .await?;
    let inbox = mbox_get["list"][0].clone();
    assert_eq!(
        inbox["unreadEmails"].as_u64(),
        Some(0),
        "Inbox unread count after $seen=true: {inbox}",
    );
    assert_eq!(
        inbox["totalEmails"].as_u64(),
        Some(1),
        "Inbox total count after seen update: {inbox}",
    );

    // (7) Email/set destroy.
    let pre_destroy_state =
        email_state(&client, &base_url, ACCOUNT_EMAIL, ACCOUNT_PASSWORD).await?;
    let destroyed = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/set",
        json!({ "accountId": "1", "destroy": [email_id] }),
    )
    .await?;
    let destroyed_ids = destroyed["destroyed"]
        .as_array()
        .ok_or_else(|| anyhow!("Email/set destroy response missing destroyed: {destroyed}"))?;
    assert!(
        destroyed_ids.iter().any(|v| v.as_str() == Some(&email_id)),
        "Email/set destroyed list did not contain id {email_id}: {destroyed}",
    );

    // (8) Email/get → notFound.
    let post_destroy_get = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/get",
        json!({ "accountId": "1", "ids": [email_id] }),
    )
    .await?;
    assert!(
        post_destroy_get["list"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "Email/get list after destroy expected empty: {post_destroy_get}",
    );
    let not_found = post_destroy_get["notFound"]
        .as_array()
        .ok_or_else(|| anyhow!("Email/get response missing notFound: {post_destroy_get}"))?;
    assert!(
        not_found.iter().any(|v| v.as_str() == Some(&email_id)),
        "Email/get notFound missing id {email_id}: {post_destroy_get}",
    );

    // Codex addition: Email/changes after destroy must show the id in
    // `destroyed`. Email/get → notFound proves lookup, but does not
    // prove the change stream observed the deletion.
    let changes_after_destroy = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Email/changes",
        json!({ "accountId": "1", "sinceState": pre_destroy_state }),
    )
    .await?;
    let destroyed_in_changes = changes_after_destroy["destroyed"]
        .as_array()
        .ok_or_else(|| anyhow!("Email/changes destroyed missing: {changes_after_destroy}"))?;
    assert!(
        destroyed_in_changes
            .iter()
            .any(|v| v.as_str() == Some(&email_id)),
        "Email/changes destroyed did not contain id {email_id}: {changes_after_destroy}",
    );

    // Codex addition: Mailbox/get post-destroy must show totalEmails:0
    // AND unreadEmails:0. A destroy that fails to decrement totals is
    // a real failure mode worth catching.
    let mbox_after = jmap_call(
        &client,
        &base_url,
        ACCOUNT_EMAIL,
        ACCOUNT_PASSWORD,
        "Mailbox/get",
        json!({ "accountId": "1", "ids": [inbox_id] }),
    )
    .await?;
    let inbox_after = mbox_after["list"][0].clone();
    assert_eq!(
        inbox_after["totalEmails"].as_u64(),
        Some(0),
        "Inbox totalEmails after destroy: {inbox_after}",
    );
    assert_eq!(
        inbox_after["unreadEmails"].as_u64(),
        Some(0),
        "Inbox unreadEmails after destroy: {inbox_after}",
    );

    let _ = initial_email_state;
    Ok(())
}

fn make_config(root: &std::path::Path) -> Config {
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
        smtp_smtps: None,
        smtp_outbound_bind: Vec::new(),
        max_message_size: None,
        dkim_selector: None,
        dkim_private_key: None,
        dkim: Default::default(),
        tls_cert: None,
        tls_key: None,
        tls: Default::default(),
        retention_operators: Vec::new(),
        bayesian_rebuild_operators: Vec::new(),
        vtoken_operators: Vec::new(),
        vtoken_delegated_peers: Vec::new(),
        tls_key_root: None,
        imap_imaps: None,
        imap_max_literal_bytes: None,
        imap_idle_status_interval_secs: None,
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

async fn create_account(
    built: &cosmix_maild::runtime::BuiltMaild,
    email: &str,
    password: &str,
) -> Result<()> {
    let runtime = built.accounts_runtime();
    let hash = bcrypt::hash(password, 4).map_err(|e| anyhow!("bcrypt: {e}"))?;

    let mut m = BTreeMap::new();
    m.insert("email".into(), PropValue::String(email.into()));
    m.insert("password".into(), PropValue::String(hash));
    m.insert("name".into(), PropValue::String("E2E Test".into()));
    m.insert("quota".into(), PropValue::Int(0));
    m.insert("spam_enabled".into(), PropValue::Bool(false));
    m.insert("spam_threshold".into(), PropValue::Float(0.5));
    let value = PropValue::Object(m);

    let key = RecordKey::collection(cosmix_maild::props::accounts::namespace_name(), email);
    let opts = SetOpts {
        expected_version: Some(Version::zero()),
        merge: MergeMode::Replace,
        actor: Actor::operator("e2e-test"),
        cause: Some("create test account".into()),
        ts_ms: 0,
    };
    runtime
        .set(key, value, opts)
        .await
        .map_err(|e| anyhow!("create_account: {e}"))?;
    Ok(())
}

fn auth_header(email: &str, password: &str) -> String {
    let raw = format!("{email}:{password}");
    let enc = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
    format!("Basic {enc}")
}

async fn jmap_call(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    password: &str,
    method: &str,
    args: Value,
) -> Result<Value> {
    let body = json!({
        "using": [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
        ],
        "methodCalls": [[method, args, "c0"]],
    });
    let resp = client
        .post(format!("{base_url}/jmap"))
        .header("Authorization", auth_header(email, password))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("JMAP {method} HTTP {status}: {text}"));
    }
    let v: Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("JMAP {method} parse: {e} body={text}"))?;
    let calls = v["methodResponses"]
        .as_array()
        .ok_or_else(|| anyhow!("JMAP {method}: methodResponses missing: {v}"))?;
    let first = calls
        .first()
        .ok_or_else(|| anyhow!("JMAP {method}: methodResponses empty: {v}"))?;
    let arr = first
        .as_array()
        .ok_or_else(|| anyhow!("JMAP {method}: response not a triple: {first}"))?;
    let name = arr
        .first()
        .and_then(|n| n.as_str())
        .ok_or_else(|| anyhow!("JMAP {method}: missing method name in response: {first}"))?;
    if name == "error" {
        return Err(anyhow!("JMAP {method} returned error: {first}"));
    }
    Ok(arr[1].clone())
}

async fn resolve_inbox_and_state(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    password: &str,
) -> Result<(String, String)> {
    let mbox = jmap_call(
        client,
        base_url,
        email,
        password,
        "Mailbox/get",
        json!({ "accountId": "1" }),
    )
    .await?;
    let list = mbox["list"]
        .as_array()
        .ok_or_else(|| anyhow!("Mailbox/get list missing: {mbox}"))?;
    let inbox = list
        .iter()
        .find(|m| m["role"].as_str() == Some("inbox"))
        .ok_or_else(|| anyhow!("no inbox role mailbox: {mbox}"))?;
    let inbox_id = inbox["id"]
        .as_str()
        .ok_or_else(|| anyhow!("inbox id not a string: {inbox}"))?
        .to_string();
    let state = email_state(client, base_url, email, password).await?;
    Ok((inbox_id, state))
}

async fn email_state(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    password: &str,
) -> Result<String> {
    let q = jmap_call(
        client,
        base_url,
        email,
        password,
        "Email/query",
        json!({ "accountId": "1", "limit": 0 }),
    )
    .await?;
    Ok(q["queryState"]
        .as_str()
        .or_else(|| q["state"].as_str())
        .ok_or_else(|| anyhow!("Email/query state missing: {q}"))?
        .to_string())
}

async fn wait_for_email_id(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    password: &str,
    inbox_id: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<String> {
    let started = Instant::now();
    loop {
        let resp = jmap_call(
            client,
            base_url,
            email,
            password,
            "Email/query",
            json!({
                "accountId": "1",
                "filter": { "inMailbox": inbox_id },
                "limit": 10,
            }),
        )
        .await?;
        let ids = resp["ids"].as_array().cloned().unwrap_or_default();
        if let Some(first) = ids.first().and_then(|v| v.as_str()) {
            return Ok(first.to_string());
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!(
                "Email/query did not surface inbox message within {:?}; last response: {}",
                timeout,
                resp,
            ));
        }
        tokio::time::sleep(poll).await;
    }
}

/// Minimal raw-SMTP inbound delivery. Returns when the server replies
/// `250` to the DATA terminator (`.<CRLF>`); `inbound::deliver` only
/// sends that line after `mailstore.create_email` has returned Ok, so
/// the caller can immediately ask JMAP about the message.
async fn mail_send_raw(
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
    wr.write_all(b"EHLO e2e.test\r\n").await?;
    // EHLO replies are multi-line (250-...\r\n then a final 250 ...\r\n).
    consume_smtp_multiline(&mut rd, b"250").await?;
    wr.write_all(format!("MAIL FROM:<{from}>\r\n").as_bytes())
        .await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(format!("RCPT TO:<{to}>\r\n").as_bytes())
        .await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(b"DATA\r\n").await?;
    expect_smtp_code(&mut rd, b"354").await?;
    let payload = format!(
        "From: <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\nMessage-ID: <e2e-{ts}@e2e.test>\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
         {body}\r\n.\r\n",
        ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    );
    wr.write_all(payload.as_bytes()).await?;
    expect_smtp_code(&mut rd, b"250").await?;
    wr.write_all(b"QUIT\r\n").await?;
    let _ = expect_smtp_code(&mut rd, b"221").await;
    Ok(())
}

async fn expect_smtp_code<R: AsyncBufReadExt + Unpin>(rd: &mut R, code: &[u8]) -> Result<()> {
    let mut line = String::new();
    rd.read_line(&mut line).await?;
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
        rd.read_line(&mut line).await?;
        let bytes = line.as_bytes();
        if !bytes.starts_with(code) {
            return Err(anyhow!(
                "SMTP expected {} got {:?}",
                String::from_utf8_lossy(code),
                line
            ));
        }
        // SMTP multiline: `<code>-` means continuation, `<code> ` means final line.
        match bytes.get(code.len()) {
            Some(b' ') => return Ok(()),
            Some(b'-') => continue,
            _ => {
                return Err(anyhow!(
                    "SMTP malformed reply for {}: {:?}",
                    String::from_utf8_lossy(code),
                    line
                ));
            }
        }
    }
}
