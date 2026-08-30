    #[test]
    fn land_task_refuses_branchless_task() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        let git_temp = tempfile::TempDir::new().unwrap();
        let result = ledger.land_task(id, git_temp.path());

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("has no branch"));
    }

    #[test]
    fn land_task_refuses_claimed_task() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as claimed with a branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "test-branch");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, claimed_by = ?2, branch = ?3 WHERE id = ?4",
                ["claimed", "test-claimant", "test-branch", &id.to_string()],
            )
            .unwrap();

        let result = ledger.land_task(id, git_temp.path());

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("claimed") || err.contains("refuses live work"));
    }

    #[test]
    fn land_task_refuses_running_task() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as running with a branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "test-branch");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, claimed_by = ?2, branch = ?3 WHERE id = ?4",
                ["running", "test-claimant", "test-branch", &id.to_string()],
            )
            .unwrap();

        let result = ledger.land_task(id, git_temp.path());

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("running") || err.contains("refuses live work"));
    }

    #[test]
    fn land_task_refuses_landing_task() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as landing with a branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "test-branch");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, claimed_by = NULL, branch = ?2 WHERE id = ?3",
                ["landing", "test-branch", &id.to_string()],
            )
            .unwrap();

        let result = ledger.land_task(id, git_temp.path());

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("landing") || err.contains("refuses live work"));
    }

    #[test]
    fn land_task_refuses_missing_branch() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as done with a non-existent branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "existing-branch");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, branch = ?2 WHERE id = ?3",
                ["done", "nonexistent-branch", &id.to_string()],
            )
            .unwrap();

        let result = ledger.land_task(id, git_temp.path());

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn land_task_succeeds_on_bounced_task_with_branch() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as bounced with a valid branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "task/1");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, branch = ?2, ladder_failures = 3 WHERE id = ?3",
                ["bounced", "task/1", &id.to_string()],
            )
            .unwrap();

        ledger.land_task(id, git_temp.path()).unwrap();

        // Verify task is now landable (done with branch, unclaimed)
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "done");
        assert!(task.claimed_by.is_none());
        assert_eq!(task.branch.as_deref(), Some("task/1"));
        assert_eq!(task.ladder_failures, 3, "ladder_failures must be untouched");
    }

    #[test]
    fn land_task_succeeds_on_failed_task_with_branch() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as failed with a valid branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "task/2");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, branch = ?2, ladder_failures = 5 WHERE id = ?3",
                ["failed", "task/2", &id.to_string()],
            )
            .unwrap();

        ledger.land_task(id, git_temp.path()).unwrap();

        // Verify task is now landable
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "done");
        assert!(task.claimed_by.is_none());
        assert_eq!(task.branch.as_deref(), Some("task/2"));
        assert_eq!(task.ladder_failures, 5, "ladder_failures must be untouched");
    }

    /// Test that a bounced task with a branch, after land_task(), is actually
    /// picked up by landable_tasks() — proving the row is in the refinery queue.
    #[test]
    fn land_task_makes_bounced_task_landable_and_refine_picks_it_up() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task("test task", "spec", "impl", "low", &[], "rust")
            .unwrap();

        // Set up the task as bounced with a valid branch
        let git_temp = tempfile::TempDir::new().unwrap();
        init_repo_with_branch(git_temp.path(), "task/42");
        ledger
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, branch = ?2, ladder_failures = 2 WHERE id = ?3",
                ["bounced", "task/42", &id.to_string()],
            )
            .unwrap();

        // Verify it's NOT in the landable queue yet (status is bounced, not done)
        let landable_before = ledger.landable_tasks().unwrap();
        assert!(
            landable_before.is_empty(),
            "bounced task should not be landable yet"
        );

        // Apply land_task
        ledger.land_task(id, git_temp.path()).unwrap();

        // Now it SHOULD be in the landable queue
        let landable_after = ledger.landable_tasks().unwrap();
        assert_eq!(landable_after.len(), 1, "task should now be landable");
        assert_eq!(landable_after[0].id, id);
        assert_eq!(landable_after[0].status, "done");
        assert_eq!(landable_after[0].branch.as_deref(), Some("task/42"));
        assert!(landable_after[0].claimed_by.is_none());
    }

    /// Helper to initialize a git repo with a branch for testing.
    fn init_repo_with_branch(repo: &std::path::Path, branch: &str) {
        use std::process::Command;

        // Initialize repo
        Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Configure user
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Create initial commit on main
        let file = repo.join("README.md");
        std::fs::write(&file, "# Test Repo").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Create the test branch
        Command::new("git")
            .args(["branch", branch])
            .current_dir(repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }
