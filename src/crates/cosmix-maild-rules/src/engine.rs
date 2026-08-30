//! Rule engine — `RuleEngine` trait + `DefaultRuleEngine`.
//!
//! Phase 1 wires real classification: parse the message into MIME
//! views, evaluate the compiled rule set against the context, then
//! decide a verdict per the spec's priority order:
//!
//! 1. Allowlist sender → `HardAccept { AllowlistSender }`.
//! 2. Blocklist sender → `HardJunk { BlocklistSender }`.
//! 3. Mail-auth hard-fail (`EngineConfig.mail_auth_hard_fail_kinds`)
//!    → `HardJunk { MailAuthHardFail }`.
//! 4. Structural anomaly (executable attachment + auth fail) →
//!    `HardJunk { StructuralAnomaly }`.
//! 5. `raw_score >= hard_junk_threshold` → `HardJunk { ScoreBreach }`.
//! 6. Else → `Continue`.
//!
//! Shadow mode downgrades all `HardJunk` shapes to `Continue` with
//! `would_junk = true`. `HardAccept` is preserved even in shadow mode.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use cosmix_maild_auth::{
    DkimOutcome, DmarcOutcome, DmarcPolicy, SpfCheck, SpfResult, VerifyResult,
};
use linkify::{LinkFinder, LinkKind};
use tokio::sync::RwLock;

use crate::compiled::{MatchInputs, RuleSet};
use crate::config::EngineConfig;
use crate::dnsbl::DnsblLookup;
use crate::error::Result;
use crate::glob_match::{SenderGlobMatcher, compile_sender_globs, matches_any};
use crate::mime::MimeViews;
use crate::preflight;
use crate::types::{
    AcceptReason, Explanation, JunkReason, MailAuthHardFailKind, ReloadReport, RuleContext,
    RuleExplanation, RuleId, RuleVerdict, VerdictShape,
};

/// Per-rule match callback installed on the engine. Invoked once per
/// matched rule per `classify` call, including the implicit
/// `mime_parse_error` rule. `explain` never fires the hook. See
/// [`DefaultRuleEngine::set_rule_match_hook`].
pub type RuleMatchHook = Arc<dyn Fn(&RuleId) + Send + Sync>;

/// Public engine surface. Object-safe so the umbrella can store it as
/// `Arc<dyn RuleEngine>`.
#[async_trait]
pub trait RuleEngine: Send + Sync {
    /// Classify a single message against the loaded rule pack.
    async fn classify(&self, ctx: &RuleContext<'_>) -> Result<RuleVerdict>;

    /// Re-run classification but emit per-rule contributions instead
    /// of a verdict. Used by `mailctl explain` and the Bus
    /// `RulesExplain` action.
    async fn explain(&self, ctx: &RuleContext<'_>) -> Result<Explanation>;

    /// Atomic swap of the loaded rule pack. Compiles + validates the
    /// next pack before swapping; on error the current pack stays
    /// live and the report carries per-rule failures.
    async fn reload(&self) -> Result<ReloadReport>;
}

/// Default rule engine. Holds an `RwLock<RuleSet>` so reload can swap
/// the pack atomically while concurrent classify calls hold a read
/// lock, and an `RwLock<EngineConfig>` so SPEC 12 Phase 3's substrate-
/// driven config reload (C3 `after_set` converge-loop) can swap the
/// engine's nine globals atomically while in-flight classifications
/// hold a read snapshot of the prior config.
pub struct DefaultRuleEngine {
    config: Arc<RwLock<EngineConfig>>,
    rule_set: Arc<RwLock<RuleSet>>,
    /// Pack file location, used by `reload`. None → reload is a no-op.
    pack_path: Option<PathBuf>,
    /// Optional DNSBL lookup. When `None`, `peer_ip_in_dnsbl` matchers
    /// always fall through to no-match (the preflight result is empty
    /// and every zone reads as `LookupFailed`). The umbrella wires a
    /// production `HickoryDnsbl` here at construction; tests can wire
    /// a fake implementing `DnsblLookup`. See
    /// `_doc/planned/maild-rules-phase2.md` D1 for the rationale.
    dnsbl: Option<Arc<dyn DnsblLookup>>,
    /// Optional per-match callback. Fired exactly once per pack rule
    /// whose matcher returns `true`, plus once for the implicit
    /// `mime_parse_error` rule. Engine-level synthetic verdict IDs,
    /// whether exposed through `matched_rules` (allowlist, blocklist)
    /// or synthetic explanation rows (mail-auth hard-fail), do NOT
    /// fire the hook — those are engine decisions, not pack-rule
    /// matches. The umbrella wires the per-rule counter increment on
    /// `cosmix-maild::rule_stats::RuleStats` here; tests can wire a
    /// recording fake. `None` is the zero-cost default (the smoke
    /// binary + crates that don't want counters pay nothing).
    ///
    /// The hook fires from `classify` only — `explain` calls
    /// `evaluate` without firing the hook, so a debug `mailctl
    /// explain` invocation doesn't inflate stats. See `_doc/planned/
    /// maild-rules-phase2.md` § "Per-rule counters".
    on_rule_matched: Option<RuleMatchHook>,
}

impl DefaultRuleEngine {
    /// Construct an engine with no loaded pack. Calling `classify`
    /// returns a `Continue { score: 0.0 }`. Use `with_pack_str` /
    /// `with_pack_path` to load a pack.
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            rule_set: Arc::new(RwLock::new(RuleSet::empty())),
            pack_path: None,
            dnsbl: None,
            on_rule_matched: None,
        }
    }

