//! Reusable sortable data table component.
//!
//! Renders a CSS-grid table from `Vec<serde_json::Value>` rows with sortable
//! column headers, row selection, and optional pagination.
//!
//! ```ignore
//! DataTable {
//!     columns: vec![
//!         DataColumn { key: "name", label: "Name", width: "1fr", sortable: true, format: None },
//!         DataColumn { key: "size", label: "Size", width: "80px", sortable: true, format: Some(fmt_size) },
//!     ],
//!     rows: data,
//!     on_row_click: move |idx| selected.set(Some(idx)),
//!     selected: selected(),
//! }
//! ```

use std::cmp::Ordering;

use dioxus::prelude::*;
use serde_json::Value;

// ── Types ──

/// Sort direction for a column.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum SortDir {
    #[default]
    None,
    Asc,
    Desc,
}

/// Column definition for DataTable.
#[derive(Clone)]
pub struct DataColumn {
    /// Key into each row's JSON object.
    pub key: &'static str,
    /// Display label in the header.
    pub label: &'static str,
    /// CSS grid column width (e.g. "1fr", "80px", "120px").
    pub width: &'static str,
    /// Whether clicking the header sorts by this column.
    pub sortable: bool,
    /// Optional cell formatter. Receives the cell value, returns display text.
    /// If `None`, uses default formatting (strips quotes from strings, clean numbers).
    pub format: Option<fn(&Value) -> String>,
}

impl PartialEq for DataColumn {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.label == other.label
            && self.width == other.width
            && self.sortable == other.sortable
            // Compare fn pointers by address (intentional)
            && self.format.map(|f| f as usize) == other.format.map(|f| f as usize)
    }
}

// ── Component ──

