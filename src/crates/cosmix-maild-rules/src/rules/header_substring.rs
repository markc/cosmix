//! `header_substring` — case-insensitive substring match against a
//! named header value.

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct HeaderSubstring {
    pub header_name: String,
    pub needle: String,
}

impl HeaderSubstring {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let header_name = self.header_name.to_ascii_lowercase();
        let needle_lc = self.needle.to_ascii_lowercase();
        inputs
            .mime
            .headers
            .iter()
            .filter(|(n, _)| n == &header_name)
            .any(|(_, v)| v.to_ascii_lowercase().contains(&needle_lc))
    }
}
