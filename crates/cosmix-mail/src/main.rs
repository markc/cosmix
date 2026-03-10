mod config;
mod tree_view;

use config::{AccountConfig, MailConfig};
use tree_view::{TreeNode, TreeView};
use cosmic::app::Settings;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::stream;
use cosmic::iced::{self, Alignment, Length, Size, Subscription};
use cosmic::widget::menu::{self, KeyBind};
use cosmic::widget::{self, icon, markdown, nav_bar, pane_grid, text_editor};
use cosmic::{executor, Core, Element};
use cosmix_lib::mail::JmapClient;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

type PortRx = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<cosmix_port::PortEvent>>>;
static PORT_RX: OnceLock<PortRx> = OnceLock::new();

const APP_ID: &str = "org.cosmix.Mail";

static MENU_ID: LazyLock<iced::id::Id> = LazyLock::new(|| iced::id::Id::new("mail_menu"));

// ---------------------------------------------------------------------------
// Menu actions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MenuAction {
    Compose,
    Refresh,
    Quit,
    Reply,
    Delete,
    SelectAll,
    MarkRead,
    MarkUnread,
    TogglePreview,
    About,
    Account0,
    Account1,
    Account2,
    Account3,
    Account4,
    AddAccount,
    RunScript(usize),
    RescanScripts,
}

impl menu::Action for MenuAction {
    type Message = Message;

    fn message(&self) -> Message {
        match self {
            MenuAction::Compose => Message::ShowCompose,
            MenuAction::Refresh => Message::Refresh,
            MenuAction::Quit => Message::Quit,
            MenuAction::Reply => Message::ShowReply,
            MenuAction::Delete => Message::DeleteSelected,
            MenuAction::SelectAll => Message::SelectAll,
            MenuAction::MarkRead => Message::MarkRead,
            MenuAction::MarkUnread => Message::MarkUnread,
            MenuAction::TogglePreview => Message::TogglePreview,
            MenuAction::About => Message::About,
            MenuAction::Account0 => Message::AccountSelected(0),
            MenuAction::Account1 => Message::AccountSelected(1),
            MenuAction::Account2 => Message::AccountSelected(2),
            MenuAction::Account3 => Message::AccountSelected(3),
            MenuAction::Account4 => Message::AccountSelected(4),
            MenuAction::AddAccount => Message::ShowAddAccount,
            MenuAction::RunScript(i) => Message::RunScript(*i),
            MenuAction::RescanScripts => Message::RescanScripts,
        }
    }
}

fn account_action(idx: usize) -> MenuAction {
    match idx {
        0 => MenuAction::Account0,
        1 => MenuAction::Account1,
        2 => MenuAction::Account2,
        3 => MenuAction::Account3,
        _ => MenuAction::Account4,
    }
}

// ── Cosmix Port Integration ──

