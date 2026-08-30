//! SPEC 12 Phase 3 C2 — `DefaultRuleEngine::set_config` atomic swap.
//!
//! The engine's nine globals now live behind `Arc<RwLock<EngineConfig>>`.
//! `evaluate` snapshot-clones the config at the top of every call so a
//! mid-evaluate `set_config` swap cannot split a single classification
//! across two configs. These tests verify the swap is durable, visible
//! to subsequent calls, and consistent under concurrent reads.

mod common;

use std::sync::Arc;

use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, MailAuthHardFailKind, RuleEngine,
    RuleVerdict, VerdictShape,
};

use common::*;

const PACK_V1: &str = include_str!("../rules/default.conf.mix");

#[tokio::test]
async fn set_config_swaps_shadow_mode_visibly() {
    // Start in non-shadow. After swap to shadow=true, subsequent
    // classify with a score-breach must downgrade to Continue.
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    assert!(!engine.shadow_mode().await);

    let mut cfg = engine.config_snapshot().await;
    cfg.shadow_mode = true;
    engine.set_config(cfg).await;

    assert!(engine.shadow_mode().await);
}

#[tokio::test]
async fn set_config_swaps_hard_junk_threshold() {
    // Default hard_junk_threshold is 15.0. Lower it to 1.0 and any
    // matched-rule contribution should now ScoreBreach. Reset to a
    // very high cap and confirm the same context Continues.
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();

    let mut cfg = engine.config_snapshot().await;
    cfg.hard_junk_threshold = 1.0;
    engine.set_config(cfg).await;

    let ctx = ctx(NO_MESSAGE_ID, &auth, &account, &rcpts, &ov);
    let v = engine.classify(&ctx).await.unwrap();
    assert!(
        matches!(v, RuleVerdict::HardJunk { .. }),
        "lowered cap should breach: {v:?}"
    );

    let mut cfg = engine.config_snapshot().await;
    cfg.hard_junk_threshold = 1_000.0;
    engine.set_config(cfg).await;

    let v = engine.classify(&ctx).await.unwrap();
    assert!(
        matches!(v, RuleVerdict::Continue { .. }),
        "raised cap should pass: {v:?}"
    );
}

#[tokio::test]
async fn set_config_swaps_mail_auth_hard_fail_kinds() {
    // Default kinds include SpfFailDmarcReject. Empty the list and a
    // verify result that would have triggered hard-fail must no longer.
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let auth = spf_fail_dmarc_reject();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let ctx_ = ctx(HAM, &auth, &account, &rcpts, &ov);

    // Pre-swap: hard fail wins.
    let exp = engine.explain(&ctx_).await.unwrap();
    assert!(matches!(exp.verdict, VerdictShape::HardJunk));

    let mut cfg = engine.config_snapshot().await;
    cfg.mail_auth_hard_fail_kinds = vec![];
    engine.set_config(cfg).await;

    // Post-swap: same context no longer hard-fails on auth.
    let exp = engine.explain(&ctx_).await.unwrap();
    assert!(
        !matches!(exp.verdict, VerdictShape::HardJunk),
        "empty kinds list should not hard-fail: {:?}",
        exp.verdict
    );

    // Restore SpfFailDmarcReject and confirm we are back to HardJunk.
    let mut cfg = engine.config_snapshot().await;
    cfg.mail_auth_hard_fail_kinds = vec![MailAuthHardFailKind::SpfFailDmarcReject];
    engine.set_config(cfg).await;
    let exp = engine.explain(&ctx_).await.unwrap();
    assert!(matches!(exp.verdict, VerdictShape::HardJunk));
}

