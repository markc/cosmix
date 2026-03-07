mod wayland;

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("list-windows");

    match command {
        "list-windows" | "lw" => wayland::toplevel::list_windows()?,
        "list-workspaces" | "ls" => wayland::workspace::list_workspaces()?,
        _ => {
            eprintln!("Usage: cosmix <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  list-windows (lw)      List all toplevel windows");
            eprintln!("  list-workspaces (ls)    List all workspaces");
        }
    }

    Ok(())
}
