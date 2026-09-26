//! Pure store core: reference wire shape, config, mime hints, and the
//! store itself (mds CAS + blobd-owned `blobd.sqlite` + root `flock`).
//!
//! No async runtime, Bus, mesh, or daemon plumbing here.

pub mod config;
pub mod mime;
pub mod reference;
pub mod store;

pub use config::{
    ByteSize, Config, DEFAULT_LANE_MAX_UPLOADS, DEFAULT_QUOTA_OWNER_BYTES,
    DEFAULT_QUOTA_TOTAL_BYTES,
};
pub use reference::{Reference, blob_id, parse_blob_id};
pub use store::{
    DEFAULT_GC_GRACE_SECS, GcSweep, LOCK_FILE, ListEntry, PutOptions, PutOutcome, QuotaReport,
    StartupReport, StatInfo, Store, StoreError, StoreOptions,
};
