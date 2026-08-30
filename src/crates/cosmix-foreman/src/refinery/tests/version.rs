    #[test]
    fn semver_bump_drops_prerelease_and_build_metadata() {
        assert_eq!(bump_semver("1.2.3+build.7", false).unwrap(), "1.2.4");
        assert_eq!(bump_semver("1.2.3-rc.1+build.7", true).unwrap(), "1.3.0");
    }

    #[test]
    fn semver_component_overflow_is_a_typed_bounce_naming_the_version() {
        for (version, minor) in [
            ("1.18446744073709551615.3", true),
            ("1.2.18446744073709551615", false),
        ] {
            let error = bump_semver(version, minor).unwrap_err();
            assert!(
                error.downcast_ref::<LandingTaskFault>().is_some(),
                "{error:#}"
            );
            assert!(error.to_string().contains(version), "{error:#}");
            assert!(error.to_string().contains("overflow"), "{error:#}");
        }
    }

    #[test]
    fn version_authority_comes_from_base_and_supports_workspace_inheritance() {
        let (_tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\n[workspace.package]\nversion='999.0.0'\n",
        )
        .unwrap();
        let package = package_manifest_at_base(&repo, &base, Path::new("crate/Cargo.toml"))
            .unwrap()
            .unwrap();
        assert_eq!(package.name, "fixture");
        assert_eq!(package.version, "0.1.0");
        assert_eq!(
            package.version_source,
            VersionSource::Workspace(PathBuf::from("Cargo.toml"))
        );
        let discarded = validate_live_package(&repo, &package).unwrap();
        assert_eq!(discarded.len(), 1);
        assert!(discarded[0].contains("999.0.0"));
        assert_eq!(
            std::fs::read_to_string(repo.join("Cargo.toml"))
                .unwrap()
                .parse::<toml_edit::DocumentMut>()
                .unwrap()["workspace"]["package"]["version"]
                .as_str(),
            Some("0.1.0")
        );

        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("crate/Cargo.toml"),
            "[package]\nname='agent-replacement'\nversion.workspace=true\nedition='2024'\n",
        )
        .unwrap();
        let error = validate_live_package(&repo, &package).unwrap_err();
        assert!(error.to_string().contains("changed package name"));
    }

    #[test]
    fn structured_rewrite_accepts_compact_and_single_quoted_toml() {
        let (_tmp, repo, _base) = version_repo();
        rewrite_package_version(&repo, Path::new("Cargo.toml"), "0.1.0", "0.1.1", true).unwrap();
        let content = std::fs::read_to_string(repo.join("Cargo.toml")).unwrap();
        let doc = content.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["workspace"]["package"]["version"].as_str(),
            Some("0.1.1")
        );
    }

    #[test]
    fn workspace_inheritance_refuses_redirect_and_membership_change() {
        let (_tmp, repo, base) = version_repo();
        let package = package_manifest_at_base(&repo, &base, Path::new("crate/Cargo.toml"))
            .unwrap()
            .unwrap();
        std::fs::create_dir(repo.join("other")).unwrap();
        std::fs::write(
            repo.join("other/Cargo.toml"),
            "[workspace]\nmembers=['../crate']\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("crate/Cargo.toml"),
            "[package]\nname='fixture'\nversion.workspace=true\nworkspace='../other'\nedition='2024'\n",
        )
        .unwrap();
        let redirected = validate_live_package(&repo, &package).unwrap_err();
        assert!(
            redirected.to_string().contains("workspace"),
            "{redirected:#}"
        );

        std::fs::write(
            repo.join("crate/Cargo.toml"),
            "[package]\nname='fixture'\nversion.workspace=true\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=[]\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        let removed = validate_live_package(&repo, &package).unwrap_err();
        assert!(removed.to_string().contains("workspace"), "{removed:#}");
    }

    #[test]
    fn concrete_version_package_cannot_add_workspace_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("crate/src")).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(
            repo.join("crate/Cargo.toml"),
            "[package]\nname='fixture'\nversion='1.2.3'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(repo.join("crate/src/lib.rs"), "pub fn base() {}\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base package"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let package = package_manifest_at_base(&repo, &base, Path::new("crate/Cargo.toml"))
            .unwrap()
            .unwrap();

        std::fs::create_dir(repo.join("branch-workspace")).unwrap();
        std::fs::write(
            repo.join("branch-workspace/Cargo.toml"),
            "[workspace]\nmembers=['../crate']\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("crate/Cargo.toml"),
            "[package]\nname='fixture'\nversion='1.2.3'\nworkspace='../branch-workspace'\nedition='2024'\n",
        )
        .unwrap();
        let error = validate_live_package(&repo, &package).unwrap_err();
        assert!(
            error.to_string().contains("`[package].workspace`"),
            "{error:#}"
        );
    }

    #[test]
    fn manifest_and_lockfile_symlinks_are_refused_without_touching_targets() {
        let (_tmp, repo, base) = version_repo();
        let outside = repo.parent().unwrap().join("outside");
        std::fs::write(&outside, "sentinel\n").unwrap();
        std::fs::remove_file(repo.join("crate/Cargo.toml")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("crate/Cargo.toml")).unwrap();
        assert!(safe_read(&repo, Path::new("crate/Cargo.toml"), "manifest").is_err());

        std::fs::remove_file(repo.join("Cargo.lock")).unwrap();
        let deleted = nearest_lockfile_at_base(&repo, &base, &repo.join("crate")).unwrap_err();
        assert!(
            deleted
                .to_string()
                .contains("deleted integration-base lockfile")
        );
        std::os::unix::fs::symlink(&outside, repo.join("Cargo.lock")).unwrap();
        assert!(nearest_lockfile_at_base(&repo, &base, &repo.join("crate")).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel\n");
    }

    #[test]
    fn changed_virtual_workspace_manifest_symlink_is_a_task_fault() {
        let (tmp, repo, base) = version_repo();
        let outside = tmp.path().join("outside-manifest");
        std::fs::write(&outside, "sentinel\n").unwrap();
        std::fs::remove_file(repo.join("Cargo.toml")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("Cargo.toml")).unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "symlink workspace manifest"]).unwrap();

        let task = version_task(&tmp.path().join("manifest.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel\n");
    }

    #[test]
    fn changed_virtual_workspace_lockfile_symlink_is_a_task_fault() {
        let (tmp, repo, base) = version_repo();
        let outside = tmp.path().join("outside-lock");
        std::fs::write(&outside, "sentinel\n").unwrap();
        std::fs::remove_file(repo.join("Cargo.lock")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("Cargo.lock")).unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "symlink workspace lock"]).unwrap();

        let task = version_task(&tmp.path().join("lock-symlink.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel\n");
    }

    #[test]
    fn added_virtual_workspace_lockfile_symlink_is_a_task_fault() {
        let (tmp, repo, _) = version_repo();
        git(&repo, &["rm", "Cargo.lock"]).unwrap();
        git(&repo, &["commit", "-m", "base without lockfile"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let outside = tmp.path().join("outside-added-lock");
        std::fs::write(&outside, "sentinel\n").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("Cargo.lock")).unwrap();
        git(&repo, &["add", "Cargo.lock"]).unwrap();
        git(&repo, &["commit", "-m", "add symlink lockfile"]).unwrap();

        let task = version_task(&tmp.path().join("added-lock-symlink.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert!(error.to_string().contains("non-regular file"), "{error:#}");
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel\n");
    }

    #[test]
    fn deleted_virtual_workspace_lockfile_is_a_task_fault() {
        let (tmp, repo, base) = version_repo();
        std::fs::remove_file(repo.join("Cargo.lock")).unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "delete workspace lock"]).unwrap();

        let task = version_task(&tmp.path().join("lock-deleted.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("workspace lockfile"),
            "{error:#}"
        );
    }

    #[test]
    fn virtual_workspace_member_removal_is_a_task_fault_without_a_selected_package() {
        let (tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=[]\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        git(&repo, &["add", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "remove workspace member"]).unwrap();

        let task = version_task(&tmp.path().join("member.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("`[workspace].members`"),
            "{error:#}"
        );
    }

    #[test]
    fn virtual_workspace_exclude_change_is_a_task_fault() {
        let (tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\nexclude=['crate']\n[workspace.package]\nversion='0.1.0'\n",
        )
        .unwrap();
        git(&repo, &["add", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "exclude workspace member"]).unwrap();

        let task = version_task(&tmp.path().join("exclude.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("`[workspace].exclude`"),
            "{error:#}"
        );
    }

    #[test]
    fn workspace_write_io_failure_uses_infrastructure_backoff() {
        let (tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\n[workspace.package]\nversion='9.9.9'\n",
        )
        .unwrap();
        git(&repo, &["add", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "edit workspace version"]).unwrap();

        let ledger = Ledger::open(&tmp.path().join("io.db")).unwrap();
        let id = ledger
            .add_task("workspace io", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "agent").unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(claimed.attempt, task.attempt);
        assert!(ledger.transition_if(id, "done", "landing").unwrap());

        FAIL_NEXT_WORKSPACE_WRITE.with(|fail| fail.set(true));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingInfrastructure>().is_some(),
            "{error:#}"
        );
        let report = landing_error_report(&task, &error);
        assert_eq!(report.reason, FindingReason::InfraRefusal);
        finish_landing_and_maybe_wake(
            &ledger,
            id,
            "bounced",
            None,
            &report,
            1,
            10,
            crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
            chrono::Utc::now(),
            || {},
        )
        .unwrap();
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "bounced");
        assert_eq!(task.infra_refusals, 1);
        assert_eq!(task.branch_contract_failures, 0);
        assert!(task.dispatch_after.is_some());
    }

    #[test]
    fn workspace_lock_read_io_failure_uses_infrastructure_backoff() {
        let (tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\n[workspace.package]\nversion='9.9.9'\n",
        )
        .unwrap();
        git(&repo, &["add", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "edit workspace version"]).unwrap();

        let ledger = Ledger::open(&tmp.path().join("lock-read-io.db")).unwrap();
        let id = ledger
            .add_task("lock read io", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger.claim_task(id, "agent").unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
        let task = ledger.task(id).unwrap().unwrap();
        assert!(ledger.transition_if(id, "done", "landing").unwrap());

        FAIL_NEXT_LOCKFILE_READ.with(|fail| fail.set(true));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingInfrastructure>().is_some(),
            "{error:#}"
        );
        let report = landing_error_report(&task, &error);
        assert_eq!(report.reason, FindingReason::InfraRefusal);
        finish_landing_and_maybe_wake(
            &ledger,
            id,
            "bounced",
            None,
            &report,
            1,
            10,
            crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
            chrono::Utc::now(),
            || {},
        )
        .unwrap();
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.infra_refusals, 1);
        assert_eq!(task.branch_contract_failures, 0);
        assert!(task.dispatch_after.is_some());
    }

    #[test]
    fn valid_root_manifest_removal_does_not_use_orphan_healing_exception() {
        let (tmp, repo, base) = version_repo();
        git(&repo, &["rm", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "remove valid workspace root"]).unwrap();

        let task = version_task(&tmp.path().join("root-removal.db"));
        let error = create_landing_version_commit(&repo, &base, &task).unwrap_err();
        assert!(
            error.downcast_ref::<LandingTaskFault>().is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("workspace manifest"),
            "{error:#}"
        );
    }

    #[test]
    fn healing_eligibility_probes_the_landings_own_bump_kind_not_always_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::create_dir_all(repo.join("orphan")).unwrap();
        // A manifest with no workspace root anywhere in its ancestry — the
        // "already unusable" orphan the healing exception exists for. Its
        // minor component is already at u64::MAX: a PATCH bump succeeds
        // (0 -> 1) but a MINOR bump overflows.
        std::fs::write(
            repo.join("orphan/Cargo.toml"),
            "[package]\nname='orphan'\nversion='1.18446744073709551615.0'\nedition='2024'\n",
        )
        .unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "malformed orphan manifest"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        git(&repo, &["rm", "orphan/Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "remove poisoned orphan manifest"]).unwrap();

        let manifest = Path::new("orphan/Cargo.toml");
        // A high-risk/feature/breaking/schema landing bumps MINOR, not
        // PATCH. Probing with PATCH regardless (the old behaviour) would
        // wrongly call this manifest still "usable" and refuse to heal it.
        assert!(
            !unusable_orphan_manifest_removed_at_head(&repo, &base, manifest, false).unwrap(),
            "a PATCH probe on a minor-overflowed version must not itself look unusable"
        );
        assert!(
            unusable_orphan_manifest_removed_at_head(&repo, &base, manifest, true).unwrap(),
            "a MINOR probe (this landing's real bump kind) must catch the overflowed minor component"
        );
    }

    #[test]
    fn virtual_workspace_version_edit_is_reset_without_a_selected_package() {
        let (tmp, repo, base) = version_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers=['crate']\n[workspace.package]\nversion='9.9.9'\n",
        )
        .unwrap();
        git(&repo, &["add", "Cargo.toml"]).unwrap();
        git(&repo, &["commit", "-m", "edit workspace version"]).unwrap();

        let task = version_task(&tmp.path().join("version.db"));
        let outcome = create_landing_version_commit(&repo, &base, &task).unwrap();
        assert_eq!(outcome.bumped, Vec::<String>::new());
        assert_eq!(outcome.discarded.len(), 1);
        let doc = std::fs::read_to_string(repo.join("Cargo.toml"))
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["workspace"]["package"]["version"].as_str(),
            Some("0.1.0")
        );
    }
