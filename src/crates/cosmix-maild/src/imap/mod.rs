//! IMAP server — IMAPS (implicit TLS, port 993) per RFC 9051.
//!
//! Phase 1 scope (per `_doc/maild/imap.md` §"Phase 1"):
//! - IMAPS listener with TLS via existing `tls_cert`/`tls_key`.
//! - Greeting; CAPABILITY, NOOP, LOGOUT, ID, LOGIN,
//!   AUTHENTICATE PLAIN / AUTHENTICATE LOGIN with SASL-IR.
//! - Connection limits: pre-auth idle timeout, per-account concurrent
//!   cap, max auth failures, max bad commands.
//! - No mailbox / message commands yet — anything outside the Phase 1
//!   verb table is rejected with `BAD command not valid` (or `NO
//!   command not allowed in current state` when state-dependent).
//!
//! Codec note: Phase 1 commands have no IMAP literals on the wire,
//! so the codec is a minimal CRLF line reader. Phase 4 (APPEND) is
//! the trigger for swapping in `imap-codec` / `imap-next` for full
//! literal-continuation handling.

pub mod capabilities;
pub mod codec;
pub mod config;
pub mod mailbox_name;
pub mod op;
pub mod response;
pub mod sasl;
pub mod seq;
pub mod server;
pub mod session;
pub mod state;

pub use config::ImapConfig;
pub use server::{ImapHandle, start};
