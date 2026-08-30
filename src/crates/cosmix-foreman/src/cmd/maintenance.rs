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
        Cmd::FleetCheck { binary, all } => {
            // Deliberately no ledger: a deploy may run this before the DB
            // exists, and unit health has nothing to do with it anyway.
            let mtime = match &binary {
                Some(path) => {
                    let mtime = unit_health::binary_mtime(path);
                    if mtime.is_none() {
                        // Say so rather than silently comparing against
                        // nothing: a deploy-age check that quietly can't
                        // run is the exact failure mode this command
                        // exists to end.
                        println!(
                            "unit health: cannot read {} — deploy-age check skipped",
                            path.display()
                        );
                    }
                    mtime
                }
                None => unit_health::current_binary_mtime(),
            };
            let health = unit_health::check_fleet_health(mtime);
            for line in unit_health::render_text(&health) {
                println!("{line}");
            }
            if all {
                for u in &health.active {
                    println!(
                        "unit health: {} active for {}s (started {})",
                        u.name,
                        u.running_secs.unwrap_or(0),
                        u.started_at.as_deref().unwrap_or("an unknown time"),
                    );
                }
            }
        }
        Cmd::Wake => {
            if wake::fire(wake::WAKE_VERB) {
                println!("wake accepted");
            } else {
                println!("wake not accepted (no citizen/broker) — backstop timer covers it");
            }
        }
        Cmd::GcScratch {
            fleet_dir,
            repo,
            terminal_age_hours,
            pool,
            pressure_percent,
            shared_max_gb,
            as_of,
            dry_run,
            confirm,
        } => {
            anyhow::ensure!(
                dry_run || confirm,
                "gc-scratch would delete real scratch; pass --confirm to run it for real \
                 (the installed timer's ExecStart always does) or --dry-run to preview first"
            );
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            let repo = resolve_project_repo_arg(repo, manifest.as_ref(), "--repo")?
                .context("gc-scratch requires --repo without --project")?;
            let fleet_dir = match (fleet_dir, manifest.as_ref()) {
                (Some(explicit), Some(project)) => {
                    let canonical = explicit
                        .canonicalize()
                        .with_context(|| format!("--fleet-dir {}", explicit.display()))?;
                    anyhow::ensure!(
                        canonical == project.root,
                        "--project {:?} fixes --fleet-dir to {}; refusing {}",
                        project.name,
                        project.root.display(),
                        canonical.display()
                    );
                    project.root.clone()
                }
                (None, Some(project)) => project.root.clone(),
                (Some(explicit), None) => explicit,
                (None, None) => {
                    anyhow::bail!("gc-scratch requires --fleet-dir without --project")
                }
            };
            let options = scratch::ScratchOptions {
                fleet_dir,
                repo,
                terminal_age_hours: terminal_age_hours
                    .unwrap_or(fleet_policy.scratch_terminal_age_hours.value),
                pressure_pool: pool.or_else(|| fleet_policy.scratch_pool.value.clone()),
                pressure_percent: pressure_percent
                    .unwrap_or(fleet_policy.scratch_pressure_percent.value),
                shared_max_gb: shared_max_gb.unwrap_or(fleet_policy.scratch_shared_max_gb.value),
                selection_time: as_of.unwrap_or_else(chrono::Utc::now),
                dry_run,
            };
            let report = scratch::sweep(&ledger, &options)?;
            for line in report.summary_lines() {
                println!("{line}");
            }
            if report.failed() {
                anyhow::bail!(
                    "gc-scratch completed with reported refusals/errors; see the sweep output above"
                );
            }
        }
        Cmd::GcCache { dir, max_gb } => {
            let dir = gc::resolve_target_dir(
                dir.or_else(|| manifest.as_ref().map(|project| project.cache_dir.clone())),
            )?;
            let report = gc::run_gc(&dir, max_gb)?;
            println!("{}", report.summary());
            match report.outcome {
                gc::GcOutcome::UnderCap => println!("cache already under cap — nothing to do"),
                gc::GcOutcome::Trimmed => {}
                // Over the cap with nothing left it is allowed to reclaim.
                // This is the state a "nothing to do" line used to hide, so
                // it exits non-zero: the nightly step that runs GC first
                // goes red instead of green-forever while the cache grows.
                gc::GcOutcome::StillOverCap => anyhow::bail!(
                    "{} is still over the cap after GC — nothing further is reclaimable \
                     under {{debug,release}}/{{deps,build,.fingerprint}}{}",
                    dir.display(),
                    if report.skipped_uncontained > 0 {
                        " (see the skipped, uncontained entries above)"
                    } else {
                        " (the bloat is outside those subdirs)"
                    }
                ),
            }
        }
        _ => unreachable!("maintenance command router mismatch"),
    }
    Ok(())
}
