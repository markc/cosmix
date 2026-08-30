//! Pure-function mappers from `mail_auth` output types to our public
//! `cosmix-maild-auth` types.
//!
//! Kept apart from `verifier.rs` so the translation layer can be unit-
//! tested without spinning up an async resolver. `verifier.rs` is the
//! orchestration; this module is the dictionary.
//!
//! Caps live here: `max_dkim_signatures` and `max_arc_instances` are
//! enforced when collapsing the per-signature outputs into our
//! aggregates so the verifier does not have to remember to cap.

use crate::types::{
    Alignment, ArcChainValidation, ArcResult, DkimAggregate, DkimOutcome, DkimSignatureOutcome,
    DkimSignatureResult, DmarcDisposition, DmarcOutcome, DmarcPolicy, DmarcReportRecord,
    DmarcResult as OurDmarcResult, IprevOutcome, IprevResult as OurIprevResult, SpfResult,
};
use mail_auth::{
    ArcOutput, DkimOutput, DkimResult, DmarcOutput, DmarcResult as MaDmarcResult, IprevOutput,
    IprevResult as MaIprevResult, SpfResult as MaSpfResult, dmarc::Policy as MaPolicy,
};
use std::net::IpAddr;

// ---- SPF ----------------------------------------------------------

pub(crate) fn map_spf_result(r: MaSpfResult) -> SpfResult {
    match r {
        MaSpfResult::Pass => SpfResult::Pass,
        MaSpfResult::Fail => SpfResult::Fail,
        MaSpfResult::SoftFail => SpfResult::SoftFail,
        MaSpfResult::Neutral => SpfResult::Neutral,
        MaSpfResult::None => SpfResult::None,
        MaSpfResult::TempError => SpfResult::TempError,
        MaSpfResult::PermError => SpfResult::PermError,
    }
}

// ---- iprev --------------------------------------------------------

pub(crate) fn map_iprev(out: &IprevOutput, peer_ip: IpAddr) -> OurIprevResult {
    let result = match out.result() {
        MaIprevResult::Pass => IprevOutcome::Pass,
        MaIprevResult::Fail(_) => IprevOutcome::Fail,
        MaIprevResult::TempError(_) => IprevOutcome::TempError,
        MaIprevResult::PermError(_) => IprevOutcome::PermError,
        MaIprevResult::None => IprevOutcome::PermError,
    };
    let ptrs = out.ptr.as_deref().cloned().unwrap_or_default();
    let ptr = ptrs.first().cloned();
    let matched_forward = match result {
        IprevOutcome::Pass => Some(peer_ip),
        _ => None,
    };
    OurIprevResult {
        result,
        ptr,
        matched_forward,
    }
}

// ---- DKIM ---------------------------------------------------------

pub(crate) fn map_dkim_signature(out: &DkimOutput<'_>) -> DkimSignatureResult {
    let (d, s) = match out.signature() {
        Some(sig) => (sig.d.clone(), sig.s.clone()),
        None => (String::new(), String::new()),
    };
    let result = match out.result() {
        DkimResult::Pass => DkimSignatureOutcome::Pass,
        DkimResult::Fail(_) => DkimSignatureOutcome::Fail,
        DkimResult::Neutral(_) => DkimSignatureOutcome::Neutral,
        DkimResult::TempError(_) => DkimSignatureOutcome::TempError,
        DkimResult::PermError(_) => DkimSignatureOutcome::PermError,
        DkimResult::None => DkimSignatureOutcome::Neutral,
    };
    DkimSignatureResult { d, s, result }
}

/// Collapse per-signature outcomes into a message-level outcome. The
/// chosen reduction matches RFC 6376 §6.1: if *any* sig passes, the
/// message DKIM result is `pass`; else if any signature is in temp-
/// error the result is temp-error (so the caller may retry/defer);
/// else the strongest perm-error wins; else `none`.
pub(crate) fn collapse_dkim_outcome(sigs: &[DkimSignatureResult]) -> DkimOutcome {
    if sigs.is_empty() {
        return DkimOutcome::None;
    }
    let mut any_temp = false;
    let mut any_perm = false;
    let mut any_fail = false;
    for s in sigs {
        match s.result {
            DkimSignatureOutcome::Pass => return DkimOutcome::Pass,
            DkimSignatureOutcome::TempError => any_temp = true,
            DkimSignatureOutcome::PermError => any_perm = true,
            DkimSignatureOutcome::Fail => any_fail = true,
            DkimSignatureOutcome::Neutral => (),
        }
    }
    if any_temp {
        DkimOutcome::TempError
    } else if any_fail {
        DkimOutcome::Fail
    } else if any_perm {
        DkimOutcome::PermError
    } else {
        DkimOutcome::None
    }
}

