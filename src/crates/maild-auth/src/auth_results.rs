//! Authentication-Results header composer (RFC 8601).
//!
//! Phase 0: empty surface; the actual builder lands in Phase 2 once
//! the verifier produces the structured fields it needs to render.

#![allow(dead_code)]

use crate::types::AuthResultsHeader;

/// Render an `AuthResultsHeader` to its wire-format string. Callers
/// should normally read `header.rendered` directly; this helper
/// exists for places that build the header struct from outside the
/// verifier (tests, ARC sealing fixture rebuilds).
pub fn render(_header: &AuthResultsHeader) -> String {
    // Phase 2.
    String::new()
}
