//! GTK CSS theme for layer-shell dialogs.
//!
//! Matches the cosmix dark theme palette used by Dioxus-rendered dialogs.
//! Uses concrete values (no CSS variables) since this is native GTK, not WebKitGTK.

use gtk::prelude::*;

/// Dark theme CSS matching cosmix alert-dialog styling.
///
/// The window itself is transparent (RGBA visual). The `.dialog-frame` container
/// provides the visible rounded rectangle with the dark background.
const DIALOG_CSS: &str = r#"
window {
    background-color: transparent;
    color: #f3f4f6;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 14px;
}

.dialog-frame {
    background-color: #030712;
    border-radius: 0.75rem;
    border: 1px solid rgba(128, 128, 128, 0.3);
}

.dialog-body {
    padding: 0.75rem 1rem;
}

.dialog-label {
    font-size: 0.9375rem;
    color: #f3f4f6;
}

.dialog-detail {
    font-size: 0.8125rem;
    color: #9ca3af;
    margin-top: 0.25rem;
}

.dialog-footer {
    padding: 0.5rem 1rem;
    background-color: #111827;
    border-top: 1px solid rgba(128, 128, 128, 0.25);
    border-radius: 0 0 0.75rem 0.75rem;
}

.btn-primary {
    padding: 0.375rem 1.25rem;
    border-radius: 0.375rem;
    background-color: #3b82f6;
    color: #ffffff;
    font-weight: 500;
    font-size: 0.875rem;
    border: none;
    min-width: 4.5rem;
}

.btn-primary:hover {
    background-color: #2563eb;
}

.btn-secondary {
    padding: 0.375rem 1.25rem;
    border-radius: 0.375rem;
    background-color: #374151;
    color: #e5e7eb;
    font-weight: 500;
    font-size: 0.875rem;
    border: none;
    min-width: 4.5rem;
}

.btn-secondary:hover {
    background-color: #4b5563;
}

.btn-danger {
    padding: 0.375rem 1.25rem;
    border-radius: 0.375rem;
    background-color: #dc2626;
    color: #ffffff;
    font-weight: 500;
    font-size: 0.875rem;
    border: none;
}

.btn-danger:hover {
    background-color: #b91c1c;
}

entry {
    background-color: #1f2937;
    color: #f3f4f6;
    border: 1px solid rgba(128, 128, 128, 0.4);
    border-radius: 0.375rem;
    padding: 0.375rem 0.5rem;
    font-size: 0.875rem;
    min-height: 1.75rem;
}

entry:focus {
    border-color: #3b82f6;
    outline: none;
}

combobox button {
    background-color: #1f2937;
    color: #f3f4f6;
    border: 1px solid rgba(128, 128, 128, 0.4);
    border-radius: 0.375rem;
    padding: 0.375rem 0.5rem;
    font-size: 0.875rem;
}

progressbar trough {
    background-color: #1f2937;
    border-radius: 0.25rem;
    min-height: 0.5rem;
}

progressbar progress {
    background-color: #3b82f6;
    border-radius: 0.25rem;
    min-height: 0.5rem;
}

.icon-info { color: #3b82f6; }
.icon-warning { color: #f59e0b; }
.icon-error { color: #ef4444; }
"#;

/// Load the dialog CSS into a GtkCssProvider and apply it screen-wide.
pub fn apply_theme() {
    let provider = gtk::CssProvider::new();
    provider
        .load_from_data(DIALOG_CSS.as_bytes())
        .expect("failed to load dialog CSS");
    gtk::StyleContext::add_provider_for_screen(
        &gdk::Screen::default().expect("no default screen"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
