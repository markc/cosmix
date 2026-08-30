//! Shared integration-test support.
//!
//! The fixture writer lives in `src/fixture.rs` and is included verbatim so
//! unit and integration tests share ONE definition with no public API and no
//! Cargo manifest edit (versioning is the refinery's job at landing).

// A test binary includes the whole file but may use only part of it.
#![allow(dead_code)]

include!("../../src/fixture.rs");
