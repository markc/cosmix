//! Yes/no questions: save changes when closing the last view of a dirty
//! buffer (D13 — never on exit), replace an existing file on Save As, and
//! overwrite a file that changed on disk.

use cosmix_edit_client::types::{Intent, TabId};
use iced::Element;

use super::{DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `edit.close` refused `CONFLICT dirty`: this tab holds the last view.
    CloseDirty { tab: TabId, name: String, intent: Intent },
    /// Save As onto an existing file.
    Overwrite { tab: TabId, path: String, intent: Intent },
    /// A plain save refused `disk_modified`.
    DiskModified { tab: TabId, name: String, intent: Intent },
}

/// The button pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Save / Replace / Overwrite.
    Accept,
    /// Don't Save (discard).
    Discard,
    Cancel,
}

impl Confirm {
    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg> {
        let m = |c: Choice| Msg::Dialog(DialogMsg::Confirm(c));
        let t = look.tokens;
        let cancel = || -> Element<'a, Msg> { look.button("Cancel", Some(m(Choice::Cancel))).style(look.secondary()).into() };
        match self {
            Confirm::CloseDirty { name, .. } => frame(
                look,
                "Save changes?",
                look.text(format!(
                    "“{name}” has unsaved changes and no other view is holding it. Save before closing?"
                ))
                .into(),
                vec![
                    look.button("Don't Save", Some(m(Choice::Discard))).style(look.danger()).into(),
                    cancel(),
                    look.button("Save", Some(m(Choice::Accept))).style(look.primary()).into(),
                ],
                460.0,
            ),
            Confirm::Overwrite { path, .. } => frame(
                look,
                "Replace file?",
                iced::widget::column![
                    look.text("This file already exists:"),
                    look.code(path.as_str()).color(t.muted_text),
                    look.text("Replace it with this buffer?"),
                ]
                .spacing(6)
                .into(),
                vec![cancel(), look.button("Replace", Some(m(Choice::Accept))).style(look.danger()).into()],
                520.0,
            ),
            Confirm::DiskModified { name, .. } => frame(
                look,
                "File changed on disk",
                look.text(format!("“{name}” changed on disk since it was loaded. Saving now overwrites those changes.")).into(),
                vec![cancel(), look.button("Overwrite", Some(m(Choice::Accept))).style(look.danger()).into()],
                480.0,
            ),
        }
    }
}
