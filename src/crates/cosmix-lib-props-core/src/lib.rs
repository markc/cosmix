//! Pure-type core of the SPEC 07 property surface.
//!
//! This crate carries `PropPath`, `PropValue`, `PropTree`,
//! `PropDescribe`, `redact`, `diff` — the read-side types every
//! `<svc>.props.*` consumer needs without taking on the substrate
//! machinery (NamespaceSpec, hooks, runtime, storage backends, audit
//! HMAC, SPEC 12 mutation router) that lives in the sibling
//! `cosmix-lib-props-store` crate (cos repo).
//!
//! The default feature set ships only the pure-type surface. Enable
//! the opt-in `bus` feature to pull `cosmix-lib-bus` + `chrono` and
//! gain the `bus::dispatch_props` (SPEC 07 §2 read wire) and
//! `publish::*` (SPEC 07 §3/§4 wire builders) modules.
//!
//! `revwrite` adds an **opt-in** lightweight in-memory revisioned write
//! store (Q8) — a global monotonic revision, per-path canonical
//! `{value,revision,source_id,op_id}`, `if_revision` optimistic
//! concurrency, per-path coalescing, and a guaranteed terminal own-op
//! echo — for daemons that accept control writes and publish
//! `props.changed`. It is generic (no domain knowledge) and independent
//! of the `bus` feature; read-only `PropTree` consumers are unaffected.

pub mod describe;
pub mod diff;
pub mod path;
pub mod redact;
pub mod revwrite;
pub mod tree;
pub mod value;

#[cfg(feature = "bus")]
pub mod bus;
#[cfg(feature = "bus")]
pub mod publish;

pub use describe::{PropDescribe, PropType};
pub use diff::diff;
pub use path::{PropPath, PropPathError};
pub use redact::redact;
pub use revwrite::{
    ChangedProp, RevWriteAck, RevWriteReject, RevWriteRequest, RevWriteResponse, RevWriteStore,
    accept_if_newer,
};
pub use tree::PropTree;
pub use value::PropValue;
