//! `cosmix-lib-dns` — P1 core of the authoritative WG-mesh DNS daemon.
//!
//! Pure logic, no Cosmix-internal Bus/mesh/config dependency. The whole
//! crate is testable under `cargo test -p cosmix-lib-dns
//! --no-default-features` (the core gate, identical to the default build
//! in P1 — there is no `cosmix` feature on the *library*; the
//! core/citizen split lives on the `cosmix-dnsd` *binary*).
//!
//! Two-layer data model (the load-bearing invariant — see `model`):
//!
//! * **Layer 1** (`model`): owner-aware source/candidate
//!   (`ParsedZones`/`ZoneSource`/`Bundle`/`RecordAssertion`). Consumed
//!   by the candidate path, **never served**.
//! * **Layer 2** (`model`): flattened answerable `ZoneSnapshot` — **no
//!   owner attribution anywhere**. The only thing the resolver sees.
//!
//! The resolver signature `resolve(&ZoneSnapshot, &Message) -> Message`
//! is the structural guard against hoisting owner data (or transport
//! size) into query time.

pub mod candidate;
pub mod canonical;
pub mod model;
pub mod persistence;
pub mod resolver;
pub mod rr;
pub mod serve;
pub mod snapshot;
pub mod store;
pub mod strict_data;
pub mod validate;
pub mod wire;

pub use candidate::{
    CandidateSnapshot, RejectReason, adopt_candidate, build_candidate, validate_candidate,
};
pub use canonical::Name;
pub use model::{
    Bundle, OwnerId, ParsedZones, RecordAssertion, RrsetKey, RrsetValue, Serial, SerialCmp,
    SoaConfig, ZoneAnswerState, ZoneName, ZoneSnapshot, ZoneSource,
};
pub use persistence::{
    FilePersistence, InMemoryPersistence, Persistence, PersistenceError, StateLoad,
};
pub use resolver::resolve;
pub use rr::{RData, RecordType};
pub use serve::{ResponseObserver, serve_tcp, serve_tcp_observed, serve_udp, serve_udp_observed};
pub use snapshot::SnapshotHash;
pub use store::{StaticZoneStore, StoreInitError, ZoneStore};
pub use strict_data::{ZonesParseError, parse_zones, parse_zones_file};
pub use wire::{decode, encode_tcp, encode_udp};
