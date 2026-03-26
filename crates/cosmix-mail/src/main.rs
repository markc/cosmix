mod components;
mod jmap;

use dioxus::prelude::*;
use components::{
    ComposeState, ComposeView, EmailList, EmailView, MailboxList,
    compose_forward, compose_reply,
};
use jmap::{Email, JmapClient, Mailbox};

const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

pub const JMAP_URL: &str = "https://mail.kanary.org:8443";
pub const JMAP_USER: &str = "markc@kanary.org";
pub const JMAP_PASS: &str = "changeme123";

/// Which panel is visible on mobile (<640px).
#[derive(Clone, Copy, PartialEq)]
enum MobileView {
    Mailboxes,
    Emails,
    Reader,
}

/// Static head elements — isolated component so it never re-renders.
#[component]
fn HeadStyles() -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: TAILWIND_CSS }
        document::Style { "html,body,#main{{ margin:0!important; padding:0!important; background:#030712!important; width:100%!important; height:100%!important; overflow:hidden!important; }}" }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        std::env::set_var("GTK_THEME", "Adwaita:dark");
    };

    #[cfg(feature = "desktop")]
    {
        use dioxus_desktop::{Config, WindowBuilder};
        use tao::window::Theme;

        let cfg = Config::new()
            .with_window(
                WindowBuilder::new()
                    .with_title("Cosmix Mail")
                    .with_inner_size(dioxus_desktop::LogicalSize::new(1400.0, 900.0))
                    .with_theme(Some(Theme::Dark))
                    .with_decorations(true),
            )
            .with_background_color((3, 7, 18, 255));

        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    dioxus::launch(app);
}

