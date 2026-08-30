//! Scoring arithmetic — composite raw_score from multi-match, threshold
//! recorded on Explanation, per-account threshold override.

mod common;

use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleEngine,
};

use common::*;

const PACK_V1: &str = include_str!("../rules/default.conf.mix");

#[tokio::test]
async fn explanation_records_per_rule_contributions() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx = ctx(NO_MESSAGE_ID, &auth, &account, &rcpts, &ov);

    let exp = engine.explain(&ctx).await.unwrap();
    let missing = exp
        .rules
        .iter()
        .find(|r| r.id == "missing_message_id")
        .expect("rule present");
    assert!(missing.matched);
    assert_eq!(missing.configured_weight, 2);
    assert_eq!(missing.contribution, 2.0);
    assert!(exp.total_score >= 2.0);
}

#[tokio::test]
async fn threshold_override_replaces_engine_threshold() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        threshold_override: Some(8.0),
        ..Default::default()
    };
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let exp = engine.explain(&ctx).await.unwrap();
    assert_eq!(exp.threshold, 8.0);
}

#[tokio::test]
async fn disabled_rule_does_not_contribute() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        disabled_rules: vec!["missing_message_id".into()],
        ..Default::default()
    };
    let ctx = ctx(NO_MESSAGE_ID, &auth, &account, &rcpts, &ov);

    let exp = engine.explain(&ctx).await.unwrap();
    let missing = exp.rules.iter().find(|r| r.id == "missing_message_id");
    // Disabled rules are skipped — they don't appear in the explanation.
    assert!(
        missing.is_none(),
        "disabled rule should not appear in explanation: {missing:?}"
    );
    // Other ham-on-clean-message rules should also be absent / not matched.
    assert!(
        exp.total_score < 2.0,
        "score should not include disabled rule's weight: {}",
        exp.total_score
    );
}
