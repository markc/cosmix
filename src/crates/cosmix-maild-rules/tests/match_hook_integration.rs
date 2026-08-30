//! Integration test for the per-rule match-hook plumbing (commit 3 of
//! `_doc/planned/maild-rules-phase2.md`).
//!
//! Asserts the spec invariants:
//!
//! - The hook fires once per matched rule per `classify` call.
//! - The hook fires for the implicit `mime_parse_error` rule.
//! - The hook does NOT fire for rules whose `matched` flag is false.
//! - The hook does NOT fire from `explain` — debug invocations must
//!   never inflate per-rule stats.
//! - The hook does NOT fire for disabled rules (account override).
//! - `None` hook is zero-cost (no panic, no spurious side effects).

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;

use cosmix_maild_auth::{
    Alignment, ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DkimOutcome,
    DmarcDisposition, DmarcOutcome, DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome,
    IprevResult, SpfCheck, SpfResult, VerifyResult,
};
use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleContext, RuleEngine, RuleId,
    RuleMatchHook,
};

fn clean_auth() -> VerifyResult {
    let peer = "203.0.113.1".parse::<IpAddr>().unwrap();
    VerifyResult {
        spf: SpfCheck::MailFrom {
            result: SpfResult::Pass,
            domain: "example.test".into(),
        },
        iprev: IprevResult {
            result: IprevOutcome::Pass,
            ptr: Some("smoke.example.test".into()),
            matched_forward: Some(peer),
        },
        dkim: DkimAggregate {
            signatures: Vec::new(),
            overall: DkimOutcome::None,
            capped: false,
        },
        dmarc: DmarcResult {
            outcome: DmarcOutcome::None,
            report_record: DmarcReportRecord {
                org_domain: "example.test".into(),
                source_ip: peer,
                policy_published: DmarcPolicy::None,
                policy_evaluated: DmarcPolicy::None,
                spf_aligned: true,
                dkim_aligned: false,
                disposition: DmarcDisposition::None,
                count: 1,
            },
        },
        arc: ArcResult {
            chain_validation: ArcChainValidation::None,
            instance_count: 0,
            oldest_pass_chain: false,
        },
        authentication_results_header: AuthResultsHeader {
            host_identity: "smoke.example.test".into(),
            rendered: "Authentication-Results: smoke.example.test; spf=pass".into(),
            spf: None,
            iprev: None,
            dkim: Vec::new(),
            dmarc: None,
            arc: None,
        },
    }
}

/// Two non-DNSBL rules so the test doesn't need a FakeDnsbl. One
/// matches the synthetic message, one doesn't.
fn pack_two_rules() -> &'static str {
    r#"
pack_version: "test-hook-1"
rule: [
  { id: "subject_present", description: "Subject header present", weight: 1,
    match: { kind: "header_present", name: "Subject" } },
  { id: "x_spam_present", description: "X-Spam header present", weight: 2,
    match: { kind: "header_present", name: "X-Spam" } }
]
"#
}

fn message_with_subject_only() -> Vec<u8> {
    // Has Subject (matches subject_present), no X-Spam (x_spam_present
    // is unmatched).
    b"From: sender@example.test\r\nTo: rcpt@alpha.local\r\nSubject: hi\r\n\r\nbody\r\n".to_vec()
}

fn ctx_for<'a>(
    msg: &'a [u8],
    verify: &'a VerifyResult,
    account: &'a AccountId,
    overrides: &'a AccountOverrides,
    rcpts: &'a [String],
) -> RuleContext<'a> {
    RuleContext {
        message: msg,
        peer_ip: "203.0.113.1".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: rcpts,
        mail_auth: verify,
        sender_authenticated: false,
        account,
        overrides,
    }
}

type RecorderLog = Arc<Mutex<Vec<RuleId>>>;

/// Recorder that captures every RuleId the hook is called with.
fn recorder() -> (RecorderLog, RuleMatchHook) {
    let log: RecorderLog = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    let hook: RuleMatchHook = Arc::new(move |id: &RuleId| log2.lock().unwrap().push(id.clone()));
    (log, hook)
}

#[tokio::test]
async fn hook_fires_once_per_matched_rule_on_classify() {
    let (log, hook) = recorder();
    let (engine, failed) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    assert!(failed.is_empty(), "rule pack compiled: {failed:?}");
    let engine = engine.with_rule_match_hook(hook);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    let _ = engine.classify(&ctx).await.unwrap();
    let calls = log.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec!["subject_present".to_string()],
        "hook fired exactly once for the matched rule",
    );
}

#[tokio::test]
async fn hook_does_not_fire_from_explain() {
    let (log, hook) = recorder();
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    let engine = engine.with_rule_match_hook(hook);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    let _ = engine.explain(&ctx).await.unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "explain() must not fire the per-rule hook (would inflate stats)",
    );
}

#[tokio::test]
async fn hook_fires_for_mime_parse_error() {
    let (log, hook) = recorder();
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    let engine = engine.with_rule_match_hook(hook);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    // A non-UTF8 / unparseable message body triggers mime.parse_failed.
    // The mail_parser crate is fairly tolerant; an empty buffer is the
    // most reliable way to force a parse failure across versions.
    let msg: Vec<u8> = Vec::new();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    let _ = engine.classify(&ctx).await.unwrap();
    let calls = log.lock().unwrap().clone();
    // If the parser ever stops treating an empty buffer as a parse
    // failure, this assertion will catch the regression — the
    // mime_parse_error implicit rule and its hook are tied together.
    assert!(
        calls.iter().any(|r| r == "mime_parse_error"),
        "hook must fire for the implicit mime_parse_error rule; got {calls:?}",
    );
}

