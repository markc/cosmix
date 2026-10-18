//! `body_regex` — regex over the configured `MatchView`.

use regex::Regex;

use crate::compiled::MatchInputs;
use crate::types::MatchView;

#[derive(Debug, Clone)]
pub struct BodyRegex {
    pub pattern: Regex,
    pub view: MatchView,
}

impl BodyRegex {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let haystack = match self.view {
            MatchView::Plain => &inputs.mime.plain,
            MatchView::Html => &inputs.mime.html,
            MatchView::Combined => &inputs.mime.combined,
        };
        self.pattern.is_match(haystack)
    }
}
