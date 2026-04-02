//! Message dialog — Info, Warning, Error with OK button.

use dioxus::prelude::*;

use crate::types::{DialogKind, MessageLevel};
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

const ICON_INFO: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>"#;
const ICON_WARNING: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>"#;
const ICON_ERROR: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>"#;

#[component]
pub fn MessageDialog(request: DialogRequest) -> Element {
    let DialogKind::Message { ref text, level, ref detail } = request.kind else {
        return rsx! {};
    };

    let (icon, bg_color) = match level {
        MessageLevel::Info => (ICON_INFO, "#3b82f6"),
        MessageLevel::Warning => (ICON_WARNING, "#f59e0b"),
        MessageLevel::Error => (ICON_ERROR, "#ef4444"),
    };

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body",
                div { class: "alert-dialog-header",
                    div {
                        class: "alert-dialog-icon",
                        style: "background:{bg_color}",
                        span { dangerous_inner_html: icon }
                    }
                    div { class: "alert-dialog-title", "{text}" }
                }
                if let Some(detail) = detail {
                    pre { class: "alert-dialog-detail", "{detail}" }
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| {
                        complete(DialogAction::Ok, DialogData::None);
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}
