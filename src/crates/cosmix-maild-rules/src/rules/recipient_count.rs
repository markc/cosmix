//! `recipient_count` — fires when `RuleContext::envelope_to.len()`
//! exceeds the configured `gt`.

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct RecipientCount {
    /// Rule fires when envelope-to count > `gt`.
    pub gt: u32,
}

impl RecipientCount {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        inputs.envelope_to_count > self.gt
    }
}
