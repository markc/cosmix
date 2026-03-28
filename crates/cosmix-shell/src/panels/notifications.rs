use dioxus::prelude::*;

/// Notifications panel — hub event feed (placeholder).
pub fn notifications_panel() -> Element {
    rsx! {
        div {
            style: "padding:16px;",
            div {
                style: "font-weight:600; font-size:var(--font-size); color:var(--fg-primary); margin-bottom:12px;",
                "Notifications"
            }
            div {
                style: "font-size:var(--font-size-sm); color:var(--fg-muted);",
                "No new notifications"
            }
        }
    }
}