/// A sortable, selectable data table backed by JSON rows.
#[component]
pub fn DataTable(
    /// Column definitions.
    columns: Vec<DataColumn>,
    /// Row data — each Value must be a JSON object.
    rows: Vec<Value>,
    /// Called when a row is clicked. Receives the row index in the sorted/paged data.
    #[props(default)]
    on_row_click: EventHandler<usize>,
    /// Index of the currently selected row (controlled from parent).
    #[props(default)]
    selected: Option<usize>,
    /// Rows per page. 0 = show all rows (no pagination).
    #[props(default)]
    page_size: usize,
) -> Element {
    let mut sort_key: Signal<Option<&'static str>> = use_signal(|| None);
    let mut sort_dir: Signal<SortDir> = use_signal(|| SortDir::None);
    let mut page: Signal<usize> = use_signal(|| 0);

    // Reset page when row count changes
    let row_count = rows.len();
    use_effect(move || {
        let _ = row_count;
        page.set(0);
    });

    // Build grid template from column widths
    let grid_template: String = columns.iter().map(|c| c.width).collect::<Vec<_>>().join(" ");

    // Sort rows
    let sorted_rows = {
        let mut r = rows;
        if let Some(key) = sort_key() {
            let dir = sort_dir();
            if dir != SortDir::None {
                r.sort_by(|a, b| {
                    let va = a.get(key).unwrap_or(&Value::Null);
                    let vb = b.get(key).unwrap_or(&Value::Null);
                    let ord = cmp_json(va, vb);
                    if dir == SortDir::Desc { ord.reverse() } else { ord }
                });
            }
        }
        r
    };

    // Pagination
    let total_pages = if page_size > 0 {
        (sorted_rows.len() + page_size - 1).max(1) / page_size.max(1)
    } else {
        1
    };
    let current_page = page().min(total_pages.saturating_sub(1));
    let visible_rows: &[Value] = if page_size > 0 {
        let start = current_page * page_size;
        let end = (start + page_size).min(sorted_rows.len());
        &sorted_rows[start..end]
    } else {
        &sorted_rows
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;background:var(--bg-primary);border:1px solid var(--border);border-radius:var(--radius-md);overflow:hidden;",

            // Header row
            div {
                style: "display:grid;grid-template-columns:{grid_template};padding:6px 12px;background:var(--bg-secondary);border-bottom:1px solid var(--border);font-size:var(--font-size-sm);color:var(--fg-muted);text-transform:uppercase;letter-spacing:0.05em;user-select:none;",
                for col in columns.iter() {
                    {
                        let key = col.key;
                        let is_sorted = sort_key() == Some(key);
                        let indicator = if is_sorted {
                            match sort_dir() {
                                SortDir::Asc => " \u{25B2}",
                                SortDir::Desc => " \u{25BC}",
                                SortDir::None => "",
                            }
                        } else {
                            ""
                        };
                        let cursor = if col.sortable { "pointer" } else { "default" };
                        let sortable = col.sortable;

                        rsx! {
                            span {
                                style: "cursor:{cursor};white-space:nowrap;overflow:hidden;text-overflow:ellipsis;",
                                onclick: move |_| {
                                    if !sortable { return; }
                                    if sort_key() == Some(key) {
                                        // Cycle: Asc → Desc → None
                                        match sort_dir() {
                                            SortDir::None => sort_dir.set(SortDir::Asc),
                                            SortDir::Asc => sort_dir.set(SortDir::Desc),
                                            SortDir::Desc => {
                                                sort_dir.set(SortDir::None);
                                                sort_key.set(None);
                                            }
                                        }
                                    } else {
                                        sort_key.set(Some(key));
                                        sort_dir.set(SortDir::Asc);
                                    }
                                    page.set(0);
                                },
                                "{col.label}{indicator}"
                            }
                        }
                    }
                }
            }

            // Data rows
            for (idx, row) in visible_rows.iter().enumerate() {
                {
                    let is_selected = selected == Some(idx);
                    let bg = if is_selected {
                        "var(--accent-subtle)"
                    } else if idx % 2 == 1 {
                        "var(--bg-secondary)"
                    } else {
                        "var(--bg-primary)"
                    };

                    rsx! {
                        div {
                            style: "display:grid;grid-template-columns:{grid_template};padding:4px 12px;background:{bg};border-bottom:1px solid var(--border-muted, var(--border));font-size:var(--font-size-sm);cursor:pointer;",
                            onclick: move |_| on_row_click.call(idx),
                            for col in columns.iter() {
                                {
                                    let val = row.get(col.key).unwrap_or(&Value::Null);
                                    let text = if let Some(fmt) = col.format {
                                        fmt(val)
                                    } else {
                                        default_display(val)
                                    };

                                    rsx! {
                                        span {
                                            style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--fg-primary);",
                                            "{text}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Empty state
            if visible_rows.is_empty() {
                div {
                    style: "padding:16px;text-align:center;color:var(--fg-muted);font-size:var(--font-size-sm);",
                    "No data"
                }
            }

            // Pagination footer
            if page_size > 0 && total_pages > 1 {
                div {
                    style: "display:flex;justify-content:center;align-items:center;gap:12px;padding:6px 12px;background:var(--bg-secondary);border-top:1px solid var(--border);font-size:var(--font-size-sm);color:var(--fg-muted);",
                    button {
                        style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-secondary);padding:2px 10px;border-radius:var(--radius-sm);cursor:pointer;font-size:var(--font-size-sm);",
                        disabled: current_page == 0,
                        onclick: move |_| page.set(current_page.saturating_sub(1)),
                        "Prev"
                    }
                    span { "{current_page + 1} / {total_pages}" }
                    button {
                        style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-secondary);padding:2px 10px;border-radius:var(--radius-sm);cursor:pointer;font-size:var(--font-size-sm);",
                        disabled: current_page + 1 >= total_pages,
                        onclick: move |_| page.set((current_page + 1).min(total_pages.saturating_sub(1))),
                        "Next"
                    }
                }
            }
        }
    }
}

// ── Helpers ──

/// Default display for a JSON value: strip quotes from strings, clean numbers.
fn default_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                // Show integers without decimal point
                if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                    format!("{}", f as i64)
                } else {
                    format!("{f}")
                }
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Compare two JSON values for sorting.
fn cmp_json(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let fa = a.as_f64().unwrap_or(0.0);
            let fb = b.as_f64().unwrap_or(0.0);
            fa.partial_cmp(&fb).unwrap_or(Ordering::Equal)
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        // Nulls sort last
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        // Mixed types: compare as strings
        _ => a.to_string().cmp(&b.to_string()),
    }
}
