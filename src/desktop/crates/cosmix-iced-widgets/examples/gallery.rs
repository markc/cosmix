//! Launch only inside a test compositor; see docs/dev/iced-widgets.md.
#[cfg(not(any(feature = "wgpu", feature = "tiny-skia")))]
compile_error!("Select gallery-wgpu or gallery-tiny-skia to build the gallery.");

use cosmix_iced_widgets::scale::format_db;
use cosmix_iced_widgets::{
    Fader, Item, Knob, LevelMeter, Menu, Note, PianoRoll, RollNotes, RollView, TextField, Toggle,
    Tokens, Waveform, WaveformPeaks,
};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Fill, Theme};

const STRIPS: usize = 8;
const ROLL_NOTES: usize = 131_072;
/// One colour per track, so a dense roll reads as separate parts.
const TRACK_COLOURS: [iced::Color; 4] = [
    iced::Color::from_rgb(0.35, 0.72, 0.55),
    iced::Color::from_rgb(0.40, 0.62, 0.86),
    iced::Color::from_rgb(0.85, 0.63, 0.35),
    iced::Color::from_rgb(0.76, 0.45, 0.72),
];

fn main() -> iced::Result {
    iced::application(Gallery::new, Gallery::update, Gallery::view)
        .title("Cosmix widget gallery")
        .theme(Theme::Dark)
        .run()
}

struct Gallery {
    value: String,
    password: String,
    last_action: String,
    gain: [f32; STRIPS],
    pan: [f32; STRIPS],
    mute: [bool; STRIPS],
    solo: [bool; STRIPS],
    level: [f32; STRIPS],
    peak: [Option<f32>; STRIPS],
    hold: [Option<f32>; STRIPS],
    peaks: WaveformPeaks,
    playhead: f32,
    notes: RollNotes,
    view: RollView,
    picked: Option<usize>,
}

#[derive(Debug, Clone)]
enum Message {
    Text(String),
    Password(String),
    Action(&'static str),
    Gain(usize, f32),
    Pan(usize, f32),
    Mute(usize, bool),
    Solo(usize, bool),
    Levels(bool),
    Seek(f32),
    View(RollView),
    Pick(usize),
}

/// Deterministic test data: a decaying chirp and a dense random song.
fn song() -> (WaveformPeaks, RollNotes) {
    let samples: Vec<f32> = (0..480_000)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            (t * t * 400.0).sin() * (-t * 0.3).exp()
        })
        .collect();
    let mut seed = 0x9e37_79b9_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let notes = (0..ROLL_NOTES)
        .map(|_| Note {
            start: (next() % 1_024_000) as f32 / 1000.0,
            length: 0.1 + (next() % 2000) as f32 / 1000.0,
            pitch: 24 + (next() % 84) as u8,
            velocity: (next() % 128) as u8,
            track: (next() % 8) as u16,
        })
        .collect();
    (
        WaveformPeaks::from_samples(&samples, 256),
        RollNotes::new(notes),
    )
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
    fn new() -> Self {
        let (peaks, notes) = song();
        Self {
            value: String::new(),
            password: String::new(),
            last_action: String::new(),
            gain: [0.0; STRIPS],
            pan: [0.0; STRIPS],
            mute: [false; STRIPS],
            solo: [false; STRIPS],
            level: [f32::NEG_INFINITY; STRIPS],
            peak: [None; STRIPS],
            hold: [None; STRIPS],
            peaks,
            playhead: 0.0,
            notes,
            view: RollView::default(),
            picked: None,
        }
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Text(value) => self.value = value,
            Message::Password(value) => self.password = value,
            Message::Action(action) => self.last_action = action.into(),
            Message::Gain(strip, db) => self.gain[strip] = db,
            Message::Pan(strip, pan) => self.pan[strip] = pan,
            Message::Mute(strip, on) => self.mute[strip] = on,
            Message::Solo(strip, on) => self.solo[strip] = on,
            // No timer: the meters only change on these buttons, so an idle
            // gallery schedules nothing once the peak lines have fallen.
            Message::Levels(loud) => {
                for strip in 0..STRIPS {
                    let level = if loud {
                        -1.0 - strip as f32 * 4.0
                    } else {
                        -48.0
                    };
                    self.level[strip] = level;
                    // A host tracks its own peak and hold; here the buttons
                    // stand in for a meter feed.
                    self.peak[strip] = Some(level + 2.0);
                    self.hold[strip] = Some(self.hold[strip].unwrap_or(level).max(level + 2.0));
                }
            }
            Message::Seek(fraction) => self.playhead = fraction,
            Message::View(view) => self.view = view,
            Message::Pick(index) => self.picked = Some(index),
        }
    }

    fn strip(&self, strip: usize, tokens: Tokens) -> Element<'_, Message> {
        let style = tokens.audio_style();
        column![
            text(format!("Ch {}", strip + 1)).size(12),
            Knob::new(self.pan[strip])
                .on_change(move |pan| Message::Pan(strip, pan))
                .style(style),
            row![
                Fader::new(self.gain[strip])
                    .on_change(move |db| Message::Gain(strip, db))
                    .style(style),
                LevelMeter::new(self.level[strip])
                    .peak(self.peak[strip])
                    .hold(self.hold[strip])
                    .clipped(self.level[strip] > -1.0)
                    .style(style),
            ]
            .spacing(4),
            text(format_db(self.gain[strip])).size(11),
            row![
                Toggle::new("M", self.mute[strip])
                    .alert(true)
                    .on_toggle(move |on| Message::Mute(strip, on))
                    .style(style),
                Toggle::new("S", self.solo[strip])
                    .on_toggle(move |on| Message::Solo(strip, on))
                    .style(style),
            ]
            .spacing(2),
        ]
        .spacing(6)
        .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let tokens = Tokens::default();
        let style = tokens.audio_style();
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
        let mixer = row((0..STRIPS).map(|strip| self.strip(strip, tokens))).spacing(12);
        let end = self.notes.end_beat();
        let picked = self
            .picked
            .map(|index| self.notes.notes()[index])
            .map_or_else(String::new, |note| {
                format!("picked pitch {} at beat {:.2}", note.pitch, note.start)
            });
        let page = column![
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
            text("Mixer (drag; Shift is fine; double-click resets)").size(24),
            row![
                button("Loud").on_press(Message::Levels(true)),
                button("Quiet").on_press(Message::Levels(false)),
            ]
            .spacing(8),
            mixer,
            text("Waveform (click to seek)").size(24),
            Waveform::new(&self.peaks)
                .playhead(Some(self.playhead))
                .on_seek(Message::Seek)
                .style(style),
            text(format!(
                "Piano roll: {} notes (wheel, Shift+wheel, Ctrl+wheel) {picked}",
                self.notes.len()
            ))
            .size(24),
            PianoRoll::new(&self.notes, self.view)
                .track_colours(&TRACK_COLOURS)
                .playhead(Some(self.playhead * end))
                .on_view(Message::View)
                .on_note(Message::Pick)
                .height(320)
                .style(style),
        ]
        .spacing(16)
        .padding(24);
        column![bar, scrollable(page).height(Fill)]
            .width(Fill)
            .height(Fill)
            .into()
    }
}
