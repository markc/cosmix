//! Launch only inside a test compositor; see docs/dev/iced-widgets.md.
#[cfg(not(any(feature = "wgpu", feature = "tiny-skia")))]
compile_error!("Select gallery-wgpu or gallery-tiny-skia to build the gallery.");

use cosmix_iced_widgets::{Item, Menu, TextField, Tokens};
use iced::widget::{column, container, text};
use iced::{Element, Fill, Theme};

fn main() -> iced::Result {
    iced::application(Gallery::default, Gallery::update, Gallery::view)
        .title("Cosmix widget gallery")
        .theme(Theme::Dark)
        .run()
}

#[derive(Default)]
struct Gallery {
    value: String,
    password: String,
    last_action: String,
}

#[derive(Debug, Clone)]
enum Message {
    Text(String),
    Password(String),
    Action(&'static str),
}

fn items() -> Vec<Item<Message>> {
    vec![
        Item::action("New", Message::Action("New")).accelerator("Ctrl+N"),
        Item::submenu(
            "Open recent",
            vec![
                Item::action("Session one", Message::Action("Session one")),
                Item::submenu(
                    "Archive",
                    vec![Item::action("Session two", Message::Action("Session two"))],
                ),
            ],
        ),
        Item::separator(),
        Item::action("Unavailable", Message::Action("Unavailable")).enabled(false),
        Item::action("Save", Message::Action("Save")).accelerator("Ctrl+S"),
    ]
}

impl Gallery {
    fn update(&mut self, message: Message) {
        match message {
            Message::Text(value) => self.value = value,
            Message::Password(value) => self.password = value,
            Message::Action(action) => self.last_action = action.into(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let tokens = Tokens::default();
        let bar = Menu::bar(vec![
            Item::submenu("File", items()),
            Item::submenu(
                "Help",
                vec![Item::action("About", Message::Action("About"))],
            ),
        ])
        .style(tokens.menu_style());
        let context = Menu::context(
            container(text(
                "Right-click here for a context menu (or click, then Shift+F10).",
            ))
            .padding(24)
            .width(Fill),
            items(),
        )
        .style(tokens.menu_style());
        container(
            column![
                bar,
                text("Text input").size(24),
                TextField::new(
                    "Type, select, paste; Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y",
                    &self.value
                )
                .on_input(Message::Text)
                .padding(10)
                .style(move |_, status| tokens.text_input(status)),
                TextField::new("Password", &self.password)
                    .secure(true)
                    .on_input(Message::Password)
                    .padding(10)
                    .style(move |_, status| tokens.text_input(status)),
                text("F10 activates the menu bar. Use arrows, Enter and Escape."),
                context,
                text(format!("Last action: {}", self.last_action)),
            ]
            .spacing(16),
        )
        .padding(24)
        .width(Fill)
        .height(Fill)
        .into()
    }
}