    /// Install a DNSBL lookup. The next `classify` / `explain` call
    /// that begins after this returns sees the new lookup; in-flight
    /// calls keep their pre-swap reference. Pass `None` to disable
    /// DNSBL matching (every `peer_ip_in_dnsbl` rule becomes no-match).
    ///
    /// Called by the umbrella once at construction with the production
    /// `HickoryDnsbl`, and by tests with a programmable fake.
    pub fn set_dnsbl(&mut self, dnsbl: Option<Arc<dyn DnsblLookup>>) {
        self.dnsbl = dnsbl;
    }

    /// Builder form of [`Self::set_dnsbl`] — chainable from
    /// `with_pack_*` constructors. `engine.with_dnsbl(lookup)`.
    pub fn with_dnsbl(mut self, dnsbl: Arc<dyn DnsblLookup>) -> Self {
        self.dnsbl = Some(dnsbl);
        self
    }

    /// Install a per-match callback. The hook is invoked once per
    /// matched pack rule per `classify` call, with the rule's
    /// `RuleId` as argument (plus once for the implicit
    /// `mime_parse_error`). `explain` does not fire the hook so a
    /// debug invocation cannot inflate stats.
    ///
    /// Pass `None` to detach the hook. Because installation requires
    /// `&mut self`, hooks are intended to be configured before the
    /// engine is shared via `Arc<dyn RuleEngine>`; this method is
    /// not for hot-swapping a hook while `classify` calls are
    /// in flight (the borrow checker statically forbids that).
    ///
    /// The umbrella installs the per-rule counter increment on
    /// `cosmix-maild::rule_stats::RuleStats` here; tests install a
    /// recording fake. See `_doc/planned/maild-rules-phase2.md` §
    /// "Per-rule counters".
    pub fn set_rule_match_hook(&mut self, hook: Option<RuleMatchHook>) {
        self.on_rule_matched = hook;
    }

    /// Builder form of [`Self::set_rule_match_hook`] — chainable
    /// from `with_pack_*` constructors. `engine.with_rule_match_hook(hook)`.
    pub fn with_rule_match_hook(mut self, hook: RuleMatchHook) -> Self {
        self.on_rule_matched = Some(hook);
        self
    }

    /// Construct an engine and immediately load a pack from a
    /// `.conf.mix` string. Per-rule compile failures are surfaced via
    /// the returned `Vec<(RuleId, String)>`.
    pub fn with_pack_str(
        config: EngineConfig,
        conf_mix_text: &str,
    ) -> Result<(Self, Vec<(crate::types::RuleId, String)>)> {
        let (set, failed) = crate::loader::load_pack_str(conf_mix_text)?;
        Ok((
            Self {
                config: Arc::new(RwLock::new(config)),
                rule_set: Arc::new(RwLock::new(set)),
                pack_path: None,
                dnsbl: None,
                on_rule_matched: None,
            },
            failed,
        ))
    }

