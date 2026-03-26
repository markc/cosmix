use dioxus::prelude::*;
use crate::jmap::Mailbox;
use super::icons::*;

#[component]
pub fn MailboxList(
    class: String,
    mailboxes: Vec<Mailbox>,
    selected: Option<String>,
    on_select: EventHandler<String>,
    on_compose: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            class: "{class}",
            style: "width:200px; min-width:200px; display:flex; flex-direction:column; background:#111827; border-right:1px solid #1f2937; height:100%;",
            // App title
            div {
                style: "height:44px; display:flex; align-items:center; padding:0 14px; border-bottom:1px solid #1f2937; gap:8px;",
                span { dangerous_inner_html: "{ICON_MAIL}" }
                span { style: "font-weight:600; color:#f3f4f6;", "cosmix" }
            }
            // Compose button
            div {
                style: "padding:8px 8px 4px;",
                button {
                    style: "width:100%; padding:7px 12px; background:#2563eb; color:#fff; border:none; border-radius:6px; cursor:pointer; font-size:12px; font-weight:500; display:flex; align-items:center; gap:6px; justify-content:center;",
                    onclick: move |_| on_compose.call(()),
                    span { dangerous_inner_html: "{ICON_FILE_EDIT}" }
                    "Compose"
                }
            }
            // Mailbox list
            nav {
                style: "flex:1; overflow-y:auto; padding:6px 0;",
                for mbox in mailboxes {
                    {
                        let id = mbox.id.clone();
                        let is_selected = selected.as_deref() == Some(&id);
                        let icon = mailbox_icon(mbox.role.as_deref());
                        let bg = if is_selected { "background:#2563eb; color:#fff;" } else { "background:transparent; color:#9ca3af;" };
                        rsx! {
                            button {
                                key: "{id}",
                                style: "width:calc(100% - 8px); text-align:left; padding:5px 12px; margin:1px 4px; border-radius:4px; display:flex; align-items:center; gap:8px; font-size:12px; border:none; cursor:pointer; {bg}",
                                onclick: move |_| on_select.call(id.clone()),
                                span { dangerous_inner_html: "{icon}" }
                                span { style: "flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;", "{mbox.name}" }
                                if mbox.unread_emails > 0 {
                                    span {
                                        style: "margin-left:auto; background:#2563eb; color:#fff; font-size:10px; padding:1px 6px; border-radius:8px; min-width:16px; text-align:center;",
                                        "{mbox.unread_emails}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Account
            div {
                style: "padding:8px 14px; border-top:1px solid #1f2937; font-size:11px; color:#6b7280; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;",
                "{crate::JMAP_USER}"
            }
        }
    }
}
