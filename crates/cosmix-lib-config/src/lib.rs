//! cosmix-config — Shared settings library for the cosmix stack.
//!
//! Provides typed configuration structs and TOML load/save for
//! `~/.config/cosmix/settings.toml`. All cosmix apps import this crate
//! to read settings instead of hardcoding values.
//!
//! The companion `cosmix-configd` daemon serves these settings over AMP
//! so they're queryable/updatable from any mesh node.
//!
//! # Usage
//!
//! ```no_run
//! let cfg = cosmix_config::load().unwrap();
//! let hub_url = &cfg.hub.ws_url;
//! let refresh = cfg.mon.refresh_interval_secs;
//! ```

mod settings;
pub mod store;

pub use settings::*;
