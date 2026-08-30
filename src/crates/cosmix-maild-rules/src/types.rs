//! Public types — frozen surface per spec §Public API.

use std::net::IpAddr;

// ---- Identifiers ----

/// Stable rule identifier, scoped to a pack version. v1 stays simple
/// — just the string from `default.toml`. We may switch to a newtype
/// with interning in v1.1 if rule sets grow large.
pub type RuleId = String;

/// Sender glob, e.g. `"brother@example.com"` or `"*@vendor.example"`.
/// Compiled at load time via the `glob` crate.
pub type SenderGlob = String;

/// Account identifier — borrowed from the umbrella's account type.
/// Treated as opaque here; we only key per-account lookups by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---- Classify-time context ----

/// Inputs the engine needs for one classification call. Borrowed so
/// the umbrella does not have to allocate just to call us.
pub struct RuleContext<'a> {
    pub peer_ip: IpAddr,
    pub envelope_from: &'a str,
    pub envelope_to: &'a [String],
    pub message: &'a [u8],
    pub mail_auth: &'a cosmix_maild_auth::VerifyResult,
    /// true when the SMTP session authenticated the submitter (local SMTP-AUTH).
    /// Mail-auth hard-fail rules and the auth half of the structural composite
    /// are skipped: SPF/DKIM/DMARC describe MX provenance, and an authenticated
    /// submission from a private address fails them by construction. All other
    /// rules and the bayesian stage still run.
    pub sender_authenticated: bool,
    pub account: &'a AccountId,
    pub overrides: &'a AccountOverrides,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AccountOverrides {
    pub disabled_rules: Vec<RuleId>,
    pub threshold_override: Option<f32>,
    pub allowlist_senders: Vec<SenderGlob>,
    pub blocklist_senders: Vec<SenderGlob>,
}

// ---- Verdicts ----

#[derive(Debug, Clone, serde::Serialize)]
pub enum RuleVerdict {
    /// Definitive accept: skip Bayesian, deliver to inbox.
    HardAccept {
        reason: AcceptReason,
        matched_rules: Vec<RuleId>,
    },

    /// Definitive junk: skip Bayesian, route to \Junk (unless
    /// shadow_mode is on, in which case the engine internally
    /// downgrades to Continue with `would_junk` recorded).
    HardJunk {
        reason: JunkReason,
        matched_rules: Vec<RuleId>,
        score: f32,
    },

    /// Ambiguous; pass to Bayesian. `score` and `matched_rules` are
    /// contextual features for the Bayesian phase and are preserved
    /// on the classification stamp regardless of the Bayesian's
    /// verdict.
    Continue {
        score: f32,
        matched_rules: Vec<RuleId>,
        /// Set when shadow_mode forced a HardJunk → Continue
        /// downgrade. Surfaced on the classification stamp by the
        /// umbrella so operators can inspect would-have-junked
        /// traffic without changing routing.
        would_junk: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AcceptReason {
    AllowlistSender,
    /// Reserved for future hard-accept rules (e.g. signed by a
    /// pinned trust anchor). Not used in v1.
    Future,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum JunkReason {
    BlocklistSender,
    /// SPF fail + DMARC reject, etc. — see EngineConfig.mail_auth_hard_fail_kinds.
    MailAuthHardFail,
    /// Composite signal — e.g. has_executable_attachment + auth fail.
    StructuralAnomaly,
    /// raw_score >= EngineConfig.hard_junk_threshold without a
    /// single disqualifying signal.
    ScoreBreach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum VerdictShape {
    HardAccept,
    HardJunk,
    Continue,
}

impl RuleVerdict {
    pub fn shape(&self) -> VerdictShape {
        match self {
            RuleVerdict::HardAccept { .. } => VerdictShape::HardAccept,
            RuleVerdict::HardJunk { .. } => VerdictShape::HardJunk,
            RuleVerdict::Continue { .. } => VerdictShape::Continue,
        }
    }
}

// ---- Mail-auth-derived hard-fail kinds ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MailAuthHardFailKind {
    SpfFailDmarcReject,
    DkimFailDmarcReject,
    /// Reserved; not enabled by default. Forwarded mail commonly has
    /// broken auth without ARC.
    BothAuthFailNoArc,
}

// ---- Explanation surface ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct Explanation {
    pub rules: Vec<RuleExplanation>,
    pub total_score: f32,
    pub threshold: f32,
    pub pack_version: String,
    pub verdict: VerdictShape,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RuleExplanation {
    pub id: RuleId,
    pub description: String,
    pub matched: bool,
    pub configured_weight: i32,
    /// Reserved for Bayesian-attributed adjustments. v1 always 0.0.
    pub adjustment: f32,
    pub effective_weight: f32,
    pub contribution: f32,
}

// ---- Reload report ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReloadReport {
    pub pack_version: String,
    pub rules_loaded: u32,
    /// Per-rule compile failures from the most recent reload. Each
    /// entry is `(rule_id, error_message)`. Serialized as an array of
    /// `{rule_id, error}` objects to match the Bus spec wire shape.
    #[serde(serialize_with = "serialize_rules_failed")]
    pub rules_failed: Vec<(RuleId, String)>,
}

fn serialize_rules_failed<S>(
    failed: &[(RuleId, String)],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(failed.len()))?;
    for (id, err) in failed {
        seq.serialize_element(&serde_json::json!({
            "rule_id": id,
            "error": err,
        }))?;
    }
    seq.end()
}

// ---- Match-view selector for body content rules ----

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchView {
    /// Decoded text/plain parts only.
    Plain,
    /// Decoded text/html parts only (after `nanohtml2text`).
    Html,
    /// Both views concatenated, plain first. Default.
    #[default]
    Combined,
}
