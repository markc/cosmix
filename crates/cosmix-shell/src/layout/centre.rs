use dioxus::prelude::*;

#[component]
pub fn CentrePanel() -> Element {
    rsx! {
        div {
            style: "background:var(--bg-primary); overflow-y:auto; display:flex; align-items:center; justify-content:center;",
            div {
                style: "text-align:center; color:var(--fg-muted);",
                div {
                    style: "font-size:var(--font-size-lg); margin-bottom:8px;",
                    "cosmix-shell"
                }
                div {
                    style: "font-size:var(--font-size-sm);",
                    "Select a panel from the sidebars, or open an app from the Launcher."
                }
            }
        }
    }
}
