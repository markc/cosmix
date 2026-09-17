//! The iced twin (feature `iced`): the raw grid under iced chrome, with menu
//! panels as `xdg_popup`s. Glue is split by concern so it can be counted:
//! `keys` (key conversion), `damage`, `clipboard`, `ime`, `popups` and `app`
//! (event loop, redraw scheduling, cursor). `chrome` is the iced program for
//! the window band; the menu panels are `cosmix-iced-widgets` panels on
//! their own popup surfaces, driven from `popups`.
//!
//! Menu keys follow `cosmix-iced-widgets`: with the menus closed, F10 reaches
//! the bar widget, which opens the first root; with them open, routing is
//! modal and every key goes to `Navigator::key`, which has no F10 arm, so F10
//! is a no-op there. The widget crate's bar only ever opens on F10 — it has
//! no toggle-closed path — so this matches it rather than diverging. Escape
//! closes one level, or the menu.

pub mod app;
pub mod chrome;
pub mod clipboard;
pub mod damage;
pub mod ime;
pub mod keys;
pub mod popups;

pub use app::IcedDemo;
