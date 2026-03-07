mod config;

use config::{AccountConfig, MailConfig};
use cosmic::app::Settings;
use cosmic::iced::{self, Alignment, Length, Size};
use cosmic::widget::menu::{self, KeyBind};
use cosmic::widget::{self, icon, markdown, nav_bar, text_editor};
use cosmic::{executor, Core, Element};
use cosmix_lib::mail::JmapClient;
use std::collections::HashMap;
use std::sync::LazyLock;

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
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::default().size(Size::new(1200., 750.));
    cosmic::app::run::<MailApp>(settings, ())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct MailApp {
    core: Core,
    accounts: Vec<Account>,
    active_account: usize,
    nav_model: nav_bar::Model,
    keybinds: HashMap<KeyBind, MenuAction>,
    emails: Vec<EmailSummary>,
    selected_email_id: Option<String>,
    selected_email: Option<EmailDetail>,
    markdown_items: Vec<markdown::Item>,
    right_pane: RightPane,
    show_preview: bool,
    error: Option<String>,
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
    MarkdownLink(markdown::Uri),
    SelectAll,
    MarkRead,
    MarkUnread,
    TogglePreview,
    About,
    Quit,
    Surface(cosmic::surface::Action),
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

        let error = if config.is_none() {
            Some("Create ~/.config/cosmix/mail.toml with [[accounts]]".into())
        } else {
            None
        };

        use cosmic::iced::keyboard::{Key, key::Named};
        use cosmic::widget::menu::key_bind::Modifier;

        let keybinds = HashMap::from([
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("n".into()) }, MenuAction::Compose),
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("r".into()) }, MenuAction::Reply),
            (KeyBind { modifiers: vec![Modifier::Ctrl], key: Key::Character("q".into()) }, MenuAction::Quit),
            (KeyBind { modifiers: vec![], key: Key::Named(Named::F5) }, MenuAction::Refresh),
            (KeyBind { modifiers: vec![], key: Key::Named(Named::Delete) }, MenuAction::Delete),
        ]);

        let mut app = MailApp {
            core,
            accounts,
            active_account: 0,
            nav_model: nav_bar::Model::default(),
            keybinds,
            emails: Vec::new(),
            selected_email_id: None,
            selected_email: None,
            markdown_items: Vec::new(),
            right_pane: RightPane::Preview,
            show_preview: true,
            error,
        };

        let task = if !app.accounts.is_empty() {
            app.accounts[0].status = AccountStatus::Connecting;
            connect_account(0, app.accounts[0].config.clone())
        } else {
            cosmic::Task::none()
        };

        (app, task)
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav_model)
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> cosmic::app::Task<Message> {
        self.nav_model.activate(id);

        let mailbox_id = self.nav_model.active_data::<String>().cloned();
        self.selected_email_id = None;
        self.selected_email = None;
        self.markdown_items.clear();
        self.right_pane = RightPane::Preview;

        let idx = self.active_account;
        if let Some(client) = self.accounts[idx].client.clone() {
            load_emails(idx, client, mailbox_id)
        } else {
            cosmic::Task::none()
        }
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        vec![cosmic::widget::responsive_menu_bar().into_element(
            self.core(),
            &self.keybinds,
            MENU_ID.clone(),
            Message::Surface,
            vec![
                (
                    "File".into(),
                    vec![
                        menu::Item::Button("New Message", Some(icon::from_name("mail-message-new-symbolic").into()), MenuAction::Compose),
                        menu::Item::Divider,
                        menu::Item::Button("Refresh", Some(icon::from_name("view-refresh-symbolic").into()), MenuAction::Refresh),
                        menu::Item::Divider,
                        menu::Item::Button("Quit", Some(icon::from_name("window-close-symbolic").into()), MenuAction::Quit),
                    ],
                ),
                (
                    "Edit".into(),
                    vec![
                        menu::Item::Button("Select All", None, MenuAction::SelectAll),
                        menu::Item::Divider,
                        menu::Item::Button("Mark as Read", None, MenuAction::MarkRead),
                        menu::Item::Button("Mark as Unread", None, MenuAction::MarkUnread),
                        menu::Item::Divider,
                        menu::Item::Button("Delete", Some(icon::from_name("edit-delete-symbolic").into()), MenuAction::Delete),
                    ],
                ),
                (
                    "View".into(),
                    vec![
                        menu::Item::CheckBox("Preview Pane", None, self.show_preview, MenuAction::TogglePreview),
                    ],
                ),
                (
                    "Help".into(),
                    vec![
                        menu::Item::Button("About Cosmix Mail", Some(icon::from_name("help-about-symbolic").into()), MenuAction::About),
                    ],
                ),
            ],
        )]
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        let mut items: Vec<Element<'_, Message>> = Vec::new();

        // Account switcher buttons
        for (i, account) in self.accounts.iter().enumerate() {
            let label = match &account.status {
                AccountStatus::Disconnected => account.config.name.clone(),
                AccountStatus::Connecting => format!("... {}", account.config.name),
                AccountStatus::Connected => {
                    let unread: u64 = account.mailboxes.iter().map(|m| m.unread).sum();
                    if unread > 0 {
                        format!("{} ({})", account.config.name, unread)
                    } else {
                        account.config.name.clone()
                    }
                }
                AccountStatus::Error(_) => format!("! {}", account.config.name),
            };

            let btn = if i == self.active_account {
                widget::button::suggested(label)
            } else {
                widget::button::standard(label)
            };

            items.push(btn.on_press_maybe(Some(Message::AccountSelected(i))).into());
        }

        items
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
                    let mb_id = self.nav_model.active_data::<String>().cloned();
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

            Message::MarkdownLink(url) => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn();
            }
        }

        cosmic::Task::none()
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

        // Two-pane: email list | email content
        let panes = widget::row()
            .push(self.view_email_list())
            .push(widget::divider::vertical::default())
            .push(self.view_content_pane())
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
        self.nav_model = nav_bar::Model::default();
        let account = &self.accounts[self.active_account];

        for mb in &account.mailboxes {
            let label = if mb.unread > 0 {
                format!("{} ({}/{})", mb.name, mb.unread, mb.total)
            } else if mb.total > 0 {
                format!("{} ({})", mb.name, mb.total)
            } else {
                mb.name.clone()
            };

            self.nav_model
                .insert()
                .text(label)
                .icon(icon::from_name(mailbox_icon_name(&mb.name)))
                .data::<String>(mb.id.clone());
        }

        // Activate inbox by default
        if let Some(pos) = account
            .mailboxes
            .iter()
            .position(|m| m.name.eq_ignore_ascii_case("inbox"))
        {
            self.nav_model.activate_position(pos as u16);
        } else {
            self.nav_model.activate_position(0);
        }
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
                        .push(widget::text::body(&email.subject))
                        .push(widget::space().width(Length::Fill))
                        .push(widget::text::caption(&email.date))
                        .spacing(spacing.space_xs),
                )
                .push(widget::text::caption(&email.from))
                .push(widget::text::caption(&email.preview))
                .spacing(1);

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
            .width(320)
            .height(Length::Fill)
            .into()
    }

    fn view_content_pane(&self) -> Element<'_, Message> {
        let content: Element<'_, Message> = match &self.right_pane {
            RightPane::Preview => self.view_email_preview(),
            RightPane::Compose { to, subject, body } => self.view_compose(to, subject, body),
            RightPane::Reply { original, body } => self.view_reply(original, body),
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

                // Email body with markdown rendering on white background
                let light_palette = cosmic::iced::Theme::Light.palette();
                let md_settings =
                    markdown::Settings::with_style(markdown::Style::from_palette(light_palette));
                let body_view = widget::scrollable::vertical(
                    widget::container(
                        markdown::view(&self.markdown_items, md_settings)
                            .map(Message::MarkdownLink),
                    )
                    .padding(spacing.space_s)
                    .width(Length::Fill)
                    .style(|_theme| widget::container::Style {
                        background: Some(cosmic::iced::Background::Color(
                            cosmic::iced::Color::WHITE,
                        )),
                        ..Default::default()
                    }),
                );

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
