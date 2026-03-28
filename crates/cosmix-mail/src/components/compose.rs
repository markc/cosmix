use dioxus::prelude::*;
use super::icons::*;
use cosmix_ui::icons::ICON_BACK;

/// State for the compose form.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct ComposeState {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
}

#[component]
pub fn ComposeView(
    state: ComposeState,
    on_back: EventHandler<()>,
    on_send: EventHandler<ComposeState>,
    on_discard: EventHandler<()>,
) -> Element {
    let mut to = use_signal(|| state.to.clone());
    let mut cc = use_signal(|| state.cc.clone());
    let mut bcc = use_signal(|| state.bcc.clone());
    let mut subject = use_signal(|| state.subject.clone());
    let mut body = use_signal(|| state.body.clone());
    let mut sending = use_signal(|| false);
    let in_reply_to = state.in_reply_to.clone();

    let can_send = !to().trim().is_empty() && !sending();

    let send_style = if can_send {
        "padding:6px 14px; background:var(--accent); color:var(--accent-fg); border:none; border-radius:var(--radius-md); cursor:pointer; font-size:var(--font-size-sm); font-weight:500; display:flex; align-items:center; gap:4px;"
    } else {
        "padding:6px 14px; background:var(--accent-subtle); color:var(--fg-muted); border:none; border-radius:var(--radius-md); cursor:not-allowed; font-size:var(--font-size-sm); font-weight:500; display:flex; align-items:center; gap:4px;"
    };

    let input_style = "flex:1; background:transparent; border:none; outline:none; color:var(--fg-primary); padding:10px 0; font-size:var(--font-size); font-family:inherit;";

    rsx! {
        div {
            style: "flex:1; display:flex; flex-direction:column; min-width:0; overflow:hidden; background:var(--bg-primary); height:100%;",
            // Header bar
            div {
                style: "flex-shrink:0; padding:12px 24px; border-bottom:1px solid var(--border); background:var(--bg-secondary); display:flex; align-items:center; justify-content:space-between;",
                div {
                    style: "display:flex; align-items:center; gap:8px;",
                    button {
                        class: "mobile-back",
                        style: "display:none; background:none; border:none; color:var(--fg-muted); cursor:pointer; padding:4px;",
                        onclick: move |_| on_back.call(()),
                        dangerous_inner_html: "{ICON_BACK}"
                    }
                    span { style: "font-size:var(--font-size); font-weight:600; color:var(--fg-primary);", "New Message" }
                }
                div {
                    style: "display:flex; gap:8px;",
                    // Discard
                    button {
                        style: "padding:6px 14px; background:none; border:1px solid var(--border); color:var(--fg-muted); border-radius:var(--radius-md); cursor:pointer; font-size:var(--font-size-sm); display:flex; align-items:center; gap:4px;",
                        onclick: move |_| on_discard.call(()),
                        span { dangerous_inner_html: "{ICON_X}" }
                        "Discard"
                    }
                    // Send
                    button {
                        style: "{send_style}",
                        disabled: !can_send,
                        onclick: {
                            let in_reply_to = in_reply_to.clone();
                            move |_| {
                                sending.set(true);
                                on_send.call(ComposeState {
                                    to: to(),
                                    cc: cc(),
                                    bcc: bcc(),
                                    subject: subject(),
                                    body: body(),
                                    in_reply_to: in_reply_to.clone(),
                                });
                            }
                        },
                        span { dangerous_inner_html: "{ICON_SEND}" }
                        if sending() { "Sending..." } else { "Send" }
                    }
                }
            }
            // Form fields
            div {
                style: "flex-shrink:0; border-bottom:1px solid var(--border);",
                // To
                div {
                    style: "display:flex; align-items:center; padding:0 24px; border-bottom:1px solid var(--border-muted);",
                    label { style: "width:50px; font-size:var(--font-size-sm); color:var(--fg-muted); flex-shrink:0;", "To" }
                    input {
                        style: "{input_style}",
                        r#type: "text",
                        value: "{to}",
                        placeholder: "recipient@example.com",
                        oninput: move |e| to.set(e.value()),
                    }
                }
                // Cc
                div {
                    style: "display:flex; align-items:center; padding:0 24px; border-bottom:1px solid var(--border-muted);",
                    label { style: "width:50px; font-size:var(--font-size-sm); color:var(--fg-muted); flex-shrink:0;", "Cc" }
                    input {
                        style: "{input_style}",
                        r#type: "text",
                        value: "{cc}",
                        oninput: move |e| cc.set(e.value()),
                    }
                }
                // Bcc
                div {
                    style: "display:flex; align-items:center; padding:0 24px; border-bottom:1px solid var(--border-muted);",
                    label { style: "width:50px; font-size:var(--font-size-sm); color:var(--fg-muted); flex-shrink:0;", "Bcc" }
                    input {
                        style: "{input_style}",
                        r#type: "text",
                        value: "{bcc}",
                        oninput: move |e| bcc.set(e.value()),
                    }
                }
                // Subject
                div {
                    style: "display:flex; align-items:center; padding:0 24px;",
                    label { style: "width:50px; font-size:var(--font-size-sm); color:var(--fg-muted); flex-shrink:0;", "Subject" }
                    input {
                        style: "{input_style} font-weight:500;",
                        r#type: "text",
                        value: "{subject}",
                        oninput: move |e| subject.set(e.value()),
                    }
                }
            }
            // Body
            div {
                style: "flex:1; overflow:hidden;",
                textarea {
                    style: "width:100%; height:100%; background:transparent; border:none; outline:none; color:var(--fg-primary); padding:16px 24px; font-size:var(--font-size); font-family:var(--font-sans); resize:none; line-height:1.6;",
                    value: "{body}",
                    placeholder: "Write your message...",
                    oninput: move |e| body.set(e.value()),
                }
            }
        }
    }
}

