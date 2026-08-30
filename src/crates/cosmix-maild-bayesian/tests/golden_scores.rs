//! Golden-score regression for maild's policy wrapper around spamlite.
//!
//! The first 52 fixture lines pin the pre-engine-swap scores bit-for-bit at the
//! displayed precision. The final HTML-only block is the sole intentional
//! change: the engine tokenizer strips markup before scoring.
//!
//! It pins the exact `score` / `label` / `cold_start` this crate produces for a
//! fixed corpus and a fixed set of messages. Port the classifier, re-run, and
//! any drift in the Robinson-Fisher math, the token-selection rule, or the
//! tokenizer's default token stream shows up as a failing assert instead of as
//! quietly-different spam filtering in production.
//!
//! Deliberately covers the paths the port is most likely to disturb:
//!   * an empty corpus (the uninformative-0.5 short circuit),
//!   * cold-start vs steady-state thresholds,
//!   * unknown-token-only messages (every token hits `x_unknown`),
//!   * strongly spammy / hammy / mixed messages (the token-selection sort,
//!     which is where maild and spamlite genuinely differ: maild truncates the
//!     top-N by |fw − 0.5|, spamlite gates on `min_distance` + `min_array_size`).
//!

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use cosmix_maild_bayesian::{
    classifier::{Classifier, DefaultClassifier},
    config::ClassifierConfig,
    error::Result,
    storage::{AccountConnection, SqliteAccountConnection, StorageBackend},
    types::{ClassifyContext, Label},
};
use cosmix_maild_rules::AccountId;

const GOLDEN: &str = "tests/fixtures/golden_scores.txt";

struct SingleConn(Arc<SqliteAccountConnection>);

#[async_trait]
impl StorageBackend for SingleConn {
    async fn open_account(&self, _account: &AccountId) -> Result<Arc<dyn AccountConnection>> {
        Ok(self.0.clone() as Arc<dyn AccountConnection>)
    }
}

/// The 21 `b:` tokens the maild tokenizer emitted for an HTML-only message as
/// of 2026-08-25 (spamlite v0.8.0 port, BEFORE spamlite `a13e2c1`): raw markup —
/// tag names, attribute names/values, CSS properties/values, class names and
/// the undecoded `&nbsp;` entity — tokenised as body words because mail-parser's
/// `text_bodies()` falls back to the text/html part when there is no
/// text/plain alternative. This list is FROZEN on purpose: it is what the
/// `html_only_markup_trained` golden trains as spam, so that golden pins the
/// current (wrong) behaviour — markup outvoting content — and moves exactly
/// when the tokenizer stops emitting these tokens. Do not regenerate it from
/// the tokenizer; that would make the golden follow the bug it is meant to
/// catch.
const HTML_MARKUP_TOKENS: &[&str] = &[
    "b:13px",
    "b:arial",
    "b:body",
    "b:class",
    "b:content",
    "b:css",
    "b:edge",
    "b:font-family",
    "b:head",
    "b:helvetica",
    "b:html",
    "b:http-equiv",
    "b:margin-top",
    "b:meta",
    "b:nbsp",
    "b:promo",
    "b:style",
    "b:table",
    "b:text",
    "b:type",
    "b:x-ua-compatible",
];

