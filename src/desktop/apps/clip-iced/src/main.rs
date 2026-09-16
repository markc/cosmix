mod bus;

use bus::{Action, Event, Request, Snapshot};
use iced::widget::{button, column, container, responsive, row, scrollable, text, text_input};
use iced::{window, Color, Element, Fill, Font, Subscription, Task, Theme};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const BG: Color = Color::from_rgb8(0x1b, 0x1d, 0x23);
const ROW: Color = Color::from_rgb8(0x20, 0x24, 0x2d);
const HOVER: Color = Color::from_rgb8(0x26, 0x2b, 0x35);
const TEXT: Color = Color::from_rgb8(0xcf, 0xd3, 0xda);
const MUTED: Color = Color::from_rgb8(0x6b, 0x72, 0x80);
const ACCENT: Color = Color::from_rgb8(0x8f, 0xb8, 0xe8);
const AMBER: Color = Color::from_rgb8(0xd9, 0xa0, 0x5b);

fn main() -> iced::Result {
    let smoke = std::env::var("CLIPPANEL_SMOKE").unwrap_or_default();
    if !smoke.is_empty() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("clipboard Tokio runtime");
        std::process::exit(runtime.block_on(bus::smoke(&smoke)));
    }
    iced::daemon(Panel::new, Panel::update, Panel::view)
        .title("CosMix Clipboard")
        .theme(Theme::Dark)
        .subscription(Panel::subscription)
        .run()
}

struct Panel {
    window: Option<window::Id>,
    data: Snapshot,
    requests: Option<mpsc::Sender<Request>>,
    filter: String,
    clear_armed: Option<Instant>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
enum Message {
    Bus(Event),
    Filter(String),
    Search,
    Action(Action),
    Clear,
    Disarm(Instant),
    Closed(window::Id),
    Done,
}

fn open() -> (window::Id, Task<Message>) {
    let (id, task) = window::open(window::Settings {
        size: iced::Size::new(720.0, 520.0),
        ..window::Settings::default()
    });
    (id, task.map(|_| Message::Done))
}

impl Panel {
    fn new() -> (Self, Task<Message>) {
        let (id, task) = open();
        (
            Self {
                window: Some(id),
                data: Snapshot::default(),
                requests: None,
                filter: String::new(),
                clear_armed: None,
                error: None,
            },
            task,
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(bus::events).map(Message::Bus),
            window::close_events().map(Message::Closed),
        ])
    }