fn app() -> Element {
    let client = use_signal(|| JmapClient::new(JMAP_URL, JMAP_USER, JMAP_PASS).unwrap());
    let mut mailboxes: Signal<Vec<Mailbox>> = use_signal(Vec::new);
    let mut selected_mailbox: Signal<Option<String>> = use_signal(|| None);
    let mut emails: Signal<Vec<Email>> = use_signal(Vec::new);
    let mut selected_email: Signal<Option<Email>> = use_signal(|| None);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    let mut compose: Signal<Option<ComposeState>> = use_signal(|| None);
    let mut refresh: Signal<u32> = use_signal(|| 0);
    let mut mobile_view: Signal<MobileView> = use_signal(|| MobileView::Emails);

    // Load mailboxes on startup
    let _load_mailboxes = use_resource(move || {
        let c = client.peek().clone();
        async move {
            match c.mailboxes().await {
                Ok(mut mboxes) => {
                    mboxes.sort_by(|a, b| {
                        let role_order = |r: &Option<String>| match r.as_deref() {
                            Some("inbox") => 0,
                            Some("drafts") => 1,
                            Some("sent") => 2,
                            Some("archive") => 3,
                            Some("junk") => 4,
                            Some("trash") => 5,
                            _ => 6,
                        };
                        role_order(&a.role).cmp(&role_order(&b.role))
                    });
                    if let Some(inbox) =
                        mboxes.iter().find(|m| m.role.as_deref() == Some("inbox"))
                    {
                        selected_mailbox.set(Some(inbox.id.clone()));
                    }
                    mailboxes.set(mboxes);
                }
                Err(e) => error_msg.set(Some(format!("Failed to load mailboxes: {e}"))),
            }
        }
    });

    // Load emails when mailbox changes or refresh bumps
    let _load_emails = use_resource(move || {
        let c = client.peek().clone();
        let _refresh = refresh();
        async move {
            let Some(mbox_id) = selected_mailbox() else {
                return;
            };
            selected_email.set(None);
            match c.email_ids(&mbox_id).await {
                Ok(ids) => match c.emails(&ids, false).await {
                    Ok(list) => emails.set(list),
                    Err(e) => error_msg.set(Some(format!("Failed to load emails: {e}"))),
                },
                Err(e) => error_msg.set(Some(format!("Failed to query emails: {e}"))),
            }
        }
    });

    // Helper: find mailbox ID by role
    let find_mailbox = move |role: &str| -> Option<String> {
        mailboxes().iter().find(|m| m.role.as_deref() == Some(role)).map(|m| m.id.clone())
    };

    let mv = mobile_view();
    let sidebar_class = if mv == MobileView::Mailboxes { "pane-sidebar mobile-active" } else { "pane-sidebar" };
    let emails_class = if mv == MobileView::Emails { "pane-emails mobile-active" } else { "pane-emails" };
    let reader_class = if mv == MobileView::Reader { "pane-reader mobile-active" } else { "pane-reader" };

    rsx! {
        HeadStyles {}
        div {
            style: "position:absolute; top:0; left:0; right:0; bottom:0; display:flex; flex-direction:row; overflow:hidden; background:#030712; color:#e5e7eb; font-size:13px; font-family:system-ui,-apple-system,sans-serif;",
            // Error banner
            if let Some(err) = error_msg() {
                div {
                    style: "position:fixed; top:0; left:0; right:0; background:rgba(127,29,29,0.95); color:#fecaca; padding:6px 16px; font-size:12px; z-index:50; display:flex; align-items:center; justify-content:space-between;",
                    span { "{err}" }
                    button {
                        style: "margin-left:12px; cursor:pointer; background:none; border:none; color:inherit;",
                        onclick: move |_| error_msg.set(None),
                        dangerous_inner_html: "{components::icons::ICON_X}"
                    }
                }
            }

            // Pane 1: Sidebar
            MailboxList {
                class: sidebar_class,
                mailboxes: mailboxes(),
                selected: selected_mailbox(),
                on_select: move |id: String| {
                    compose.set(None);
                    selected_mailbox.set(Some(id));
                    mobile_view.set(MobileView::Emails);
                },
                on_compose: move |_| {
                    compose.set(Some(ComposeState::default()));
                    mobile_view.set(MobileView::Reader);
                }
            }

            // Pane 2: Email list
            EmailList {
                class: emails_class,
                emails: emails(),
                selected_id: selected_email().map(|e| e.id.clone()),
                on_select: move |email: Email| {
                    compose.set(None);
                    mobile_view.set(MobileView::Reader);
                    let c = client.peek().clone();
                    spawn(async move {
                        match c.emails(&[email.id.clone()], true).await {
                            Ok(full) => {
                                if let Some(e) = full.into_iter().next() {
                                    selected_email.set(Some(e));
                                }
                            }
                            Err(e) => error_msg.set(Some(format!("Failed to load email: {e}"))),
                        }
                    });
                },
                on_menu: move |_| {
                    mobile_view.set(MobileView::Mailboxes);
                }
            }

            // Pane 3: Compose or Email view
            div {
                class: "{reader_class}",
                style: "flex:1; display:flex; flex-direction:column; min-width:0; overflow:hidden; height:100%;",
                if let Some(state) = compose() {
                    ComposeView {
                        state: state,
                        on_back: move |_| { mobile_view.set(MobileView::Emails); },
                        on_send: move |cs: ComposeState| {
                            let c = client.peek().clone();
                            let drafts_id = find_mailbox("drafts").unwrap_or_default();
                            spawn(async move {
                                let to_addrs: Vec<String> = cs.to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                                let cc_addrs: Vec<String> = cs.cc.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                                match c.send_compose(
                                    JMAP_USER,
                                    &to_addrs,
                                    &cc_addrs,
                                    &cs.subject,
                                    &cs.body,
                                    cs.in_reply_to.as_deref(),
                                    &drafts_id,
                                ).await {
                                    Ok(()) => {
                                        compose.set(None);
                                        refresh.set(refresh() + 1);
                                    }
                                    Err(e) => error_msg.set(Some(format!("Send failed: {e}"))),
                                }
                            });
                        },
                        on_discard: move |_| {
                            compose.set(None);
                            mobile_view.set(MobileView::Emails);
                        }
                    }
                } else {
                    EmailView {
                        email: selected_email(),
                        on_back: move |_| { mobile_view.set(MobileView::Emails); },
                        on_reply: move |email: Email| {
                            compose.set(Some(compose_reply(&email)));
                        },
                        on_forward: move |email: Email| {
                            compose.set(Some(compose_forward(&email)));
                        },
                        on_delete: move |email: Email| {
                            let c = client.peek().clone();
                            let trash_id = find_mailbox("trash").unwrap_or_default();
                            spawn(async move {
                                let result = if !trash_id.is_empty() {
                                    c.update_email(&email.id, serde_json::json!({"mailboxIds": {&trash_id: true}})).await
                                } else {
                                    c.destroy_email(&email.id).await
                                };
                                match result {
                                    Ok(()) => {
                                        selected_email.set(None);
                                        refresh.set(refresh() + 1);
                                    }
                                    Err(e) => error_msg.set(Some(format!("Delete failed: {e}"))),
                                }
                            });
                        },
                        on_archive: move |email: Email| {
                            let c = client.peek().clone();
                            let archive_id = find_mailbox("archive").unwrap_or_default();
                            if archive_id.is_empty() { return; }
                            spawn(async move {
                                match c.update_email(&email.id, serde_json::json!({"mailboxIds": {&archive_id: true}})).await {
                                    Ok(()) => {
                                        selected_email.set(None);
                                        refresh.set(refresh() + 1);
                                    }
                                    Err(e) => error_msg.set(Some(format!("Archive failed: {e}"))),
                                }
                            });
                        },
                        on_toggle_read: move |email: Email| {
                            let c = client.peek().clone();
                            let is_read = email.is_read();
                            spawn(async move {
                                match c.update_email(&email.id, serde_json::json!({"keywords": {"$seen": !is_read}})).await {
                                    Ok(()) => {
                                        refresh.set(refresh() + 1);
                                    }
                                    Err(e) => error_msg.set(Some(format!("Toggle read failed: {e}"))),
                                }
                            });
                        },
                    }
                }
            }
        }
    }
}