/// A deterministic corpus. No randomness, no clock, no filesystem: the same
/// counts on every machine, so a score change means the MATH changed.
///
/// `with_markup_spam` additionally trains `HTML_MARKUP_TOKENS` as spam 30
/// times — a stand-in for the real-world shape where ham corpora are seeded
/// from `.Sent` (plain/multipart) and markup lands almost entirely on the
/// spam side. It is a separate flag, not part of the base corpus, so the three
/// original runs' totals — and therefore every existing golden line — stay
/// byte-identical.
async fn corpus(trained: bool, with_markup_spam: bool) -> Arc<SqliteAccountConnection> {
    let conn = Arc::new(SqliteAccountConnection::open_path(Path::new(":memory:"), 100).unwrap());
    if !trained {
        return conn;
    }
    if with_markup_spam {
        let toks: Vec<String> = HTML_MARKUP_TOKENS.iter().map(|s| s.to_string()).collect();
        for i in 0..30 {
            conn.record_label(&format!("html{i}"), &toks, Label::Spam, 0)
                .await
                .unwrap();
        }
    }
    // 60 ham / 40 spam — deliberately asymmetric, so any accidental swap of
    // total_good/total_spam (an easy transcription error in a port) moves the
    // score instead of cancelling out.
    for i in 0..60 {
        conn.record_label(
            &format!("h{i}"),
            &[
                "h:subject:meeting".into(),
                "h:subject:tomorrow".into(),
                "b:agenda".into(),
                "b:discuss".into(),
                "b:project".into(),
            ],
            Label::Ham,
            0,
        )
        .await
        .unwrap();
    }
    for i in 0..40 {
        conn.record_label(
            &format!("s{i}"),
            &[
                "h:subject:buy".into(),
                "h:subject:now".into(),
                "b:viagra".into(),
                "b:discount".into(),
                "b:free".into(),
            ],
            Label::Spam,
            0,
        )
        .await
        .unwrap();
    }
    // A token seen in BOTH classes, skewed spam — exercises the Robinson
    // correction rather than the degenerate good==0 / spam==0 edges.
    for i in 0..20 {
        conn.record_label(&format!("m{i}"), &["b:offer".into()], Label::Spam, 0)
            .await
            .unwrap();
    }
    for i in 0..5 {
        conn.record_label(&format!("mh{i}"), &["b:offer".into()], Label::Ham, 0)
            .await
            .unwrap();
    }
    conn
}

fn msg(subject: &str, body: &str) -> Vec<u8> {
    format!("Subject: {subject}\r\nFrom: a@b.test\r\nTo: c@d.test\r\n\r\n{body}\r\n").into_bytes()
}

/// The fixed message set. Each name is stable so a golden diff names the case.
fn cases() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("spammy", msg("buy now", "viagra discount free")),
        ("hammy", msg("meeting tomorrow", "agenda discuss project")),
        ("mixed", msg("buy tomorrow", "agenda viagra offer")),
        ("skewed_shared", msg("offer", "offer offer offer")),
        ("all_unknown", msg("zzqqxx", "nevertrained tokens here")),
        ("empty_body", msg("meeting", "")),
    ]
}

/// An HTML-only message (no text/plain alternative) whose CONTENT tokens
/// (`b:audit`, `b:report`, `b:uniquecontentbeta`, the `h:from:*` anchors) are
/// never trained, while its markup tokens are exactly `HTML_MARKUP_TOKENS`.
/// Against the markup-trained corpus, today's tokenizer scores it as spam on
/// markup alone — the shape of the real false positive spamlite `a13e2c1`
/// fixed (an auditor's weekly notification at SPAM 0.98 with every content
/// token ham). Once maild carries that fix, the markup tokens are no longer
/// emitted, every remaining token is unknown, and this case must fall to the
/// neutral 0.5 with zero contributions. That is the ONE golden line the
/// engine swap is allowed to move.
fn html_only_case() -> Vec<u8> {
    b"From: noreply@auditdashboard.example\r\nTo: c@d.test\r\nSubject: weekly notification\r\n\
Content-Type: text/html; charset=utf-8\r\n\r\n\
<html><head><meta http-equiv=\"X-UA-Compatible\" content=\"IE=edge\">\
<style type=\"text/css\">.promo { margin-top: 13px; font-family: helvetica, arial; }</style>\
</head><body><table><tr><td class=\"promo\">Your&nbsp;audit report is ready uniquecontentbeta</td></tr></table></body></html>\r\n"
        .to_vec()
}

