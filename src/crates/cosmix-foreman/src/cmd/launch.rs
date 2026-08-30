use super::launch_support::{RemoveOnDrop, sweep_stale_configs, write_new};
use super::*;

/// Run one task through one driver with governor reservation, reserved-cap
/// enforcement, and (opt-in) the policy hook. Returns the task's final
/// status string.
pub(super) fn launch(
    db: &std::path::Path,
    db_create: cosmix_foreman::state::DbCreateMode,
    ledger: &Ledger,
    spec: LaunchSpec,
    fleet_policy: &FleetPolicy,
    manifest: Option<&ProjectManifest>,
) -> Result<&'static str> {
    let LaunchSpec {
        task,
        agent,
        model,
        workdir,
        mut budget,
        stall_secs,
        permission_mode,
        mut extra_args,
        branch,
        integration,
        verify_subdir,
        no_governor,
        no_verify,
        policy,
        allow_operator_driven,
        worktree_template,
        profiles,
        project_pack,
    } = spec;
    loop {
        let Some(task_budget) = ledger.task_budget_remainder(task)? else {
            break;
        };
        if task_budget.remaining_usd > 0.0 {
            cosmix_foreman::governor::require_task_budget_metering(
                agent,
                Some(task_budget.limit_usd),
            )?;
            budget.max_budget_usd = Some(cosmix_foreman::governor::task_reservation_usd(
                fleet_policy.reserve_usd.value,
                budget.max_budget_usd,
                Some(task_budget.remaining_usd),
            ));
            break;
        }

        let required_usd = budget
            .max_budget_usd
            .unwrap_or(fleet_policy.reserve_usd.value);
        let parked = ledger_write_with_busy_retry("parking budget-exhausted task", || {
            ledger.park_task_budget_exhausted(
                task,
                task_budget.limit_usd,
                task_budget.charged_usd,
                task_budget.remaining_usd,
                required_usd,
            )
        })?;
        if parked {
            println!(
                "task {task} PARKED: budget ${:.4} charged, ${:.4} remaining, \
                 ${required_usd:.4} required — see findings",
                task_budget.charged_usd, task_budget.remaining_usd
            );
            return Ok("parked");
        }

        // A concurrent operator top-up wins over the stale exhaustion
        // snapshot. Re-evaluate it; an already-parked task is the same normal
        // outcome. Any other state movement belongs to the claim guard.
        let current = ledger
            .task(task)?
            .with_context(|| format!("no task {task}"))?;
        if current.status == "parked" {
            return Ok("parked");
        }
        if current.budget_usd != Some(task_budget.limit_usd) {
            continue;
        }
        anyhow::bail!(
            "task {task} changed state while its exhausted budget was being parked; re-run"
        );
    }
    // Lane eligibility + credentials: a manifest's `lanes` table is an
    // ADDITIONAL refusal gate, checked before anything else spends
    // governor headroom. A denial files a typed operator blocker without
    // consuming a quality charge or an infrastructure refusal.
    if let Some(manifest) = manifest
        && let Err(reason) =
            manifest.check_lane(agent, cosmix_foreman::manifest::credential_in_environment)
    {
        if !ledger.task_has_open_finding_reason(task, FindingReason::PolicyDenied)? {
            ledger_write_with_busy_retry("filing project-policy blocker", || {
                ledger.file_finding_reasoned(
                    Some(task),
                    "blocker",
                    "project policy denied the routed agent",
                    &reason,
                    "dispatch",
                    FindingReason::PolicyDenied,
                )
            })?;
        }
        return Err(ProjectPolicyDenied(reason).into());
    }
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("workdir {}", workdir.display()))?;
    // A branched launch runs in the task's OWN worktree (sibling of the
    // integration clone), never in the clone itself — otherwise the branch
    // the ledger records would exist nowhere and every agent would dirty
    // the shared integration tree.
    //
    // Provisioning also REBASES the branch onto the integration head (see
    // `ensure_task_worktree`): reuse carries partial state on purpose, but a
    // branch still sitting on an old integration commit re-tests old in-tree
    // code, so a landed harness fix never reaches it. A clean replay is
    // recorded on the run below; a conflicted one bounces here.
    let (workdir, rebase) = match &branch {
        Some(b) => {
            let wt = cosmix_foreman::refinery::ensure_task_worktree_named_in(
                &workdir,
                task,
                b,
                Some(&integration),
                &worktree_template,
                manifest.map(|project| project.root.as_path()),
            )?;
            (wt.path, wt.rebase)
        }
        None => (workdir, None),
    };
    if let Some(outcome) = &rebase
        && outcome.conflicted()
    {
        // The aborted branch is safe to launch. Record the conflict before
        // claiming so lowering can put the finding first and issue a trusted
        // rebase-before-anything-else instruction. No attempt exists yet, so
        // provisioning cannot consume a quality charge.
        let branch = branch.as_deref().unwrap_or_default();
        cosmix_foreman::refinery::bounce_rebase_conflict(
            ledger,
            task,
            branch,
            &integration,
            outcome,
        )?;
        eprintln!(
            "dispatch: task {task} provisioning found {branch} conflicts with {integration} \
             at {}; launching on the aborted branch with a rebase-first handoff",
            outcome.base()
        );
    }
    if let Some(outcome) = rebase.as_ref().filter(|outcome| !outcome.conflicted()) {
        cosmix_foreman::refinery::resolve_completed_rebase(ledger, task, outcome)?;
    }
    let mut settings_file: Option<RemoveOnDrop> = None;
    let mut hook_mounts: Option<cosmix_foreman::sandbox::HookMounts> = None;
    // `--policy` names an INTENT ("contain this agent"), not a mechanism.
    // The hook gate is a claude-CLI mechanism, so codex — which has no hook
    // surface — used to be refused outright here, which meant a codex rung
    // on the ladder could not start at all under the dispatch unit's
    // unconditional --policy. Codex is contained by its own sandbox instead:
    // the driver passes `--sandbox workspace-write`, verified 2026-08-19 by
    // effect to override the user config's `danger-full-access` (a write
    // outside the workspace was blocked, with a control proving the probe
    // could see one).
    //
    // What that buys, stated honestly so nobody reads "--policy" as more
    // than it is on this lane:
    //   - write containment to the worktree: YES, kernel-enforced;
    //   - the fleet's own rails (ledger.db, .foreman/, STOP) sit OUTSIDE
    //     any task worktree, so writes to them are blocked by the same
    //     sandbox;
    //   - the self-modification rail on policy.rs, which lives INSIDE the
    //     worktree: NOT gated per tool call here — it is caught at the
    //     landing by the mandatory merge-authority review;
    //   - reads outside the worktree: the vendor workspace-write mode does
    //     NOT restrict them. The optional FOREMAN_SANDBOX=bwrap view does,
    //     with only Codex's own credential bound back in; it remains off by
    //     default until the separate soak changes that setting.
    let mechanism = cosmix_foreman::policy::policy_mechanism(agent);
    if policy && mechanism == cosmix_foreman::policy::PolicyMechanism::OwnSandbox {
        println!(
            "foreman: task {task} runs on {} — contained by its own driver \
             sandbox, not the hook gate (writes confined to the worktree; \
             reads are restricted only with the default-off \
             FOREMAN_SANDBOX=bwrap view)",
            agent.as_str()
        );
    }
    if policy && mechanism == cosmix_foreman::policy::PolicyMechanism::HookGate {
        // These claude flags disable hooks entirely — accepting them would
        // advertise a policy gate that is not there.
        for bad in ["--safe-mode", "--bare"] {
            anyhow::ensure!(
                !extra_args
                    .iter()
                    .any(|a| a == bad || a.starts_with(&format!("{bad}="))),
                "--policy is incompatible with {bad} (it disables hooks)"
            );
        }
        let integration_base =
            cosmix_foreman::policy::resolve_integration_base(&workdir, &integration)
                .map_err(anyhow::Error::msg)?;
        let ctx = cosmix_foreman::policy::PolicyContext {
            task_id: task,
            worktree: workdir.clone(),
            branch: branch.clone(),
            provider: if agent == AgentKind::Glm {
                "zai".into()
            } else {
                "anthropic".into()
            },
            integration_base,
            integration_branch: integration.clone(),
            task_ref_template: manifest
                .map(|project| project.branch_template.clone())
                .unwrap_or_else(|| "task/{id}".into()),
            package_manifest_template: manifest
                .and_then(|project| project.package_manifest_template.clone())
                .or_else(|| {
                    manifest
                        .is_none()
                        .then(|| cosmix_foreman::policy::LEGACY_PACKAGE_MANIFEST_TEMPLATE.into())
                }),
            restrict_manifest_edits: manifest
                .is_some_and(|project| project.restrict_manifest_edits),
            task_crates: Vec::new(),
        };
        let db_abs = db.canonicalize().unwrap_or_else(|_| db.to_path_buf());
        let foreman_bin = std::env::current_exe().context("resolving the foreman binary path")?;
        let settings = cosmix_foreman::policy::hook_settings(
            &ctx,
            &db_abs,
            db_create,
            manifest.map(|project| project.path.as_path()),
            &foreman_bin,
        );
        // Absolute path (claude's cwd is the worktree; a relative path would
        // resolve THERE — agent-controlled ground), next to the ledger,
        // unique per invocation so concurrent runs of the same task cannot
        // swap each other's hook context.
        let settings_dir = db_abs.parent().context("ledger path has no parent")?;
        sweep_stale_configs(settings_dir);
        let settings_path = settings_dir.join(format!(
            "policy-settings-task{task}-{}.json",
            std::process::id()
        ));
        write_new(&settings_path, &serde_json::to_string_pretty(&settings)?)?;
        extra_args.push("--settings".into());
        extra_args.push(settings_path.to_string_lossy().into_owned());
        let project = manifest
            .map(|project| {
                let git_common_dir = driver::codex::git_common_dir(&project.repo)
                    .map(PathBuf::from)
                    .with_context(|| {
                        format!(
                            "resolving Git common directory for project repo {}",
                            project.repo.display()
                        )
                    })?;
                Ok::<_, anyhow::Error>(cosmix_foreman::sandbox::ProjectHookMounts {
                    manifest: project.path.clone(),
                    repo: project.repo.clone(),
                    git_common_dir,
                })
            })
            .transpose()?;
        hook_mounts = Some(cosmix_foreman::sandbox::HookMounts {
            foreman_bin,
            ledger: db_abs,
            settings: settings_path.clone(),
            project,
        });
        settings_file = Some(RemoveOnDrop(settings_path));
    }
    // Driver construction can refuse (bad budget, missing env) — do it
    // BEFORE reserving so a refusal cannot strand a hold. The typed marker
    // routes that refusal to the infrastructure counter and short backoff.
    let resume_session = ledger.latest_resumable_session(task, agent.as_str(), model.as_deref())?;
    if let Some(session) = &resume_session {
        println!("foreman: resuming resource-exhausted session {session}");
    }
    let executor = driver::build(
        agent,
        model.clone(),
        permission_mode,
        extra_args,
        fleet_policy,
        hook_mounts,
    )
    .map_err(|e| e.context(driver::RungRefusal))?;
    // Reservation, not just admission: the hold makes concurrent runs unable
    // to jointly exceed the daily ceilings.
    let governor = Governor::from_policy(db, fleet_policy);
    let reservation = if no_governor {
        None
    } else {
        // A governed reservation always injects a token hold below — refuse
        // to make one at all for a driver that can never report usage for it
        // to bite against (a lane that reports no usage cannot enforce a
        // token cap, so reserving one would sell a ceiling that is fiction).
        // comment on this call for why it's checked here, before reserve().
        cosmix_foreman::governor::require_token_cap_enforcement(
            &executor.capabilities(),
            agent.as_str(),
        )?;
        let id = ledger_write_with_busy_retry("reserving run capacity", || {
            governor.reserve(
                ledger,
                &format!("{}@{}", agent.as_str(), std::process::id()),
                Some(task),
                &budget,
                agent,
            )
        })?;
        // A governed run is CAPPED at what it reserved — a hold the run can
        // outspend would make the ceiling fiction. Tokens for every vendor
        // that can enforce them (checked just above); dollars only where the
        // driver can enforce them too (the subprocess claude lane — codex/glm
        // all refuse the flag).
        if budget.max_output_tokens.is_none() {
            budget.max_output_tokens = Some(fleet_policy.reserve_tokens.value);
        }
        if budget.max_budget_usd.is_none() && executor.capabilities().enforces_cost_cap {
            budget.max_budget_usd = Some(fleet_policy.reserve_usd.value);
        }
        Some(id)
    };
    let opts = RunOptions {
        workdir,
        budget,
        model,
        resume_session,
        stall_secs,
        echo: true,
        verify: !no_verify,
        branch,
        verify_subdir,
        allow_operator_driven,
        // A conflicted rebase was aborted, so the requested integration base
        // is not falsely recorded as an ancestor of the launched tree.
        rebased_onto: rebase
            .as_ref()
            .filter(|outcome| !outcome.conflicted())
            .map(|outcome| outcome.base().to_string()),
        profiles,
        project_pack,
    };
    let result = run_task_with_policy(ledger, task, executor.as_ref(), &opts, fleet_policy);
    if let Some(id) = reservation {
        // Actuals are in `runs` now (or the run never started); either way
        // the hold has served its purpose. On failure the hold self-heals:
        // the next governor sweep after this process exits observes its pid
        // gone and removes the row immediately; only ownerless rows wait for
        // the four-hour expiry fallback.
        if let Err(e) = ledger_write_with_busy_retry("releasing run reservation", || {
            governor.release(ledger, id)
        }) {
            eprintln!(
                "foreman: releasing reservation {id} failed ({e:#}); it will be \
                 removed by the next governor sweep after this process exits"
            );
        }
    }
    drop(settings_file);
    let report = result?;
    println!(
        "run {} finished: {} (task -> {}) in={} out={} cost={} {}ms",
        report.run_id,
        report.outcome.stop.as_str(),
        report.task_status,
        report.outcome.usage.input_tokens,
        report.outcome.usage.output_tokens,
        report
            .outcome
            .usage
            .cost_usd
            .map(|c| format!("${c:.4}"))
            .unwrap_or_else(|| "?".into()),
        report.duration_ms,
    );
    if let Some(err) = &report.outcome.error {
        eprintln!("error: {err}");
    }
    Ok(report.task_status)
}
