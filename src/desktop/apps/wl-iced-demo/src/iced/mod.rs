//! The iced twin (feature `iced`): the raw grid under iced chrome, with menu
//! panels as `xdg_popup`s. Glue is split by concern so it can be counted:
//! `keys` (key conversion), `damage`, `clipboard`, `ime`, `popups` (plus the
//! pure `crate::menus`), and `app` (event loop, redraw scheduling, cursor).
//! `chrome` and `dropdown` are the iced programs themselves.

pub mod app;
pub mod chrome;
pub mod clipboard;
pub mod damage;
pub mod dropdown;
pub mod ime;
pub mod keys;
pub mod popups;

pub use app::IcedDemo;
