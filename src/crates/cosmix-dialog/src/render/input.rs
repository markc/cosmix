//! Input dialogs — Entry (single-line) and Password (masked).

use dioxus::prelude::*;

use crate::types::DialogKind;
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

const ICON_EDIT: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/></svg>"#;

#[component]
pub fn InputDialog(request: DialogRequest) -> Element {
    let (prompt, default_val, is_password) = match &request.kind {
        DialogKind::Entry { text, default, .. } => {
            (text.clone(), default.clone().unwrap_or_default(), false)
        }
        DialogKind::Password { text } => (text.clone(), String::new(), true),
        _ => return rsx! {},
    };

    let mut value = use_signal(|| default_val.clone());

    let submit = move || {
        let text = value.read().clone();
        complete(DialogAction::Ok, DialogData::Text(text));
        exit();
    };

    let cancel = move || {
        complete(DialogAction::Cancel, DialogData::None);
        exit();
    };

    let input_type = if is_password { "password" } else { "text" };

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body",
                div { class: "alert-dialog-header",
                    div {
                        class: "alert-dialog-icon",
                        style: "background:#8b5cf6",
                        span { dangerous_inner_html: ICON_EDIT }
                    }
                    div { class: "alert-dialog-title", "{prompt}" }
                }
                div { class: "alert-dialog-input",
                    input {
                        class: "alert-dialog-field-input",
                        r#type: input_type,
                        value: "{value}",
                        oninput: move |e: FormEvent| {
                            value.set(e.value());
                        },
                        onkeydown: move |e: KeyboardEvent| {
                            if e.key() == Key::Enter {
                                submit();
                            } else if e.key() == Key::Escape {
                                cancel();
                            }
                        },
                    }
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| cancel(),
                    "Cancel"
                }
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| submit(),
                    "OK"
                }
            }
        }
    }
}
