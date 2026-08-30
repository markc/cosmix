//! Cross-mesh trust + grant verification machinery.
//!
//! See `_doc/planned/cosmix-cross-mesh-authz.md` for the design.
//!
//! Crate split (CLAUDE.md "core-first, citizen-on-feature"):
//!
//! - **core** (`--no-default-features`) — envelope parse, the shared
//!   v1 [`canonical`] serialiser, Ed25519 signature verify, the SPEC 13
//!   signed-inventory types + [`inventory::SignedInventory::verify`]
//!   (trust-root verification: signatures, epoch monotonicity, in-band
//!   verify-key adoption, genesis anti-lockout, recovery override),
//!   freshness check, and capability-bag math against passed-in
//!   `TrustedMesh` / `Grant` fixture values. No SPEC-12 / wgd
//!   dependencies; the only Bus dependency is `cosmix-lib-bus` with
//!   default features off, consumed solely for the SPEC 01 §4.1 label
//!   grammar (`is_valid_label`) so inventory validation and Bus
//!   addressing can never disagree. Tests run in isolation against
//!   hand-rolled fixtures.
//!
//! - **`cosmix` feature** (default) — adds the substrate / mesh
//!   integration layer: the `with_cross_mesh_grants` combinator
//!   factory wrapping a base [`cosmix_props::AuthPolicy`], the
//!   `TrustGrantsCache` subscriber that mirrors wgd's
//!   `trusted_meshes`, `grants`, and `cross_mesh_exposable`
//!   namespaces, and `NamespaceSpec` drafts for the five wgd
//!   namespaces themselves.
//!
//! The `HttpsVerifier` end-to-end pipeline (plan §6) is **not** in
//! this P0 deliverable; it lands in P2.

pub mod admission;
pub mod canonical;
pub mod caps;
pub mod ctk_caps;
pub mod envelope;
pub mod freshness;
pub mod inventory;
pub mod routing;
pub mod sig;

#[cfg(feature = "cosmix")]
pub mod cache;

#[cfg(feature = "cosmix")]
pub mod combinator;

#[cfg(feature = "cosmix")]
pub mod wgd_namespaces;

#[cfg(feature = "cosmix")]
pub mod wgd_records;

#[cfg(feature = "cosmix")]
pub mod subscriber;
