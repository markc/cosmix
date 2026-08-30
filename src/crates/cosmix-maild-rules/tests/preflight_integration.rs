//! End-to-end integration test for the DNSBL preflight + matcher
//! path. Builds a `DefaultRuleEngine` with a fake `DnsblLookup`
//! installed and a rule pack containing `peer_ip_in_dnsbl` rules,
//! then exercises `classify` and `explain` against listed /
//! not-listed / broken peer IPs.
//!
//! Asserts:
//!
//! - A peer listed on the configured zone produces a matched rule.
//! - A peer that isn't listed produces no DNSBL match.
//! - A transient lookup failure (`LookupFailed`) is treated as
//!   no-match — false negatives are preferable to false positives.
//! - The matcher is wired into score accumulation: a matched DNSBL
//!   rule contributes its weight to `raw_score`.
//! - A rule with multiple zones matches if *any* zone returns
//!   `Listed`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use cosmix_maild_auth::{
    Alignment, ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DkimOutcome,
    DmarcDisposition, DmarcOutcome, DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome,
    IprevResult, SpfCheck, SpfResult, VerifyResult,
};
use cosmix_maild_rules::dnsbl::{DnsblLookup, DnsblResult};
use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleContext, RuleEngine,
    RuleVerdict,
};

/// Programmable fake — listed/not-listed verdicts keyed by (peer_ip, zone),
/// missing entries default to `LookupFailed` so we can test the
/// transient-failure no-match contract.
struct FakeDnsbl {
    verdicts: Mutex<HashMap<(IpAddr, String), DnsblResult>>,
    calls: AtomicU64,
}

impl FakeDnsbl {
    fn new() -> Self {
        Self {
            verdicts: Mutex::new(HashMap::new()),
            calls: AtomicU64::new(0),
        }
    }

    fn set(&self, ip: &str, zone: &str, verdict: DnsblResult) {
        let ip: IpAddr = ip.parse().unwrap();
        self.verdicts
            .lock()
            .unwrap()
            .insert((ip, zone.to_string()), verdict);
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DnsblLookup for FakeDnsbl {
    async fn is_listed(&self, ip: IpAddr, zone: &str) -> DnsblResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.verdicts
            .lock()
            .unwrap()
            .get(&(ip, zone.to_string()))
            .copied()
            .unwrap_or(DnsblResult::LookupFailed)
    }
    async fn reason(&self, _ip: IpAddr, _zone: &str) -> Option<String> {
        None
    }
}

/// Synthetic auth verdict that doesn't trip the mail-auth-hard-fail
/// short-circuit. SPF + DKIM + DMARC all pass cleanly. Shape mirrors
/// the smoke binary at `src/bin/cosmix-maild-rules-smoke.rs`.
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

/// Minimal pack that exercises only the DNSBL matcher.
fn dnsbl_only_pack() -> &'static str {
    r#"
pack_version: "test-dnsbl-1"
rule: [
  { id: "dnsbl_spamhaus", description: "Spamhaus ZEN", weight: 7,
    match: { kind: "peer_ip_in_dnsbl", zones: [ "zen.spamhaus.org" ] } },
  { id: "dnsbl_multi", description: "Multiple zones", weight: 4,
    match: { kind: "peer_ip_in_dnsbl", zones: [ "a.test", "b.test", "c.test" ] } }
]
"#
}

fn message() -> Vec<u8> {
    b"From: sender@example.test\r\nTo: rcpt@alpha.local\r\nSubject: hi\r\n\r\nbody\r\n".to_vec()
}

#[tokio::test]
async fn listed_peer_matches_dnsbl_rule() {
    let fake = Arc::new(FakeDnsbl::new());
    fake.set("203.0.113.5", "zen.spamhaus.org", DnsblResult::Listed);

    let (engine, failed) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    assert!(failed.is_empty(), "rule pack compiled: {failed:?}");
    let engine = engine.with_dnsbl(fake.clone() as Arc<dyn DnsblLookup>);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.5".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            would_junk,
        } => {
            assert_eq!(score, 7.0, "spamhaus rule weight contributed");
            assert!(
                matched_rules.iter().any(|r| r == "dnsbl_spamhaus"),
                "matched_rules contained dnsbl_spamhaus: {matched_rules:?}"
            );
            assert!(!would_junk);
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    assert!(fake.call_count() >= 1);
}

