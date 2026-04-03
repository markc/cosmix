//! cosmix-dialog-iced — Pure Rust layer-shell dialog utility.
//!
//! No GTK, no C dependencies, no unsafe code.
//! Uses Iced + iced_layershell + tiny-skia for rendering.
//!
//! Usage:
//!   cosmix-dialog-iced info --text="Hello world"
//!   cosmix-dialog-iced confirm --text="Continue?"
//!   cosmix-dialog-iced input --text="Name:" --entry-text="Mark"
//!   cosmix-dialog-iced password --text="Secret:"
//!   cosmix-dialog-iced progress --text="Working..." --pulsate

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod dialogs;
mod theme;

use clap::{Parser, Subcommand};
use iced::{Color, Element, Task, Theme};
use iced_layershell::application;
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings};
use iced_layershell::to_layer_message;

// ── CLI ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "cosmix-dialog-iced", about = "Pure Rust layer-shell dialog")]
struct Cli {
    #[command(subcommand)]
    mode: CliMode,

    #[arg(long, global = true)]
    title: Option<String>,

    #[arg(long, global = true)]
    width: Option<u32>,

    #[arg(long, global = true)]
    height: Option<u32>,
}

#[derive(Subcommand, Clone, Debug)]
enum CliMode {
    Info {
        #[arg(long)]
        text: String,
    },
    Warning {
        #[arg(long)]
        text: String,
    },
    Error {
        #[arg(long)]
        text: String,
    },
    Confirm {
        #[arg(long)]
        text: String,
        #[arg(long)]
        yes_label: Option<String>,
        #[arg(long)]
        no_label: Option<String>,
        #[arg(long)]
        cancel: bool,
    },
    Input {
        #[arg(long)]
        text: String,
        #[arg(long)]
        entry_text: Option<String>,
        #[arg(long)]
        placeholder: Option<String>,
    },
    Password {
        #[arg(long)]
        text: String,
    },
    Progress {
        #[arg(long, default_value = "")]
        text: String,
        #[arg(long)]
        pulsate: bool,
        #[arg(long)]
        auto_close: bool,
    },
}

impl CliMode {
    fn default_size(&self) -> (u32, u32) {
        match self {
            CliMode::Info { .. } | CliMode::Warning { .. } | CliMode::Error { .. } => (340, 120),
            CliMode::Confirm { cancel: true, .. } => (400, 130),
            CliMode::Confirm { .. } => (360, 130),
            CliMode::Input { .. } => (360, 150),
            CliMode::Password { .. } => (360, 140),
            CliMode::Progress { .. } => (420, 130),
        }
    }
}

// ── Messages ─────────────────────────────────────────────────────────

#[to_layer_message]
#[derive(Debug, Clone)]
pub enum Message {
    InputChanged(String),
    Submit,
    Cancel,
    Dismiss,
    ProgressTick,
}

// ── State ────────────────────────────────────────────────────────────

struct DialogState {
    mode: CliMode,
    input_value: String,
    progress_fraction: f32,
}

// ── App logic ────────────────────────────────────────────────────────

static CLI_MODE: std::sync::OnceLock<CliMode> = std::sync::OnceLock::new();

fn boot() -> DialogState {
    let mode = CLI_MODE.get().expect("CLI_MODE not set").clone();
    let input_value = match &mode {
        CliMode::Input { entry_text, .. } => entry_text.clone().unwrap_or_default(),
        _ => String::new(),
    };
    DialogState {
        mode,
        input_value,
        progress_fraction: 0.0,
    }
}

fn update(state: &mut DialogState, message: Message) -> Task<Message> {
    match message {
        Message::InputChanged(s) => {
            state.input_value = s;
            Task::none()
        }
        Message::Submit => {
            match &state.mode {
                CliMode::Input { .. } | CliMode::Password { .. } => {
                    print!("{}", state.input_value);
                }
                CliMode::Confirm { .. } => {
                    print!("true");
                }
                _ => {}
            }
            std::process::exit(0);
        }
        Message::Cancel => {
            match &state.mode {
                CliMode::Confirm { .. } => print!("false"),
                _ => {}
            }
            std::process::exit(1);
        }
        Message::Dismiss => {
            std::process::exit(1);
        }
        Message::ProgressTick => {
            // Read from stdin would go here; for now just pulse
            if matches!(state.mode, CliMode::Progress { pulsate: true, .. }) {
                state.progress_fraction += 0.02;
                if state.progress_fraction > 1.0 {
                    state.progress_fraction = 0.0;
                }
            }
            Task::none()
        }
        _ => Task::none(),
    }
}

fn view(state: &DialogState) -> Element<'_, Message> {
    match &state.mode {
        CliMode::Info { text: t } => dialogs::message::view(t, "info"),
        CliMode::Warning { text: t } => dialogs::message::view(t, "warning"),
        CliMode::Error { text: t } => dialogs::message::view(t, "error"),
        CliMode::Confirm {
            text: t,
            yes_label,
            no_label,
            cancel,
        } => dialogs::question::view(
            t,
            yes_label.as_deref().unwrap_or("Yes"),
            no_label.as_deref().unwrap_or("No"),
            *cancel,
        ),
        CliMode::Input {
            text: t,
            placeholder,
            ..
        } => dialogs::entry::view(
            t,
            &state.input_value,
            placeholder.as_deref().unwrap_or(""),
        ),
        CliMode::Password { text: t } => dialogs::password::view(t, &state.input_value),
        CliMode::Progress {
            text: t, pulsate, ..
        } => dialogs::progress::view(t, state.progress_fraction, *pulsate),
    }
}

fn the_theme(_state: &DialogState) -> Theme {
    Theme::Dark
}

/// Transparent surface background — lets rounded corners show through.
fn the_style(_state: &DialogState, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}

fn subscription(_state: &DialogState) -> iced::Subscription<Message> {
    // TODO: progress pulsation via iced time subscription
    iced::Subscription::none()
}

// ── Entry point ──────────────────────────────────────────────────────

fn main() -> Result<(), iced_layershell::Error> {
    let cli = Cli::parse();
    let (w, h) = match (cli.width, cli.height) {
        (Some(w), Some(h)) => (w, h),
        _ => cli.mode.default_size(),
    };

    CLI_MODE.set(cli.mode).expect("CLI_MODE already set");

    application(boot, || String::from("cosmix-dialog"), update, view)
        .theme(the_theme)
        .style(the_style)
        .subscription(subscription)
        .settings(Settings {
            layer_settings: LayerShellSettings {
                layer: Layer::Overlay,
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                size: Some((w, h)),
                exclusive_zone: -1,
                anchor: Anchor::empty(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}
