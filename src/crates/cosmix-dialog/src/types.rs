//! Dialog type definitions, field specs, and supporting types.
//!
//! Covers all three tiers of the dialog system design.
//! Tier 1 (MVP) types are implemented; Tier 2/3 are defined for forward compatibility.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Dialog Kind ──────────────────────────────────────────────────────────

/// What kind of dialog to show. Each variant carries its type-specific parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DialogKind {
    // === Tier 1: MVP ===

    /// Info/Warning/Error message with OK button.
    Message {
        text: String,
        level: MessageLevel,
        #[serde(default)]
        detail: Option<String>,
    },
    /// Yes/No (optionally Cancel) question.
    Question {
        text: String,
        #[serde(default)]
        yes_label: Option<String>,
        #[serde(default)]
        no_label: Option<String>,
        #[serde(default)]
        cancel: bool,
    },
    /// Single-line text entry.
    Entry {
        text: String,
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
    },
    /// Password entry (masked input).
    Password { text: String },
    /// Multi-line text input.
    TextInput {
        text: String,
        #[serde(default)]
        default: Option<String>,
    },
    /// Dropdown selection.
    ComboBox {
        text: String,
        items: Vec<String>,
        #[serde(default)]
        default: Option<usize>,
        #[serde(default)]
        editable: bool,
    },
    /// Multi-select checklist.
    CheckList {
        text: String,
        items: Vec<ListItem>,
    },
    /// Single-select radio list.
    RadioList {
        text: String,
        items: Vec<ListItem>,
    },
    /// File open dialog (native).
    FileOpen {
        #[serde(default)]
        filters: Vec<FileFilter>,
        #[serde(default)]
        directory: Option<String>,
        #[serde(default)]
        multiple: bool,
    },
    /// File save dialog (native).
    FileSave {
        #[serde(default)]
        filters: Vec<FileFilter>,
        #[serde(default)]
        directory: Option<String>,
        #[serde(default)]
        default_name: Option<String>,
    },
    /// Directory selection (native).
    DirectorySelect {
        #[serde(default)]
        directory: Option<String>,
    },
    /// Progress bar (updated via stdin or AMP).
    Progress {
        text: String,
        #[serde(default)]
        pulsate: bool,
        #[serde(default)]
        auto_close: bool,
    },
    /// Multi-field structured form.
    Form {
        text: String,
        fields: Vec<FormField>,
    },
    /// Scrollable text viewer (from stdin, file, or inline).
    TextViewer {
        source: TextSource,
        #[serde(default)]
        checkbox: Option<String>,
    },

    // === Tier 2: Extended (defined, not yet rendered) ===

    /// Numeric slider/scale.
    Scale {
        text: String,
        min: f64,
        max: f64,
        step: f64,
        #[serde(default)]
        default: Option<f64>,
    },
    /// Calendar date picker.
    Calendar {
        text: String,
        #[serde(default)]
        default_date: Option<String>,
        #[serde(default)]
        format: Option<String>,
    },
    /// Notification toast (auto-dismiss).
    Notification {
        text: String,
        #[serde(default = "default_notification_timeout")]
        timeout: u32,
        #[serde(default)]
        icon: Option<String>,
    },
}

fn default_notification_timeout() -> u32 {
    5
}

// ── Supporting Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MessageLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub checked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileFilter {
    pub name: String,
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TextSource {
    Stdin(String),
    File(PathBuf),
    Inline(String),
}

// ── Form Field Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldKind {
    Text {
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
    },
    Password,
    Number {
        #[serde(default)]
        default: Option<f64>,
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default)]
        step: Option<f64>,
    },
    Toggle {
        #[serde(default)]
        default: bool,
    },
    Select {
        items: Vec<String>,
        #[serde(default)]
        default: Option<usize>,
    },
    TextArea {
        #[serde(default)]
        default: Option<String>,
        #[serde(default = "default_textarea_rows")]
        rows: usize,
    },
    Label {
        text: String,
    },
    Separator,
}

fn default_textarea_rows() -> usize {
    4
}
