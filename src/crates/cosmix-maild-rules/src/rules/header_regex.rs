//! `header_regex` — matches when the named header value matches the
//! compiled regex.

use regex::Regex;

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct HeaderRegex {
    pub header_name: String,
    pub pattern: Regex,
}

impl HeaderRegex {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let needle = self.header_name.to_ascii_lowercase();
        inputs
            .mime
            .headers
            .iter()
            .filter(|(n, _)| n == &needle)
            .any(|(_, v)| self.pattern.is_match(v))
    }
}
