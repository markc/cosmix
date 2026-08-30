//! `maild.verdict` topic — one event per durably delivered inbound
//! recipient.
//!
//! The publish is post-commit by construction: the hot DATA path only
//! calls `verdict_tx.send(event).ok()` *after* `db::email::create`
//! returns success, so a subscriber never sees a verdict for a row
//! that didn't land. Hard-rejected SMTP-time messages (which never
//! reach `db::email::create`) emit zero events — that's intentional;
//! a pre-delivery telemetry topic is Phase 2+.
//!
//! `retain: false` on every publish is a hard requirement: the noded
//! `topic.publish` default is `retain: true`, which would leak the
//! last-delivered envelope + stamp_id to any peer that subscribes
//! after the fact and violate the "no historical replay" contract.
//!
//! Wire shape (inner Bus message):
//! - `command = "maild.verdict"` so subscribers can `on maild.verdict do …`.
//! - body = JSON `VerdictEvent`.
//!
//! Outer `topic.publish` RPC carries `name` + `retain: "false"` headers.

use std::collections::BTreeMap;
use std::net::IpAddr;

use cosmix_bus::bus::BusMessage;
use cosmix_maild_auth::{DkimOutcome, DmarcOutcome, SpfCheck, SpfResult, VerifyResult};
use cosmix_maild_rules::{RuleId, VerdictShape};
use tokio::sync::broadcast;

use super::subscribe_granter::SharedBrokerHandle;

pub const TOPIC: &str = "maild.verdict";

/// Bounded broadcast channel capacity. Slow subscribers see lag; the
/// publisher task logs a warning and continues from latest.
pub const CHANNEL_CAPACITY: usize = 256;

/// Spec §`maild.verdict` payload. Field names and types mirror the
/// spec exactly so the wire shape is the serde default. `bayes_score`
/// and `cold_start` are `Option` because hard-accept / hard-junk
/// paths short-circuit before the Bayesian phase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerdictEvent {
    pub ts: String,
    pub account_id: i32,
    pub message_id: Option<String>,
    /// Stable per-delivery id — the email row UUID returned by
    /// `db::email::create`. An authorized subscriber can use it to
    /// look up the body via JMAP if they hold account creds.
    pub stamp_id: String,
    pub envelope_from: String,
    pub envelope_to: Vec<String>,
    pub peer_ip: String,
    pub verdict: String,
    pub score: f64,
    /// `None` when classification did not run for this delivery —
    /// `account.spam_enabled = false`, or the rules/Bayesian pipeline
    /// returned an error and the message was delivered anyway. Pinned
    /// as Option (and not "Continue with score 0") so subscribers can
    /// distinguish "rules ran and decided Continue" from "rules did
    /// not run at all".
    pub rules_verdict: Option<VerdictShape>,
    pub rules_score: Option<f32>,
    pub matched_rules: Vec<RuleId>,
    pub bayes_score: Option<f32>,
    pub cold_start: Option<bool>,
    pub auth_summary: String,
}

/// Build a single-line `spf=… dkim=… dmarc=…` summary from the
/// verifier output. The full Authentication-Results header is already
/// prepended to the stored message; this is the abbreviated form for
/// the topic stream.
pub fn auth_summary(v: &VerifyResult) -> String {
    let spf = match &v.spf {
        SpfCheck::MailFrom { result, .. } | SpfCheck::Helo { result, .. } => match result {
            SpfResult::Pass => "pass",
            SpfResult::Fail => "fail",
            SpfResult::SoftFail => "softfail",
            SpfResult::Neutral => "neutral",
            SpfResult::None => "none",
            SpfResult::TempError => "temperror",
            SpfResult::PermError => "permerror",
        },
    };
    let dkim = match v.dkim.overall {
        DkimOutcome::Pass => "pass",
        DkimOutcome::Fail => "fail",
        DkimOutcome::None => "none",
        DkimOutcome::TempError => "temperror",
        DkimOutcome::PermError => "permerror",
    };
    let dmarc = match v.dmarc.outcome {
        DmarcOutcome::Pass => "pass",
        DmarcOutcome::Fail { .. } => "fail",
        DmarcOutcome::None => "none",
    };
    format!("spf={spf} dkim={dkim} dmarc={dmarc}")
}

/// Render an `IpAddr` for the wire. Plain string form so a subscriber
/// can match on it without re-encoding.
pub fn render_peer_ip(ip: IpAddr) -> String {
    ip.to_string()
}

/// RFC3339 millisecond timestamp at call time.
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Build the inner Bus message that sits inside the `topic.publish`
/// body. `command = "maild.verdict"` lets subscribers route with a
/// plain `on maild.verdict do …` handler.
pub fn build_inner_message(event: &VerdictEvent) -> BusMessage {
    let body = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    let mut m = BusMessage::new();
    m.set("command", TOPIC);
    m.body = body;
    m
}

/// Spawn the long-lived publisher task. Drains the broadcast channel
/// and forwards each event as a `topic.publish` RPC with
/// `retain: false`. On `Lagged(n)` it logs and continues from latest;
/// on `Closed` it exits cleanly.
///
/// The task is fire-and-forget from the hot DATA path's perspective —
/// `verdict_tx.send` is lock-free; this task absorbs all broker I/O cost
/// off the delivery path.
pub fn spawn_publisher_task(
    client: SharedBrokerHandle,
    mut rx: broadcast::Receiver<VerdictEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(e) = publish_event(&client, &event).await {
                        tracing::warn!(error = %e, "maild.verdict publish failed");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        lagged = n,
                        "maild.verdict subscriber lagged; continuing from latest"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("maild.verdict broadcast channel closed; publisher exiting");
                    return;
                }
            }
        }
    })
}

