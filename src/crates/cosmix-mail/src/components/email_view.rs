use dioxus::prelude::*;
use crate::jmap::Email;
use super::icons::*;
use cosmix_ui::icons::{ICON_REPLY, ICON_FORWARD, ICON_MAIL_OPEN, ICON_BACK};

const ACTION_BTN: &str = "padding:4px 10px; background:none; border:1px solid var(--border); color:var(--fg-muted); border-radius:var(--radius-sm); cursor:pointer; font-size:var(--font-size-sm); display:flex; align-items:center; gap:4px;";

#[component]
pub fn EmailView(
    email: Option<Email>,
    on_back: EventHandler<()>,
    on_reply: EventHandler<Email>,
    on_forward: EventHandler<Email>,
    on_delete: EventHandler<Email>,
    on_archive: EventHandler<Email>,
    on_toggle_read: EventHandler<Email>,
) -> Element {
    let Some(email) = email else {
        return rsx! {
            div {
                style: "flex:1; display:flex; align-items:center; justify-content:center; color:var(--fg-muted); height:100%;",
                div { style: "text-align:center;",
                    div { style: "margin-bottom:8px; opacity:0.3;", dangerous_inner_html: r#"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="16" x="2" y="4" rx="2"/><path d="m22 7-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 7"/></svg>"# }
                    span { style: "font-size:var(--font-size-sm);", "Select a message to read" }
                }
            }
        };
    };

    let body_html = if let Some(html) = email.html_body_value() {
        html.to_string()
    } else if let Some(text) = email.text_body_value() {
        render_markdown(text)
    } else {
        "<p style=\"color:var(--fg-muted)\">No content</p>".to_string()
    };

    let to_display = email
        .to
        .as_ref()
        .map(|addrs| {
            addrs
                .iter()
                .map(|a| a.email.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let read_label = if email.is_read() { "Mark Unread" } else { "Mark Read" };

    let email_reply = email.clone();
    let email_fwd = email.clone();
    let email_del = email.clone();
    let email_arch = email.clone();
    let email_read = email.clone();

    rsx! {
        div {
            style: "flex:1; display:flex; flex-direction:column; min-width:0; overflow:hidden; background:var(--bg-primary); height:100%;",
            // Email header
            div {
                style: "flex-shrink:0; padding:12px 16px 10px; border-bottom:1px solid var(--border); background:var(--bg-secondary);",
                // Mobile back + subject row
                div {
                    style: "display:flex; align-items:center; gap:8px; margin-bottom:6px;",
                    button {
                        class: "mobile-back",
                        style: "display:none; background:none; border:none; color:var(--fg-muted); cursor:pointer; padding:4px; flex-shrink:0;",
                        onclick: move |_| on_back.call(()),
                        dangerous_inner_html: "{ICON_BACK}"
                    }
                    h2 { style: "font-size:var(--font-size-lg); font-weight:600; color:var(--fg-primary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;",
                        "{email.subject.as_deref().unwrap_or(\"(no subject)\")}"
                    }
                }
                div {
                    style: "display:flex; flex-wrap:wrap; gap:8px 12px; font-size:var(--font-size-sm); color:var(--fg-muted); align-items:center;",
                    span { style: "display:inline-flex; align-items:center; gap:4px;",
                        span { dangerous_inner_html: "{ICON_USER}" }
                        span { style: "color:var(--fg-secondary);", "{email.from_display()}" }
                    }
                    span { style: "display:inline-flex; align-items:center; gap:4px;",
                        "To: "
                        span { style: "color:var(--fg-secondary);", "{to_display}" }
                    }
                    span { style: "display:inline-flex; align-items:center; gap:4px;",
                        span { dangerous_inner_html: "{ICON_CLOCK}" }
                        "{email.date_short()}"
                    }
                    if email.has_attachment.unwrap_or(false) {
                        span { style: "display:inline-flex; align-items:center; gap:4px; color:var(--warning);",
                            span { dangerous_inner_html: "{ICON_PAPERCLIP}" }
                            "attachment"
                        }
                    }
                }
            }
            // Action bar
            div {
                class: "action-bar",
                style: "flex-shrink:0; padding:8px 16px; border-bottom:1px solid var(--border); display:flex; flex-wrap:wrap; gap:6px; background:var(--bg-secondary);",
                button {
                    style: "{ACTION_BTN}",
                    onclick: move |_| on_reply.call(email_reply.clone()),
                    span { dangerous_inner_html: "{ICON_REPLY}" }
                    "Reply"
                }
                button {
                    style: "{ACTION_BTN}",
                    onclick: move |_| on_forward.call(email_fwd.clone()),
                    span { dangerous_inner_html: "{ICON_FORWARD}" }
                    "Forward"
                }
                button {
                    style: "{ACTION_BTN}",
                    onclick: move |_| on_archive.call(email_arch.clone()),
                    span { dangerous_inner_html: "{ICON_ARCHIVE}" }
                    span { class: "desktop-only", "Archive" }
                }
                button {
                    style: "{ACTION_BTN}",
                    onclick: move |_| on_delete.call(email_del.clone()),
                    span { dangerous_inner_html: "{ICON_TRASH}" }
                    span { class: "desktop-only", "Delete" }
                }
                button {
                    style: "{ACTION_BTN}",
                    onclick: move |_| on_toggle_read.call(email_read.clone()),
                    span { dangerous_inner_html: "{ICON_MAIL_OPEN}" }
                    span { class: "desktop-only", "{read_label}" }
                }
            }
            // Email body
            div {
                style: "flex:1; overflow-y:auto; padding:16px;",
                div {
                    class: "prose",
                    style: "max-width:768px;",
                    dangerous_inner_html: "{body_html}"
                }
            }
        }
    }
}

pub fn render_markdown(text: &str) -> String {
    cosmix_ui::markdown::render_simple(text)
}
