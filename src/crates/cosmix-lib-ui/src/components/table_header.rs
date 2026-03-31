//! Grid-based table header component.
//!
//! **Deprecated:** Prefer [`super::DataTable`] for new code — it includes
//! sortable headers, row rendering, selection, and pagination.

use dioxus::prelude::*;

/// A single column definition for the table header.
#[derive(Clone, PartialEq)]
pub struct Column {
    pub label: &'static str,
    pub style: &'static str,
}

/// A grid-based table header row with uppercase muted labels.
///
/// Pass a `grid_template` CSS value (e.g. `"1fr 80px 130px"`) and column definitions.
#[component]
pub fn TableHeader(grid_template: String, columns: Vec<Column>) -> Element {
    rsx! {
        div {
            style: "display:grid; grid-template-columns:{grid_template}; padding:6px 12px; background:var(--bg-secondary); border-bottom:1px solid var(--border); font-size:var(--font-size-sm); color:var(--fg-muted); text-transform:uppercase; letter-spacing:0.05em;",
            for col in columns.iter() {
                span { style: "{col.style}", "{col.label}" }
            }
        }
    }
}
