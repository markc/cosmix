//! `body_substring` — case-insensitive substring over `MatchView`.

use crate::compiled::MatchInputs;
use crate::types::MatchView;

#[derive(Debug, Clone)]
pub struct BodySubstring {
    pub needle: String,
    pub view: MatchView,
}

impl BodySubstring {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let haystack = match self.view {
            MatchView::Plain => &inputs.mime.plain,
            MatchView::Html => &inputs.mime.html,
            MatchView::Combined => &inputs.mime.combined,
        };
        let needle = self.needle.to_ascii_lowercase();
        haystack.to_ascii_lowercase().contains(&needle)
    }
}