fn start_port(
    state: Arc<Mutex<MailPortState>>,
    notifier: tokio::sync::mpsc::UnboundedSender<cosmix_port::PortEvent>,
) -> Option<cosmix_port::PortHandle> {
    let s1 = state.clone();

    let port = cosmix_port::Port::new("cosmix-mail")
        .events(notifier)
        .command("status", "Get mail app status summary", move |_| {
            let ps = s1.lock().unwrap();
            Ok(serde_json::json!({
                "accounts": ps.account_count,
                "emails": ps.email_count,
                "selected": ps.selected_email,
            }))
        })
        .standard_help()
        .standard_info("Cosmix Mail", env!("CARGO_PKG_VERSION"))
        .standard_activate();

    match port.start() {
        Ok(handle) => {
            tracing::info!("Cosmix port started at {}", handle.socket_path.display());
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("Failed to start cosmix port: {e}");
            None
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::default().size(Size::new(1200., 750.));
    cosmic::app::run::<MailApp>(settings, ())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pane layout
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum PaneKind {
    Folders,
    Messages,
    Content,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Shared state exposed to port command handlers (read-only queries).
struct MailPortState {
    account_count: usize,
    email_count: usize,
    selected_email: Option<String>,
}

struct MailApp {
    core: Core,
    accounts: Vec<Account>,
    active_account: usize,
    mailbox_tree: TreeView,
    panes: pane_grid::State<PaneKind>,
    keybinds: HashMap<KeyBind, MenuAction>,
    emails: Vec<EmailSummary>,
    selected_email_id: Option<String>,
    selected_email: Option<EmailDetail>,
    markdown_items: Vec<markdown::Item>,
    right_pane: RightPane,
    show_preview: bool,
    error: Option<String>,
    port_state: Arc<Mutex<MailPortState>>,
    port_scripts: Vec<cosmix_port::ScriptInfo>,
    _port_handle: Option<cosmix_port::PortHandle>,
}

struct Account {
    config: AccountConfig,
    client: Option<JmapClient>,
    mailboxes: Vec<Mailbox>,
    status: AccountStatus,
}

enum RightPane {
    Preview,
    Compose {
        to: String,
        subject: String,
        body: text_editor::Content,
    },
    Reply {
        original: EmailDetail,
        body: text_editor::Content,
    },
    AddAccount {
        name: String,
        url: String,
        user: String,
        pass: String,
    },
}

#[derive(Clone, Debug)]
enum AccountStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Debug)]
struct Mailbox {
    id: String,
    name: String,
    total: u64,
    unread: u64,
}

#[derive(Clone, Debug)]
struct EmailSummary {
    id: String,
    from: String,
    subject: String,
    date: String,
    preview: String,
}

#[derive(Clone, Debug)]
struct EmailDetail {
    id: String,
    from: String,
    to: String,
    subject: String,
    date: String,
    body: String,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Message {
    AccountSelected(usize),
    AccountConnected(usize, Result<ConnectResult, String>),
    EmailsLoaded(usize, Result<Vec<EmailSummary>, String>),
    EmailSelected(String),
    EmailLoaded(Result<EmailDetail, String>),
    ShowCompose,
    ComposeTo(String),
    ComposeSubject(String),
    ComposeBody(text_editor::Action),
    ComposeSend,
    ComposeSent(Result<(), String>),
    ShowReply,
    ReplyBody(text_editor::Action),
    ReplySend,
    ReplySent(Result<(), String>),
    DeleteEmail(String),
    DeleteSelected,
    EmailDeleted(Result<(), String>),
    Refresh,
    DismissError,
    CancelCompose,
    MarkdownLink(String),
    SelectAll,
    MarkRead,
    MarkUnread,
    TogglePreview,
    About,
    Quit,
    Surface(cosmic::surface::Action),
    PaneResized(pane_grid::ResizeEvent),
    FolderSelected(widget::segmented_button::Entity),
    ShowAddAccount,
    AddAccountName(String),
    AddAccountUrl(String),
    AddAccountUser(String),
    AddAccountPass(String),
    SaveAccount,
    SyncFromPort,
    PortActivate,
    RunScript(usize),
    RescanScripts,
    ScriptsUpdated(Vec<cosmix_port::ScriptInfo>),
}

struct ConnectResult {
    client: JmapClient,
    mailboxes: Vec<Mailbox>,
}

impl Clone for ConnectResult {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            mailboxes: self.mailboxes.clone(),
        }
    }
}

impl std::fmt::Debug for ConnectResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectResult")
            .field("mailboxes", &self.mailboxes)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Async operations (return app::Task which wraps Action)
// ---------------------------------------------------------------------------

fn connect_account(idx: usize, config: AccountConfig) -> cosmic::app::Task<Message> {
    cosmic::task::future(async move {
        let result = (|| -> Result<ConnectResult, String> {
            let client =
                JmapClient::connect_named(&config.name, &config.url, &config.user, &config.pass)
                    .map_err(|e| e.to_string())?;
            let mailboxes_json = client.mailboxes().map_err(|e| e.to_string())?;
            let mailboxes = parse_mailboxes(&mailboxes_json);
            Ok(ConnectResult { client, mailboxes })
        })();
        Message::AccountConnected(idx, result)
    })
}

fn load_emails(idx: usize, client: JmapClient, mailbox_id: Option<String>) -> cosmic::app::Task<Message> {
    cosmic::task::future(async move {
        let result = client
            .query(mailbox_id.as_deref(), Some(50))
            .map(|j| parse_emails(&j))
            .map_err(|e| e.to_string());
        Message::EmailsLoaded(idx, result)
    })
}

fn load_email(client: JmapClient, email_id: String) -> cosmic::app::Task<Message> {
    cosmic::task::future(async move {
        let result = client
            .read(&email_id)
            .map(|j| parse_email_detail(&j))
            .map_err(|e| e.to_string());
        Message::EmailLoaded(result)
    })
}

// ---------------------------------------------------------------------------
// Icon helpers
// ---------------------------------------------------------------------------

fn mailbox_icon_name(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "inbox" => "mail-folder-inbox-symbolic",
        "sent" | "sent items" | "sent mail" => "mail-folder-outbox-symbolic",
        "drafts" | "draft" => "folder-symbolic",
        "junk" | "junk mail" | "spam" => "mail-mark-junk-symbolic",
        "trash" | "deleted" | "deleted items" | "bin" => "user-trash-symbolic",
        _ => "folder-symbolic",
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

impl cosmic::Application for MailApp {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, cosmic::app::Task<Message>) {
        let port_state = Arc::new(Mutex::new(MailPortState {
            account_count: 0,
            email_count: 0,
            selected_email: None,
        }));
        let (port_tx, port_rx) = tokio::sync::mpsc::unbounded_channel();
        PORT_RX.set(Arc::new(tokio::sync::Mutex::new(port_rx))).ok();
        let port_handle = start_port(port_state.clone(), port_tx);

        let config = MailConfig::load().ok();
        let accounts: Vec<Account> = config
            .as_ref()
            .map(|c| {
                c.accounts
                    .iter()
                    .map(|ac| Account {
                        config: ac.clone(),
                        client: None,
                        mailboxes: Vec::new(),
                        status: AccountStatus::Disconnected,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let no_accounts = config.is_none() || accounts.is_empty();

        use cosmic::iced::keyboard::{Key, key::Named};
        use cosmic::widget::menu::key_bind::Modifier;

        let keybinds = HashMap::from([
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("n".into()) }, MenuAction::Compose),
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("r".into()) }, MenuAction::Reply),
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("q".into()) }, MenuAction::Quit),
            (KeyBind { modifiers: vec![], key: Key::Named(Named::F5) }, MenuAction::Refresh),
            (KeyBind { modifiers: vec![], key: Key::Named(Named::Delete) }, MenuAction::Delete),
        ]);

        let pane_config = pane_grid::Configuration::Split {
            axis: pane_grid::Axis::Vertical,
            ratio: 0.18,
            a: Box::new(pane_grid::Configuration::Pane(PaneKind::Folders)),
            b: Box::new(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Vertical,
                ratio: 0.4,
                a: Box::new(pane_grid::Configuration::Pane(PaneKind::Messages)),
                b: Box::new(pane_grid::Configuration::Pane(PaneKind::Content)),
            }),
        };
        let panes = pane_grid::State::with_configuration(pane_config);

        let mut app = MailApp {
            core,
            accounts,
            active_account: 0,
            mailbox_tree: TreeView::new(),
            panes,
            keybinds,
            emails: Vec::new(),
            selected_email_id: None,
            selected_email: None,
            markdown_items: Vec::new(),
            right_pane: if no_accounts {
                RightPane::AddAccount {
                    name: String::new(),
                    url: "https://".into(),
                    user: String::new(),
                    pass: String::new(),
                }
            } else {
                RightPane::Preview
            },
            show_preview: true,
            error: None,
            port_state: port_state.clone(),
            port_scripts: Vec::new(),
            _port_handle: port_handle,
        };

        // Sync initial port state
        {
            let mut ps = port_state.lock().unwrap();
            ps.account_count = app.accounts.len();
        }

        let task = if !app.accounts.is_empty() {
            app.accounts[0].status = AccountStatus::Connecting;
            connect_account(0, app.accounts[0].config.clone())
        } else {
            cosmic::Task::none()
        };

        (app, task)
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        None
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        let mut trees: Vec<(String, Vec<menu::Item<MenuAction, String>>)> = vec![
                    (
                        "File".into(),
                        vec![
                            menu::Item::Button("New Message".into(), Some(icon::from_name("mail-message-new-symbolic").into()), MenuAction::Compose),
                            menu::Item::Divider,
                            menu::Item::Button("Refresh".into(), Some(icon::from_name("view-refresh-symbolic").into()), MenuAction::Refresh),
                            menu::Item::Divider,
                            menu::Item::Button("About Cosmix Mail".into(), Some(icon::from_name("help-about-symbolic").into()), MenuAction::About),
                            menu::Item::Divider,
                            menu::Item::Button("Quit".into(), Some(icon::from_name("window-close-symbolic").into()), MenuAction::Quit),
                        ],
                    ),
                    (
                        "Edit".into(),
                        vec![
                            menu::Item::Button("Reply".into(), Some(icon::from_name("mail-reply-all-symbolic").into()), MenuAction::Reply),
                            menu::Item::Divider,
                            menu::Item::Button("Select All".into(), None, MenuAction::SelectAll),
                            menu::Item::Divider,
                            menu::Item::Button("Mark as Read".into(), None, MenuAction::MarkRead),
                            menu::Item::Button("Mark as Unread".into(), None, MenuAction::MarkUnread),
                            menu::Item::Divider,
                            menu::Item::Button("Delete".into(), Some(icon::from_name("edit-delete-symbolic").into()), MenuAction::Delete),
                            menu::Item::Divider,
                            menu::Item::CheckBox("Preview Pane".into(), None, self.show_preview, MenuAction::TogglePreview),
                        ],
                    ),
                    (
                        "Mailboxes".into(),
                        {
                            let mut items: Vec<menu::Item<MenuAction, String>> = self
                                .accounts
                                .iter()
                                .enumerate()
                                .map(|(i, acc)| {
                                    let label = match &acc.status {
                                        AccountStatus::Disconnected => acc.config.name.clone(),
                                        AccountStatus::Connecting => {
                                            format!("... {}", acc.config.name)
                                        }
                                        AccountStatus::Connected => {
                                            let unread: u64 =
                                                acc.mailboxes.iter().map(|m| m.unread).sum();
                                            if unread > 0 {
                                                format!("{} ({})", acc.config.name, unread)
                                            } else {
                                                acc.config.name.clone()
                                            }
                                        }
                                        AccountStatus::Error(_) => {
                                            format!("! {}", acc.config.name)
                                        }
                                    };
                                    let is_active = i == self.active_account;
                                    menu::Item::CheckBox(
                                        label,
                                        Some(
                                            icon::from_name("mail-folder-inbox-symbolic").into(),
                                        ),
                                        is_active,
                                        account_action(i),
                                    )
                                })
                                .collect();
                            items.push(menu::Item::Divider);
                            items.push(menu::Item::Button(
                                "+ Add Account".to_string(),
                                Some(icon::from_name("list-add-symbolic").into()),
                                MenuAction::AddAccount,
                            ));
                            items
                        },
                    ),
        ];

        // Scripts menu
        if !self.port_scripts.is_empty() {
            let mut script_items: Vec<menu::Item<MenuAction, String>> = self.port_scripts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    menu::Item::Button(
                        s.display_name.clone(),
                        Some(icon::from_name("text-x-script-symbolic").into()),
                        MenuAction::RunScript(i),
                    )
                })
                .collect();
            script_items.push(menu::Item::Divider);
            script_items.push(menu::Item::Button(
                "Rescan Scripts".to_string(),
                Some(icon::from_name("view-refresh-symbolic").into()),
                MenuAction::RescanScripts,
            ));
            trees.push(("Scripts".into(), script_items));
        }

        let bar = menu::bar(
            trees
                .into_iter()
                .map(|mt| {
                    menu::Tree::with_children(
                        cosmic::widget::RcElementWrapper::new(Element::from(menu::root(mt.0))),
                        menu::items(&self.keybinds, mt.1),
                    )
                })
                .collect(),
        )
        .item_width(menu::ItemWidth::Uniform(240))
        .item_height(menu::ItemHeight::Dynamic(36))
        .spacing(2.0)
        .on_surface_action(Message::Surface)
        .window_id_maybe(self.core().main_window_id());

        vec![bar.into()]
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        Vec::new()
    }

    fn update(&mut self, message: Message) -> cosmic::app::Task<Message> {
        match message {
            Message::AccountSelected(idx) => {
                if idx < self.accounts.len() {
                    self.active_account = idx;
                    self.selected_email = None;
                    self.selected_email_id = None;
                    self.markdown_items.clear();
                    self.emails.clear();
                    self.right_pane = RightPane::Preview;

                    if matches!(self.accounts[idx].status, AccountStatus::Connected) {
                        self.rebuild_nav_model();
                        if let Some(client) = self.accounts[idx].client.clone() {
                            let mb_id = self.accounts[idx]
                                .mailboxes
                                .iter()
                                .find(|m| m.name.eq_ignore_ascii_case("inbox"))
                                .map(|m| m.id.clone());
                            return load_emails(idx, client, mb_id);
                        }
                    } else if matches!(self.accounts[idx].status, AccountStatus::Disconnected) {
                        self.accounts[idx].status = AccountStatus::Connecting;
                        return connect_account(idx, self.accounts[idx].config.clone());
                    }
                }
            }

            Message::AccountConnected(idx, result) => {
                if idx < self.accounts.len() {
                    match result {
                        Ok(cr) => {
                            self.accounts[idx].client = Some(cr.client.clone());
                            self.accounts[idx].mailboxes = cr.mailboxes;
                            self.accounts[idx].status = AccountStatus::Connected;

                            if idx == self.active_account {
                                self.rebuild_nav_model();
                            }

                            if let Some(inbox) = self.accounts[idx]
                                .mailboxes
                                .iter()
                                .find(|m| m.name.eq_ignore_ascii_case("inbox"))
                            {
                                let mb_id = inbox.id.clone();
                                return load_emails(idx, cr.client, Some(mb_id));
                            }
                        }
                        Err(e) => {
                            self.accounts[idx].status = AccountStatus::Error(e.clone());
                            self.error =
                                Some(format!("{}: {e}", self.accounts[idx].config.name));
                        }
                    }
                }
            }

            Message::EmailsLoaded(idx, result) => {
                if idx == self.active_account {
                    match result {
                        Ok(emails) => self.emails = emails,
                        Err(e) => self.error = Some(e),
                    }
                }
            }

            Message::EmailSelected(email_id) => {
                self.selected_email_id = Some(email_id.clone());
                self.right_pane = RightPane::Preview;
                let idx = self.active_account;
                if let Some(client) = self.accounts[idx].client.clone() {
                    return load_email(client, email_id);
                }
            }

            Message::EmailLoaded(result) => match result {
                Ok(detail) => {
                    self.markdown_items = markdown::parse(&detail.body).collect();
                    self.selected_email = Some(detail);
                    self.right_pane = RightPane::Preview;
                }
                Err(e) => self.error = Some(e),
            },

            Message::ShowCompose => {
                self.right_pane = RightPane::Compose {
                    to: String::new(),
                    subject: String::new(),
                    body: text_editor::Content::new(),
                };
            }

            Message::CancelCompose => {
                self.right_pane = RightPane::Preview;
            }

            Message::ComposeTo(to) => {
                if let RightPane::Compose { to: ref mut t, .. } = self.right_pane {
                    *t = to;
                }
            }

            Message::ComposeSubject(subj) => {
                if let RightPane::Compose {
                    subject: ref mut s, ..
                } = self.right_pane
                {
                    *s = subj;
                }
            }

            Message::ComposeBody(action) => {
                if let RightPane::Compose { body: ref mut b, .. } = self.right_pane {
                    b.perform(action);
                }
            }

            Message::ComposeSend => {
                let idx = self.active_account;
                if let (Some(client), RightPane::Compose { to, subject, body }) =
                    (self.accounts[idx].client.clone(), &self.right_pane)
                {
                    let to = to.clone();
                    let subject = subject.clone();
                    let body_text = body.text();
                    return cosmic::task::future(async move {
                        let result = client
                            .send(&to, &subject, &body_text)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        Message::ComposeSent(result)
                    });
                }
            }

            Message::ComposeSent(result) => match result {
                Ok(()) => {
                    self.right_pane = RightPane::Preview;
                    return self.update(Message::Refresh);
                }
                Err(e) => self.error = Some(format!("Send failed: {e}")),
            },

            Message::ShowReply => {
                if let Some(ref detail) = self.selected_email {
                    self.right_pane = RightPane::Reply {
                        original: detail.clone(),
                        body: text_editor::Content::new(),
                    };
                }
            }

            Message::ReplyBody(action) => {
                if let RightPane::Reply { body: ref mut b, .. } = self.right_pane {
                    b.perform(action);
                }
            }

            Message::ReplySend => {
                let idx = self.active_account;
                if let (Some(client), RightPane::Reply { original, body }) =
                    (self.accounts[idx].client.clone(), &self.right_pane)
                {
                    let id = original.id.clone();
                    let body_text = body.text();
                    return cosmic::task::future(async move {
                        let result = client
                            .reply(&id, &body_text)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        Message::ReplySent(result)
                    });
                }
            }

            Message::ReplySent(result) => match result {
                Ok(()) => {
                    self.right_pane = RightPane::Preview;
                    return self.update(Message::Refresh);
                }
                Err(e) => self.error = Some(format!("Reply failed: {e}")),
            },

            Message::DeleteEmail(email_id) => {
                let idx = self.active_account;
                if let Some(client) = self.accounts[idx].client.clone() {
                    self.selected_email = None;
                    self.markdown_items.clear();
                    self.selected_email_id = None;
                    return cosmic::task::future(async move {
                        let result = client
                            .delete(&email_id)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        Message::EmailDeleted(result)
                    });
                }
            }

            Message::EmailDeleted(result) => {
                if let Err(e) = result {
                    self.error = Some(format!("Delete failed: {e}"));
                }
                return self.update(Message::Refresh);
            }

            Message::Refresh => {
                let idx = self.active_account;
                if let Some(client) = self.accounts[idx].client.clone() {
                    let mb_id = self.mailbox_tree.active_id().map(|s| s.to_string());
                    return load_emails(idx, client, mb_id);
                }
            }

            Message::DismissError => {
                self.error = None;
            }

            Message::DeleteSelected => {
                if let Some(ref id) = self.selected_email_id {
                    let id = id.clone();
                    return self.update(Message::DeleteEmail(id));
                }
            }

            Message::SelectAll => {}
            Message::MarkRead => {}
            Message::MarkUnread => {}

            Message::TogglePreview => {
                self.show_preview = !self.show_preview;
            }

            Message::About => {}

            Message::Quit => {
                return cosmic::iced::exit();
            }

            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }

            Message::PaneResized(event) => {
                self.panes.resize(event.split, event.ratio);
            }

            Message::FolderSelected(entity) => {
                // Check if the activated item has children — toggle expand/collapse
                if let Some(node_id) = self.mailbox_tree.activated(entity) {
                    let node_id = node_id.to_string();
                    if self.mailbox_tree.has_children(&node_id) {
                        self.mailbox_tree.toggle(&node_id);
                        return cosmic::Task::none();
                    }
                }

                // Leaf node — select and load emails
                self.mailbox_tree.model_mut().activate(entity);

                let mailbox_id = self.mailbox_tree.activated(entity).map(|s| s.to_string());
                self.selected_email_id = None;
                self.selected_email = None;
                self.markdown_items.clear();
                self.right_pane = RightPane::Preview;

                let idx = self.active_account;
                if let Some(client) = self.accounts[idx].client.clone() {
                    return load_emails(idx, client, mailbox_id);
                }
            }

            Message::ShowAddAccount => {
                self.right_pane = RightPane::AddAccount {
                    name: String::new(),
                    url: "https://".into(),
                    user: String::new(),
                    pass: String::new(),
                };
            }

            Message::AddAccountName(v) => {
                if let RightPane::AddAccount { name, .. } = &mut self.right_pane {
                    *name = v;
                }
            }

            Message::AddAccountUrl(v) => {
                if let RightPane::AddAccount { url, .. } = &mut self.right_pane {
                    *url = v;
                }
            }

            Message::AddAccountUser(v) => {
                if let RightPane::AddAccount { user, .. } = &mut self.right_pane {
                    *user = v;
                }
            }

            Message::AddAccountPass(v) => {
                if let RightPane::AddAccount { pass, .. } = &mut self.right_pane {
                    *pass = v;
                }
            }

            Message::SaveAccount => {
                if let RightPane::AddAccount {
                    name,
                    url,
                    user,
                    pass,
                } = &self.right_pane
                {
                    if name.is_empty() || url.is_empty() || user.is_empty() || pass.is_empty() {
                        self.error = Some("All fields are required".into());
                        return cosmic::Task::none();
                    }

                    let new_config = AccountConfig {
                        name: name.clone(),
                        url: url.clone(),
                        user: user.clone(),
                        pass: pass.clone(),
                    };

                    // Save to config file
                    let mut mail_config = MailConfig::load().unwrap_or(MailConfig {
                        accounts: Vec::new(),
                    });
                    mail_config.accounts.push(new_config.clone());
                    if let Err(e) = mail_config.save() {
                        self.error = Some(format!("Failed to save config: {e}"));
                        return cosmic::Task::none();
                    }

                    // Add to runtime state
                    let idx = self.accounts.len();
                    self.accounts.push(Account {
                        config: new_config.clone(),
                        client: None,
                        mailboxes: Vec::new(),
                        status: AccountStatus::Connecting,
                    });
                    self.active_account = idx;
                    self.right_pane = RightPane::Preview;

                    return connect_account(idx, new_config);
                }
            }

            Message::MarkdownLink(ref url) => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(url)
                    .spawn();
            }
            Message::SyncFromPort | Message::PortActivate => {}
            Message::RunScript(i) => {
                if let Some(script) = self.port_scripts.get(i) {
                    let path = script.path.clone();
                    return cosmic::app::Task::perform(
                        async move {
                            let _ = tokio::process::Command::new("cosmix")
                                .args(["run-for", &path, "COSMIX-MAIL.1"])
                                .output()
                                .await;
                        },
                        |_| cosmic::Action::App(Message::SyncFromPort),
                    );
                }
            }
            Message::RescanScripts => {
                return cosmic::app::Task::perform(
                    async {
                        let _ = tokio::process::Command::new("cosmix")
                            .args(["rescan-scripts", "COSMIX-MAIL.1"])
                            .output()
                            .await;
                    },
                    |_| cosmic::Action::App(Message::SyncFromPort),
                );
            }
            Message::ScriptsUpdated(scripts) => {
                self.port_scripts = scripts;
            }
        }

        // Sync port state for read-only queries
        {
            let mut ps = self.port_state.lock().unwrap();
            ps.account_count = self.accounts.len();
            ps.email_count = self.emails.len();
            ps.selected_email = self.selected_email_id.clone();
        }

        cosmic::Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(|| {
            stream::channel(16, |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
                let rx = PORT_RX.get().expect("port receiver not initialized");
                loop {
                    match rx.lock().await.recv().await {
                        Some(cosmix_port::PortEvent::Activate) => {
                            let _ = output.send(Message::PortActivate).await;
                        }
                        Some(cosmix_port::PortEvent::ScriptsUpdated(scripts)) => {
                            let _ = output.send(Message::ScriptsUpdated(scripts)).await;
                        }
                        Some(cosmix_port::PortEvent::Command { .. }) => {
                            let _ = output.send(Message::SyncFromPort).await;
                        }
                        None => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            })
        })
    }

    // -----------------------------------------------------------------------
    // View — main content area (right of nav bar, below header)
    // -----------------------------------------------------------------------

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let mut content = widget::column().spacing(0);

        // Error bar
        if let Some(ref err) = self.error {
            content = content.push(
                widget::row()
                    .push(widget::text::body(format!("Error: {err}")))
                    .push(widget::space().width(Length::Fill))
                    .push(
                        widget::button::destructive("Dismiss")
                            .on_press_maybe(Some(Message::DismissError)),
                    )
                    .spacing(spacing.space_s)
                    .padding(spacing.space_xxs),
            );
        }

        // Three-pane layout with resizable dividers
        let panes = cosmic::widget::PaneGrid::new(&self.panes, |_id, kind, _maximized| {
            let body: Element<'_, Message> = match kind {
                PaneKind::Folders => self.view_folders(),
                PaneKind::Messages => self.view_email_list(),
                PaneKind::Content => self.view_content_pane(),
            };
            pane_grid::Content::new(body)
        })
        .on_resize(4, Message::PaneResized)
        .width(Length::Fill)
        .height(Length::Fill);

        content = content.push(panes);

        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

