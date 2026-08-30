fn landing_task_for_push_journal(ledger: &Ledger, branch: &str) -> (i64, i64) {
    let id = ledger
        .add_task("push journal", "spec", "impl", "low", &[], "none")
        .unwrap();
    let task = ledger.claim_task(id, "test").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "test",
                generation: task.attempt,
            },
            None,
            Some(branch),
        )
        .unwrap();
    ledger.finish_task(id, "test", "done").unwrap();
    assert!(ledger.transition_if(id, "done", "landing").unwrap());
    (id, task.attempt)
}

#[test]
fn update_and_delete_intents_are_distinct_and_pin_the_verified_tip() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let (task_id, attempt) = landing_task_for_push_journal(&ledger, "task/116");
    let tip = "0123456789abcdef0123456789abcdef01234567";

    let (update, delete) = ledger
        .record_push_intents_before_landing(task_id, attempt, "main", tip)
        .unwrap();

    assert_eq!(update.kind, PushIntentKind::Update);
    assert_eq!(update.refspec, format!("{tip}:refs/heads/main"));
    assert_eq!(delete.kind, PushIntentKind::Delete);
    assert_eq!(delete.refspec, ":refs/heads/task/116");
    assert_ne!(update.id, delete.id);
    assert_eq!(update.verified_tip, tip);
    assert_eq!(delete.verified_tip, tip);
    assert_eq!(update.outcome, PushIntentOutcome::Unknown);
    assert_eq!(delete.outcome, PushIntentOutcome::Unknown);
}

#[test]
fn intent_identity_is_immutable_while_failed_replay_is_claimed_as_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let contender = Ledger::open(&db).unwrap();
    let (task_id, attempt) = landing_task_for_push_journal(&ledger, "task/116");
    let tip = "0123456789abcdef0123456789abcdef01234567";
    let (update, _) = ledger
        .record_push_intents_before_landing(task_id, attempt, "main", tip)
        .unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    let error = conn
        .execute(
            "UPDATE push_intents SET refspec = 'refs/heads/main' WHERE id = ?1",
            [update.id],
        )
        .unwrap_err();
    assert!(error.to_string().contains("immutable intent"), "{error}");

    assert!(
        ledger
            .record_push_outcome(update.id, PushIntentOutcome::Failed, "rejected")
            .unwrap()
    );
    assert!(
        ledger
            .claim_failed_push_for_replay(update.id)
            .unwrap()
    );
    assert_eq!(
        ledger
            .push_intents_for_attempt(task_id, attempt)
            .unwrap()[0]
            .outcome,
        PushIntentOutcome::Unknown
    );
    assert!(
        !contender
            .claim_failed_push_for_replay(update.id)
            .unwrap(),
        "the failed-to-unknown transition is a single-winner replay claim"
    );
    assert!(
        ledger
            .record_push_outcome(update.id, PushIntentOutcome::Succeeded, "replayed")
            .unwrap()
    );
    assert!(
        !ledger
            .record_push_outcome(update.id, PushIntentOutcome::Failed, "too late")
            .unwrap(),
        "succeeded delivery evidence must be terminal"
    );
}
