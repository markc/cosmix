//! ced, the CosMix Editor (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md`):
//! an iced desktop editor over the `edit` Bus service. The buffer model —
//! mirror, editor model, highlighting, diagnostics — lives in the iced-free
//! `cosmix-edit-client` crate; this crate holds the controller (tabs, actions,
//! `ced.*` verbs), the Bus transport, the editor widget and the chrome.
//!
//! Stage S froze: [`actions`], [`keymap`], [`verbs`] (+ fixtures), [`dirs`],
//! [`session`]'s format, [`config`]'s keys, the controller / bus / editor
//! signatures, and a compiling editor-widget stub.

pub mod actions;
pub mod app;
pub mod bus;
pub mod chrome;
pub mod config;
pub mod controller;
pub mod dirs;
pub mod editor;
pub mod headless;
pub mod keymap;
pub mod keys;
pub mod lint;
pub mod macros;
pub mod session;
pub mod theme;
pub mod verbs;
