//! Text dialogs — TextViewer (read-only scrollable) and TextInput (multi-line editor).

use dioxus::prelude::*;

use crate::types::{DialogKind, TextSource};
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

/// Read-only text viewer.
#[component]
pub fn TextViewerDialog(request: DialogRequest) -> Element {
    let DialogKind::TextViewer { ref source, ref checkbox } = request.kind else {
        return rsx! {};
    };

    let content = match source {
        TextSource::Stdin(s) | TextSource::Inline(s) => s.clone(),
        TextSource::File(path) => std::fs::read_to_string(path).unwrap_or_else(|e| {
            format!("Error reading {}: {e}", path.display())
        }),
    };

    let mut checked = use_signal(|| false);
    let has_checkbox = checkbox.is_some();
    let checkbox_label = checkbox.clone().unwrap_or_default();

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                pre { class: "alert-dialog-scroll",
                    style: "padding:0.5rem; font-family:var(--font-mono,monospace); font-size:0.8125rem; line-height:1.5; white-space:pre-wrap; word-break:break-word",
                    "{content}"
                }
                if has_checkbox {
                    label { class: "alert-dialog-list-item",
                        input {
                            r#type: "checkbox",
                            checked: "{checked}",
                            onchange: move |e: FormEvent| {
                                checked.set(e.value() == "true");
                            },
                        }
                        "{checkbox_label}"
                    }
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| {
                        complete(DialogAction::Cancel, DialogData::None);
                        exit();
                    },
                    "Cancel"
                }
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| {
                        if has_checkbox {
                            complete(DialogAction::Ok, DialogData::Bool(*checked.read()));
                        } else {
                            complete(DialogAction::Ok, DialogData::None);
                        }
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}

/// Multi-line text editor.
#[component]
pub fn TextInputDialog(request: DialogRequest) -> Element {
    let DialogKind::TextInput { ref text, ref default } = request.kind else {
        return rsx! {};
    };

    let prompt = text.clone();
    let mut value = use_signal(|| default.clone().unwrap_or_default());

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                if !prompt.is_empty() {
                    div { class: "alert-dialog-title", "{prompt}" }
                }
                textarea {
                    class: "alert-dialog-field-textarea",
                    style: "flex:1; resize:none",
                    value: "{value}",
                    oninput: move |e: FormEvent| {
                        value.set(e.value());
                    },
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| {
                        complete(DialogAction::Cancel, DialogData::None);
                        exit();
                    },
                    "Cancel"
                }
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| {
                        complete(DialogAction::Ok, DialogData::Text(value.read().clone()));
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}