// ---------------------------------------------------------------------------
// View helpers
// ---------------------------------------------------------------------------

impl MailApp {
    fn rebuild_nav_model(&mut self) {
        let account = &self.accounts[self.active_account];

        // JMAP mailboxes can have parentId for nesting.
        // For now, build a flat list (depth 0) since our JMAP client
        // doesn't expose parentId yet. The TreeView is ready for
        // nested structure when we add parentId support.
        let nodes: Vec<TreeNode> = account
            .mailboxes
            .iter()
            .map(|mb| {
                let label = if mb.unread > 0 {
                    format!("{} ({}/{})", mb.name, mb.unread, mb.total)
                } else if mb.total > 0 {
                    format!("{} ({})", mb.name, mb.total)
                } else {
                    mb.name.clone()
                };
                TreeNode::new(
                    &mb.id,
                    label,
                    mailbox_icon_name(&mb.name),
                    0,     // depth — flat for now
                    false, // has_children — flat for now
                )
            })
            .collect();

        self.mailbox_tree.set_nodes(nodes);

        // Activate inbox by default
        if let Some(inbox) = account
            .mailboxes
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("inbox"))
        {
            self.mailbox_tree.activate(&inbox.id);
        }
    }

    fn view_folders(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let new_msg_btn = widget::button::suggested("+ New Message")
            .leading_icon(icon::from_name("mail-message-new-symbolic"))
            .on_press_maybe(Some(Message::ShowCompose))
            .width(Length::Fill)
            .class(cosmic::theme::Button::Custom {
                active: Box::new(|_focused, theme| {
                    let c = &theme.cosmic().accent_button;
                    cosmic::widget::button::Style {
                        background: Some(cosmic::iced::Background::Color(c.base.into())),
                        text_color: Some(c.on.into()),
                        icon_color: Some(c.on.into()),
                        border_radius: [4.0; 4].into(),
                        ..Default::default()
                    }
                }),
                disabled: Box::new(|theme| {
                    let c = &theme.cosmic().accent_button;
                    cosmic::widget::button::Style {
                        background: Some(cosmic::iced::Background::Color(c.base.into())),
                        text_color: Some(c.on.into()),
                        icon_color: Some(c.on.into()),
                        border_radius: [4.0; 4].into(),
                        ..Default::default()
                    }
                }),
                hovered: Box::new(|_focused, theme| {
                    let c = &theme.cosmic().accent_button;
                    cosmic::widget::button::Style {
                        background: Some(cosmic::iced::Background::Color(c.hover.into())),
                        text_color: Some(c.on.into()),
                        icon_color: Some(c.on.into()),
                        border_radius: [4.0; 4].into(),
                        ..Default::default()
                    }
                }),
                pressed: Box::new(|_focused, theme| {
                    let c = &theme.cosmic().accent_button;
                    cosmic::widget::button::Style {
                        background: Some(cosmic::iced::Background::Color(c.pressed.into())),
                        text_color: Some(c.on.into()),
                        icon_color: Some(c.on.into()),
                        border_radius: [4.0; 4].into(),
                        ..Default::default()
                    }
                }),
            });

        let tree = self.mailbox_tree.view(Message::FolderSelected);

        widget::column()
            .push(
                widget::container(new_msg_btn).padding(spacing.space_xxs),
            )
            .push(widget::scrollable::vertical(tree))
            .height(Length::Fill)
            .into()
    }

    fn view_email_list(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut col = widget::column().spacing(1).padding(spacing.space_xxs);

        if self.emails.is_empty() {
            col = col.push(
                widget::container(widget::text::body("No messages"))
                    .padding(spacing.space_l)
                    .width(Length::Fill)
                    .align_x(Alignment::Center),
            );
        }

        for email in &self.emails {
            let is_selected = self
                .selected_email_id
                .as_ref()
                .is_some_and(|id| id == &email.id);

            let item = widget::column()
                .push(
                    widget::row()
                        .push(widget::text::caption(&email.from))
                        .push(widget::space().width(Length::Fill))
                        .push(widget::text::caption(&email.date))
                        .spacing(spacing.space_xs),
                )
                .push(widget::text::body(&email.subject))
                .spacing(2);

            let btn = widget::button::custom(item)
                .on_press(Message::EmailSelected(email.id.clone()))
                .width(Length::Fill)
                .padding(spacing.space_xxs);

            let btn = if is_selected {
                btn.selected(true).class(cosmic::theme::Button::ListItem)
            } else {
                btn.class(cosmic::theme::Button::ListItem)
            };

            col = col.push(btn);
        }

        widget::container(widget::scrollable::vertical(col))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_content_pane(&self) -> Element<'_, Message> {
        let content: Element<'_, Message> = match &self.right_pane {
            RightPane::Preview => self.view_email_preview(),
            RightPane::Compose { to, subject, body } => self.view_compose(to, subject, body),
            RightPane::Reply { original, body } => self.view_reply(original, body),
            RightPane::AddAccount {
                name,
                url,
                user,
                pass,
            } => self.view_add_account(name, url, user, pass),
        };

        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .class(cosmic::theme::Container::Card)
            .into()
    }

    fn view_email_preview(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        match &self.selected_email {
            None => widget::container(widget::text::body("Select an email to read"))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .into(),

            Some(detail) => {
                let toolbar = widget::row()
                    .push(
                        widget::button::standard("Reply")
                            .leading_icon(icon::from_name("mail-reply-all-symbolic"))
                            .on_press_maybe(Some(Message::ShowReply)),
                    )
                    .push(
                        widget::button::destructive("Delete")
                            .leading_icon(icon::from_name("edit-delete-symbolic"))
                            .on_press_maybe(Some(Message::DeleteEmail(detail.id.clone()))),
                    )
                    .spacing(spacing.space_s)
                    .padding(spacing.space_xs);

                let header = widget::column()
                    .push(widget::text::title4(&detail.subject))
                    .push(
                        widget::row()
                            .push(widget::text::caption(format!("From: {}", detail.from)))
                            .push(widget::space().width(Length::Fill))
                            .push(widget::text::caption(&detail.date)),
                    )
                    .push(widget::text::caption(format!("To: {}", detail.to)))
                    .push(widget::divider::horizontal::default())
                    .spacing(spacing.space_xxs)
                    .padding([0, spacing.space_s as u16]);

                // Email body with GFM markdown, black text on white background
                let md_theme = cosmix_markdown::Theme::light();
                let md_view = cosmix_markdown::view(
                    &detail.body,
                    &md_theme,
                    Message::MarkdownLink,
                );
                let body_view = widget::container(
                    widget::scrollable::vertical(
                        widget::container(md_view)
                            .padding(spacing.space_s)
                            .width(Length::Fill),
                    ),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme| widget::container::Style {
                    background: Some(cosmic::iced::Background::Color(
                        cosmic::iced::Color::WHITE,
                    )),
                    ..Default::default()
                });

                widget::column()
                    .push(toolbar)
                    .push(header)
                    .push(body_view)
                    .height(Length::Fill)
                    .into()
            }
        }
    }

    fn view_compose<'a>(
        &'a self,
        to: &'a str,
        subject: &'a str,
        body: &'a text_editor::Content,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let account = &self.accounts[self.active_account];

        widget::column()
            .push(
                widget::row()
                    .push(widget::text::title4("New Message"))
                    .push(widget::space().width(Length::Fill))
                    .push(
                        widget::button::suggested("Send")
                            .leading_icon(icon::from_name("mail-send-symbolic"))
                            .on_press_maybe(Some(Message::ComposeSend)),
                    )
                    .push(
                        widget::button::standard("Cancel")
                            .on_press_maybe(Some(Message::CancelCompose)),
                    )
                    .spacing(spacing.space_s)
                    .padding(spacing.space_xs),
            )
            .push(widget::divider::horizontal::default())
            .push(
                widget::container(
                    widget::column()
                        .push(widget::text::caption(format!(
                            "From: {}",
                            account.config.user
                        )))
                        .push(
                            widget::text_input::text_input("To", to)
                                .on_input(Message::ComposeTo),
                        )
                        .push(
                            widget::text_input::text_input("Subject", subject)
                                .on_input(Message::ComposeSubject),
                        )
                        .push(widget::divider::horizontal::default())
                        .push(
                            widget::text_editor(body)
                                .on_action(Message::ComposeBody)
                                .height(Length::Fill),
                        )
                        .spacing(spacing.space_xxs)
                        .padding(spacing.space_xs),
                )
                .height(Length::Fill),
            )
            .into()
    }

    fn view_reply<'a>(
        &'a self,
        original: &'a EmailDetail,
        body: &'a text_editor::Content,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();

        widget::column()
            .push(
                widget::row()
                    .push(widget::text::title4("Reply"))
                    .push(widget::space().width(Length::Fill))
                    .push(
                        widget::button::suggested("Send")
                            .leading_icon(icon::from_name("mail-send-symbolic"))
                            .on_press_maybe(Some(Message::ReplySend)),
                    )
                    .push(
                        widget::button::standard("Cancel")
                            .on_press_maybe(Some(Message::CancelCompose)),
                    )
                    .spacing(spacing.space_s)
                    .padding(spacing.space_xs),
            )
            .push(widget::divider::horizontal::default())
            .push(
                widget::container(
                    widget::column()
                        .push(widget::text::caption(format!("To: {}", original.from)))
                        .push(widget::text::body(format!("Re: {}", original.subject)))
                        .push(widget::divider::horizontal::default())
                        .push(
                            widget::text_editor(body)
                                .on_action(Message::ReplyBody)
                                .height(200),
                        )
                        .push(widget::divider::horizontal::default())
                        .push(widget::text::caption("Original:"))
                        .push(widget::scrollable::vertical(widget::text::caption(
                            &original.body,
                        )))
                        .spacing(spacing.space_xxs)
                        .padding(spacing.space_xs),
                )
                .height(Length::Fill),
            )
            .into()
    }

    fn view_add_account<'a>(
        &'a self,
        name: &'a str,
        url: &'a str,
        user: &'a str,
        pass: &'a str,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();

        let can_save = !name.is_empty() && !url.is_empty() && !user.is_empty() && !pass.is_empty();

        widget::column()
            .push(
                widget::row()
                    .push(widget::text::title4("Add Mail Account"))
                    .push(widget::space().width(Length::Fill))
                    .push(
                        widget::button::suggested("Save")
                            .leading_icon(icon::from_name("document-save-symbolic"))
                            .on_press_maybe(can_save.then_some(Message::SaveAccount)),
                    )
                    .push(
                        widget::button::standard("Cancel")
                            .on_press_maybe(Some(Message::CancelCompose)),
                    )
                    .spacing(spacing.space_s)
                    .padding(spacing.space_xs),
            )
            .push(widget::divider::horizontal::default())
            .push(
                widget::container(
                    widget::column()
                        .push(widget::text::body(
                            "Enter your JMAP mail server details below.",
                        ))
                        .push(widget::space().height(spacing.space_s))
                        .push(
                            widget::text_input::text_input("Account name (e.g. Work)", name)
                                .on_input(Message::AddAccountName),
                        )
                        .push(
                            widget::text_input::text_input(
                                "Server URL (e.g. https://mail.example.com:8443)",
                                url,
                            )
                            .on_input(Message::AddAccountUrl),
                        )
                        .push(
                            widget::text_input::text_input("Username / Email", user)
                                .on_input(Message::AddAccountUser),
                        )
                        .push(
                            widget::text_input::secure_input("Password", pass, None, false)
                                .on_input(Message::AddAccountPass),
                        )
                        .spacing(spacing.space_s)
                        .padding(spacing.space_m)
                        .max_width(500),
                )
                .width(Length::Fill)
                .align_x(Alignment::Center)
                .padding(spacing.space_l),
            )
            .height(Length::Fill)
            .into()
    }
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

