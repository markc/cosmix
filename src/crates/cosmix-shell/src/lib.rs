//! cosmix-shell — DCS (Dual Carousel Sidebar) shell for the cosmix stack.
//!
//! Three-column layout: left carousel | centre panel | right carousel.
//! Absorbs cosmix apps as embedded panels; floating windows for task-focused tools.

pub mod layout;
pub mod panels;

use dioxus::prelude::*;
pub use cosmix_ui::app_init::THEME;
use cosmix_ui::app_init::use_theme_css;

use layout::topnav::TopNav;
use layout::sidebar::Sidebar;
use layout::centre::CentrePanel;

/// Which left panel is active.
pub static LEFT_PANEL: GlobalSignal<usize> = Signal::global(|| 0);
/// Which right panel is active.
pub static RIGHT_PANEL: GlobalSignal<usize> = Signal::global(|| 0);
/// Left sidebar pinned state.
pub static LEFT_PINNED: GlobalSignal<bool> = Signal::global(|| true);
/// Right sidebar pinned state.
pub static RIGHT_PINNED: GlobalSignal<bool> = Signal::global(|| true);

/// Left panel definitions.
pub const LEFT_PANELS: &[&str] = &["Launcher", "Files", "Navigator"];
/// Right panel definitions.
pub const RIGHT_PANELS: &[&str] = &["Monitor", "Settings", "Notifications"];

/// The main shell component.
pub fn shell_app() -> Element {
    use_theme_css();

    let left_pinned = *LEFT_PINNED.read();
    let right_pinned = *RIGHT_PINNED.read();

    let grid_cols = match (left_pinned, right_pinned) {
        (true, true) => "280px 1fr 280px",
        (true, false) => "280px 1fr",
        (false, true) => "1fr 280px",
        (false, false) => "1fr",
    };

    rsx! {
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-family:var(--font-sans);",

            TopNav {}

            div {
                style: "flex:1; display:grid; grid-template-columns:{grid_cols}; overflow:hidden;",

                if left_pinned {
                    Sidebar { side: "left" }
                }

                CentrePanel {}

                if right_pinned {
                    Sidebar { side: "right" }
                }
            }
        }
    }
}