#[tokio::test]
async fn hook_does_not_fire_for_disabled_rule() {
    let (log, hook) = recorder();
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    let engine = engine.with_rule_match_hook(hook);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides {
        disabled_rules: vec!["subject_present".to_string()],
        ..AccountOverrides::default()
    };
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    let _ = engine.classify(&ctx).await.unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "disabled rule must be skipped — hook must NOT fire for it",
    );
}

#[tokio::test]
async fn no_hook_installed_is_zero_cost() {
    // Engine without with_rule_match_hook — classify must succeed and
    // produce the expected verdict. This is the default for the smoke
    // binary and for crates that don't want counters.
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    // No .with_rule_match_hook(...).

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    // Just exercise both paths — no panic = pass.
    let _ = engine.classify(&ctx).await.unwrap();
    let _ = engine.explain(&ctx).await.unwrap();
}

/// Verdict-shape short-circuits (allowlist / blocklist /
/// mail-auth hard-fail) are engine decisions, not pack-rule matches.
/// Their synthetic verdict IDs surface on the verdict (allowlist /
/// blocklist as `matched_rules` entries; mail-auth hard-fail also as
/// a synthetic `RuleExplanation { matched: true }` row), but they
/// must NOT fire the per-rule match hook. A future refactor that
/// moved hook firing to "anything in matched_rules output" or
/// "any RuleExplanation with matched=true" would silently start
/// counting synthetic IDs — this regression test pins the current
/// semantics so that bug class is caught immediately.
#[tokio::test]
async fn hook_does_not_fire_for_synthetic_short_circuits() {
    let pack = r#"
pack_version: "test-hook-synth-1"
rule: [
  { id: "subject_present", description: "Subject header present", weight: 1,
    match: { kind: "header_present", name: "Subject" } }
]
"#;

    let verify_clean = clean_auth();
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let account = AccountId::new("test");

    // --- allowlist short-circuit ---
    {
        let (log, hook) = recorder();
        let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack).unwrap();
        let engine = engine.with_rule_match_hook(hook);
        let overrides = AccountOverrides {
            allowlist_senders: vec!["sender@example.test".to_string()],
            ..AccountOverrides::default()
        };
        let ctx = ctx_for(&msg, &verify_clean, &account, &overrides, &rcpts);
        let _ = engine.classify(&ctx).await.unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "allowlist short-circuit must not fire the hook (engine decision, not a pack-rule match)",
        );
    }

    // --- blocklist short-circuit ---
    {
        let (log, hook) = recorder();
        let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack).unwrap();
        let engine = engine.with_rule_match_hook(hook);
        let overrides = AccountOverrides {
            blocklist_senders: vec!["sender@example.test".to_string()],
            ..AccountOverrides::default()
        };
        let ctx = ctx_for(&msg, &verify_clean, &account, &overrides, &rcpts);
        let _ = engine.classify(&ctx).await.unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "blocklist short-circuit must not fire the hook",
        );
    }

    // --- mail-auth hard-fail short-circuit ---
    {
        let (log, hook) = recorder();
        let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack).unwrap();
        let engine = engine.with_rule_match_hook(hook);
        let peer = "203.0.113.50".parse::<IpAddr>().unwrap();
        // SpfFailDmarcReject is in EngineConfig::default().mail_auth_hard_fail_kinds:
        // craft SPF Fail + DMARC published=Reject.
        let verify = VerifyResult {
            spf: SpfCheck::MailFrom {
                result: SpfResult::Fail,
                domain: "example.test".into(),
            },
            iprev: IprevResult {
                result: IprevOutcome::Pass,
                ptr: Some("smoke.example.test".into()),
                matched_forward: Some(peer),
            },
            dkim: DkimAggregate {
                signatures: Vec::new(),
                overall: DkimOutcome::None,
                capped: false,
            },
            dmarc: DmarcResult {
                outcome: DmarcOutcome::Fail {
                    policy: DmarcPolicy::Reject,
                    alignment: Alignment::NotAligned,
                },
                report_record: DmarcReportRecord {
                    org_domain: "example.test".into(),
                    source_ip: peer,
                    policy_published: DmarcPolicy::Reject,
                    policy_evaluated: DmarcPolicy::Reject,
                    spf_aligned: false,
                    dkim_aligned: false,
                    disposition: DmarcDisposition::Reject,
                    count: 1,
                },
            },
            arc: ArcResult {
                chain_validation: ArcChainValidation::None,
                instance_count: 0,
                oldest_pass_chain: false,
            },
            authentication_results_header: AuthResultsHeader {
                host_identity: "smoke.example.test".into(),
                rendered: "Authentication-Results: smoke.example.test; spf=fail".into(),
                spf: None,
                iprev: None,
                dkim: Vec::new(),
                dmarc: None,
                arc: None,
            },
        };
        let overrides = AccountOverrides::default();
        let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);
        let _ = engine.classify(&ctx).await.unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "mail-auth hard-fail short-circuit must not fire the hook",
        );
    }
}

#[tokio::test]
async fn set_rule_match_hook_can_detach() {
    let (log, hook) = recorder();
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), pack_two_rules()).unwrap();
    let mut engine = engine.with_rule_match_hook(hook);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message_with_subject_only();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = ctx_for(&msg, &verify, &account, &overrides, &rcpts);

    let _ = engine.classify(&ctx).await.unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);

    // Detach.
    engine.set_rule_match_hook(None);
    let _ = engine.classify(&ctx).await.unwrap();
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "after detach the recorder must not grow",
    );
}
