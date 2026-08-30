#[cfg(test)]
mod tests {
    use super::{
        ClaimToken, FAIL_SCHEMA_14_BEFORE_COMMIT, FindingReason, Ledger, LedgerCreate, TaskControls,
        TaskStatus, PushIntentKind, PushIntentOutcome, Usage, VersionBump, deps_form_cycle,
        derived_version_bump, infra_retry_backoff_secs,
    };
    use std::collections::HashMap;

    /// Backdate a task's lease directly through a second connection to the
    /// same file — `Ledger` never exposes a way to forge a stale lease
    /// through its own API, deliberately: nothing outside
    /// `reap_dead_claims` should write `lease_until` after claim time.
    fn backdate_lease(db: &std::path::Path, task_id: i64, lease_rfc3339: &str) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute(
            "UPDATE tasks SET lease_until = ?1 WHERE id = ?2",
            rusqlite::params![lease_rfc3339, task_id],
        )
        .unwrap();
    }

    /// Age a claim the same way: `claimed_at` is written at claim time and
    /// never moves, so a test that wants a genuinely old claim (rather than
    /// one whose lease was merely shortened) has to move it directly.
    /// Passing `None` reproduces a claim taken before the column existed.
    fn backdate_claimed_at(db: &std::path::Path, task_id: i64, claimed_rfc3339: Option<&str>) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute(
            "UPDATE tasks SET claimed_at = ?1 WHERE id = ?2",
            rusqlite::params![claimed_rfc3339, task_id],
        )
        .unwrap();
    }

    include!("tests_tasks.rs");
    include!("tests_claim_leases.rs");
    include!("tests_connection.rs");
    include!("tests_reaping.rs");
    include!("tests_busy_findings_budget.rs");
    include!("tests_landing.rs");
    include!("tests_push_journal.rs");
}
