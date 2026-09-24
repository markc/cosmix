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

fn with_subject(subject: &str) -> Vec<u8> {
    format!(
        "From: accounts@example.invalid\r\n\
To: y@example.invalid\r\n\
Subject: {subject}\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <c@example.invalid>\r\n\
\r\n\
Please review the outstanding balance on your account.\r\n"
    )
    .into_bytes()
}

/// The 2026-09-16 pending-account campaign scored 0.07-0.16 in Bayes. The
/// subject fingerprint alone must hard-junk it under the DEFAULT engine
/// config, because a Continue score cannot move routing while
/// `rules_score_bias_k` is 0.
#[tokio::test]
async fn scam_account_reference_subject_hard_junks_the_campaign() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    for subject in [
        "Pending Account Matter-7G4K2Q",
        "Account Settlement Follow-Up-X9B2KD7",
        "Follow-Up on Account Status-ab12cd",
    ] {
        let msg = with_subject(subject);
        let verdict = engine
            .classify(&ctx(&msg, &auth, &account, &rcpts, &ov))
            .await
            .unwrap();
        let RuleVerdict::HardJunk {
            reason,
            matched_rules,
            ..
        } = verdict
        else {
            panic!("{subject:?}: expected HardJunk, got {verdict:?}");
        };
        assert_eq!(reason, JunkReason::ScoreBreach, "{subject:?}");
        assert!(
            matched_rules
                .iter()
                .any(|r| r == "scam_account_reference_subject"),
            "{subject:?}: {matched_rules:?}"
        );
    }
}

/// Legitimate account mail is spaced, digits-only, or a plain word after the
/// hyphen; none of it may reach the rule.
#[tokio::test]
async fn scam_account_reference_subject_spares_ordinary_account_mail() {
    let engine = engine_with_config(EngineConfig::default());
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    for subject in [
        "Account status - ref 12345",
        "Account Status-Update",
        "Pending Account Matter-20260916",
        "Your account statement for September",
        "Account Status-Q3",
        "Re: Account Status-Q3 review",
        // Billing/ticket references that an earlier, broader pattern junked.
        "Account Status-INV2024",
        "Account Settlement-2024Q3",
        "Account Status-FY2026",
        "Account Follow-Up-TKT99812",
        "Account Status-Win10",
        "account status-covid19",
        "Account status-Level5",
        "Your account status-Update2",
    ] {
        let msg = with_subject(subject);
        let exp = engine
            .explain(&ctx(&msg, &auth, &account, &rcpts, &ov))
            .await
            .unwrap();
        let hit = exp
            .rules
            .iter()
            .find(|r| r.id == "scam_account_reference_subject")
            .expect("rule evaluated");
        assert!(!hit.matched, "{subject:?} must not match");
    }
}

/// The rule is meant to junk on its own. Pin its weight to the default
/// hard_junk_threshold so raising the threshold cannot silently turn it into
/// a Continue score, which routes nothing while rules_score_bias_k is 0.
#[tokio::test]
async fn scam_account_reference_subject_weight_equals_hard_junk_threshold() {
    let config = EngineConfig::default();
    let engine = engine_with_config(config.clone());
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let msg = with_subject("Pending Account Matter-7G4K2Q");
    let exp = engine
        .explain(&ctx(&msg, &auth, &account, &rcpts, &ov))
        .await
        .unwrap();
    let hit = exp
        .rules
        .iter()
        .find(|r| r.id == "scam_account_reference_subject")
        .expect("rule evaluated");
    assert!(hit.matched);
    assert_eq!(hit.configured_weight as f32, config.hard_junk_threshold);
}
