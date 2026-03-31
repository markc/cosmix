mod launcher;
mod navigator;
mod notifications;

pub use launcher::launcher_panel;
pub use navigator::navigator_panel;
pub use notifications::notifications_panel;

use dioxus::prelude::*;

/// Placeholder: files panel (will be replaced by cosmix_files::files_panel() when absorbed).
pub fn files_panel() -> Element {
    placeholder_panel("Files", "File browser — will absorb cosmix-files component")
}

/// Placeholder: monitor panel (will be replaced by cosmix_mon::mon_panel() when absorbed).
pub fn monitor_panel() -> Element {
    placeholder_panel("Monitor", "System monitor — will absorb cosmix-mon component")
}

/// Placeholder: settings panel (will be replaced by cosmix_settings::settings_panel() when absorbed).
pub fn settings_panel() -> Element {
    placeholder_panel("Settings", "Settings editor — will absorb cosmix-settings component")
}

fn placeholder_panel(title: &str, description: &str) -> Element {
    rsx! {
        div {
            style: "padding:16px;",
            div {
                style: "font-weight:600; font-size:var(--font-size); color:var(--fg-primary); margin-bottom:8px;",
                "{title}"
            }
            div {
                style: "font-size:var(--font-size-sm); color:var(--fg-muted);",
                "{description}"
            }
        }
    }
}