#[tokio::test]
async fn not_listed_peer_does_not_match() {
    let fake = Arc::new(FakeDnsbl::new());
    fake.set("203.0.113.6", "zen.spamhaus.org", DnsblResult::NotListed);
    fake.set("203.0.113.6", "a.test", DnsblResult::NotListed);
    fake.set("203.0.113.6", "b.test", DnsblResult::NotListed);
    fake.set("203.0.113.6", "c.test", DnsblResult::NotListed);

    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    let engine = engine.with_dnsbl(fake.clone() as Arc<dyn DnsblLookup>);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.6".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => {
            assert_eq!(score, 0.0);
            assert!(
                !matched_rules.iter().any(|r| r.starts_with("dnsbl_")),
                "no dnsbl_* in matched_rules: {matched_rules:?}"
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[tokio::test]
async fn lookup_failure_treated_as_no_match() {
    // FakeDnsbl returns LookupFailed for any (ip, zone) not in its
    // verdicts table — emulating a flaky upstream. The plan's
    // "don't poison classify" intent says these become no-match.
    let fake = Arc::new(FakeDnsbl::new());
    // Intentionally leave the verdicts table empty.

    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    let engine = engine.with_dnsbl(fake as Arc<dyn DnsblLookup>);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.7".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => {
            assert_eq!(score, 0.0);
            assert!(!matched_rules.iter().any(|r| r.starts_with("dnsbl_")));
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[tokio::test]
async fn multi_zone_rule_matches_on_any_listed_zone() {
    // dnsbl_multi has zones [a.test, b.test, c.test]; list the peer
    // on b.test only and assert the rule matches.
    let fake = Arc::new(FakeDnsbl::new());
    fake.set("203.0.113.8", "a.test", DnsblResult::NotListed);
    fake.set("203.0.113.8", "b.test", DnsblResult::Listed);
    fake.set("203.0.113.8", "c.test", DnsblResult::NotListed);
    // dnsbl_spamhaus zone not listed.
    fake.set("203.0.113.8", "zen.spamhaus.org", DnsblResult::NotListed);

    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    let engine = engine.with_dnsbl(fake as Arc<dyn DnsblLookup>);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.8".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => {
            assert_eq!(score, 4.0, "dnsbl_multi weight contributed");
            assert!(matched_rules.iter().any(|r| r == "dnsbl_multi"));
            assert!(!matched_rules.iter().any(|r| r == "dnsbl_spamhaus"));
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[tokio::test]
async fn disabled_dnsbl_rule_does_not_call_lookup() {
    // Per Codex round 1 (commit 2b) MAJOR: a rule disabled by the
    // account override must not trigger DNS lookups for its zones —
    // the rule cannot match anyway, and issuing the lookup would
    // waste a DNS round-trip and leak the peer IP to a third-party
    // DNSBL for a verdict the engine will discard.
    let fake = Arc::new(FakeDnsbl::new());
    // Don't populate any verdicts — every lookup would return
    // LookupFailed if it ever ran. We assert it doesn't run.

    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    let engine = engine.with_dnsbl(fake.clone() as Arc<dyn DnsblLookup>);

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides {
        disabled_rules: vec!["dnsbl_spamhaus".to_string(), "dnsbl_multi".to_string()],
        ..AccountOverrides::default()
    };
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.10".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => {
            assert_eq!(score, 0.0);
            assert!(!matched_rules.iter().any(|r| r.starts_with("dnsbl_")));
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    assert_eq!(
        fake.call_count(),
        0,
        "disabled DNSBL rules must not trigger any lookup calls",
    );
}

#[tokio::test]
async fn mail_auth_hard_fail_short_circuits_before_dnsbl_lookup() {
    // Per Codex round 1 (commit 2b) MAJOR: a verdict already decided
    // by mail-auth hard-fail must not pay DNSBL resolver-latency cost
    // and must not leak the peer IP to a third-party DNSBL for a
    // verdict the engine is about to discard.
    let fake = Arc::new(FakeDnsbl::new());
    fake.set("203.0.113.11", "zen.spamhaus.org", DnsblResult::Listed);
    fake.set("203.0.113.11", "a.test", DnsblResult::Listed);
    fake.set("203.0.113.11", "b.test", DnsblResult::Listed);
    fake.set("203.0.113.11", "c.test", DnsblResult::Listed);

    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    let engine = engine.with_dnsbl(fake.clone() as Arc<dyn DnsblLookup>);

    // SpfFailDmarcReject triggers — default EngineConfig has it in
    // mail_auth_hard_fail_kinds. SPF Fail + DMARC published=Reject.
    let peer = "203.0.113.11".parse::<IpAddr>().unwrap();
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
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: peer,
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::HardJunk { reason, .. } => {
            assert!(
                matches!(reason, cosmix_maild_rules::JunkReason::MailAuthHardFail),
                "expected MailAuthHardFail, got {reason:?}"
            );
        }
        other => panic!("expected HardJunk(MailAuthHardFail), got {other:?}"),
    }
    assert_eq!(
        fake.call_count(),
        0,
        "mail-auth hard-fail must short-circuit before any DNSBL lookup",
    );
}

#[tokio::test]
async fn engine_without_dnsbl_lookup_does_not_match_dnsbl_rules() {
    // Engine with no DnsblLookup wired — preflight returns empty,
    // every zone reads as LookupFailed, peer_ip_in_dnsbl never matches.
    let (engine, _) =
        DefaultRuleEngine::with_pack_str(EngineConfig::default(), dnsbl_only_pack()).unwrap();
    // No .with_dnsbl(...) — dnsbl field stays None.

    let verify = clean_auth();
    let account = AccountId::new("test");
    let overrides = AccountOverrides::default();
    let msg = message();
    let rcpts: Vec<String> = vec!["rcpt@alpha.local".to_string()];
    let ctx = RuleContext {
        message: &msg,
        peer_ip: "203.0.113.9".parse().unwrap(),
        envelope_from: "sender@example.test",
        envelope_to: &rcpts,
        mail_auth: &verify,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };
    let verdict = engine.classify(&ctx).await.unwrap();
    match verdict {
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => {
            assert_eq!(score, 0.0);
            assert!(!matched_rules.iter().any(|r| r.starts_with("dnsbl_")));
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}
