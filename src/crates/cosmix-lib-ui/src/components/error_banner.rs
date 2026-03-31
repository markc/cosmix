//! Dismissible error banner component.

use dioxus::prelude::*;

/// A full-width error banner that appears when `message` is `Some`.
///
/// Uses semantic danger colors from the theme. Renders nothing when message is None.
#[component]
pub fn ErrorBanner(message: Option<String>) -> Element {
    if let Some(msg) = message {
        rsx! {
            div {
                style: "padding:8px 16px; background:var(--danger); color:var(--bg-primary); font-size:var(--font-size-sm); flex-shrink:0;",
                "{msg}"
            }
        }
    } else {
        rsx! {}
    }
}
