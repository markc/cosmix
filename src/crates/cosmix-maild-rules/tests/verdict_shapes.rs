//! Tests verdict-shape priority: allowlist > blocklist > mail-auth
//! hard-fail > structural anomaly > score breach > continue. Plus
//! shadow-mode downgrade behavior.

mod common;

use cosmix_maild_rules::{
    AcceptReason, AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, JunkReason,
    RuleEngine, RuleVerdict,
};

use common::*;

const PACK_V1: &str = include_str!("../rules/default.conf.mix");

fn engine_with_config(config: EngineConfig) -> DefaultRuleEngine {
    DefaultRuleEngine::with_pack_str(config, PACK_V1)
        .expect("pack parses")
        .0
}

#[tokio::test]
async fn allowlist_short_circuits_to_hard_accept() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = spf_fail_dmarc_reject(); // would otherwise hard-fail
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["sender@example.invalid".into()],
        ..Default::default()
    };
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::HardAccept { reason, .. } = verdict else {
        panic!("expected HardAccept, got {verdict:?}");
    };
    assert_eq!(reason, AcceptReason::AllowlistSender);
}

#[tokio::test]
async fn blocklist_produces_hard_junk() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        blocklist_senders: vec!["sender@example.invalid".into()],
        ..Default::default()
    };
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::HardJunk { reason, .. } = verdict else {
        panic!("expected HardJunk, got {verdict:?}");
    };
    assert_eq!(reason, JunkReason::BlocklistSender);
}

#[tokio::test]
async fn mail_auth_spf_fail_dmarc_reject_hard_junks() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = spf_fail_dmarc_reject();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let mut ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::HardJunk {
        reason,
        matched_rules,
        ..
    } = verdict
    else {
        panic!("expected HardJunk, got {verdict:?}");
    };
    assert_eq!(reason, JunkReason::MailAuthHardFail);
    assert!(
        matched_rules
            .iter()
            .any(|id| id == "mail_auth.spf_fail_dmarc_reject")
    );

    ctx.sender_authenticated = true;
    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::Continue { matched_rules, .. } = verdict else {
        panic!("expected ordinary rules path, got {verdict:?}");
    };
    assert!(matched_rules.iter().any(|id| id == "spf_fail_soft"));
    assert!(
        !matched_rules
            .iter()
            .any(|id| id == "mail_auth.spf_fail_dmarc_reject")
    );
}

#[tokio::test]
async fn shadow_mode_downgrades_hard_junk_to_continue() {
    let config = EngineConfig {
        shadow_mode: true,
        ..EngineConfig::default()
    };
    let engine = engine_with_config(config);

    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        blocklist_senders: vec!["sender@example.invalid".into()],
        ..Default::default()
    };
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::Continue { would_junk, .. } = verdict else {
        panic!("expected Continue (shadow-downgraded), got {verdict:?}");
    };
    assert!(would_junk, "shadow mode should set would_junk = true");
}

#[tokio::test]
async fn shadow_mode_preserves_hard_accept() {
    let config = EngineConfig {
        shadow_mode: true,
        ..EngineConfig::default()
    };
    let engine = engine_with_config(config);

    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides {
        allowlist_senders: vec!["sender@example.invalid".into()],
        ..Default::default()
    };
    let ctx = ctx(HAM, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    assert!(
        matches!(verdict, RuleVerdict::HardAccept { .. }),
        "HardAccept should survive shadow mode, got {verdict:?}"
    );
}

#[tokio::test]
async fn structural_anomaly_executable_plus_auth_fail() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = dkim_fail();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();

    // Crafted multipart with an attachment named "evil.exe".
    let msg = b"From: a@b.c\r\n\
To: y@example.invalid\r\n\
Subject: file\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <e@b.c>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=BOUNDARY\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain\r\n\
\r\n\
See attached.\r\n\
--BOUNDARY\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"evil.exe\"\r\n\
\r\n\
MZbinary\r\n\
--BOUNDARY--\r\n";
    let mut ctx = ctx(msg, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::HardJunk { reason, .. } = verdict else {
        panic!("expected HardJunk, got {verdict:?}");
    };
    assert_eq!(reason, JunkReason::StructuralAnomaly);

    ctx.sender_authenticated = true;
    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::Continue { matched_rules, .. } = verdict else {
        panic!("expected ordinary rules path, got {verdict:?}");
    };
    assert!(
        matched_rules
            .iter()
            .any(|id| id == "executable_attachment_present")
    );
    assert!(matched_rules.iter().any(|id| id == "dkim_fail_soft"));
}

#[tokio::test]
async fn score_breach_when_raw_score_above_hard_junk_threshold() {
    // Pin hard_junk_threshold low so a single rule trips it.
    let config = EngineConfig {
        hard_junk_threshold: 3.0,
        ..EngineConfig::default()
    };
    let engine = engine_with_config(config);

    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    // ALL_CAPS_SUBJECT + missing_message_id... actually has Message-ID.
    // Use a body with crypto solicitation language to get weight=3.
    let msg = b"From: a@b.c\r\n\
To: y@example.invalid\r\n\
Subject: news\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <c@b.c>\r\n\
\r\n\
Send 0.1 BTC to my bitcoin wallet for payment.\r\n";
    let ctx = ctx(msg, &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let RuleVerdict::HardJunk { reason, .. } = verdict else {
        panic!("expected HardJunk via ScoreBreach, got {verdict:?}");
    };
    assert_eq!(reason, JunkReason::ScoreBreach);
}