fn parse_mailboxes(json: &serde_json::Value) -> Vec<Mailbox> {
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };
    let mut mailboxes: Vec<Mailbox> = arr
        .iter()
        .map(|m| Mailbox {
            id: m["id"].as_str().unwrap_or("").to_string(),
            name: m["name"].as_str().unwrap_or("?").to_string(),
            total: m["totalEmails"].as_u64().unwrap_or(0),
            unread: m["unreadEmails"].as_u64().unwrap_or(0),
        })
        .collect();
    mailboxes.sort_by(|a, b| {
        let ai = a.name.eq_ignore_ascii_case("inbox");
        let bi = b.name.eq_ignore_ascii_case("inbox");
        match (ai, bi) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        }
    });
    mailboxes
}

fn parse_emails(json: &serde_json::Value) -> Vec<EmailSummary> {
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .map(|e| {
            let from = e["from"]
                .as_array()
                .and_then(|a| a.first())
                .map(|f| {
                    let name = f["name"].as_str().unwrap_or("");
                    let email = f["email"].as_str().unwrap_or("");
                    if name.is_empty() {
                        email.to_string()
                    } else {
                        format!("{name} <{email}>")
                    }
                })
                .unwrap_or_else(|| "Unknown".into());
            let date_raw = e["receivedAt"].as_str().unwrap_or("");
            let date = if date_raw.len() >= 16 {
                date_raw[..16].replace('T', " ")
            } else {
                date_raw.to_string()
            };
            EmailSummary {
                id: e["id"].as_str().unwrap_or("").to_string(),
                from,
                subject: e["subject"].as_str().unwrap_or("(no subject)").to_string(),
                date,
                preview: e["preview"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect(),
            }
        })
        .collect()
}

fn parse_email_detail(json: &serde_json::Value) -> EmailDetail {
    let from = json["from"]
        .as_array()
        .and_then(|a| a.first())
        .map(|f| {
            let name = f["name"].as_str().unwrap_or("");
            let email = f["email"].as_str().unwrap_or("");
            if name.is_empty() {
                email.to_string()
            } else {
                format!("{name} <{email}>")
            }
        })
        .unwrap_or_else(|| "Unknown".into());
    let to = json["to"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| t["email"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let body = json["bodyValues"]
        .as_object()
        .and_then(|bv| bv.values().next())
        .and_then(|v| v["value"].as_str())
        .unwrap_or("")
        .to_string();
    let date_raw = json["receivedAt"].as_str().unwrap_or("");
    let date = if date_raw.len() >= 16 {
        date_raw[..16].replace('T', " ")
    } else {
        date_raw.to_string()
    };
    EmailDetail {
        id: json["id"].as_str().unwrap_or("").to_string(),
        from,
        to,
        subject: json["subject"]
            .as_str()
            .unwrap_or("(no subject)")
            .to_string(),
        date,
        body,
    }
}
