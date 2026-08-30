use super::launch::launch;
use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db,
        db_create,
        fleet_policy,
    } = context;
    match command {
        Cmd::Run {
            task,
            agent,
            model,
            workdir,
            max_turns,
            max_budget_usd,
            max_output_tokens,
            max_wall_secs,
            stall_secs,
            permission_mode,
            extra_args,
            branch,
            integration,
            subdir,
            no_governor,
            no_verify,
            policy,
        } => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            let workdir = resolve_project_repo_arg(workdir, manifest.as_ref(), "--workdir")?
                .unwrap_or_else(|| PathBuf::from("."));
            let integration = resolve_project_integration_arg(integration, manifest.as_ref())?;
            let subdir = subdir.or_else(|| manifest.as_ref().and_then(|m| m.subdir.clone()));
            if let Some(usd) = max_budget_usd {
                anyhow::ensure!(
                    usd.is_finite() && usd >= 0.0,
                    "--max-budget-usd must be a finite non-negative value"
                );
            }
            let spec = LaunchSpec {
                task,
                agent,
                model,
                workdir,
                budget: Budget {
                    max_turns,
                    max_budget_usd,
                    max_output_tokens,
                    max_wall_secs,
                },
                stall_secs,
                permission_mode,
                extra_args,
                branch,
                integration,
                verify_subdir: subdir,
                no_governor,
                no_verify,
                policy,
                allow_operator_driven: true,
                worktree_template: manifest
                    .as_ref()
                    .map(|m| m.worktree_template.clone())
                    .unwrap_or_else(|| "task-{id}".to_string()),
                profiles: manifest
                    .as_ref()
                    .map(|m| m.profiles.clone())
                    .unwrap_or_default(),
                project_pack: manifest
                    .as_ref()
                    .map(|m| m.instruction_pack.clone())
                    .unwrap_or_default(),
            };
            let status = launch(
                &db,
                db_create,
                &ledger,
                spec,
                &fleet_policy,
                manifest.as_ref(),
            )?;
            if status != "done" {
                std::process::exit(1);
            }
        }
        _ => unreachable!("run command router mismatch"),
    }
    Ok(())
}
