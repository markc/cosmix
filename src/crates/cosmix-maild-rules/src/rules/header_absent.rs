//! `header_absent` — matches when the named header is missing.

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct HeaderAbsent {
    pub header_name: String,
}

impl HeaderAbsent {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let needle = self.header_name.to_ascii_lowercase();
        !inputs.mime.headers.iter().any(|(n, _)| n == &needle)
    }
}
