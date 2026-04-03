//! cosmix-dialog — GUI dialog system for the Cosmix stack.
//!
//! Three modes of operation:
//! 1. **CLI** — `cosmix-dialog --msgbox "Hello"` → stdout + exit code (kdialog/zenity compatible)
//! 2. **Mix builtin** — `$name = dialog entry "Name?"` → direct value return
//! 3. **AMP service** — persistent daemon, mesh-addressable, live-updatable (Phase 3)
//!
//! See `_doc/2026-04-02-mix-dialog.md` for the full design.

pub mod types;
pub mod cli;
pub mod window;
pub mod render;
pub mod backend;

#[cfg(feature = "layer-shell")]
pub mod layer;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub use types::DialogKind;

// ── Request / Response ───────────────────────────────────────────────────

/// What the caller sends — describes which dialog to show and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DialogRequest {
    /// Dialog type and parameters.
    pub kind: DialogKind,
    /// Window title (auto-generated from kind if None).
    #[serde(default)]
    pub title: Option<String>,
    /// Window dimensions (width, height). Auto-sized if None.
    #[serde(default)]
    pub size: Option<(u32, u32)>,
    /// Timeout in seconds (0 = no timeout).
    #[serde(default)]
    pub timeout: u32,
    /// Whether to output JSON instead of plain text.
    #[serde(default)]
    pub json_output: bool,
}

impl DialogRequest {
    /// Get the effective window title, falling back to a sensible default.
    pub fn effective_title(&self) -> &str {
        self.title.as_deref().unwrap_or_else(|| match &self.kind {
            DialogKind::Message { level, .. } => match level {
                types::MessageLevel::Info => "Info",
                types::MessageLevel::Warning => "Warning",
                types::MessageLevel::Error => "Error",
            },
            DialogKind::Question { .. } => "Question",
            DialogKind::Entry { .. } | DialogKind::Password { .. } => "Input",
            DialogKind::TextInput { .. } => "Text Input",
            DialogKind::ComboBox { .. } => "Select",
            DialogKind::CheckList { .. } => "Check List",
            DialogKind::RadioList { .. } => "Radio List",
            DialogKind::FileOpen { .. } => "Open File",
            DialogKind::FileSave { .. } => "Save File",
            DialogKind::DirectorySelect { .. } => "Select Directory",
            DialogKind::Progress { .. } => "Progress",
            DialogKind::Form { .. } => "Form",
            DialogKind::TextViewer { .. } => "Text",
            DialogKind::Scale { .. } => "Scale",
            DialogKind::Calendar { .. } => "Calendar",
            DialogKind::Notification { .. } => "Notification",
        })
    }

    /// Get the default window size for this dialog kind.
    ///
    /// Sizes below 240px are achievable via the layer-shell backend.
    /// The Dioxus backend will be clamped to 240px by cosmic-comp.
    pub fn default_size(&self) -> (u32, u32) {
        self.size.unwrap_or_else(|| match &self.kind {
            DialogKind::Message { detail: Some(_), .. } => (420, 200),
            DialogKind::Message { .. } => (340, 120),
            DialogKind::Question { cancel: true, .. } => (400, 130),
            DialogKind::Question { .. } => (360, 130),
            DialogKind::Entry { .. } => (360, 140),
            DialogKind::Password { .. } => (360, 130),
            DialogKind::TextInput { .. } | DialogKind::TextViewer { .. } => (560, 360),
            DialogKind::ComboBox { .. } => (380, 150),
            DialogKind::CheckList { items, .. } | DialogKind::RadioList { items, .. } => {
                // header=28+28 + items*28 + footer=40
                let h = 96 + (items.len() as u32 * 28).min(280);
                (420, h)
            }
            DialogKind::Form { fields, .. } => {
                let h = 100 + (fields.len() as u32 * 44).min(440);
                (520, h)
            }
            DialogKind::Progress { .. } => (420, 100),
            DialogKind::Scale { .. } => (420, 180),
            DialogKind::Calendar { .. } => (360, 400),
            DialogKind::Notification { .. } => (320, 80),
            _ => (420, 200),
        })
    }
}

/// What comes back — the user's decision and any returned data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DialogResult {
    /// The user's decision (Ok, Cancel, Yes, No, etc.)
    pub action: DialogAction,
    /// Returned data (structured, not flat strings).
    pub data: DialogData,
    /// AMP return code: 0=ok, 1=cancel, 5=timeout, 10=error.
    pub rc: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DialogAction {
    Ok,
    Cancel,
    Yes,
    No,
    Custom(String),
    Timeout,
    Error(String),
}

/// Structured return data — the key improvement over flat strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DialogData {
    None,
    Text(String),
    Number(f64),
    Bool(bool),
    FilePath(PathBuf),
    FilePaths(Vec<PathBuf>),
    Color(String),
    Date(String),
    Selection(Vec<String>),
    Form(BTreeMap<String, String>),
}

impl DialogResult {
    /// Format the result for stdout output.
    pub fn to_stdout(&self, json: bool) -> String {
        if json {
            serde_json::to_string(self).unwrap_or_default()
        } else {
            match &self.data {
                DialogData::None => String::new(),
                DialogData::Text(s) => s.clone(),
                DialogData::Number(n) => n.to_string(),
                DialogData::Bool(b) => b.to_string(),
                DialogData::FilePath(p) => p.display().to_string(),
                DialogData::FilePaths(ps) => ps.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
                DialogData::Color(c) => c.clone(),
                DialogData::Date(d) => d.clone(),
                DialogData::Selection(items) => items.join("\n"),
                DialogData::Form(map) => map.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }
        }
    }
}
