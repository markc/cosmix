//! cosmix-config — Shared settings library for the cosmix stack.
//!
//! Each service owns a typed `*Settings` struct and a dedicated
//! `.conf.mix` file at `~/.config/cosmix/<service>.conf.mix`. Load one
//! via [`store::load_service`] — a missing file is materialised with
//! `Default::default()` so the on-disk file always mirrors live config.
//!
//! # Usage
//!
//! ```no_run
//! let skills = cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
//!     .unwrap_or_default();
//! let min_conf = skills.min_confidence;
//!
//! let domains = cosmix_config::store::load_service::<cosmix_config::DomainsSettings>("domains")
//!     .unwrap_or_default();
//! let first = domains.map.values().next();
//! ```

pub mod acme_policy;
// `client_helpers` wraps `cosmix_lib_client::NodedClient`'s native
// async constructors. The web variant of lib-client has its own
// self-contained `noded_url_from_origin()` resolver and doesn't need
// this module — so it's native-only by both target and feature.
#[cfg(all(feature = "client-helpers", not(target_arch = "wasm32")))]
pub mod client_helpers;
pub mod mix_data;
pub mod node;
pub mod paths;
mod settings;
pub mod store;

pub use acme_policy::{
    AcmeResolveError, AcmeTosAcceptance, AcmeTosGateError, AcmeVhostPlan, ResolvedWebdAcme,
    resolve_webd_acme,
};
pub use mix_data::{MixError, MixResult, Value, load_mix_data, parse_mix_data};
// The serde `.conf.mix` bridge, re-exported so cos crates deserialize /
// serialize typed config without each re-declaring the cosmix-lib-mix
// dep + `serde` feature. See
// `_doc/planned/2026-05-29-conf-mix-config-migration.md`.
pub use cosmix_mix::{from_conf_mix_str, to_conf_mix_string};
pub use paths::{CosmixDir, cosmix_path, current_uid};
pub use settings::*;
pub use store::{cosmix_src, load_conf_mix_path};
