//! Shared UI components for cosmix apps.

mod status_bar;
mod error_banner;
mod table_header;
mod data_table;
pub mod ui_registry;

pub use status_bar::StatusBar;
pub use error_banner::ErrorBanner;
pub use table_header::TableHeader;
pub use data_table::{DataTable, DataColumn, SortDir};
pub use ui_registry::{
    AmpButton, AmpToggle, AmpInput,
    UiRegistry, UiCommand, UiElement, ElementKind, ElementState,
    UI_CMD, UI_REGISTRY,
};