    /// Construct an engine and load a pack from a file path. The path
    /// is remembered for subsequent `reload()` calls.
    pub fn with_pack_path(
        config: EngineConfig,
        path: impl Into<PathBuf>,
    ) -> Result<(Self, Vec<(crate::types::RuleId, String)>)> {
        let path = path.into();
        let (set, failed) = crate::loader::load_pack_file(&path)?;
        Ok((
            Self {
                config: Arc::new(RwLock::new(config)),
                rule_set: Arc::new(RwLock::new(set)),
                pack_path: Some(path),
                dnsbl: None,
                on_rule_matched: None,
            },
            failed,
        ))
    }

    /// Atomic snapshot of the currently-loaded rule pack's metadata
    /// (`pack_version`, `rules_loaded`). Acquires the read lock once
    /// so a concurrent `reload()` can never swap the pack between the
    /// two reads. Used by the `maild.rules.stats` Bus action.
    pub async fn pack_metadata(&self) -> (String, u32) {
        let pack = self.rule_set.read().await;
        (pack.pack_version.clone(), pack.rules.len() as u32)
    }

    /// Whether shadow mode is on. Acquires the config read lock.
    /// Used by `maild.rules.stats`. (SPEC 12 Phase 3 C2: previously
    /// lock-free; now takes the same read lock as `classify` so a
    /// substrate-driven `set_config` swap is observed atomically.)
    pub async fn shadow_mode(&self) -> bool {
        self.config.read().await.shadow_mode
    }

    /// Atomic snapshot-clone of the current engine config. Used by
    /// the C3 converge-loop to compare proposed vs. live before
    /// deciding to apply, and by tests that need to inspect the
    /// live config without holding a lock.
    pub async fn config_snapshot(&self) -> EngineConfig {
        self.config.read().await.clone()
    }

    /// Atomic swap of the engine config. The next `classify` /
    /// `explain` / `shadow_mode` call that begins after this returns
    /// sees the new config; in-flight calls keep their pre-swap
    /// snapshot. C3's `after_set` converge-loop uses this to apply a
    /// post-commit substrate row to the live engine.
    ///
    /// **Lifecycle contract (SPEC 12 Phase 3 §C2/C3):** This is a
    /// total replacement, not a merge — the caller must supply a
    /// fully-formed, **semantically validated** `EngineConfig`.
    /// Validation is the substrate's responsibility: the engine itself
    /// performs no domain checks here, so a caller that passes a
    /// hand-built config (e.g. with `hard_junk_threshold < threshold`
    /// or `body_scan_bytes = 0`) will poison the engine. The intended
    /// caller is the C3 `after_set` converge-loop, which derives the
    /// `EngineConfig` from a substrate row that has already passed
    /// `validate()` + `record_to_engine_config()`'s defensive bounds.
    ///
    /// Concurrent callers serialize through this method's write lock,
    /// but read-modify-write flows must be serialized **externally**
    /// (the C3 converge-loop uses `apply_lock: Mutex<()>` for this).
    /// Two unsynchronized RMW callers can lose updates: A snapshots,
    /// B snapshots, both mutate different fields, both call
    /// `set_config` — the loser's mutation is overwritten.
    pub async fn set_config(&self, cfg: EngineConfig) {
        let mut guard = self.config.write().await;
        *guard = cfg;
    }

