//! Smoke binary. Reads a single RFC 5322 message from stdin, runs it
//! through the engine loaded with the v1.0 default pack, and prints
//! the verdict + per-rule explanation.
//!
//! Usage:
//!   cat sample.eml | cosmix-maild-rules-smoke
//!
//! The `VerifyResult` is constructed inline as a synthetic "all pass"
//! stub — the real wiring inside cosmix-maild passes the
//! authenticator's actual output.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};

use cosmix_maild_auth::{
    ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DkimOutcome, DmarcDisposition,
    DmarcOutcome, DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome, IprevResult, SpfCheck,
    SpfResult, VerifyResult,
};
use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleContext, RuleEngine,
};

fn synthetic_verify_result() -> VerifyResult {
    let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    VerifyResult {
        spf: SpfCheck::MailFrom {
            result: SpfResult::Pass,
            domain: "example.invalid".into(),
        },
        iprev: IprevResult {
            result: IprevOutcome::Pass,
            ptr: Some("smoke.example.invalid".into()),
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
                org_domain: "example.invalid".into(),
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
            host_identity: "smoke.example.invalid".into(),
            rendered: "Authentication-Results: smoke.example.invalid; spf=pass".into(),
            spf: None,
            iprev: None,
            dkim: Vec::new(),
            dmarc: None,
            arc: None,
        },
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;

    // Load the v1.0 default pack alongside the crate. Falls back to
    // the empty engine if the pack is missing (e.g. when run from a
    // staged binary).
    let pack_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("rules")
        .join("default.conf.mix");
    let engine = if pack_path.exists() {
        let (eng, failed) = DefaultRuleEngine::with_pack_path(EngineConfig::default(), &pack_path)?;
        if !failed.is_empty() {
            eprintln!("[smoke] {} rule(s) failed to compile:", failed.len());
            for (id, err) in &failed {
                eprintln!("  - {id}: {err}");
            }
        }
        eng
    } else {
        DefaultRuleEngine::new(EngineConfig::default())
    };
    let auth = synthetic_verify_result();
    let account = AccountId::new("smoke");
    let overrides = AccountOverrides::default();
    let rcpts: Vec<String> = vec!["smoke@example.invalid".into()];

    let ctx = RuleContext {
        peer_ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        envelope_from: "smoke@example.invalid",
        envelope_to: &rcpts,
        message: &buf,
        mail_auth: &auth,
        sender_authenticated: false,
        account: &account,
        overrides: &overrides,
    };

    let verdict = engine.classify(&ctx).await?;
    let explanation = engine.explain(&ctx).await?;

    println!("verdict: {:?}", verdict);
    println!("explanation: {:?}", explanation);
    Ok(())
}
