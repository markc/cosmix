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
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "list-windows" | "lw" => wayland::toplevel::list_windows()?,
        "list-workspaces" | "lws" => wayland::workspace::list_workspaces()?,
        "activate" | "a" => {
            let query = args.get(2).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: cosmix activate <app_id or title>");
                std::process::exit(1);
            });
            wayland::toplevel::activate_window(query)?;
        }
        "close" | "c" => {
            let query = args.get(2).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: cosmix close <app_id or title>");
                std::process::exit(1);
            });
            wayland::toplevel::close_window(query)?;
        }
        "minimize" | "min" => {
            let query = args.get(2).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: cosmix minimize <app_id or title>");
                std::process::exit(1);
            });
            wayland::toplevel::minimize_window(query)?;
        }
        "maximize" | "max" => {
            let query = args.get(2).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: cosmix maximize <app_id or title>");
                std::process::exit(1);
            });
            wayland::toplevel::maximize_window(query)?;
        }
        _ => {
            eprintln!("cosmix — ARexx for COSMIC");
            eprintln!();
            eprintln!("Usage: cosmix <command> [args]");
            eprintln!();
            eprintln!("Query:");
            eprintln!("  list-windows  (lw)     List all toplevel windows");
            eprintln!("  list-workspaces (lws)  List all workspaces");
            eprintln!();
            eprintln!("Window control:");
            eprintln!("  activate (a) <query>   Activate/focus a window");
            eprintln!("  close (c) <query>      Close a window");
            eprintln!("  minimize (min) <query>  Toggle minimize");
            eprintln!("  maximize (max) <query>  Toggle maximize");
            eprintln!();
            eprintln!("Query matches against app_id and title (case-insensitive, substring).");
        }
    }

    Ok(())
}
