use dioxus::prelude::*;

/// Navigator panel — bookmarks and quick-jump tree (placeholder).
pub fn navigator_panel() -> Element {
    rsx! {
        div {
            style: "padding:16px;",
            div {
                style: "font-weight:600; font-size:var(--font-size); color:var(--fg-primary); margin-bottom:12px;",
                "Navigator"
            }
            div {
                style: "font-size:var(--font-size-sm); color:var(--fg-muted); margin-bottom:16px;",
                "Bookmarks and quick-jump tree"
            }
            // Placeholder bookmark items
            for label in ["Home", "Documents", "Projects", "Downloads"].iter() {
                div {
                    style: "padding:6px 8px; cursor:pointer; border-radius:var(--radius-sm); color:var(--fg-secondary); font-size:var(--font-size-sm);",
                    "{label}"
                }
            }
        }
    }
}
