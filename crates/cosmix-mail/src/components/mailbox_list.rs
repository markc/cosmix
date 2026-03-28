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
            style: "width:200px; min-width:200px; display:flex; flex-direction:column; background:var(--bg-secondary); border-right:1px solid var(--border); height:100%;",
            // App title
            div {
                style: "height:36px; display:flex; align-items:center; padding:0 14px; border-bottom:1px solid var(--border); gap:8px;",
                span { dangerous_inner_html: "{ICON_MAIL}" }
                span { style: "font-weight:600; color:var(--fg-primary);", "cosmix" }
            }
            // Compose button
            div {
                style: "padding:8px 8px 4px;",
                button {
                    style: "width:100%; padding:7px 12px; background:var(--accent); color:var(--accent-fg); border:none; border-radius:var(--radius-md); cursor:pointer; font-size:var(--font-size-sm); font-weight:500; display:flex; align-items:center; gap:6px; justify-content:center;",
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
                        let bg = if is_selected { "background:var(--accent); color:var(--accent-fg);" } else { "background:transparent; color:var(--fg-muted);" };
                        rsx! {
                            button {
                                key: "{id}",
                                style: "width:calc(100% - 8px); text-align:left; padding:5px 12px; margin:1px 4px; border-radius:var(--radius-sm); display:flex; align-items:center; gap:8px; font-size:var(--font-size-sm); border:none; cursor:pointer; {bg}",
                                onclick: move |_| on_select.call(id.clone()),
                                span { dangerous_inner_html: "{icon}" }
                                span { style: "flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;", "{mbox.name}" }
                                if mbox.unread_emails > 0 {
                                    span {
                                        style: "margin-left:auto; background:var(--accent); color:var(--accent-fg); font-size:10px; padding:1px 6px; border-radius:8px; min-width:16px; text-align:center;",
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
                style: "padding:8px 14px; border-top:1px solid var(--border); font-size:11px; color:var(--fg-muted); overflow:hidden; text-overflow:ellipsis; white-space:nowrap;",
                "{crate::JMAP_USER}"
            }
        }
    }
}