/// Fourth run: the markup-trained corpus against the HTML-only case only.
async fn run_html_markup() -> Vec<String> {
    let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(corpus(true, true).await));
    let config = ClassifierConfig {
        cold_start_floor: 0,
        spam_threshold: 0.5,
        ..ClassifierConfig::default()
    };
    let classifier = DefaultClassifier::new(config, backend);
    let account = AccountId::new("golden");
    let body = html_only_case();
    let ctx = ClassifyContext {
        message: &body,
        account: &account,
        rules_score: 0.0,
        matched_rules: &[],
        trusted: false,
    };
    let v = classifier.classify(&ctx).await.unwrap();
    let mut out = vec![format!(
        "trained=true floor=0 markup_spam=30 case=html_only_markup_trained label={:?} score={:.6} cold_start={} threshold={:.4} contribs={}",
        v.label,
        v.score,
        v.cold_start,
        v.threshold,
        v.contributions.len()
    )];
    for c in v.contributions.iter().take(5) {
        out.push(format!(
            "    tok={} spam={} ham={} p={:.6}",
            c.token, c.spam_count, c.ham_count, c.probability
        ));
    }
    // Structural pin, independent of the numbers: the engine tokenizer must
    // never surface raw HTML markup as evidence.
    let markup_in_contribs = v
        .contributions
        .iter()
        .filter(|c| HTML_MARKUP_TOKENS.contains(&c.token.as_str()))
        .count();
    assert_eq!(
        markup_in_contribs, 0,
        "spamlite tokenizer leaked markup into contributions"
    );
    out.push(format!(
        "    markup_tokens_in_contributions={markup_in_contribs}"
    ));
    out
}

async fn run(trained: bool, cold_start_floor: u32) -> Vec<String> {
    let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(corpus(trained, false).await));
    let config = ClassifierConfig {
        cold_start_floor,
        spam_threshold: 0.5,
        ..ClassifierConfig::default()
    };
    let classifier = DefaultClassifier::new(config, backend);
    let account = AccountId::new("golden");

    let mut out = Vec::new();
    for (name, body) in cases() {
        let ctx = ClassifyContext {
            message: &body,
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v = classifier.classify(&ctx).await.unwrap();
        // Pin the score to 6 decimals: enough to catch a real math change,
        // loose enough not to fail on last-bit FP noise across architectures.
        out.push(format!(
            "trained={trained} floor={cold_start_floor} case={name} label={:?} score={:.6} cold_start={} threshold={:.4} contribs={}",
            v.label,
            v.score,
            v.cold_start,
            v.threshold,
            v.contributions.len()
        ));
        // The per-token math is part of the contract too — a token-selection
        // change would otherwise hide behind an unchanged final score.
        for c in v.contributions.iter().take(5) {
            out.push(format!(
                "    tok={} spam={} ham={} p={:.6}",
                c.token, c.spam_count, c.ham_count, c.probability
            ));
        }
    }
    out
}

#[tokio::test]
async fn golden_scores_unchanged() {
    let mut lines = Vec::new();
    // steady-state (past cold start), cold-start, and empty-corpus paths
    lines.extend(run(true, 0).await);
    lines.extend(run(true, 1000).await);
    lines.extend(run(false, 100).await);
    // Appended LAST so the three runs above keep their exact fixture lines.
    lines.extend(run_html_markup().await);
    let actual = lines.join("\n") + "\n";

    let path = Path::new(GOLDEN);
    if std::env::var("GOLDEN_UPDATE").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &actual).unwrap();
        eprintln!("golden updated: {GOLDEN}");
        return;
    }

    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("missing golden {GOLDEN} ({e}); generate with GOLDEN_UPDATE=1"));

    if expected != actual {
        // Show the first divergence — a 100-line diff is useless in CI output.
        for (i, (e, a)) in expected.lines().zip(actual.lines()).enumerate() {
            if e != a {
                panic!(
                    "golden score drift at line {}:\n  expected: {e}\n  actual:   {a}\n\
                     \nThe classifier's default behaviour CHANGED. If that is intentional, \
                     review the full diff and regenerate with GOLDEN_UPDATE=1.",
                    i + 1
                );
            }
        }
        panic!(
            "golden score drift: {} expected lines vs {} actual",
            expected.lines().count(),
            actual.lines().count()
        );
    }
}
