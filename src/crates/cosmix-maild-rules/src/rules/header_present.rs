//! `header_present` — matches when the named header appears at least
//! once in the message.

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct HeaderPresent {
    pub header_name: String,
}

impl HeaderPresent {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let needle = self.header_name.to_ascii_lowercase();
        inputs.mime.headers.iter().any(|(n, _)| n == &needle)
    }
}
