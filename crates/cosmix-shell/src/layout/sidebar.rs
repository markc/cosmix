use dioxus::prelude::*;
use crate::{LEFT_PANEL, RIGHT_PANEL, LEFT_PANELS, RIGHT_PANELS};
use crate::layout::carousel::Carousel;
use crate::panels::{launcher_panel, files_panel, navigator_panel, monitor_panel, settings_panel, notifications_panel};

#[component]
pub fn Sidebar(side: String) -> Element {
    let is_left = side == "left";
    let border = if is_left {
        "border-right:1px solid var(--border);"
    } else {
        "border-left:1px solid var(--border);"
    };

    if is_left {
        let panels: Vec<String> = LEFT_PANELS.iter().map(|s| s.to_string()).collect();
        let active_idx = *LEFT_PANEL.read();

        let panel_content = match active_idx {
            0 => launcher_panel(),
            1 => files_panel(),
            2 => navigator_panel(),
            _ => launcher_panel(),
        };

        rsx! {
            div {
                style: "background:var(--bg-secondary); {border} overflow:hidden; display:flex; flex-direction:column;",
                Carousel {
                    panels: panels,
                    active: active_idx,
                    on_navigate: move |idx| { *LEFT_PANEL.write() = idx; },
                    {panel_content}
                }
            }
        }
    } else {
        let panels: Vec<String> = RIGHT_PANELS.iter().map(|s| s.to_string()).collect();
        let active_idx = *RIGHT_PANEL.read();

        let panel_content = match active_idx {
            0 => monitor_panel(),
            1 => settings_panel(),
            2 => notifications_panel(),
            _ => monitor_panel(),
        };

        rsx! {
            div {
                style: "background:var(--bg-secondary); {border} overflow:hidden; display:flex; flex-direction:column;",
                Carousel {
                    panels: panels,
                    active: active_idx,
                    on_navigate: move |idx| { *RIGHT_PANEL.write() = idx; },
                    {panel_content}
                }
            }
        }
    }
}
