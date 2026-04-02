//! Dialog renderer dispatch — routes DialogKind to the appropriate Dioxus component.

pub mod message;
pub mod question;
pub mod input;
pub mod text_viewer;
pub mod choice;
pub mod progress;
pub mod form;

use dioxus::prelude::*;
use crate::{DialogRequest, DialogKind};

/// Render the appropriate dialog component for the given request.
#[component]
pub fn DialogView(request: DialogRequest) -> Element {
    match &request.kind {
        DialogKind::Message { .. } => rsx! { message::MessageDialog { request } },
        DialogKind::Question { .. } => rsx! { question::QuestionDialog { request } },
        DialogKind::Entry { .. } | DialogKind::Password { .. } => rsx! { input::InputDialog { request } },
        DialogKind::TextViewer { .. } => rsx! { text_viewer::TextViewerDialog { request } },
        DialogKind::TextInput { .. } => rsx! { text_viewer::TextInputDialog { request } },
        DialogKind::ComboBox { .. } => rsx! { choice::ComboBoxDialog { request } },
        DialogKind::CheckList { .. } => rsx! { choice::CheckListDialog { request } },
        DialogKind::RadioList { .. } => rsx! { choice::RadioListDialog { request } },
        DialogKind::Progress { .. } => rsx! { progress::ProgressDialog { request } },
        DialogKind::Form { .. } => rsx! { form::FormDialog { request } },
        _ => rsx! {
            div { class: "alert-dialog",
                div { class: "alert-dialog-body",
                    div { class: "alert-dialog-description", "Dialog type not yet implemented" }
                }
            }
        },
    }
}