    /// Run a single classification, returning the verdict and the full
    /// `Explanation` so `classify` and `explain` can share the work.
    ///
    /// `fire_hook = true` invokes `self.on_rule_matched` once per
    /// matched rule (including the implicit `mime_parse_error`).
    /// `classify` passes `true`; `explain` passes `false` so a debug
    /// `mailctl explain` invocation cannot inflate per-rule stats.
    async fn evaluate(&self, ctx: &RuleContext<'_>, fire_hook: bool) -> (RuleVerdict, Explanation) {
        // SPEC 12 Phase 3 C2 — snapshot-clone the config at the top so a
        // substrate-driven `set_config` swap landing mid-evaluate does
        // not split this call across two configs. The clone is cheap
        // (one Vec<MailAuthHardFailKind> + scalars). Subsequent reads
        // go through the snapshot, not `self.config`.
        let cfg = self.config.read().await.clone();
        // Phase 2 commit 2b — snapshot-clone the rule pack so we can
        // drop the read lock before any await (DNSBL preflight in
        // particular). Holding the read lock across network I/O would
        // pin out a concurrent `reload()` for the duration of the
        // slowest resolver. The clone is `Vec<CompiledRule>` worth of
        // small structs — cheap relative to the DNS round-trip.
        let pack = self.rule_set.read().await.clone();

        // ---- Per-account overrides ----
        let allowlist = compile_sender_globs(&ctx.overrides.allowlist_senders);
        let blocklist = compile_sender_globs(&ctx.overrides.blocklist_senders);
        let envelope_from_lc = ctx.envelope_from.to_ascii_lowercase();
        let threshold = ctx.overrides.threshold_override.unwrap_or(cfg.threshold);

        // ---- Allowlist: short-circuit before parsing the body. ----
        if matches_any(&allowlist, &envelope_from_lc) {
            let exp = Explanation {
                rules: Vec::new(),
                total_score: 0.0,
                threshold,
                pack_version: pack.pack_version.clone(),
                verdict: VerdictShape::HardAccept,
            };
            let verdict = RuleVerdict::HardAccept {
                reason: AcceptReason::AllowlistSender,
                matched_rules: vec![allowlist_matched_id(&allowlist, &envelope_from_lc)],
            };
            return (verdict, exp);
        }

        // ---- Blocklist: short-circuit (still observe shadow mode). ----
        if matches_any(&blocklist, &envelope_from_lc) {
            let matched = vec![blocklist_matched_id(&blocklist, &envelope_from_lc)];
            return Self::junk_or_shadow(
                cfg.shadow_mode,
                JunkReason::BlocklistSender,
                matched,
                0.0,
                threshold,
                pack.pack_version.clone(),
                Vec::new(),
            );
        }

        // ---- Parse the message once. ----
        let mime = MimeViews::parse(ctx.message, &cfg);

        // ---- Implicit `mime_parse_error` rule (weight 1, hard-coded). ----
        let mut implicit = Vec::new();
        if mime.parse_failed {
            let id: RuleId = "mime_parse_error".to_string();
            implicit.push(RuleExplanation {
                id: id.clone(),
                description: "MIME parse failed".to_string(),
                matched: true,
                configured_weight: 1,
                adjustment: 0.0,
                effective_weight: 1.0,
                contribution: 1.0,
            });
            if fire_hook && let Some(hook) = &self.on_rule_matched {
                hook(&id);
            }
        }

        // ---- Mail-auth hard-fail: engine-level decision (independent
        //      of rule pack contents). Recorded on Explanation as a
        //      synthetic rule entry so operators can see the reason. ----
        //
        //      Phase 2 commit 2b — this short-circuit MUST sit before
        //      the DNSBL preflight. A verdict already decided by auth
        //      policy must not pay resolver-latency cost, and (more
        //      importantly) must not leak the peer IP to a third-party
        //      DNSBL for a verdict we are about to discard.
        if !ctx.sender_authenticated
            && let Some(kind) = mail_auth_hard_fail(&cfg, ctx.mail_auth)
        {
            let synth = RuleExplanation {
                id: kind_id(&kind),
                description: format!("mail-auth hard fail ({:?})", kind),
                matched: true,
                configured_weight: 0,
                adjustment: 0.0,
                effective_weight: 0.0,
                contribution: 0.0,
            };
            return Self::junk_or_shadow(
                cfg.shadow_mode,
                JunkReason::MailAuthHardFail,
                vec![synth.id.clone()],
                0.0,
                threshold,
                pack.pack_version.clone(),
                vec![synth],
            );
        }

        // ---- Pre-compute URL count (capped). ----
        let url_count = count_urls(&mime.combined, cfg.url_extraction_cap);

        // Disabled-rule predicate, shared by the DNSBL preflight zone
        // filter and the rule-loop skip below. Keeping a single
        // definition prevents the two sites from drifting apart — a
        // future change to disabled-rule semantics (e.g. account-tier
        // scoping) only needs to touch this closure.
        let is_rule_disabled = |id: &RuleId| ctx.overrides.disabled_rules.iter().any(|d| d == id);

        // ---- DNSBL preflight (async I/O bounded to one step before
        //      the sync matcher loop runs). When no DnsblLookup is
        //      installed or the pack carries no `peer_ip_in_dnsbl`
        //      rules, this is a constant-time empty result. Rules
        //      disabled by the account override are filtered out
        //      before zone collection — a disabled rule cannot match
        //      anyway, and issuing its DNS lookup would waste a
        //      round-trip + leak the peer IP for a discarded verdict.
        let dnsbl_preflight = if let Some(lookup) = &self.dnsbl {
            let zones = preflight::pending_zones(&pack.rules, is_rule_disabled);
            preflight::run(lookup, ctx.peer_ip, &zones).await
        } else {
            preflight::DnsblPreflightResult::empty()
        };

        let inputs = MatchInputs {
            mime: &mime,
            mail_auth: ctx.mail_auth,
            peer_ip: ctx.peer_ip,
            envelope_to_count: ctx.envelope_to.len() as u32,
            url_count,
            dnsbl_preflight: &dnsbl_preflight,
        };

        // ---- Walk the rule set. ----
        let mut explanations = implicit;
        let mut matched_rules = if mime.parse_failed {
            vec!["mime_parse_error".to_string()]
        } else {
            Vec::new()
        };
        let mut raw_score: f32 = if mime.parse_failed { 1.0 } else { 0.0 };

        for rule in pack.rules.iter() {
            if is_rule_disabled(&rule.id) {
                continue;
            }
            let matched = rule.matcher.matches(&inputs);
            let configured = rule.weight as f32;
            let effective = configured.max(0.0);
            let contrib = if matched { effective } else { 0.0 };
            if matched {
                matched_rules.push(rule.id.clone());
                raw_score += contrib;
                if fire_hook && let Some(hook) = &self.on_rule_matched {
                    hook(&rule.id);
                }
            }
            explanations.push(RuleExplanation {
                id: rule.id.clone(),
                description: rule.description.clone(),
                matched,
                configured_weight: rule.weight,
                adjustment: 0.0,
                effective_weight: effective,
                contribution: contrib,
            });
        }

        // ---- Structural anomaly composite. ----
        let exec_attachment = mime.attachments.iter().any(|a| a.executable);
        let auth_failed = !ctx.sender_authenticated
            && (matches!(
                ctx.mail_auth.spf,
                SpfCheck::MailFrom {
                    result: SpfResult::Fail,
                    ..
                } | SpfCheck::Helo {
                    result: SpfResult::Fail,
                    ..
                },
            ) || matches!(ctx.mail_auth.dkim.overall, DkimOutcome::Fail)
                || matches!(ctx.mail_auth.dmarc.outcome, DmarcOutcome::Fail { .. }));
        if exec_attachment && auth_failed {
            return Self::junk_or_shadow(
                cfg.shadow_mode,
                JunkReason::StructuralAnomaly,
                matched_rules,
                raw_score,
                threshold,
                pack.pack_version.clone(),
                explanations,
            );
        }

        // ---- Score-breach. ----
        if raw_score >= cfg.hard_junk_threshold {
            return Self::junk_or_shadow(
                cfg.shadow_mode,
                JunkReason::ScoreBreach,
                matched_rules,
                raw_score,
                threshold,
                pack.pack_version.clone(),
                explanations,
            );
        }

        // ---- Continue. ----
        let exp = Explanation {
            rules: explanations,
            total_score: raw_score,
            threshold,
            pack_version: pack.pack_version.clone(),
            verdict: VerdictShape::Continue,
        };
        let verdict = RuleVerdict::Continue {
            score: raw_score,
            matched_rules,
            would_junk: false,
        };
        (verdict, exp)
    }

