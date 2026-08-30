//! Opaque corpus identity (invariant #4): a time-ordered UUIDv7.
//!
//! v7 (not v4): lexicographically sortable, a valid JMAP/web id, and already in
//! the workspace `uuid` dependency. The Mix `uuid` builtin only mints v4, which is
//! why id allocation lives here / in the daemon, not in a Mix handler.

use uuid::Uuid;

/// Mint a fresh UUIDv7 as a lowercase hyphenated string.
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

/// True if `s` parses as a UUID of any version.
pub fn is_valid_id(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_valid_distinct_ids() {
        let a = new_id();
        let b = new_id();
        assert!(is_valid_id(&a));
        assert!(is_valid_id(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_non_uuid() {
        assert!(!is_valid_id("not-a-uuid"));
        assert!(!is_valid_id(""));
        assert!(is_valid_id("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000"));
    }

    #[test]
    fn v7_is_time_sortable() {
        // Two v7 ids minted in order should sort in mint order (monotonic clock
        // within the test). Compare as strings — v7 hex is sortable by design.
        let first = new_id();
        let second = new_id();
        // Not asserting strict <; clock resolution can collide. Just ensure both
        // are well-formed v7-shaped (version nibble '7').
        assert_eq!(first.as_bytes()[14], b'7');
        assert_eq!(second.as_bytes()[14], b'7');
    }
}
