//! Daemon policy limits (plan §3.8). Core limits: `cosmix_edit_core::limits`.

const MIB: u64 = 1024 * 1024;

/// Open buffers (scratch included). Checked by the router.
pub const MAX_BUFFERS: usize = 256;
/// Aggregate bytes across buffers: text + retained log text + live snapshots.
/// Enforced by the router-owned [`crate::router::Budget`].
pub const MAX_TOTAL_BYTES: u64 = 1024 * MIB;
/// Encoded JSON per reply.
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;
/// Encoded JSON per event; larger → `resync {reason: "oversized"}`.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// Encoded bytes of events waiting to publish; over → drop + `resync_pending`.
pub const MAX_PUBLISH_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// Live snapshots per buffer (their bytes count in `MAX_TOTAL_BYTES`).
pub const MAX_SNAPSHOTS_PER_BUFFER: usize = 2;
/// Queued commands per buffer actor; full → RESOURCE_LIMIT `busy` at once.
pub const ACTOR_INBOX: usize = 256;
/// Queued commands for the router; full → RESOURCE_LIMIT `busy`.
pub const ROUTER_INBOX: usize = 1_024;
/// Cached successful replies per buffer for op_id dedup (LRU).
pub const DEDUP_ENTRIES: usize = 1_024;
/// Ancestor levels watched while a bound file's parent directory is missing.
pub const WATCH_ANCESTOR_DEPTH: usize = 8;
/// Resync retry backoff (a retry of a known-pending send, not a poll).
pub const RESYNC_BACKOFF_BASE_MS: u64 = 250;
pub const RESYNC_BACKOFF_CAP_MS: u64 = 30_000;
/// SIGTERM budget: log dirty buffers and exit within this.
pub const SHUTDOWN_BUDGET_MS: u64 = 10_000;
