use anyhow::Result;
use iced::widget::{button, column, container, row, text, text_input, scrollable};
use iced::{Element, Length, Task, Theme};
use std::sync::Mutex;

static DIALOG_RESULT: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Clone)]
enum Message {
    InputChanged(String),
    Submit,
    Confirm(bool),
    Selected(usize),
}

enum DialogKind {
    Message { title: String, body: String },
    Input { prompt: String },
    Confirm { question: String },
    List { title: String, items: Vec<String> },
}

struct DialogApp {
    kind: DialogKind,
    input_value: String,
}

impl DialogApp {
    fn new(kind: DialogKind) -> (Self, Task<Message>) {
        (
            Self {
                kind,
                input_value: String::new(),
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InputChanged(val) => {
                self.input_value = val;
                Task::none()
            }
            Message::Submit => {
                *DIALOG_RESULT.lock().unwrap() = Some(self.input_value.clone());
                iced::exit()
            }
            Message::Confirm(yes) => {
                *DIALOG_RESULT.lock().unwrap() = Some(if yes { "yes".into() } else { "no".into() });
                iced::exit()
            }
            Message::Selected(idx) => {
                if let DialogKind::List { items, .. } = &self.kind {
                    *DIALOG_RESULT.lock().unwrap() = items.get(idx).cloned();
                }
                iced::exit()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let content: Element<Message> = match &self.kind {
            DialogKind::Message { title, body } => {
                column![
                    text(title.clone()).size(20),
                    text(body.clone()).size(14),
                    button("OK").on_press(Message::Confirm(true)).padding(8),
                ]
                .spacing(12)
                .into()
            }
            DialogKind::Input { prompt } => {
                column![
                    text(prompt.clone()).size(16),
                    text_input("Type here...", &self.input_value)
                        .on_input(Message::InputChanged)
                        .on_submit(Message::Submit)
                        .padding(8),
                    row![
                        button("Cancel").on_press(Message::Confirm(false)).padding(8),
                        button("OK").on_press(Message::Submit).padding(8),
                    ]
                    .spacing(8),
                ]
                .spacing(12)
                .into()
            }
            DialogKind::Confirm { question } => {
                column![
                    text(question.clone()).size(16),
                    row![
                        button("No").on_press(Message::Confirm(false)).padding(8),
                        button("Yes").on_press(Message::Confirm(true)).padding(8),
                    ]
                    .spacing(8),
                ]
                .spacing(12)
                .into()
            }
            DialogKind::List { title, items } => {
                let mut col = column![text(title.clone()).size(16)].spacing(4);
                for (i, item) in items.iter().enumerate() {
                    col = col.push(
                        button(text(item.clone()).size(14))
                            .on_press(Message::Selected(i))
                            .padding(6)
                            .width(Length::Fill),
                    );
                }
                col = col.push(
                    button("Cancel").on_press(Message::Confirm(false)).padding(8),
                );
                scrollable(col).into()
            }
        };

        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn run_dialog(kind: DialogKind) -> Result<Option<String>> {
    // Reset result
    *DIALOG_RESULT.lock().unwrap() = None;

    let window_settings = iced::window::Settings {
        size: iced::Size::new(400.0, 250.0),
        resizable: true,
        decorations: true,
        ..Default::default()
    };

    iced::application("Cosmix", DialogApp::update, DialogApp::view)
        .theme(DialogApp::theme)
        .antialiasing(true)
        .window(window_settings)
        .run_with(move || DialogApp::new(kind))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(DIALOG_RESULT.lock().unwrap().clone())
}

// CLI: cosmix dialog <type> <args...>
// Prints result to stdout, exits 0 for OK/yes, 1 for cancel/no
pub fn dialog_cmd(args: &[String]) -> Result<()> {
    let dtype = args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Usage: cosmix dialog <message|input|confirm|list> <args...>");
        std::process::exit(1);
    });

    match dtype {
        "message" | "msg" => {
            let title = args.get(1).cloned().unwrap_or_else(|| "Cosmix".into());
            let body = args.get(2).cloned().unwrap_or_default();
            run_dialog(DialogKind::Message { title, body })?;
        }
        "input" => {
            let prompt = args.get(1).cloned().unwrap_or_else(|| "Enter value:".into());
            let result = run_dialog(DialogKind::Input { prompt })?;
            match result {
                Some(val) if val != "no" => print!("{val}"),
                _ => std::process::exit(1),
            }
        }
        "confirm" => {
            let question = args.get(1).cloned().unwrap_or_else(|| "Are you sure?".into());
            let result = run_dialog(DialogKind::Confirm { question })?;
            if result.as_deref() != Some("yes") {
                std::process::exit(1);
            }
        }
        "list" => {
            let title = args.get(1).cloned().unwrap_or_else(|| "Select:".into());
            let items: Vec<String> = args[2..].to_vec();
            if items.is_empty() {
                eprintln!("Usage: cosmix dialog list <title> <item1> <item2> ...");
                std::process::exit(1);
            }
            let result = run_dialog(DialogKind::List { title, items })?;
            match result {
                Some(val) => print!("{val}"),
                None => std::process::exit(1),
            }
        }
        _ => {
            eprintln!("Unknown dialog type: {dtype}");
            eprintln!("Types: message, input, confirm, list");
            std::process::exit(1);
        }
    }

    Ok(())
}