    fn junk_or_shadow(
        shadow_mode: bool,
        reason: JunkReason,
        matched_rules: Vec<crate::types::RuleId>,
        score: f32,
        threshold: f32,
        pack_version: String,
        explanations: Vec<RuleExplanation>,
    ) -> (RuleVerdict, Explanation) {
        if shadow_mode {
            let exp = Explanation {
                rules: explanations,
                total_score: score,
                threshold,
                pack_version,
                verdict: VerdictShape::Continue,
            };
            (
                RuleVerdict::Continue {
                    score,
                    matched_rules,
                    would_junk: true,
                },
                exp,
            )
        } else {
            let exp = Explanation {
                rules: explanations,
                total_score: score,
                threshold,
                pack_version,
                verdict: VerdictShape::HardJunk,
            };
            (
                RuleVerdict::HardJunk {
                    reason,
                    matched_rules,
                    score,
                },
                exp,
            )
        }
    }
}

#[async_trait]
impl RuleEngine for DefaultRuleEngine {
    async fn classify(&self, ctx: &RuleContext<'_>) -> Result<RuleVerdict> {
        Ok(self.evaluate(ctx, true).await.0)
    }

    async fn explain(&self, ctx: &RuleContext<'_>) -> Result<Explanation> {
        Ok(self.evaluate(ctx, false).await.1)
    }

