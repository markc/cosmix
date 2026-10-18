// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Mark Constable <mc@mixos.dev>

//! `maild-auth` — SPF / DKIM / DMARC / ARC / iprev primitives.
//!
//! Wraps `stalwartlabs/mail-auth` behind a stable Cosmix-shaped trait
//! surface. Inbound: `Verifier` runs all five checks at DATA time and
//! produces a structured `AuthResultsHeader` ready to prepend.
//! Outbound: `Signer` adds DKIM signatures on submission and ARC seals
//! on forward.
//!
//! See `_doc/2026-04-30-maild-auth-doc.md`,
//! `_doc/2026-04-30-maild-auth-spec.md` (R4 — frozen public API),
//! and `_doc/2026-04-30-maild-auth-plan.md` (6-phase plan,
//! 3.5-day budget).
//!
//! Phase 0 ships the trait surface + types + error enum; concrete
//! verifier/signer behavior lands in Phase 1+.

pub mod ar_trim;
pub mod auth_results;
pub mod error;
pub mod keygen;
pub mod mapping;
pub mod resolver;
pub mod signer;
pub mod types;
pub mod verifier;

pub use crate::error::{Error, Result};
pub use crate::keygen::{DkimKeyMaterial, KeySpec};
pub use crate::signer::{MailAuthSigner, Signer, detect_algorithm};
pub use crate::types::*;
pub use crate::verifier::{MailAuthVerifier, Verifier};