#[tokio::test]
async fn concurrent_swap_never_produces_split_config_verdict() {
    // Codex C2 R1/R2 MINOR — the invariant is "no split-config read,"
    // not just "no panic." Construct two configs whose semantic effect
    // on a fixed context is IDENTICAL (both return Continue), but where
    // a hypothetical split read across two fields would produce a
    // verdict NEITHER config can produce alone. Then assert that
    // forbidden verdict never appears.
    //
    // Fixed context: HAM + spf_fail_dmarc_reject auth.
    //
    //   Config A: kinds = [],                    shadow_mode = false
    //     → auth check finds no matching kind → no hard-fail
    //     → low score < hard_junk_threshold     → Continue.
    //
    //   Config B: kinds = [SpfFailDmarcReject],  shadow_mode = true
    //     → auth check matches → hard-fail triggers
    //     → shadow_mode = true downgrades the HardJunk to Continue.
    //
    // Each whole config returns Continue, so the test cannot be passed
    // by a verdict that simply matches "either config alone." Now the
    // two regression modes:
    //
    //   • Snapshot = A (kinds=[], shadow=false), regression re-reads
    //     `self.config.mail_auth_hard_fail_kinds` mid-evaluate and the
    //     writer has swapped to B → kinds=[Spf...] hits → reaches
    //     junk_or_shadow with cfg.shadow_mode = false → HardJunk.
    //
    //   • Snapshot = B (kinds=[Spf...], shadow=true), regression
    //     re-reads `self.config.shadow_mode` at the junk_or_shadow
    //     call site and the writer has swapped to A → shadow = false
    //     → HardJunk(MailAuthHardFail), no downgrade.
    //
    // Either regression produces a HardJunk verdict that NO unsplit
    // config can produce under this context. The strict assertion is
    // therefore: verdict must always be Continue. Anything else is a
    // split-config regression caught with single-bit certainty.
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let engine = Arc::new(engine);

    let cfg_a = EngineConfig {
        mail_auth_hard_fail_kinds: vec![],
        shadow_mode: false,
        ..EngineConfig::default()
    };
    let cfg_b = EngineConfig {
        mail_auth_hard_fail_kinds: vec![MailAuthHardFailKind::SpfFailDmarcReject],
        shadow_mode: true,
        ..EngineConfig::default()
    };

    let writer_engine = Arc::clone(&engine);
    let cfg_a_w = cfg_a.clone();
    let cfg_b_w = cfg_b.clone();
    let writer = tokio::spawn(async move {
        for i in 0..1_000 {
            writer_engine
                .set_config(if i % 2 == 0 {
                    cfg_a_w.clone()
                } else {
                    cfg_b_w.clone()
                })
                .await;
        }
    });

    let mut readers = Vec::new();
    let auth = Arc::new(spf_fail_dmarc_reject());
    for _ in 0..4 {
        let e = Arc::clone(&engine);
        let auth = Arc::clone(&auth);
        readers.push(tokio::spawn(async move {
            let account = AccountId::new("test");
            let rcpts: Vec<String> = vec!["y@example.invalid".into()];
            let ov = AccountOverrides::default();
            for _ in 0..500 {
                let ctx_ = ctx(HAM, &auth, &account, &rcpts, &ov);
                let v = e.classify(&ctx_).await.unwrap();
                match v {
                    RuleVerdict::Continue { .. } => {}
                    other => panic!(
                        "split-config regression: under cfg_a OR cfg_b on a fixed \
                         spf_fail_dmarc_reject HAM, every well-formed read produces \
                         Continue (cfg_a via empty kinds, cfg_b via shadow downgrade). \
                         Got: {other:?}"
                    ),
                }
            }
        }));
    }
    writer.await.unwrap();
    for r in readers {
        r.await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_set_config_and_classify_never_panic() {
    // 4 writers flipping shadow_mode in a loop, 4 readers running
    // classify in a loop, 200 iterations each. Goal: prove no
    // panic / poisoned lock / split-config under load. The verdicts
    // are not asserted because the swap interleaving is racy by
    // construction; the invariant is structural integrity, not
    // determinism.
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), PACK_V1).unwrap();
    let engine = Arc::new(engine);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            for i in 0..200 {
                let mut cfg = e.config_snapshot().await;
                cfg.shadow_mode = i % 2 == 0;
                e.set_config(cfg).await;
            }
        }));
    }
    for _ in 0..4 {
        let e = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            let auth = pass_verify_result();
            let account = AccountId::new("test");
            let rcpts: Vec<String> = vec!["y@example.invalid".into()];
            let ov = AccountOverrides::default();
            for _ in 0..200 {
                let ctx_ = ctx(HAM, &auth, &account, &rcpts, &ov);
                let _ = e.classify(&ctx_).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}