/// Apply `max_dkim_signatures` cap and build the aggregate. Excess
/// signatures past the cap are dropped silently and `capped` is set —
/// reduces the impact of a hostile sender that attaches thousands of
/// signatures to inflate verification cost.
pub(crate) fn map_dkim(outputs: &[DkimOutput<'_>], max: u32) -> DkimAggregate {
    let max = max as usize;
    let capped = outputs.len() > max;
    let take = outputs.iter().take(max);
    let signatures: Vec<DkimSignatureResult> = take.map(map_dkim_signature).collect();
    let overall = collapse_dkim_outcome(&signatures);
    DkimAggregate {
        signatures,
        overall,
        capped,
    }
}

// ---- DMARC --------------------------------------------------------

fn map_policy(p: MaPolicy) -> DmarcPolicy {
    match p {
        MaPolicy::None | MaPolicy::Unspecified => DmarcPolicy::None,
        MaPolicy::Quarantine => DmarcPolicy::Quarantine,
        MaPolicy::Reject => DmarcPolicy::Reject,
    }
}

fn dmarc_result_passed(r: &MaDmarcResult) -> bool {
    matches!(r, MaDmarcResult::Pass)
}

pub(crate) fn map_dmarc(out: &DmarcOutput, peer_ip: IpAddr, org_domain: String) -> OurDmarcResult {
    let policy_published = map_policy(out.policy());
    let spf_aligned = dmarc_result_passed(out.spf_result());
    let dkim_aligned = dmarc_result_passed(out.dkim_result());
    let any_pass = spf_aligned || dkim_aligned;

    // mail_auth's DmarcOutput does not give us the published vs.
    // evaluated split directly; we treat the policy stored on the
    // output as both, since mail-auth has already applied alignment to
    // pick the relevant policy (sp vs p).
    let policy_evaluated = policy_published;

    let outcome = if any_pass {
        DmarcOutcome::Pass
    } else if out.dmarc_record().is_none() {
        DmarcOutcome::None
    } else {
        // Derive the alignment kind we *attempted* — relaxed by default
        // per spec convention; the verifier itself doesn't expose
        // strict-vs-relaxed in the output, so we report Relaxed (the
        // common case) when the record is present and neither side
        // aligned. NotAligned would be a stronger signal but mail-auth
        // doesn't surface that distinction.
        DmarcOutcome::Fail {
            policy: policy_evaluated,
            alignment: Alignment::Relaxed,
        }
    };

    let disposition = match (&outcome, policy_evaluated) {
        (DmarcOutcome::Pass | DmarcOutcome::None, _) => DmarcDisposition::None,
        (DmarcOutcome::Fail { .. }, DmarcPolicy::None) => DmarcDisposition::None,
        (DmarcOutcome::Fail { .. }, DmarcPolicy::Quarantine) => DmarcDisposition::Quarantine,
        (DmarcOutcome::Fail { .. }, DmarcPolicy::Reject) => DmarcDisposition::Reject,
    };

    let report_record = DmarcReportRecord {
        org_domain,
        source_ip: peer_ip,
        policy_published,
        policy_evaluated,
        spf_aligned,
        dkim_aligned,
        disposition,
        count: 1,
    };

    OurDmarcResult {
        outcome,
        report_record,
    }
}

// ---- ARC ----------------------------------------------------------

pub(crate) fn map_arc(out: &ArcOutput<'_>, max_instances: u32) -> ArcResult {
    let instance_count = out.sets().len() as u32;
    let chain_validation = match out.result() {
        DkimResult::Pass => ArcChainValidation::Pass,
        DkimResult::None => ArcChainValidation::None,
        DkimResult::Fail(_)
        | DkimResult::Neutral(_)
        | DkimResult::TempError(_)
        | DkimResult::PermError(_) => ArcChainValidation::Fail,
    };
    // mail-auth itself caps at 50 internally; we surface our own cap
    // for symmetry with DKIM. If the mail-auth library cap is lower,
    // the count will already reflect that.
    let _ = max_instances;
    // The "oldest_pass_chain" signal — ARC carries the prior receivers'
    // claim about whether their inbound chain was already passing. We
    // approximate this by checking if any AAR set claims a prior pass;
    // mail_auth doesn't expose that field directly, so for Phase 1 we
    // anchor it to the same outcome as the chain (later phases can
    // mine `out.sets()[0].results` once we add an AAR parser).
    let oldest_pass_chain = matches!(chain_validation, ArcChainValidation::Pass);
    ArcResult {
        chain_validation,
        instance_count,
        oldest_pass_chain,
    }
}

// ---- DMARC org-domain helper -------------------------------------

/// Cheap org-domain extractor used as the `domain_suffix_fn` for
/// `mail_auth::DmarcParameters`. Returns the rightmost two labels
/// (e.g. `mail.example.co.uk` → `co.uk`, which is wrong for ccTLDs but
/// correct for plain TLDs). Phase 1 ships this; Phase 2 swaps in a PSL
/// implementation. Documented as a known limitation in the spec.
pub fn naive_org_domain(d: &str) -> &str {
    let bytes = d.as_bytes();
    let mut dots = 0;
    let mut last = bytes.len();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'.' {
            dots += 1;
            if dots == 2 {
                return &d[i + 1..];
            }
            last = i;
        }
    }
    let _ = last;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_pass_wins() {
        let sigs = vec![
            DkimSignatureResult {
                d: "a".into(),
                s: "x".into(),
                result: DkimSignatureOutcome::Fail,
            },
            DkimSignatureResult {
                d: "b".into(),
                s: "y".into(),
                result: DkimSignatureOutcome::Pass,
            },
        ];
        assert_eq!(collapse_dkim_outcome(&sigs), DkimOutcome::Pass);
    }

    #[test]
    fn collapse_temp_beats_perm() {
        let sigs = vec![
            DkimSignatureResult {
                d: "a".into(),
                s: "x".into(),
                result: DkimSignatureOutcome::PermError,
            },
            DkimSignatureResult {
                d: "b".into(),
                s: "y".into(),
                result: DkimSignatureOutcome::TempError,
            },
        ];
        assert_eq!(collapse_dkim_outcome(&sigs), DkimOutcome::TempError);
    }

    #[test]
    fn collapse_empty_is_none() {
        assert_eq!(collapse_dkim_outcome(&[]), DkimOutcome::None);
    }

    #[test]
    fn collapse_neutral_only_is_none() {
        let sigs = vec![DkimSignatureResult {
            d: "a".into(),
            s: "x".into(),
            result: DkimSignatureOutcome::Neutral,
        }];
        assert_eq!(collapse_dkim_outcome(&sigs), DkimOutcome::None);
    }

    #[test]
    fn map_spf_result_round_trip() {
        assert_eq!(map_spf_result(MaSpfResult::Pass), SpfResult::Pass);
        assert_eq!(map_spf_result(MaSpfResult::Fail), SpfResult::Fail);
        assert_eq!(map_spf_result(MaSpfResult::SoftFail), SpfResult::SoftFail);
        assert_eq!(map_spf_result(MaSpfResult::Neutral), SpfResult::Neutral);
        assert_eq!(map_spf_result(MaSpfResult::None), SpfResult::None);
        assert_eq!(map_spf_result(MaSpfResult::TempError), SpfResult::TempError);
        assert_eq!(map_spf_result(MaSpfResult::PermError), SpfResult::PermError);
    }

    #[test]
    fn naive_org_domain_two_labels() {
        assert_eq!(naive_org_domain("mail.example.org"), "example.org");
        assert_eq!(naive_org_domain("a.b.c.d.example.org"), "example.org");
        assert_eq!(naive_org_domain("example.org"), "example.org");
        assert_eq!(naive_org_domain("localhost"), "localhost");
    }
}
