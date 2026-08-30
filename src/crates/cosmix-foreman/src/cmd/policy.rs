use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db: _,
        db_create: _,
        fleet_policy: _,
    } = context;
    match command {
        Cmd::PolicyCheck {
            task,
            worktree,
            provider,
            branch,
            integration_base,
        } => {
            let ctx = cosmix_foreman::policy::PolicyContext {
                task_id: task,
                worktree,
                branch,
                provider,
                integration_base,
                integration_branch: manifest
                    .as_ref()
                    .map(|project| project.integration.clone())
                    .unwrap_or_else(|| "main".into()),
                task_ref_template: manifest
                    .as_ref()
                    .map(|project| project.branch_template.clone())
                    .unwrap_or_else(|| "task/{id}".into()),
                package_manifest_template: manifest
                    .as_ref()
                    .and_then(|project| project.package_manifest_template.clone())
                    .or_else(|| {
                        manifest.is_none().then(|| {
                            cosmix_foreman::policy::LEGACY_PACKAGE_MANIFEST_TEMPLATE.into()
                        })
                    }),
                restrict_manifest_edits: manifest
                    .as_ref()
                    .is_some_and(|project| project.restrict_manifest_edits),
                task_crates: Vec::new(),
            };
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            std::process::exit(cosmix_foreman::policy::run_check(&ctx, &ledger));
        }
        _ => unreachable!("policy command router mismatch"),
    }
}
