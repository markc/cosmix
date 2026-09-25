//! The `edit` Bus citizen (ced E0). Owns text buffers for humans and agents
//! alike; every buffer is a `cosmix-edit-core` `Buffer` inside its own actor.
//!
//! Design and frozen contracts: cmctl `_plan/2026-09-26-ced-e0-implementation.md`.
//! Where each contract lives: refusal precedence + path/byte reservations →
//! [`router`]; per-buffer ordering + op_id dedup → [`actor`]; attested origin
//! derivation → [`caller`]; load/save/identities → [`files`]; watch
//! re-registration → [`watch`]; publisher loss/resync → [`events`]; props tree
//! → [`props`]; daemon limits → [`limits`].
//!
//! **E0 buffers are volatile**: unsaved text is lost on any daemon stop,
//! crash or heap-OOM abort. `edit.info` reports `volatile: true`.
//!
//! In-process use (tests): [`router::Editd::start`] with any
//! [`events::EventSink`], then [`router::Editd::handle`] synthesized
//! `IncomingCommand`s — no broker needed.

pub mod actor;
pub mod bus;
pub mod caller;
pub mod events;
pub mod files;
pub mod limits;
pub mod props;
pub mod refusal;
pub mod router;
pub mod watch;

mod readiness;

pub use router::{Config, Editd};