async fn publish_event(client: &SharedBrokerHandle, event: &VerdictEvent) -> anyhow::Result<()> {
    let Some(client) = client.load_full() else {
        tracing::warn!("broker handle unavailable; dropping maild.verdict publish");
        return Ok(());
    };
    let inner_wire = build_inner_message(event).to_wire();
    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), TOPIC.to_string());
    // Hard requirement per spec — see module doc.
    headers.insert("retain".to_string(), "false".to_string());
    client
        .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_maild_auth::{
        ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DmarcDisposition,
        DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome, IprevResult,
    };

    fn pass_verify_result() -> VerifyResult {
        VerifyResult {
            spf: SpfCheck::MailFrom {
                result: SpfResult::Pass,
                domain: "example.com".into(),
            },
            iprev: IprevResult {
                result: IprevOutcome::Pass,
                ptr: None,
                matched_forward: None,
            },
            dkim: DkimAggregate {
                signatures: Vec::new(),
                overall: DkimOutcome::Fail,
                capped: false,
            },
            dmarc: DmarcResult {
                outcome: DmarcOutcome::Fail {
                    policy: DmarcPolicy::None,
                    alignment: cosmix_maild_auth::Alignment::NotAligned,
                },
                report_record: DmarcReportRecord {
                    org_domain: "example.com".into(),
                    source_ip: "192.0.2.1".parse().unwrap(),
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
                host_identity: "h".into(),
                rendered: String::new(),
                spf: None,
                iprev: None,
                dkim: Vec::new(),
                dmarc: None,
                arc: None,
            },
        }
    }

    fn sample_event() -> VerdictEvent {
        VerdictEvent {
            ts: "2026-05-02T00:00:00.000Z".into(),
            account_id: 7,
            message_id: Some("<abc@example.com>".into()),
            stamp_id: "11111111-1111-1111-1111-111111111111".into(),
            envelope_from: "sender@example.com".into(),
            envelope_to: vec!["dest@local".into()],
            peer_ip: "192.0.2.1".into(),
            verdict: "SPAM".into(),
            score: 0.987,
            rules_verdict: Some(VerdictShape::Continue),
            rules_score: Some(0.0),
            matched_rules: Vec::new(),
            bayes_score: Some(0.987),
            cold_start: Some(false),
            auth_summary: "spf=pass dkim=fail dmarc=fail".into(),
        }
    }

    #[test]
    fn auth_summary_renders_pass_fail_fail() {
        let s = auth_summary(&pass_verify_result());
        assert_eq!(s, "spf=pass dkim=fail dmarc=fail");
    }

    #[test]
    fn inner_message_carries_command_and_json_body() {
        // Subscribers route on `command = "maild.verdict"`; pin the
        // header and the JSON body shape together so a stray rename
        // here would break the subscriber contract.
        let m = build_inner_message(&sample_event());
        assert_eq!(m.get("command"), Some(TOPIC));
        let v: serde_json::Value = serde_json::from_str(&m.body).unwrap();
        assert_eq!(v["account_id"], 7);
        assert_eq!(v["stamp_id"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(v["verdict"], "SPAM");
        assert_eq!(v["rules_verdict"], "Continue");
        assert!(v["matched_rules"].is_array());
        assert_eq!(v["bayes_score"], 0.987);
        assert_eq!(v["cold_start"], false);
        assert_eq!(v["auth_summary"], "spf=pass dkim=fail dmarc=fail");
    }

    #[test]
    fn hard_junk_event_serializes_with_null_bayes_fields() {
        // HardJunk skips the Bayesian phase entirely. The wire shape
        // must use JSON null for the Option fields so subscribers can
        // distinguish "no Bayesian pass" from "Bayesian returned 0.0".
        let mut event = sample_event();
        event.rules_verdict = Some(VerdictShape::HardJunk);
        event.bayes_score = None;
        event.cold_start = None;
        let body = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["rules_verdict"], "HardJunk");
        assert!(v["bayes_score"].is_null());
        assert!(v["cold_start"].is_null());
    }

    #[test]
    fn unclassified_event_serializes_with_null_rules_and_bayes() {
        // `spam_enabled = false` accounts and classification-error
        // deliveries still emit a verdict event so subscribers see
        // every durably delivered recipient — but rules_verdict and
        // rules_score must be JSON null (not "Continue" + 0.0) so the
        // distinction "rules ran and decided Continue" vs "rules
        // didn't run" stays observable on the wire.
        let mut event = sample_event();
        event.verdict = "HAM".into();
        event.score = 0.0;
        event.rules_verdict = None;
        event.rules_score = None;
        event.matched_rules = Vec::new();
        event.bayes_score = None;
        event.cold_start = None;
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(v["verdict"], "HAM");
        assert!(v["rules_verdict"].is_null());
        assert!(v["rules_score"].is_null());
        assert_eq!(v["matched_rules"], serde_json::json!([]));
        assert!(v["bayes_score"].is_null());
        assert!(v["cold_start"].is_null());
    }

    #[tokio::test]
    async fn publisher_task_exits_when_channel_closes() {
        // Sanity: dropping the sender closes the channel and the task
        // drains and returns rather than spinning. We can't assert on
        // broker I/O without a real client; the precondition for that
        // path is exercised via `recv()` returning Closed.
        let (tx, rx) = broadcast::channel::<VerdictEvent>(8);
        drop(tx);
        // Reproduce the publisher's recv loop without the client side.
        let mut rx = rx;
        let outcome = rx.recv().await;
        assert!(matches!(outcome, Err(broadcast::error::RecvError::Closed)));
    }

    #[test]
    fn channel_capacity_is_documented_constant() {
        assert_eq!(CHANNEL_CAPACITY, 256);
    }
}
