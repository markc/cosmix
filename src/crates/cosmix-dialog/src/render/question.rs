//! Question dialog — Yes/No with optional Cancel button.

use dioxus::prelude::*;

use crate::types::DialogKind;
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

const ICON_QUESTION: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>"#;

#[component]
pub fn QuestionDialog(request: DialogRequest) -> Element {
    let DialogKind::Question { ref text, ref yes_label, ref no_label, cancel } = request.kind else {
        return rsx! {};
    };

    let yes_text = yes_label.as_deref().unwrap_or("Yes");
    let no_text = no_label.as_deref().unwrap_or("No");

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body",
                div { class: "alert-dialog-header",
                    div {
                        class: "alert-dialog-icon",
                        style: "background:#f59e0b",
                        span { dangerous_inner_html: ICON_QUESTION }
                    }
                    div { class: "alert-dialog-title", "{text}" }
                }
            }
            div { class: "alert-dialog-actions",
                if cancel {
                    div {
                        class: "alert-dialog-ghost",
                        onclick: move |_| {
                            complete(DialogAction::Cancel, DialogData::None);
                            exit();
                        },
                        "Cancel"
                    }
                }
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| {
                        complete(DialogAction::No, DialogData::None);
                        exit();
                    },
                    "{no_text}"
                }
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| {
                        complete(DialogAction::Yes, DialogData::None);
                        exit();
                    },
                    "{yes_text}"
                }
            }
        }
    }
}