/// Build a ComposeState for replying to an email.
pub fn compose_reply(email: &crate::jmap::Email) -> ComposeState {
    let to = email
        .from
        .as_ref()
        .and_then(|addrs| addrs.first())
        .map(|a| a.email.clone())
        .unwrap_or_default();

    let subject = email
        .subject
        .as_deref()
        .map(|s| {
            if s.starts_with("Re: ") || s.starts_with("re: ") {
                s.to_string()
            } else {
                format!("Re: {s}")
            }
        })
        .unwrap_or_default();

    let quoted = email
        .text_body_value()
        .map(|text| {
            let from = email.from_display();
            let date = email.date_short();
            let mut q = format!("\n\nOn {date}, {from} wrote:\n");
            for line in text.lines() {
                q.push_str("> ");
                q.push_str(line);
                q.push('\n');
            }
            q
        })
        .unwrap_or_default();

    let in_reply_to = email
        .message_id
        .as_ref()
        .and_then(|ids| ids.first())
        .cloned();

    ComposeState {
        to,
        subject,
        body: quoted,
        in_reply_to,
        ..Default::default()
    }
}

/// Build a ComposeState for forwarding an email.
pub fn compose_forward(email: &crate::jmap::Email) -> ComposeState {
    let subject = email
        .subject
        .as_deref()
        .map(|s| {
            if s.starts_with("Fwd: ") || s.starts_with("fwd: ") {
                s.to_string()
            } else {
                format!("Fwd: {s}")
            }
        })
        .unwrap_or_default();

    let body = email
        .text_body_value()
        .map(|text| {
            let from = email.from_display();
            let date = email.date_short();
            let subj = email.subject.as_deref().unwrap_or("(no subject)");
            format!(
                "\n\n---------- Forwarded message ----------\nFrom: {from}\nDate: {date}\nSubject: {subj}\n\n{text}"
            )
        })
        .unwrap_or_default();

    ComposeState {
        subject,
        body,
        ..Default::default()
    }
}
