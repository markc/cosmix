//! The iced twin (feature `iced`): the raw grid under iced chrome, with menu
//! panels as `xdg_popup`s. Glue is split by concern so it can be counted:
//! `keys` (key conversion), `damage`, `clipboard`, `ime`, `popups` and `app`
//! (event loop, redraw scheduling, cursor). `chrome` is the iced program for
//! the window band; the menu panels are `cosmix-iced-widgets` panels on
//! their own popup surfaces, driven from `popups`.

pub mod app;
pub mod chrome;
pub mod clipboard;
pub mod damage;
pub mod ime;
pub mod keys;
pub mod popups;

pub use app::IcedDemo;
