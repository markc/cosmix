use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Cmd::AttachmentHarm {
        claude_projects,
        limit,
        json,
    } = command
    else {
        unreachable!("attachment-harm command module called with another command");
    };
    let root = claude_projects.unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/unavailable-home"))
            .join(".claude/projects")
    });
    let report = cosmix_foreman::attachment_harm::analyse(
        &cosmix_foreman::attachment_harm::AnalysisOptions {
            claude_projects: root,
            ledger: Some(context.db),
            limit,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", cosmix_foreman::attachment_harm::render_text(&report));
    }
    Ok(())
}
