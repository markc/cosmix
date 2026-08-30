//! Rule kinds. One module per kind; each exposes a `Rule` struct +
//! a `match_against(ctx) -> bool` (or `-> u32` for count-based kinds)
//! method. Phase 0 only declares the modules so the public surface
//! is observable; per-kind logic lands in Phase 1.
//!
//! Kind list — frozen per spec §RuleKinds. Items marked *(reserved)*
//! are scaffolded but not invoked by `default.toml` in v1.

pub mod alignment;
pub mod attachment_count;
pub mod body_regex;
pub mod body_substring;
pub mod header_absent;
pub mod header_present;
pub mod header_regex;
pub mod header_substring;
pub mod mail_auth;
pub mod peer_ip_in_cidr;
pub mod peer_ip_in_dnsbl;
pub mod recipient_count;
pub mod structure;
pub mod url_count;
