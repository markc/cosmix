fn assert_claim_metadata_cleared(ledger: &Ledger, id: i64) {
    let metadata: (Option<String>, Option<String>, Option<i64>, Option<String>) = ledger
        .conn
        .query_row(
            "SELECT claimed_by, lease_until, claim_pid, claimed_at FROM tasks WHERE id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(metadata, (None, None, None, None));
}

fn leased_task(ledger: &Ledger, title: &str, claimant: &str) -> super::Task {
    let id = ledger
        .add_task(title, "spec", "impl", "low", &[], "none")
        .unwrap();
    ledger.claim_task(id, claimant).unwrap()
}

/// Slice A's core contract, including the case attempt 7 missed: the generic
/// claim path writes no local pid, yet it receives a readable, renewable,
/// expiring lease exactly like a local runner claim.
#[test]
fn pidless_claim_writes_reads_and_advances_its_heartbeat_lease() {
    let temp = tempfile::TempDir::new().unwrap();
    let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
    let task = leased_task(&ledger, "remote work", "remote-worker");

    let initial = task
        .lease_until
        .as_deref()
        .expect("claim_task must read its written lease back through Task");
    let initial = chrono::DateTime::parse_from_rfc3339(initial).unwrap();
    let claim_pid: Option<i64> = ledger
        .conn
        .query_row(
            "SELECT claim_pid FROM tasks WHERE id = ?1",
            rusqlite::params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(claim_pid, None, "generic/MCP claims must have claim_pid NULL");

    let heartbeat_at = (initial - chrono::Duration::hours(23)).to_rfc3339();
    let renewed = ledger
        .renew_claim_at(
            task.id,
            ClaimToken {
                owner: "remote-worker",
                generation: task.attempt,
            },
            &heartbeat_at,
        )
        .unwrap();
    let renewed = chrono::DateTime::parse_from_rfc3339(&renewed).unwrap();
    assert_eq!(renewed, initial + chrono::Duration::hours(1));
    assert_eq!(
        ledger.task(task.id).unwrap().unwrap().lease_until,
        Some(renewed.to_rfc3339()),
        "the renewed database value must be readable through the normal task surface"
    );
    let claim_pid: Option<i64> = ledger
        .conn
        .query_row(
            "SELECT claim_pid FROM tasks WHERE id = ?1",
            rusqlite::params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(claim_pid, None, "heartbeat must not invent a local pid");
}

#[test]
fn heartbeat_is_generation_fenced() {
    let temp = tempfile::TempDir::new().unwrap();
    let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
    let first = leased_task(&ledger, "fenced work", "same-name");
    ledger.requeue_task(first.id, true).unwrap();
    let second = ledger.claim_task(first.id, "same-name").unwrap();
    let before = second.lease_until.clone();

    let error = ledger
        .renew_claim(
            first.id,
            ClaimToken {
                owner: "same-name",
                generation: first.attempt,
            },
        )
        .expect_err("an old generation must not renew a replacement claim");
    assert!(format!("{error:#}").contains("refusing heartbeat"));
    assert_eq!(ledger.task(first.id).unwrap().unwrap().lease_until, before);
}

/// Exercise each dispatch-claim release API. The source audit below covers
/// the unclaimed landing/parking transitions as well; together they prevent
/// a release path from leaving a future expiry attached to healthy state.
#[test]
fn every_normal_dispatch_release_api_clears_the_lease() {
    let temp = tempfile::TempDir::new().unwrap();
    let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();

    let task = leased_task(&ledger, "legacy finish", "worker");
    ledger.finish_task(task.id, "worker", "done").unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "fenced finish", "worker");
    ledger
        .finish_claimed(
            task.id,
            ClaimToken {
                owner: "worker",
                generation: task.attempt,
            },
            TaskStatus::Bounced,
        )
        .unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "operator status", "worker");
    ledger.set_status(task.id, TaskStatus::Done).unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "forced requeue", "worker");
    ledger.requeue_task(task.id, true).unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "agent bounce", "remote");
    ledger
        .finish_agent_bounce(task.id, "remote", task.attempt, "cannot finish", 3)
        .unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "verified", "remote");
    assert!(
        ledger
            .complete_verified(
                task.id,
                ClaimToken {
                    owner: "remote",
                    generation: task.attempt,
                },
                ".",
                "green",
                true,
                &[],
            )
            .unwrap()
    );
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "infrastructure", "worker");
    ledger
        .finish_infrastructure_failure_at(
            task.id,
            ClaimToken {
                owner: "worker",
                generation: task.attempt,
            },
            "2026-08-30T00:00:00Z",
        )
        .unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let task = leased_task(&ledger, "background", "worker");
    ledger
        .finish_abandoned_background_at(
            task.id,
            ClaimToken {
                owner: "worker",
                generation: task.attempt,
            },
            "background helper remained live",
            "2026-08-30T00:00:00Z",
        )
        .unwrap();
    assert_claim_metadata_cleared(&ledger, task.id);

    let id = ledger
        .add_task("classified", "spec", "impl", "low", &[], "none")
        .unwrap();
    let (task, run_id) = ledger
        .start_attempt(id, "worker", None, None, "claude", None)
        .unwrap();
    ledger
        .finish_task_classified_at(
            id,
            ClaimToken {
                owner: "worker",
                generation: task.attempt,
            },
            run_id,
            "done",
            None,
            None,
            3,
            5,
            3,
            "2026-08-30T00:00:00Z",
        )
        .unwrap();
    assert_claim_metadata_cleared(&ledger, id);
}

/// Every production SQL statement which clears a dispatch claim must clear
/// its lease in the same statement. `park_retire.rs` is intentionally absent:
/// its one `claimed_by` release belongs to the distinct scratch-GC interlock,
/// whose stamped owner must not be reinterpreted as a dispatch claim.
#[test]
fn every_dispatch_claim_clearing_statement_clears_lease_atomically() {
    let sources = [
        ("claims.rs", include_str!("claims.rs")),
        ("requeue.rs", include_str!("requeue.rs")),
        ("finish_worker.rs", include_str!("finish_worker.rs")),
        ("finish_landing.rs", include_str!("finish_landing.rs")),
        ("finding_types.rs", include_str!("finding_types.rs")),
        ("verification.rs", include_str!("verification.rs")),
        ("landing.rs", include_str!("landing.rs")),
        ("reaping.rs", include_str!("reaping.rs")),
    ];
    for (name, source) in sources {
        for (offset, _) in source.match_indices("claimed_by = NULL") {
            let statement_tail = &source[offset..source.len().min(offset + 220)];
            assert!(
                statement_tail.contains("lease_until = NULL"),
                "{name} clears claimed_by without atomically clearing lease_until: {statement_tail}"
            );
        }
    }
}
