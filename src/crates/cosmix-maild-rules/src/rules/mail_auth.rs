//! `mail_auth` — fires when `cosmix-maild-auth`'s `VerifyResult`
//! matches the configured outcome predicates. Each `Option<…>` field
//! that is `Some` adds an AND-clause; the rule fires when all
//! configured predicates hold.
//!
//! Note: this is the *operator-configurable* mail-auth rule kind for
//! contributing to the soft score. The umbrella's HardJunk verdict on
//! mail-auth uses `EngineConfig.mail_auth_hard_fail_kinds` (engine-
//! level, separate from the rule pack).

use cosmix_maild_auth::{DkimOutcome, DmarcOutcome, SpfCheck, SpfResult};

use crate::compiled::MatchInputs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpfMatch {
    Pass,
    Fail,
    SoftFail,
    Neutral,
    None,
    TempError,
    PermError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimMatch {
    Pass,
    Fail,
    None,
    TempError,
    PermError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarcMatch {
    Pass,
    Fail,
    None,
}

#[derive(Debug, Clone)]
pub struct MailAuth {
    pub spf: Option<SpfMatch>,
    pub dkim: Option<DkimMatch>,
    pub dmarc: Option<DmarcMatch>,
}

impl MailAuth {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        if self.spf.is_none() && self.dkim.is_none() && self.dmarc.is_none() {
            return false;
        }
        if let Some(want) = self.spf {
            let actual = match &inputs.mail_auth.spf {
                SpfCheck::MailFrom { result, .. } | SpfCheck::Helo { result, .. } => *result,
            };
            if !spf_eq(want, actual) {
                return false;
            }
        }
        if let Some(want) = self.dkim
            && !dkim_eq(want, inputs.mail_auth.dkim.overall)
        {
            return false;
        }
        if let Some(want) = self.dmarc
            && !dmarc_eq(want, &inputs.mail_auth.dmarc.outcome)
        {
            return false;
        }
        true
    }
}

fn spf_eq(want: SpfMatch, got: SpfResult) -> bool {
    matches!(
        (want, got),
        (SpfMatch::Pass, SpfResult::Pass)
            | (SpfMatch::Fail, SpfResult::Fail)
            | (SpfMatch::SoftFail, SpfResult::SoftFail)
            | (SpfMatch::Neutral, SpfResult::Neutral)
            | (SpfMatch::None, SpfResult::None)
            | (SpfMatch::TempError, SpfResult::TempError)
            | (SpfMatch::PermError, SpfResult::PermError)
    )
}

fn dkim_eq(want: DkimMatch, got: DkimOutcome) -> bool {
    matches!(
        (want, got),
        (DkimMatch::Pass, DkimOutcome::Pass)
            | (DkimMatch::Fail, DkimOutcome::Fail)
            | (DkimMatch::None, DkimOutcome::None)
            | (DkimMatch::TempError, DkimOutcome::TempError)
            | (DkimMatch::PermError, DkimOutcome::PermError)
    )
}

fn dmarc_eq(want: DmarcMatch, got: &DmarcOutcome) -> bool {
    matches!(
        (want, got),
        (DmarcMatch::Pass, DmarcOutcome::Pass)
            | (DmarcMatch::Fail, DmarcOutcome::Fail { .. })
            | (DmarcMatch::None, DmarcOutcome::None)
    )
}