    async fn reload(&self) -> Result<ReloadReport> {
        let Some(path) = &self.pack_path else {
            // No pack file configured — return the current state.
            let set = self.rule_set.read().await;
            return Ok(ReloadReport {
                pack_version: set.pack_version.clone(),
                rules_loaded: set.rules.len() as u32,
                rules_failed: Vec::new(),
            });
        };
        let (next, failed) = crate::loader::load_pack_file(path)?;
        let report = ReloadReport {
            pack_version: next.pack_version.clone(),
            rules_loaded: next.rules.len() as u32,
            rules_failed: failed,
        };
        let mut guard = self.rule_set.write().await;
        *guard = next;
        Ok(report)
    }
}

// ---- Helpers ----

fn allowlist_matched_id(matchers: &[SenderGlobMatcher], addr: &str) -> String {
    if matchers.iter().any(|m| m.matches(addr)) {
        "allowlist_sender".to_string()
    } else {
        String::new()
    }
}

fn blocklist_matched_id(matchers: &[SenderGlobMatcher], addr: &str) -> String {
    if matchers.iter().any(|m| m.matches(addr)) {
        "blocklist_sender".to_string()
    } else {
        String::new()
    }
}

/// Map `EngineConfig.mail_auth_hard_fail_kinds` against the
/// `VerifyResult`. Returns the first matching kind, if any.
fn mail_auth_hard_fail(cfg: &EngineConfig, verify: &VerifyResult) -> Option<MailAuthHardFailKind> {
    let spf_failed = matches!(
        verify.spf,
        SpfCheck::MailFrom {
            result: SpfResult::Fail,
            ..
        } | SpfCheck::Helo {
            result: SpfResult::Fail,
            ..
        },
    );
    let dkim_failed = matches!(verify.dkim.overall, DkimOutcome::Fail);
    let dmarc_reject_published = matches!(
        verify.dmarc.report_record.policy_published,
        DmarcPolicy::Reject,
    );
    let arc_pass = matches!(
        verify.arc.chain_validation,
        cosmix_maild_auth::ArcChainValidation::Pass,
    );

    for kind in &cfg.mail_auth_hard_fail_kinds {
        match kind {
            MailAuthHardFailKind::SpfFailDmarcReject if spf_failed && dmarc_reject_published => {
                return Some(*kind);
            }
            MailAuthHardFailKind::DkimFailDmarcReject if dkim_failed && dmarc_reject_published => {
                return Some(*kind);
            }
            MailAuthHardFailKind::BothAuthFailNoArc if spf_failed && dkim_failed && !arc_pass => {
                return Some(*kind);
            }
            _ => {}
        }
    }
    None
}

fn kind_id(kind: &MailAuthHardFailKind) -> String {
    match kind {
        MailAuthHardFailKind::SpfFailDmarcReject => "mail_auth.spf_fail_dmarc_reject".into(),
        MailAuthHardFailKind::DkimFailDmarcReject => "mail_auth.dkim_fail_dmarc_reject".into(),
        MailAuthHardFailKind::BothAuthFailNoArc => "mail_auth.both_fail_no_arc".into(),
    }
}

/// Extract URL count from the combined body view, capped at `cap`.
fn count_urls(haystack: &str, cap: u32) -> u32 {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let mut count = 0u32;
    for link in finder.links(haystack) {
        // Strip cid: and data: per spec.
        let s = link.as_str();
        if s.starts_with("cid:") || s.starts_with("data:") {
            continue;
        }
        count += 1;
        if count >= cap {
            break;
        }
    }
    count
}
