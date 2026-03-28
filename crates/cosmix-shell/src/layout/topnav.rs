use dioxus::prelude::*;
use crate::{LEFT_PINNED, RIGHT_PINNED};

#[component]
pub fn TopNav() -> Element {
    rsx! {
        div {
            style: "height:40px; background:var(--bg-secondary); border-bottom:1px solid var(--border); display:flex; align-items:center; padding:0 12px; gap:8px; flex-shrink:0;",

            // Left sidebar toggle
            button {
                style: "background:none; border:1px solid var(--border); color:var(--fg-secondary); padding:4px 8px; border-radius:var(--radius-sm); cursor:pointer; font-size:var(--font-size-sm);",
                onclick: move |_| {
                    let current = *LEFT_PINNED.read();
                    *LEFT_PINNED.write() = !current;
                },
                dangerous_inner_html: "{ICON_MENU}"
            }

            // Title
            span {
                style: "font-weight:600; font-size:var(--font-size); color:var(--fg-primary); margin-left:4px;",
                "cosmix-shell"
            }

            // Spacer
            div { style: "flex:1;" }

            // Right sidebar toggle
            button {
                style: "background:none; border:1px solid var(--border); color:var(--fg-secondary); padding:4px 8px; border-radius:var(--radius-sm); cursor:pointer; font-size:var(--font-size-sm);",
                onclick: move |_| {
                    let current = *RIGHT_PINNED.read();
                    *RIGHT_PINNED.write() = !current;
                },
                dangerous_inner_html: "{ICON_MENU}"
            }
        }
    }
}

const ICON_MENU: &str = r#"<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="3" y1="6" x2="21" y2="6"/><line x1="3" y1="12" x2="21" y2="12"/><line x1="3" y1="18" x2="21" y2="18"/></svg>"#;
