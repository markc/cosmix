use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db: _,
        db_create: _,
        fleet_policy,
    } = context;
    match command {
        Cmd::Verify {
            dir,
            profile,
            tier,
            task,
        } => {
            // Profile-owned cwd wins over --dir, same as tier 0's
            // run_profile: a profile that names its own directory verifies
            // there regardless of what the caller pointed --dir at. Built-in
            // profiles (rust, none) have no owned cwd, so --dir is used
            // as-is, unchanged from before profiles could own a directory.
            let dir = resolve_project_repo_arg(dir, manifest.as_ref(), "--dir")?
                .unwrap_or_else(|| PathBuf::from("."));
            let profile = profile
                .or_else(|| manifest.as_ref().map(|project| project.verifier.clone()))
                .unwrap_or_else(|| "rust".to_string());
            let profile = resolve_profile(manifest.as_ref(), &profile)?;
            let _project_clone_lane = if tier == 2 {
                match &manifest {
                    Some(project) => {
                        cosmix_foreman::clone_lock::acquire_lane_in_project(&project.root)?
                    }
                    None => None,
                }
            } else {
                None
            };
            let task_context = match task {
                Some(task) => {
                    let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
                    let row = ledger
                        .task(task)?
                        .with_context(|| format!("no task {task}"))?;
                    Some((task, ledger, row))
                }
                None => None,
            };
            let crates = task_context
                .as_ref()
                .map(|(_, _, row)| row.crates.as_slice())
                .unwrap_or_default();
            let gate_task = task_context
                .as_ref()
                .map(|(task, _, row)| (*task, row.attempt));
            let request = cosmix_foreman::verify::GateRequest::operator_local(
                tier,
                &dir,
                &profile,
                manifest
                    .as_ref()
                    .and_then(|project| project.subdir.as_deref()),
                crates,
                &fleet_policy,
                gate_task,
            )?;
            let report = cosmix_foreman::verify::GateRunner::run_gate(
                &cosmix_foreman::verify::LOCAL_GATE_RUNNER,
                &request,
            )?;
            if let Some((task, ledger, task_row)) = task_context {
                let report_json = serde_json::to_string(&report)?;
                if let Some(run_id) = ledger.latest_implementation_run(task)? {
                    ledger.record_run_verification(
                        task,
                        run_id,
                        tier as i64,
                        report.pass,
                        &report_json,
                    )?;
                    if task_row.status == "landed" {
                        ledger.set_run_quality(
                            run_id,
                            if report.pass {
                                "landed"
                            } else {
                                "post_land_regression"
                            },
                        )?;
                    }
                } else {
                    ledger.record_verification(task, tier as i64, report.pass, &report_json)?;
                }
                cosmix_foreman::verify::file_sccache_bypass_findings(
                    &ledger,
                    task,
                    &report,
                    "foreman verify",
                )?;
            }
            print_verify_report(&report);
            if !report.pass {
                std::process::exit(1);
            }
        }
        Cmd::PhysicalAcceptance {
            dir,
            device,
            connector,
            max_secs,
            take_vt_and_display,
        } => {
            let dir = resolve_project_repo_arg(dir, manifest.as_ref(), "--dir")?
                .unwrap_or_else(|| PathBuf::from("."));
            // Clap requires the acknowledgement flag. Keep the value in the
            // match so a future parser refactor cannot silently stop checking
            // it while leaving an apparently deliberate flag in --help.
            anyhow::ensure!(
                take_vt_and_display,
                "--take-vt-and-display is required for PHYSICAL acceptance"
            );
            let report = cosmix_foreman::verify::run_compositor_physical_acceptance_with_policy(
                &dir,
                &device,
                &connector,
                Duration::from_secs(max_secs),
                &fleet_policy,
            )?;
            print_verify_report(&report);
            if !report.pass {
                std::process::exit(1);
            }
        }
        _ => unreachable!("verify command router mismatch"),
    }
    Ok(())
}
