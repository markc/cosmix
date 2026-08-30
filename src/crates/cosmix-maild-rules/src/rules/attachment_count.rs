//! `attachment_count` — fires when MIME attachment count crosses the
//! threshold. With `executable_only`, only attachments matching the
//! executable extension/content-type list are counted.

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct AttachmentCount {
    /// Rule fires when count > `gt`.
    pub gt: u32,
    pub executable_only: bool,
}

impl AttachmentCount {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        let count = inputs
            .mime
            .attachments
            .iter()
            .filter(|a| !self.executable_only || a.executable)
            .count() as u32;
        count > self.gt
    }
}