    fn send(&mut self, request: Request) {
        if let Some(sender) = &self.requests {
            if sender.try_send(request).is_err() {
                self.error = Some("Bus action queue unavailable".into());
            }
        } else {
            self.error = Some("Bus connecting".into());
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Bus(Event::Ready(sender)) => self.requests = Some(sender),
            Message::Bus(Event::Snapshot(snapshot)) => {
                self.data = *snapshot;
                self.error = None;
            }
            Message::Bus(Event::Error(error)) => self.error = Some(error),
            Message::Bus(Event::Toggle) => {
                if let Some(id) = self.window.take() {
                    return window::close(id);
                }
                let (id, task) = open();
                self.window = Some(id);
                return task;
            }
            Message::Closed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
            }
            Message::Filter(value) => self.filter = value,
            Message::Search => self.send(Request::Query(self.filter.clone())),
            Message::Action(action) => self.send(Request::Action(action)),
            Message::Clear => {
                let now = Instant::now();
                if self
                    .clear_armed
                    .is_some_and(|at| now.duration_since(at) < Duration::from_millis(2500))
                {
                    self.clear_armed = None;
                    self.send(Request::Action(Action::Clear));
                } else {
                    self.clear_armed = Some(now);
                    return Task::perform(
                        async move {
                            tokio::time::sleep(Duration::from_millis(2500)).await;
                            now
                        },
                        Message::Disarm,
                    );
                }
            }
            Message::Disarm(at) => {
                if self.clear_armed == Some(at) {
                    self.clear_armed = None;
                }
            }
            Message::Done => {}
        }
        Task::none()
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let local = &self.data.local;
        let skipped = if local.skipped > 0 {
            format!(" · {} skipped", local.skipped)
        } else {
            String::new()
        };
        let status = format!(
            "rev {} · {} entries{} · {}{}",
            local.revision,
            local.total,
            skipped,
            local.persistence,
            if local.paused { " · PAUSED" } else { "" }
        );
        let armed = self.clear_armed.is_some();
        let clear = button(text(if armed { "Sure?" } else { "Clear" }).size(12))
            .on_press(Message::Clear)
            .style(move |theme, state| {
                let mut style = row_style(theme, state);
                if armed {
                    style.background = Some(Color::from_rgb8(0x5a, 0x26, 0x26).into());
                    style.text_color = Color::from_rgb8(0xff, 0xb0, 0xb0);
                }
                style
            });
        let header = row![
            text("CosMix Clipboard").size(18),
            text(status)
                .size(11)
                .color(if local.persistence == "ok" {
                    MUTED
                } else {
                    AMBER
                })
                .width(Fill),
            text_input("search…", &self.filter)
                .on_input(Message::Filter)
                .on_submit(Message::Search)
                .size(12)
                .width(150),
            button(text(if local.paused { "Resume" } else { "Pause" }).size(12))
                .on_press(Message::Action(Action::Pause(!local.paused)))
                .style(row_style),
            clear,
        ]
        .spacing(8)
        .align_y(iced::Center);
        let columns = row![
            text("ID").width(36),
            text("BYTES").width(52),
            text("AGE").width(38),
            text("PREVIEW").width(Fill)
        ]
        .spacing(8);
        let filter = self.filter.to_lowercase();
        let rows = column(
            local
                .entries
                .iter()
                .filter(|entry| {
                    entry.preview.to_lowercase().contains(&filter)
                        || bus::string(&entry.id).to_lowercase().contains(&filter)
                })
                .map(|entry| entry_row(entry, false)),
        )
        .spacing(3);
        let mut content = column![
            header,
            container(columns).style(|_| container::Style {
                text_color: Some(MUTED),
                ..Default::default()
            }),
            scrollable(rows).height(Fill)
        ]
        .spacing(8);
        let remote = &self.data.remote;
        if remote.ok {
            let node = remote.target.split('.').nth(1).unwrap_or(&remote.target);
            content = content.push(text(format!("REMOTE · {node}")).size(11).color(MUTED));
            for entry in remote.entries.iter().take(4) {
                content = content.push(entry_row(entry, true));
            }
        }
        if let Some(error) = &self.error {
            content = content.push(text(error).size(11).color(AMBER));
        }
        content = content.push(text("Click an entry to make it the live selection, then paste (Ctrl+V). Updates arrive live over the desktop.clipboard.changed Bus topic.").size(9).color(MUTED));
        container(content)
            .padding(14)
            .width(Fill)
            .height(Fill)
            .style(|_| container::Style {
                background: Some(BG.into()),
                text_color: Some(TEXT),
                ..Default::default()
            })
            .into()
    }
}

fn row_style(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(
            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                HOVER
            } else {
                ROW
            }
            .into(),
        ),
        text_color: TEXT,
        border: iced::Border {
            color: Color::from_rgb8(0x2c, 0x31, 0x3c),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn entry_row(entry: &bus::Entry, remote: bool) -> Element<'_, Message> {
    let preview = responsive(move |size| {
        let limit = (size.width / 8.0).floor().max(1.0) as usize;
        let value = elide(&entry.preview, limit);
        text(value)
            .font(Font::MONOSPACE)
            .size(13)
            .wrapping(iced::widget::text::Wrapping::None)
            .into()
    });
    let cells = row![
        text(bus::string(&entry.id))
            .size(13)
            .color(ACCENT)
            .width(36),
        text(format!("{}b", entry.bytes))
            .size(12)
            .color(MUTED)
            .width(52),
        text(age(entry.at)).size(12).color(MUTED).width(38),
        container(preview).width(Fill).height(18),
    ]
    .spacing(8)
    .align_y(iced::Center);
    button(cells)
        .width(Fill)
        .height(if remote { 30 } else { 38 })
        .padding([4, 10])
        .style(row_style)
        .on_press(Message::Action(if remote {
            Action::RemotePick(entry.id.clone())
        } else {
            Action::Pick(entry.id.clone())
        }))
        .into()
}

fn elide(value: &str, limit: usize) -> String {
    let value = value.replace(['\n', '\r', '\t'], " ");
    if value.chars().count() <= limit {
        value
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(limit.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn age(at: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let seconds = (now - at).max(0.0) as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
