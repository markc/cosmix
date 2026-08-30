//! Freshness check (plan §6 step 4) — pure timestamp arithmetic.
//!
//! The HTTPS verifier extracts `envelope.ts`, parses it as RFC 3339,
//! and rejects the call if `|now - ts| > freshness_window` (default
//! ±5 minutes). Replay defense lives in a separate layer (plan §7,
//! `wgd.cross_mesh.claim_nonce`) — this module is the time-window
//! check only.

use chrono::{DateTime, Duration, Utc};

/// Default freshness window per plan §6 step 4: ±5 minutes.
pub const DEFAULT_FRESHNESS_WINDOW_SECS: i64 = 5 * 60;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FreshnessError {
    #[error("envelope.ts is not RFC 3339: {0}")]
    InvalidTimestamp(String),

    #[error(
        "envelope timestamp is outside the freshness window: |now - ts| = {drift_secs}s, window = {window_secs}s"
    )]
    OutsideWindow { drift_secs: i64, window_secs: i64 },
}

/// Parse an RFC 3339 timestamp into [`DateTime<Utc>`]. Wraps the
/// underlying error so callers see a structured [`FreshnessError`].
pub fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>, FreshnessError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| FreshnessError::InvalidTimestamp(e.to_string()))
}

/// Plan §6 step 4: reject if the envelope timestamp drifts outside
/// `window` from `now` in either direction.
///
/// Symmetric window — a producer's clock that runs *ahead* of the
/// verifier's by more than the window is also rejected. This
/// matters because a future-skewed envelope, combined with replay,
/// would otherwise let an attacker capture a signed envelope and
/// hold it valid for an extended period.
pub fn check_freshness(
    envelope_ts: DateTime<Utc>,
    now: DateTime<Utc>,
    window: Duration,
) -> Result<(), FreshnessError> {
    // Compare the full chrono::Duration, not its truncated whole-second
    // representation: `(now - ts).num_seconds()` rounds fractional
    // drift toward zero, which would let a drift of `window + 0.999s`
    // slip through. abs() makes the check symmetric — both past- and
    // future-skewed envelopes are rejected at the same threshold.
    let drift = (now - envelope_ts).abs();
    if drift > window {
        return Err(FreshnessError::OutsideWindow {
            drift_secs: drift.num_seconds(),
            window_secs: window.num_seconds(),
        });
    }
    Ok(())
}

/// Convenience: parse + check in one call.
pub fn check_envelope_ts(
    envelope_ts_rfc3339: &str,
    now: DateTime<Utc>,
    window: Duration,
) -> Result<(), FreshnessError> {
    let ts = parse_rfc3339(envelope_ts_rfc3339)?;
    check_freshness(ts, now, window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 20, 3, 14, 0).unwrap()
    }

    #[test]
    fn parse_accepts_rfc3339_z() {
        let dt = parse_rfc3339("2026-05-20T03:14:00Z").unwrap();
        assert_eq!(dt, now());
    }

    #[test]
    fn parse_accepts_rfc3339_with_offset() {
        let dt = parse_rfc3339("2026-05-20T13:14:00+10:00").unwrap();
        assert_eq!(dt, now());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            parse_rfc3339("not a timestamp"),
            Err(FreshnessError::InvalidTimestamp(_))
        ));
    }

    #[test]
    fn check_accepts_within_window() {
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        assert!(check_freshness(now(), now(), window).is_ok());
        assert!(check_freshness(now() - Duration::seconds(60), now(), window).is_ok());
        assert!(check_freshness(now() + Duration::seconds(60), now(), window).is_ok());
    }

    #[test]
    fn check_rejects_past_window() {
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        let err = check_freshness(now() - Duration::seconds(301), now(), window).unwrap_err();
        match err {
            FreshnessError::OutsideWindow {
                drift_secs,
                window_secs,
            } => {
                assert_eq!(drift_secs, 301);
                assert_eq!(window_secs, DEFAULT_FRESHNESS_WINDOW_SECS);
            }
            _ => panic!("expected OutsideWindow"),
        }
    }

    #[test]
    fn check_rejects_future_skew_window() {
        // Symmetric window — future-skewed envelopes are also
        // rejected. This matters because a producer with an
        // ahead-skewed clock could otherwise have a much longer
        // effective replay window than `window` itself.
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        assert!(check_freshness(now() + Duration::seconds(301), now(), window).is_err());
    }

    #[test]
    fn boundary_accepts_exact_window_edge() {
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        // |now - ts| == window is accepted; > window is rejected.
        assert!(
            check_freshness(
                now() - Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS),
                now(),
                window
            )
            .is_ok()
        );
        assert!(
            check_freshness(
                now() - Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS + 1),
                now(),
                window
            )
            .is_err()
        );
    }

    #[test]
    fn boundary_rejects_subsecond_drift_past_window() {
        // Regression: a previous implementation compared
        // `num_seconds()` instead of the chrono Duration, so a drift
        // of `window + 999ms` slipped through (truncates to `window`).
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        let drift = window + Duration::milliseconds(1);
        assert!(check_freshness(now() - drift, now(), window).is_err());
        assert!(check_freshness(now() + drift, now(), window).is_err());
    }

    #[test]
    fn check_envelope_ts_composes_parse_and_window() {
        let window = Duration::seconds(DEFAULT_FRESHNESS_WINDOW_SECS);
        assert!(check_envelope_ts("2026-05-20T03:14:00Z", now(), window).is_ok());
        assert!(check_envelope_ts("garbage", now(), window).is_err());
        assert!(check_envelope_ts("2025-01-01T00:00:00Z", now(), window).is_err());
    }
}
