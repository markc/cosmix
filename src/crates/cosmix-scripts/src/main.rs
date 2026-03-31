//! cosmix-scripts — Script manager for Cosmix.
//!
//! Discovers scripts from ~/.local/scripts/ (*.mix, *.sh).
//! Runs .mix files with the embedded mix-core evaluator (AMP-enabled).
//! Runs .sh files with bash.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use clap::{Parser, Subcommand};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "cosmix-scripts", about = "Cosmix script manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List all scripts
    List,
    /// Run a script by name
    Run {
        name: String,
        /// Connect to the AMP hub for IPC (default: try, but don't fail)
        #[arg(long)]
        no_hub: bool,
    },
    /// Open a script in cosmix-edit
    Edit { name: String },
    /// Delete a script (moves to trash)
    Delete { name: String },
    /// Create a new script
    New {
        /// Script name (without extension)
        name: Option<String>,
        /// Language: mix or bash (default: mix)
        #[arg(short, long, default_value = "mix")]
        lang: String,
    },
    /// Open the scripts folder in the file manager
    OpenFolder,
}

fn scripts_dir() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/scripts")
}

struct ScriptEntry {
    name: String,
    path: PathBuf,
    lang: &'static str,
}

fn discover_scripts() -> Vec<ScriptEntry> {
    let dir = scripts_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut scripts: Vec<ScriptEntry> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            let lang = match ext {
                "mix" => "mix",
                "sh" => "bash",
                _ => return None,
            };
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            Some(ScriptEntry { name, path, lang })
        })
        .collect();

    scripts.sort_by(|a, b| a.name.cmp(&b.name));
    scripts
}

fn find_script(name: &str) -> Option<ScriptEntry> {
    discover_scripts().into_iter().find(|s| s.name == name)
}

async fn run_mix_script(path: &Path, no_hub: bool) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    // Try connecting to the hub for AMP IPC (optional)
    let hub = if no_hub {
        None
    } else {
        cosmix_client::HubClient::connect_anonymous_default()
            .await
            .ok()
    };

    if let Some(hub) = hub {
        let hub = Arc::new(hub);
        let result = cosmix_script::execute_mix(
            &source,
            hub,
            "scripts",
            &HashMap::new(),
        )
        .await;

        if let Some(ref body) = result.body {
            print!("{body}");
        }
        if let Some(ref err) = result.error {
            eprintln!("{err}");
            std::process::exit(1);
        }
    } else {
        // Run without AMP — basic Mix execution only
        match mix_core::run_capturing(&source).await {
            Ok((_val, stdout, stderr)) => {
                if !stdout.is_empty() {
                    print!("{stdout}");
                }
                if !stderr.is_empty() {
                    eprint!("{stderr}");
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_bash_script(path: &Path) {
    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    match Command::new("bash").arg(path).output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            if text.is_empty() {
                println!("{name}: (no output)");
            } else {
                print!("{text}");
            }
        }
        Err(e) => {
            eprintln!("Failed to run {name}: {e}");
            std::process::exit(1);
        }
    }
}

fn edit_script(path: &Path) {
    if Command::new("cosmix-edit").arg(path).spawn().is_err() {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "xdg-open".into());
        let _ = Command::new(&editor).arg(path).spawn();
    }
}

fn delete_script(path: &Path) {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let confirmed = Command::new("cosmix-dialog")
        .args(["confirm", "--text", &format!("Delete '{name}'?")])
        .status()
        .is_ok_and(|s| s.success());

    if !confirmed {
        return;
    }

    let trash_dir = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Trash/files");
    let _ = std::fs::create_dir_all(&trash_dir);
    match std::fs::rename(path, trash_dir.join(&name)) {
        Ok(()) => println!("Moved {name} to trash"),
        Err(e) => eprintln!("Failed to delete {name}: {e}"),
    }
}

fn new_script(name: Option<&str>, lang: &str) {
    let dir = scripts_dir();
    let _ = std::fs::create_dir_all(&dir);

    let ext = match lang {
        "bash" | "sh" => "sh",
        _ => "mix",
    };

    let path = if let Some(name) = name {
        dir.join(format!("{name}.{ext}"))
    } else {
        let mut i = 1;
        loop {
            let candidate = if i == 1 {
                dir.join(format!("new-script.{ext}"))
            } else {
                dir.join(format!("new-script-{i}.{ext}"))
            };
            if !candidate.exists() {
                break candidate;
            }
            i += 1;
        }
    };

    if path.exists() {
        eprintln!("Script already exists: {}", path.display());
        std::process::exit(1);
    }

    let template = match ext {
        "sh" => "#!/usr/bin/env bash\n# New cosmix script\nset -euo pipefail\n\necho \"hello from cosmix\"\n",
        _ => "-- @script New Script\n-- @description A new Mix script\n\nprint \"hello from cosmix\"\n",
    };

    let _ = std::fs::write(&path, template);
    println!("Created {}", path.display());
    edit_script(&path);
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        None | Some(Cmd::List) => {
            let scripts = discover_scripts();
            if scripts.is_empty() {
                println!("No scripts found in {}", scripts_dir().display());
                return;
            }
            for s in &scripts {
                println!("  {} ({})", s.name, s.lang);
            }
        }
        Some(Cmd::Run { name, no_hub }) => match find_script(&name) {
            Some(s) => match s.lang {
                "mix" => run_mix_script(&s.path, no_hub).await,
                _ => run_bash_script(&s.path),
            },
            None => {
                eprintln!("Script not found: {name}");
                std::process::exit(1);
            }
        },
        Some(Cmd::Edit { name }) => match find_script(&name) {
            Some(s) => edit_script(&s.path),
            None => {
                eprintln!("Script not found: {name}");
                std::process::exit(1);
            }
        },
        Some(Cmd::Delete { name }) => match find_script(&name) {
            Some(s) => delete_script(&s.path),
            None => {
                eprintln!("Script not found: {name}");
                std::process::exit(1);
            }
        },
        Some(Cmd::New { name, lang }) => {
            new_script(name.as_deref(), &lang);
        }
        Some(Cmd::OpenFolder) => {
            let dir = scripts_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = Command::new("xdg-open").arg(&dir).spawn();
        }
    }
}
