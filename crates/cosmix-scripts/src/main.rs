//! cosmix-scripts — Lua + Bash script manager for Cosmix.
//!
//! Discovers scripts from ~/.local/scripts/ (*.lua, *.sh).
//! Provides list, run, edit, delete, and new subcommands.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    Run { name: String },
    /// Open a script in cosmix-edit
    Edit { name: String },
    /// Delete a script (moves to trash)
    Delete { name: String },
    /// Create a new script
    New {
        /// Script name (without extension)
        name: Option<String>,
        /// Language: lua or bash (default: lua)
        #[arg(short, long, default_value = "lua")]
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
                "lua" => "lua",
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

fn run_script(path: &Path) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("lua");
    let interpreter = match ext {
        "sh" => "bash",
        _ => "lua",
    };

    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    match Command::new(interpreter).arg(path).output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            if text.is_empty() {
                println!("{name}: (no output)");
            } else {
                // Try cosmix-dialog for GUI display, fall back to stdout
                let mut child = Command::new("cosmix-dialog")
                    .args(["text-info", "--title", &name])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .ok();
                if let Some(ref mut c) = child {
                    use std::io::Write;
                    if let Some(ref mut stdin) = c.stdin {
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = c.wait();
                } else {
                    print!("{text}");
                }
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

    // Try cosmix-dialog for confirmation, fall back to just deleting
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
        _ => "lua",
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
        _ => "#!/usr/bin/env lua\n-- New cosmix script\n\nprint(\"hello from cosmix\")\n",
    };

    let _ = std::fs::write(&path, template);
    println!("Created {}", path.display());
    edit_script(&path);
}

fn main() {
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
        Some(Cmd::Run { name }) => match find_script(&name) {
            Some(s) => run_script(&s.path),
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
