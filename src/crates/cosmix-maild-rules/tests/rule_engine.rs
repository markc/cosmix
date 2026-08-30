//! Per-kind smoke tests. One positive case per kind, plus a few
//! negative controls to confirm matchers don't fire on clean ham.

mod common;

use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleEngine, RuleVerdict,
};

use common::*;

const PACK_V1: &str = include_str!("../rules/default.conf.mix");

async fn engine() -> DefaultRuleEngine {
    let (eng, failed) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1)
        .expect("default pack parses");
    assert!(
        failed.is_empty(),
        "default pack compiles cleanly: {failed:?}"
    );
    eng
}

fn matched_rules(verdict: &RuleVerdict) -> Vec<String> {
    match verdict {
        RuleVerdict::HardAccept { matched_rules, .. }
        | RuleVerdict::HardJunk { matched_rules, .. }
        | RuleVerdict::Continue { matched_rules, .. } => matched_rules.clone(),
    }
}

#[tokio::test]
async fn ham_message_continues_with_zero_score() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["bob@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::Continue {
        score,
        matched_rules,
        would_junk,
    } = verdict
    else {
        panic!("expected Continue, got {verdict:?}");
    };
    assert_eq!(score, 0.0);
    assert!(matched_rules.is_empty(), "ham matched: {matched_rules:?}");
    assert!(!would_junk);
}

#[tokio::test]
async fn header_absent_fires_on_missing_message_id() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(NO_MESSAGE_ID, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "missing_message_id"),
        "expected missing_message_id, got {matched:?}"
    );
}

#[tokio::test]
async fn header_regex_fires_on_all_caps_subject() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["target@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(ALL_CAPS_SUBJECT, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "all_uppercase_subject"),
        "expected all_uppercase_subject, got {matched:?}"
    );
}

#[tokio::test]
async fn structure_html_only_fires_for_html_only_message() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(HTML_ONLY, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "html_only_no_text"),
        "expected html_only_no_text, got {matched:?}"
    );
}

#[tokio::test]
async fn body_regex_fires_on_phishing_language() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["v@example.invalid".into()];
    let ov = AccountOverrides::default();
    let msg = b"From: a@b.c\r\n\
To: v@example.invalid\r\n\
Subject: notice\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <p@b.c>\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Please verify your password to continue.</p></body></html>\r\n";
    let ctx = ctx(msg, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "phishing_password_reset"),
        "expected phishing_password_reset, got {matched:?}"
    );
}

#[tokio::test]
async fn url_count_fires_above_threshold() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["x@example.invalid".into()];
    let ov = AccountOverrides::default();

    let mut body = String::from("Visit:\r\n");
    for i in 0..30 {
        body.push_str(&format!("https://example{i}.invalid/page\r\n"));
    }
    let raw = format!(
        "From: a@b.c\r\nTo: x@example.invalid\r\nSubject: links\r\n\
         Date: Mon, 27 Apr 2026 10:00:00 +0000\r\nMessage-ID: <u@b.c>\r\n\
         \r\n{body}"
    );
    let ctx = ctx(raw.as_bytes(), &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "url_count_high"),
        "expected url_count_high, got {matched:?}"
    );
}

#[tokio::test]
async fn recipient_count_fires_above_threshold() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = (0..30).map(|i| format!("r{i}@example.invalid")).collect();
    let ov = AccountOverrides::default();
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "many_recipients"),
        "expected many_recipients, got {matched:?}"
    );
}

#[tokio::test]
async fn mail_auth_spf_fail_contributes_score() {
    let engine = engine().await;
    let auth = spf_fail();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = matched_rules(&verdict);
    assert!(
        matched.iter().any(|m| m == "spf_fail_soft"),
        "expected spf_fail_soft (engine-level hard-fail not triggered without DMARC reject), got {matched:?}"
    );
}
