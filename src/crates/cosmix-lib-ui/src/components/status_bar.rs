//! Bottom status bar component.

use dioxus::prelude::*;

/// A thin status bar at the bottom of the window.
///
/// Renders children in a single row with secondary background and muted text.
/// Typically shows item counts, connection status, or other brief info.
#[component]
pub fn StatusBar(children: Element) -> Element {
    rsx! {
        div {
            style: "padding:4px 12px; background:var(--bg-secondary); border-top:1px solid var(--border); color:var(--fg-muted); font-size:var(--font-size-sm); flex-shrink:0; display:flex; align-items:center; justify-content:space-between;",
            {children}
        }
    }
}
