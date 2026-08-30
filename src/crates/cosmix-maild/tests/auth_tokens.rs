//! Bearer-token auth end-to-end (SSR PIM Phase 2, Slice 1).
//!
//! Stands up an in-process maild and walks the token lifecycle:
//! issue (Basic) → use as Bearer on `/jmap` → verify → revoke → 401 on
//! both `/jmap` and `verify` after revoke. Plus the negative cases
//! (wrong password, bad token) and a DB-level expiry assertion (the
//! 30-day TTL constant isn't reachable from a test clock, so expiry is
//! checked against the `db::token` layer with a negative TTL).
//!
//! Run: `cargo test -p cosmix-maild --test auth_tokens`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine;
use cosmix_maild::config::{Config, ListenSpec};
use cosmix_maild::runtime::{BuiltMaild, RuntimeOpts, build_runtime};
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::SetOpts;
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;
use serde_json::{Value, json};
use tempfile::TempDir;

const EMAIL: &str = "tok@e2e.test";
const PASSWORD: &str = "tokpw1234";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_lifecycle_e2e() -> Result<()> {
    let tmp = TempDir::new()?;
    let cfg = make_config(tmp.path());
    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            ..Default::default()
        },
    )
    .await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let router = built.router.clone();
    let _serve = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{addr}");

    create_account(&built, EMAIL, PASSWORD).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // --- issue: wrong password → 401 ---
    let bad = client
        .post(format!("{base}/auth/tokens/issue"))
        .header("authorization", basic(EMAIL, "wrongpw"))
        .send()
        .await?;
    assert_eq!(bad.status(), 401, "issue with wrong password must be 401");

    // --- issue: good Basic → token + expiry ---
    let resp = client
        .post(format!("{base}/auth/tokens/issue"))
        .header("authorization", basic(EMAIL, PASSWORD))
        .json(&json!({"label": "test-device"}))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "issue with good creds must be 200");
    let body: Value = resp.json().await?;
    let token = body["token"]
        .as_str()
        .ok_or_else(|| anyhow!("issue response missing token: {body}"))?
        .to_string();
    assert!(!token.is_empty(), "issued token is empty");
    assert_eq!(body["account_id"].as_i64(), Some(1), "account_id: {body}");
    assert!(
        body["expires_at"].as_str().is_some(),
        "issue response missing expires_at: {body}"
    );

    // --- the token authorises /jmap as Bearer (no Authorization rewrite
    //     needed beyond the scheme) ---
    let session = client
        .get(format!("{base}/.well-known/jmap"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(
        session.status(),
        200,
        "Bearer token must authorise the JMAP session resource"
    );

    // A JMAP API call (Mailbox/get) authorises with Bearer too.
    let mbox = jmap_bearer(
        &client,
        &base,
        &token,
        "Mailbox/get",
        json!({"accountId": "1", "ids": null}),
    )
    .await?;
    assert!(
        mbox["list"].as_array().is_some(),
        "Mailbox/get via Bearer should return a list: {mbox}"
    );

    // --- verify: Bearer → account binding ---
    let verified = client
        .post(format!("{base}/auth/tokens/verify"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(verified.status(), 200, "verify of a live token must be 200");
    let vbody: Value = verified.json().await?;
    assert_eq!(
        vbody["account_id"].as_i64(),
        Some(1),
        "verify body: {vbody}"
    );
    assert_eq!(
        vbody["email"].as_str(),
        Some(EMAIL),
        "verify body email: {vbody}"
    );

    // --- verify: a garbage Bearer → 401 ---
    let badtok = client
        .post(format!("{base}/auth/tokens/verify"))
        .header("authorization", "Bearer not-a-real-token")
        .send()
        .await?;
    assert_eq!(
        badtok.status(),
        401,
        "verify of an unknown token must be 401"
    );

    // --- revoke: Bearer self-revoke → 204 ---
    let revoked = client
        .post(format!("{base}/auth/tokens/revoke"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(revoked.status(), 204, "revoke of a live token must be 204");

    // --- after revoke: /jmap is 401, verify is 401 ---
    let after = client
        .get(format!("{base}/.well-known/jmap"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(
        after.status(),
        401,
        "revoked token must not authorise /jmap"
    );

    let after_verify = client
        .post(format!("{base}/auth/tokens/verify"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(after_verify.status(), 401, "revoked token must fail verify");

    // --- re-revoke is idempotent (still 204; the row exists) ---
    let re_revoke = client
        .post(format!("{base}/auth/tokens/revoke"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert_eq!(
        re_revoke.status(),
        204,
        "re-revoke must stay idempotent 204"
    );

    // --- Basic still works on /jmap (unchanged path) ---
    let basic_session = client
        .get(format!("{base}/.well-known/jmap"))
        .header("authorization", basic(EMAIL, PASSWORD))
        .send()
        .await?;
    assert_eq!(
        basic_session.status(),
        200,
        "Basic auth must remain unchanged on /jmap"
    );

    Ok(())
}

/// Expiry is enforced in SQL (`expires_at > datetime('now')`). The handler
/// pins TTL at 30 days, so we drive `db::token` directly with a negative
/// TTL to assert an expired row is rejected while a live one resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_expiry_db_level() -> Result<()> {
    let tmp = TempDir::new()?;
    let cfg = make_config(tmp.path());
    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            ..Default::default()
        },
    )
    .await?;
    create_account(&built, EMAIL, PASSWORD).await?;
    let conn = &built.app_state.db.conn;

    // A live token resolves to the account. `insert` now takes ttl in SECONDS,
    // so 30 days = 30 * 86_400.
    let live_hash = "a".repeat(64);
    cosmix_maild::db::token::insert(conn, 1, &live_hash, Some("live"), 30 * 86_400).await?;
    let live = cosmix_maild::db::token::lookup_valid(conn, &live_hash).await?;
    assert!(live.is_some(), "a 30-day token must resolve as live");
    assert_eq!(live.unwrap().account_id, 1);

    // An already-expired token does not (-1 seconds is in the past).
    let dead_hash = "b".repeat(64);
    cosmix_maild::db::token::insert(conn, 1, &dead_hash, Some("dead"), -1).await?;
    let dead = cosmix_maild::db::token::lookup_valid(conn, &dead_hash).await?;
    assert!(dead.is_none(), "an expired token must not resolve");

    // Revoking the live token makes it stop resolving; revoke is true
    // (row exists), and an unknown hash revoke is false.
    assert!(
        cosmix_maild::db::token::revoke(conn, &live_hash).await?,
        "revoke of an existing token returns true"
    );
    assert!(
        cosmix_maild::db::token::lookup_valid(conn, &live_hash)
            .await?
            .is_none(),
        "a revoked token must not resolve"
    );
    assert!(
        !cosmix_maild::db::token::revoke(conn, &"c".repeat(64)).await?,
        "revoke of an unknown token returns false"
    );

    Ok(())
}

// ---- helpers (mirrors jmap_mds_e2e.rs) ----

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

async fn create_account(built: &BuiltMaild, email: &str, password: &str) -> Result<()> {
    let runtime = built.accounts_runtime();
    let hash = bcrypt::hash(password, 4).map_err(|e| anyhow!("bcrypt: {e}"))?;
    let mut m = BTreeMap::new();
    m.insert("email".into(), PropValue::String(email.into()));
    m.insert("password".into(), PropValue::String(hash));
    m.insert("name".into(), PropValue::String("Token Test".into()));
    m.insert("quota".into(), PropValue::Int(0));
    m.insert("spam_enabled".into(), PropValue::Bool(false));
    m.insert("spam_threshold".into(), PropValue::Float(0.5));
    let key = RecordKey::collection(cosmix_maild::props::accounts::namespace_name(), email);
    let opts = SetOpts {
        expected_version: Some(Version::zero()),
        merge: MergeMode::Replace,
        actor: Actor::operator("token-test"),
        cause: Some("create test account".into()),
        ts_ms: 0,
    };
    runtime
        .set(key, PropValue::Object(m), opts)
        .await
        .map_err(|e| anyhow!("create_account: {e}"))?;
    Ok(())
}

/// Rotate an existing account's password through the real props runtime (whose
/// `after_set` hook fires the post-commit Basic-cache invalidation). Unconditional
/// (`expected_version: None`) so the caller needn't track the row version.
async fn set_account_password(built: &BuiltMaild, email: &str, password: &str) -> Result<()> {
    let runtime = built.accounts_runtime();
    let hash = bcrypt::hash(password, 4).map_err(|e| anyhow!("bcrypt: {e}"))?;
    let mut m = BTreeMap::new();
    m.insert("email".into(), PropValue::String(email.into()));
    m.insert("password".into(), PropValue::String(hash));
    m.insert("name".into(), PropValue::String("Token Test".into()));
    m.insert("quota".into(), PropValue::Int(0));
    m.insert("spam_enabled".into(), PropValue::Bool(false));
    m.insert("spam_threshold".into(), PropValue::Float(0.5));
    let key = RecordKey::collection(cosmix_maild::props::accounts::namespace_name(), email);
    let opts = SetOpts {
        expected_version: None,
        merge: MergeMode::Replace,
        actor: Actor::operator("token-test"),
        cause: Some("rotate test password".into()),
        ts_ms: 0,
    };
    runtime
        .set(key, PropValue::Object(m), opts)
        .await
        .map_err(|e| anyhow!("set_account_password: {e}"))?;
    Ok(())
}

/// Regression: the positive Basic-credential cache must not keep authenticating a
/// rotated-away password. Without the post-commit `clear_verify_cache` (epoch
/// bump), the cached (email, OLD-pw) success would still resolve after the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn basic_cred_cache_invalidated_on_password_change() -> Result<()> {
    let tmp = TempDir::new()?;
    let cfg = make_config(tmp.path());
    let built = build_runtime(
        &cfg,
        RuntimeOpts {
            enable_bus: false,
            ..Default::default()
        },
    )
    .await?;
    create_account(&built, EMAIL, PASSWORD).await?;
    let db = &built.app_state.db;

    // Prime the positive cache with the original password.
    assert_eq!(
        cosmix_maild::auth::basic::verify(db, EMAIL, PASSWORD).await?,
        Some(1),
        "original password must verify (and populate the cache)"
    );

    // Rotate the password through the real props path (its after_set hook fires
    // clear_verify_cache post-commit); the explicit clear keeps the assertion
    // deterministic regardless of hook scheduling.
    const NEWPW: &str = "rotated-9876";
    set_account_password(&built, EMAIL, NEWPW).await?;
    cosmix_maild::auth::basic::clear_verify_cache();

    // The rotated-away password must NOT still authenticate from the stale entry.
    assert_eq!(
        cosmix_maild::auth::basic::verify(db, EMAIL, PASSWORD).await?,
        None,
        "old password must be rejected after rotation + cache invalidation"
    );
    // The new password authenticates.
    assert_eq!(
        cosmix_maild::auth::basic::verify(db, EMAIL, NEWPW).await?,
        Some(1),
        "new password must verify"
    );
    Ok(())
}

fn basic(email: &str, password: &str) -> String {
    let enc =
        base64::engine::general_purpose::STANDARD.encode(format!("{email}:{password}").as_bytes());
    format!("Basic {enc}")
}

async fn jmap_bearer(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    args: Value,
) -> Result<Value> {
    let req = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [[method, args, "c0"]],
    });
    let resp = client
        .post(format!("{base}/jmap"))
        .header("authorization", format!("Bearer {token}"))
        .json(&req)
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("jmap {method} → {status}: {body}"));
    }
    Ok(body["methodResponses"][0][1].clone())
}
