mod components;
mod hub;
mod jmap;

use std::sync::Arc;
use dioxus::prelude::*;
use cosmix_ui::menu::{action_shortcut, menubar, standard_file_menu, separator, submenu, MenuBar, Shortcut};
use cosmix_ui::theme::{ThemeParams, generate_css};
use components::{
    ComposeState, ComposeView, EmailList, EmailView, MailboxList,
    compose_forward, compose_reply,
};
use jmap::{Email, JmapClient, Mailbox};

pub const JMAP_URL: &str = "https://mail.kanary.org:8443";
pub const JMAP_USER: &str = "markc@kanary.org";
pub const JMAP_PASS: &str = "changeme123";

// ── Theme ──

static THEME: GlobalSignal<ThemeParams> = Signal::global(|| {
    cosmix_config::store::load()
        .map(|s| ThemeParams {
            hue: s.global.theme_hue,
            dark: s.global.theme_dark,
            font_size: s.global.font_size,
        })
        .unwrap_or_default()
});

/// Which panel is visible on mobile (<640px).
#[derive(Clone, Copy, PartialEq)]
enum MobileView {
    Mailboxes,
    Emails,
    Reader,
}

fn main() {
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("Cosmix Mail", 1400.0, 900.0);
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
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);

    // Connect to hub as "mail" service
    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default("mail").await {
                Ok(c) => {
                    let client = Arc::new(c);
                    tracing::info!("connected to cosmix-hub as 'mail'");
                    hub_client.set(Some(client.clone()));

                    // Register for config changes
                    let _ = client.call(
                        "configd",
                        "config.watch",
                        serde_json::json!({ "watcher": "mail" }),
                    ).await;

                    // Handle incoming commands
                    let client2 = client.clone();
                    tokio::spawn(async move {
                        if let Some(mut rx) = client2.incoming_async().await {
                            while let Some(cmd) = rx.recv().await {
                                if cmd.command == "config.changed" {
                                    if let Ok(settings) = cosmix_config::store::load() {
                                        *THEME.write() = ThemeParams {
                                            hue: settings.global.theme_hue,
                                            dark: settings.global.theme_dark,
                                            font_size: settings.global.font_size,
                                        };
                                    }
                                    let _ = client2.respond(&cmd, 0, r#"{"status":"ok"}"#).await;
                                }
                            }
                        }
                    });
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });
    });

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

    let theme = THEME.read().clone();
    let theme_css = generate_css(&theme);
    let fs = theme.font_size;

    let app_menu = menubar(vec![
        standard_file_menu(vec![
            action_shortcut("compose", "New Message", Shortcut::ctrl('n')),
            separator(),
        ]),
        submenu("View", vec![
            action_shortcut("refresh", "Refresh", Shortcut::ctrl('r')),
        ]),
    ]);

    rsx! {
        document::Style { "{theme_css}" }
        document::Style { "{MAIL_CSS}" }
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-size:{fs}px; font-family:var(--font-sans);",

            MenuBar {
                menu: app_menu,
                hub: Some(hub_client),
                on_action: move |id: String| match id.as_str() {
                    "compose" => {
                        compose.set(Some(ComposeState::default()));
                        mobile_view.set(MobileView::Reader);
                    }
                    "refresh" => { refresh.set(refresh() + 1); }
                    "quit" => std::process::exit(0),
                    _ => {}
                },
            }

            div {
                style: "flex:1; display:flex; flex-direction:row; overflow:hidden;",

                // Error banner
                if let Some(err) = error_msg() {
                    div {
                        style: "position:fixed; top:28px; left:0; right:0; background:var(--danger); color:#fff; padding:6px 16px; font-size:12px; z-index:50; display:flex; align-items:center; justify-content:space-between;",
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
}

/// App-specific CSS — prose for email body, responsive pane layout.
const MAIL_CSS: &str = r#"
/* Prose — email body rendering */
.prose { font-size: 0.9375rem; line-height: 1.7; color: var(--fg-primary); }
.prose h1 { margin: 1.5rem 0 .75rem; font-size: 1.75rem; font-weight: 700; }
.prose h2 { margin: 1.25rem 0 .5rem; font-size: 1.4rem; font-weight: 700; }
.prose h3 { margin: 1rem 0 .5rem; font-size: 1.15rem; font-weight: 600; }
.prose p { margin: .75rem 0; }
.prose ul, .prose ol { margin: .5rem 0; padding-left: 1.5rem; }
.prose ul { list-style-type: disc; }
.prose ol { list-style-type: decimal; }
.prose li { margin: .25rem 0; }
.prose a { color: var(--accent); text-decoration: underline; }
.prose a:hover { color: var(--accent-hover); }
.prose blockquote { color: var(--fg-muted); border-left: 3px solid var(--border); margin: .75rem 0; padding-left: 1rem; font-style: italic; }
.prose code { color: var(--fg-primary); background: var(--bg-tertiary); border-radius: .25rem; padding: .15rem .35rem; font-size: .85em; }
.prose pre { background: var(--bg-secondary); border: 1px solid var(--border); border-radius: .375rem; margin: .75rem 0; padding: 1rem; overflow-x: auto; }
.prose pre code { color: var(--fg-secondary); background: none; padding: 0; font-size: .85em; }
.prose table { border-collapse: collapse; width: 100%; margin: .75rem 0; font-size: .875rem; }
.prose th, .prose td { text-align: left; border: 1px solid var(--border); padding: .5rem .75rem; }
.prose th { background: var(--bg-tertiary); font-weight: 600; }
.prose hr { border-color: var(--border); margin: 1.5rem 0; }
.prose strong { font-weight: 700; }
.prose del { color: var(--fg-muted); text-decoration: line-through; }

/* Responsive pane layout */
@media (max-width: 640px) {
    .pane-sidebar, .pane-emails, .pane-reader { display: none !important; }
    .pane-sidebar.mobile-active,
    .pane-emails.mobile-active,
    .pane-reader.mobile-active { display: flex !important; flex: 1 !important; width: 100% !important; min-width: 0 !important; }
    .mobile-back { display: flex !important; }
    .desktop-only { display: none !important; }
}
"#;
