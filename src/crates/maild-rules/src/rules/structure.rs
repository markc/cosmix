//! `structure` — composite structural-anomaly checks (HTML-only,
//! deep nesting, missing Message-ID, executable attachments).

use crate::compiled::MatchInputs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructureCheck {
    HtmlOnlyNoAlternative,
    DeepMultipartNesting,
    MissingMessageId,
    HasExecutableAttachment,
}

#[derive(Debug, Clone)]
pub struct Structure {
    pub check: StructureCheck,
}

impl Structure {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        match self.check {
            StructureCheck::HtmlOnlyNoAlternative => inputs.mime.html_only,
            StructureCheck::DeepMultipartNesting => inputs.mime.deep_nesting,
            StructureCheck::MissingMessageId => inputs.mime.missing_message_id,
            StructureCheck::HasExecutableAttachment => {
                inputs.mime.attachments.iter().any(|a| a.executable)
            }
        }
    }
}
