//! Third-party code vendored into cosmix-edit-core. See `msedit/README.md`
//! for provenance and the patch log.

// Upstream allows these; scoped here so the crate's own code keeps the full lint set.
#[allow(
    dead_code,
    unused_imports,
    clippy::missing_transmute_annotations,
    clippy::new_without_default,
    clippy::missing_safety_doc,
    clippy::len_without_is_empty,
    stable_features
)]
pub(crate) mod msedit {
    pub mod document;
    pub mod gap_buffer;
    pub mod helpers;
    pub mod simd;
    pub mod stdext {
        pub mod helpers;
        pub mod sys_unix;
    }
}
