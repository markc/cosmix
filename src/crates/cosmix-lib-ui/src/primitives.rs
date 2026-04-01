//! Re-export dioxus-primitives with cosmix presets.
//!
//! Apps use `cosmix_ui::primitives::*` instead of importing dioxus-primitives
//! directly. This provides a stable API surface even if the upstream crate
//! reorganises its modules.

pub use dioxus_primitives::{
    accordion, alert_dialog, checkbox, collapsible, context_menu, dialog,
    dropdown_menu, hover_card, label, popover, progress, radio_group,
    scroll_area, select, separator, slider, switch, tabs, toast, toggle,
    toggle_group, toolbar, tooltip, virtual_list,
};
