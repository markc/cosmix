//! `url_count` — fires when the extracted URL count exceeds the
//! configured `gt` threshold (subject to
//! `EngineConfig.url_extraction_cap` in `MatchInputs::url_count`).

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct UrlCount {
    /// Threshold; rule fires when extracted URL count > `gt`.
    pub gt: u32,
}

impl UrlCount {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        inputs.url_count > self.gt
    }
}
