//! CLI argument parsing — kdialog/zenity compatible interface.
//!
//! Parses command-line arguments into a `DialogRequest`.

use clap::{Parser, Subcommand};

use crate::types::*;
use crate::DialogRequest;

#[derive(Parser)]
#[command(name = "cosmix-dialog", about = "GUI dialog utility for the Cosmix stack")]
pub struct Cli {
    #[command(subcommand)]
    pub mode: CliMode,

    /// Output JSON instead of plain text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Window title override.
    #[arg(long, global = true)]
    pub title: Option<String>,

    /// Window width.
    #[arg(long, global = true)]
    pub width: Option<u32>,

    /// Window height.
    #[arg(long, global = true)]
    pub height: Option<u32>,

    /// Timeout in seconds (0 = no timeout).
    #[arg(long, global = true, default_value = "0")]
    pub timeout: u32,
}

#[derive(Subcommand)]
pub enum CliMode {
    /// Show an info/warning/error message.
    #[command(name = "info")]
    Info {
        /// Message text.
        #[arg(long)]
        text: String,
    },
    /// Show a warning message.
    #[command(name = "warning")]
    Warning {
        #[arg(long)]
        text: String,
    },
    /// Show an error message.
    #[command(name = "error")]
    Error {
        #[arg(long)]
        text: String,
    },
    /// Ask a yes/no question.
    #[command(name = "confirm")]
    Confirm {
        #[arg(long)]
        text: String,
        /// Label for the Yes button.
        #[arg(long)]
        yes_label: Option<String>,
        /// Label for the No button.
        #[arg(long)]
        no_label: Option<String>,
        /// Show a Cancel button.
        #[arg(long)]
        cancel: bool,
    },
    /// Single-line text input.
    #[command(name = "input")]
    Input {
        #[arg(long)]
        text: String,
        /// Default value for the input field.
        #[arg(long)]
        entry_text: Option<String>,
        /// Placeholder text.
        #[arg(long)]
        placeholder: Option<String>,
    },
    /// Password input (masked).
    #[command(name = "password")]
    Password {
        #[arg(long)]
        text: String,
    },
    /// Display scrollable text from stdin.
    #[command(name = "text-info")]
    TextInfo,
    /// Multi-line text editor.
    #[command(name = "text-input")]
    TextInput {
        #[arg(long, default_value = "")]
        text: String,
        /// Default content.
        #[arg(long)]
        default: Option<String>,
    },
    /// Dropdown selection.
    #[command(name = "combo")]
    Combo {
        #[arg(long)]
        text: String,
        /// Items to choose from.
        #[arg(long, num_args = 1..)]
        items: Vec<String>,
        /// Allow custom input.
        #[arg(long)]
        editable: bool,
    },
    /// Multi-select checklist.
    #[command(name = "checklist")]
    CheckList {
        #[arg(long)]
        text: String,
        /// Items as "key:label:on/off" triplets.
        #[arg(long, num_args = 1..)]
        items: Vec<String>,
    },
    /// Single-select radio list.
    #[command(name = "radiolist")]
    RadioList {
        #[arg(long)]
        text: String,
        /// Items as "key:label:on/off" triplets.
        #[arg(long, num_args = 1..)]
        items: Vec<String>,
    },
    /// Progress bar (reads percentage from stdin).
    #[command(name = "progress")]
    Progress {
        #[arg(long, default_value = "")]
        text: String,
        /// Indeterminate (pulsating) mode.
        #[arg(long)]
        pulsate: bool,
        /// Close automatically when 100% reached.
        #[arg(long)]
        auto_close: bool,
    },
    // TODO: form, file-open, file-save, directory, scale, calendar
}

impl Cli {
    /// Convert parsed CLI args into a DialogRequest.
    pub fn into_request(self, stdin_text: Option<String>) -> DialogRequest {
        let kind = match self.mode {
            CliMode::Info { text } => DialogKind::Message {
                text,
                level: MessageLevel::Info,
                detail: None,
            },
            CliMode::Warning { text } => DialogKind::Message {
                text,
                level: MessageLevel::Warning,
                detail: None,
            },
            CliMode::Error { text } => DialogKind::Message {
                text,
                level: MessageLevel::Error,
                detail: None,
            },
            CliMode::Confirm { text, yes_label, no_label, cancel } => DialogKind::Question {
                text,
                yes_label,
                no_label,
                cancel,
            },
            CliMode::Input { text, entry_text, placeholder } => DialogKind::Entry {
                text,
                default: entry_text,
                placeholder,
            },
            CliMode::Password { text } => DialogKind::Password { text },
            CliMode::TextInfo => DialogKind::TextViewer {
                source: TextSource::Stdin(stdin_text.unwrap_or_default()),
                checkbox: None,
            },
            CliMode::TextInput { text, default } => DialogKind::TextInput { text, default },
            CliMode::Combo { text, items, editable } => DialogKind::ComboBox {
                text,
                items,
                default: None,
                editable,
            },
            CliMode::CheckList { text, items } => DialogKind::CheckList {
                text,
                items: parse_list_items(&items),
            },
            CliMode::RadioList { text, items } => DialogKind::RadioList {
                text,
                items: parse_list_items(&items),
            },
            CliMode::Progress { text, pulsate, auto_close } => DialogKind::Progress {
                text,
                pulsate,
                auto_close,
            },
        };

        let size = match (self.width, self.height) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        };

        DialogRequest {
            kind,
            title: self.title,
            size,
            timeout: self.timeout,
            json_output: self.json,
        }
    }
}

/// Parse "key:label:on/off" triplets into ListItems.
fn parse_list_items(items: &[String]) -> Vec<ListItem> {
    items.iter().map(|s| {
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        match parts.len() {
            3 => ListItem {
                key: parts[0].to_string(),
                label: parts[1].to_string(),
                checked: parts[2] == "on",
            },
            2 => ListItem {
                key: parts[0].to_string(),
                label: parts[1].to_string(),
                checked: false,
            },
            _ => ListItem {
                key: s.clone(),
                label: s.clone(),
                checked: false,
            },
        }
    }).collect()
}
