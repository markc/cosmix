//! MIME view selection — `match.view = "plain"` should not match
//! HTML content and vice versa. Plus malformed-MIME → mime_parse_error
//! implicit rule.

mod common;

use cosmix_maild_rules::{
    AccountId, AccountOverrides, DefaultRuleEngine, EngineConfig, RuleEngine, RuleVerdict,
};

use common::*;

// Pack with three otherwise-identical body rules differing only in `view`.
const VIEW_PACK: &str = r#"
pack_version: "view-test"
rule: [
  { id: "needle_in_plain", description: "needle in plain", weight: 1,
    match: { kind: "body_substring", substring: "secretneedle", view: "plain" } },
  { id: "needle_in_html", description: "needle in html", weight: 1,
    match: { kind: "body_substring", substring: "secretneedle", view: "html" } },
  { id: "needle_in_combined", description: "needle in combined", weight: 1,
    match: { kind: "body_substring", substring: "secretneedle", view: "combined" } }
]
"#;

#[tokio::test]
async fn body_substring_view_plain_matches_only_plain_content() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), VIEW_PACK).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    // Plain-only message containing the needle.
    let msg = b"From: a@b.c\r\n\
To: y@example.invalid\r\n\
Subject: t\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <p@b.c>\r\n\
\r\n\
the secretneedle lives in plain text\r\n";
    let ctx = ctx(msg, &auth, &account, &rcpts, &ov);

    let exp = engine.explain(&ctx).await.unwrap();
    let plain = exp
        .rules
        .iter()
        .find(|r| r.id == "needle_in_plain")
        .unwrap();
    let html = exp.rules.iter().find(|r| r.id == "needle_in_html").unwrap();
    let comb = exp
        .rules
        .iter()
        .find(|r| r.id == "needle_in_combined")
        .unwrap();
    assert!(plain.matched, "plain-view rule should match plain text");
    assert!(!html.matched, "html-view rule should not match plain-only");
    assert!(
        comb.matched,
        "combined-view rule should match plain content"
    );
}

#[tokio::test]
async fn body_substring_view_html_matches_only_html_content() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), VIEW_PACK).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    let msg = b"From: a@b.c\r\n\
To: y@example.invalid\r\n\
Subject: t\r\n\
Date: Mon, 27 Apr 2026 10:00:00 +0000\r\n\
Message-ID: <h@b.c>\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body>the secretneedle lives in html</body></html>\r\n";
    let ctx = ctx(msg, &auth, &account, &rcpts, &ov);

    let exp = engine.explain(&ctx).await.unwrap();
    let plain = exp
        .rules
        .iter()
        .find(|r| r.id == "needle_in_plain")
        .unwrap();
    let html = exp.rules.iter().find(|r| r.id == "needle_in_html").unwrap();
    assert!(!plain.matched, "plain-view rule should not match html-only");
    assert!(html.matched, "html-view rule should match html content");
}

#[tokio::test]
async fn malformed_mime_fires_mime_parse_error() {
    let (engine, _) = DefaultRuleEngine::with_pack_str(EngineConfig::default(), VIEW_PACK).unwrap();
    let auth = pass_verify_result();
    let account = AccountId::new("test");
    let rcpts: Vec<String> = vec!["y@example.invalid".into()];
    let ov = AccountOverrides::default();
    // Empty buffer → mail-parser returns None.
    let ctx = ctx(b"", &auth, &account, &rcpts, &ov);

    let verdict = engine.classify(&ctx).await.unwrap();
    let matched = match &verdict {
        RuleVerdict::HardAccept { matched_rules, .. }
        | RuleVerdict::HardJunk { matched_rules, .. }
        | RuleVerdict::Continue { matched_rules, .. } => matched_rules.clone(),
    };
    assert!(
        matched.iter().any(|r| r == "mime_parse_error"),
        "expected mime_parse_error, got {matched:?}"
    );
}
