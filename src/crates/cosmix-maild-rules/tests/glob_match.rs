//! Sender-glob matching for allowlist / blocklist patterns.

mod common;

use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleEngine, RuleVerdict,
};

use common::*;

const PACK_V1: &str = include_str!("../rules/default.conf.mix");

async fn engine() -> DefaultRuleEngine {
    DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1)
        .unwrap()
        .0
}

#[tokio::test]
async fn literal_pattern_matches_exact_address() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["alice@example.com".into()],
        ..Default::default()
    };
    let ctx = ctx_from("alice@example.com", HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(matches!(verdict, RuleVerdict::HardAccept { .. }));
}

#[tokio::test]
async fn literal_pattern_is_case_insensitive() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["Alice@Example.COM".into()],
        ..Default::default()
    };
    let ctx = ctx_from("alice@example.com", HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(matches!(verdict, RuleVerdict::HardAccept { .. }));
}

#[tokio::test]
async fn wildcard_pattern_matches_domain() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["*@vendor.example".into()],
        ..Default::default()
    };
    let ctx = ctx_from("billing@vendor.example", HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(matches!(verdict, RuleVerdict::HardAccept { .. }));
}

#[tokio::test]
async fn wildcard_pattern_does_not_match_other_domains() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["*@vendor.example".into()],
        ..Default::default()
    };
    let ctx = ctx_from(
        "billing@othervendor.example",
        HAM,
        &auth,
        &account,
        &rcpts,
        &ov,
    );

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(
        !matches!(verdict, RuleVerdict::HardAccept { .. }),
        "should not allowlist different domain, got {verdict:?}"
    );
}

#[tokio::test]
async fn allowlist_takes_precedence_over_blocklist() {
    let engine = engine().await;
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["alice@example.com".into()],
        blocklist_senders: vec!["alice@example.com".into()],
        ..Default::default()
    };
    let ctx = ctx_from("alice@example.com", HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(
        matches!(verdict, RuleVerdict::HardAccept { .. }),
        "allowlist > blocklist; got {verdict:?}"
    );
}
