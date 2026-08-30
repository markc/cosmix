//! TLS primitives shared by webd, maild, and any future Cosmix
//! daemon that terminates TLS on a public-facing or mesh-facing
//! socket. Gated behind the existing `tls` Cargo feature.
//!
//! The pre-existing crate-root `load_tls_config` (a single-cert
//! helper) lives at the crate root and is unchanged; this module
//! houses the multi-identity SNI surface plus the Let's-Encrypt
//! chain validator that webd Phase 1 introduced. Phase 2 extends
//! the validator to accept LE staging chains (alongside
//! production) via a new entry point and adds a vendored
//! staging-root DER set.

pub mod le_intermediates;
pub mod le_staging_roots;
pub mod le_validator;
pub mod sni;

pub use le_intermediates::{
    Environment, IntermediatePin, IntermediateState, LE_INTERMEDIATE_PINS,
    LE_STAGING_INTERMEDIATE_PINS,
};
pub use le_staging_roots::{
    FAKE_LE_ROOT_X1_DER, FAKE_LE_ROOT_X2_DER, FAKE_LE_ROOT_YE_DER, FAKE_LE_ROOT_YR_DER,
    LE_STAGING_TRUST_ROOTS,
};
pub use le_validator::{ChainEnvironment, validate_le_chain, validate_le_chain_for_environment};
pub use sni::{SniCertResolver, TlsIdentity, pick_identity};
