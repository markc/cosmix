//! Shared test fixtures.
//!
//! Each integration test compiles this module independently and only uses
//! a subset of its helpers; per-target dead-code analysis flags the rest.
//! `#![allow(dead_code)]` at the module level matches the standard idiom
//! for shared `tests/common/` modules.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr};

use cosmix_maild_auth::{
    ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DkimOutcome, DmarcDisposition,
    DmarcOutcome, DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome, IprevResult, SpfCheck,
    SpfResult, VerifyResult,
};
use cosmix_maild_rules::{AccountId, AccountOverrides, RuleContext};

pub fn pass_verify_result() -> VerifyResult {
    let peer = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5));
    VerifyResult {
        spf: SpfCheck::MailFrom {
            result: SpfResult::Pass,
            domain: "example.invalid".into(),
        },
        iprev: IprevResult {
            result: IprevOutcome::Pass,
            ptr: Some("smtp.example.invalid".into()),
            matched_forward: Some(peer),
        },
        dkim: DkimAggregate {
            signatures: Vec::new(),
            overall: DkimOutcome::Pass,
            capped: false,
        },
        dmarc: DmarcResult {
            outcome: DmarcOutcome::Pass,
            report_record: DmarcReportRecord {
                org_domain: "example.invalid".into(),
                source_ip: peer,
                policy_published: DmarcPolicy::None,
                policy_evaluated: DmarcPolicy::None,
                spf_aligned: true,
                dkim_aligned: true,
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
            host_identity: "test.example.invalid".into(),
            rendered: "Authentication-Results: test.example.invalid; spf=pass".into(),
            spf: None,
            iprev: None,
            dkim: Vec::new(),
            dmarc: None,
            arc: None,
        },
    }
}

pub fn spf_fail_dmarc_reject() -> VerifyResult {
    let mut v = pass_verify_result();
    v.spf = SpfCheck::MailFrom {
        result: SpfResult::Fail,
        domain: "example.invalid".into(),
    };
    v.dmarc.outcome = DmarcOutcome::Fail {
        policy: DmarcPolicy::Reject,
        alignment: cosmix_maild_auth::Alignment::NotAligned,
    };
    v.dmarc.report_record.policy_published = DmarcPolicy::Reject;
    v.dmarc.report_record.policy_evaluated = DmarcPolicy::Reject;
    v
}

pub fn dkim_fail() -> VerifyResult {
    let mut v = pass_verify_result();
    v.dkim.overall = DkimOutcome::Fail;
    v
}

pub fn spf_fail() -> VerifyResult {
    let mut v = pass_verify_result();
    v.spf = SpfCheck::MailFrom {
        result: SpfResult::Fail,
        domain: "example.invalid".into(),
    };
    v
}

/// Build a `RuleContext` from raw message bytes, default account, no overrides.
pub fn ctx<'a>(
    message: &'a [u8],
    auth: &'a VerifyResult,
    account: &'a AccountId,
    rcpts: &'a [String],
    overrides: &'a AccountOverrides,
) -> RuleContext<'a> {
    RuleContext {
        peer_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)),
        envelope_from: "sender@example.invalid",
        envelope_to: rcpts,
        message,
        mail_auth: auth,
        sender_authenticated: false,
        account,
        overrides,
    }
}

/// Build a context with a custom envelope-from (for allowlist/blocklist tests).
pub fn ctx_from<'a>(
    envelope_from: &'a str,
    message: &'a [u8],
    auth: &'a VerifyResult,
    account: &'a AccountId,
    rcpts: &'a [String],
    overrides: &'a AccountOverrides,
) -> RuleContext<'a> {
    RuleContext {
        peer_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)),
        envelope_from,
        envelope_to: rcpts,
        message,
        mail_auth: auth,
        sender_authenticated: false,
        account,
        overrides,
    }
}

pub const HAM: &[u8] = b"From: alice@example.invalid\r\n\
To: bob@example.invalid\r\n\
Subject: Lunch tomorrow\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <abc123@example.invalid>\r\n\
\r\n\
Want to grab lunch tomorrow at noon?\r\n";

pub const ALL_CAPS_SUBJECT: &[u8] = b"From: spammer@example.invalid\r\n\
To: target@example.invalid\r\n\
Subject: BUY NOW LIMITED OFFER ACT TODAY\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <spam@example.invalid>\r\n\
\r\n\
Body content here.\r\n";

pub const HTML_ONLY: &[u8] = b"From: x@example.invalid\r\n\
To: y@example.invalid\r\n\
Subject: HTML newsletter\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <html@example.invalid>\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Hello</p></body></html>\r\n";

pub const NO_MESSAGE_ID: &[u8] = b"From: x@example.invalid\r\n\
To: y@example.invalid\r\n\
Subject: missing id\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
\r\n\
Body.\r\n";
