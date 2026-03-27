//! cosmix-launcher — System tray launcher for Cosmix apps and Lua scripts.
//!
//! Uses StatusNotifierItem (SNI) protocol via ksni to register as a tray
//! client with the existing COSMIC desktop watcher.
//!
//! Discovers:
//! - Cosmix desktop apps from ~/.local/share/applications/ (Categories=Cosmix)
//! - Lua scripts from ~/.local/lua/*.lua
//!
//! Click the tray icon to see the menu. Scripts have Run/Edit/Delete options.

use std::path::{Path, PathBuf};
use std::process::Command;

use ksni::{self, blocking::TrayMethods, MenuItem as KsniMenuItem};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── Discovery ──

struct AppEntry {
    name: String,
    exec: String,
}

struct ScriptEntry {
    name: String,
    path: PathBuf,
}

fn discover_cosmix_apps() -> Vec<AppEntry> {
    let app_dir = dirs_next::data_dir()
        .map(|d| d.join("applications"))
        .unwrap_or_else(|| PathBuf::from("/usr/share/applications"));

    let mut apps = Vec::new();

    let Ok(entries) = std::fs::read_dir(&app_dir) else {
        return apps;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "desktop") {
            if let Some(app) = parse_desktop_entry(&path) {
                apps.push(app);
            }
        }
    }

    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}

fn parse_desktop_entry(path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut name = None;
    let mut exec = None;
    let mut categories = String::new();
    let mut in_desktop_entry = false;

    for line in content.lines() {
        let line = line.trim();
        if line == "[Desktop Entry]" {
            in_desktop_entry = true;
            continue;
        }
        if line.starts_with('[') {
            in_desktop_entry = false;
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some(val) = line.strip_prefix("Name=") {
            name = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("Exec=") {
            let clean = val.split_whitespace()
                .take_while(|s| !s.starts_with('%'))
                .collect::<Vec<_>>()
                .join(" ");
            exec = Some(clean);
        } else if let Some(val) = line.strip_prefix("Categories=") {
            categories = val.to_string();
        }
    }

    if !categories.split(';').any(|c| c == "Cosmix") {
        return None;
    }

    Some(AppEntry {
        name: name?,
        exec: exec?,
    })
}

fn lua_scripts_dir() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/lua")
}

fn discover_lua_scripts() -> Vec<ScriptEntry> {
    let dir = lua_scripts_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut scripts: Vec<ScriptEntry> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "lua"))
        .map(|e| ScriptEntry {
            name: e.path().file_stem().unwrap_or_default().to_string_lossy().to_string(),
            path: e.path(),
        })
        .collect();

    scripts.sort_by(|a, b| a.name.cmp(&b.name));
    scripts
}

// ── Action handlers ──

fn launch_app(exec: &str) {
    let parts: Vec<&str> = exec.split_whitespace().collect();
    if let Some((cmd, args)) = parts.split_first() {
        let _ = Command::new(cmd).args(args).spawn();
    }
}

fn run_script(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        match Command::new("lua").arg(&path).output() {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout).to_string()
                    + &String::from_utf8_lossy(&output.stderr);
                if !text.is_empty() {
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
                    }
                }
            }
            Err(e) => {
                let _ = Command::new("cosmix-dialog")
                    .args(["error", "--text", &format!("Failed to run script: {e}")])
                    .spawn();
            }
        }
    });
}

fn edit_script(path: &Path) {
    if Command::new("cosmix-edit").arg(path).spawn().is_err() {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "xdg-open".into());
        let _ = Command::new(&editor).arg(path).spawn();
    }
}

fn delete_script(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let status = Command::new("cosmix-dialog")
            .args(["confirm", "--text", &format!("Delete '{name}'?")])
            .status();
        if status.is_ok_and(|s| s.success()) {
            let trash_dir = dirs_next::data_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("Trash/files");
            let _ = std::fs::create_dir_all(&trash_dir);
            let _ = std::fs::rename(&path, trash_dir.join(&name));
        }
    });
}

fn new_script() {
    let dir = lua_scripts_dir();
    let _ = std::fs::create_dir_all(&dir);

    let mut i = 1;
    let path = loop {
        let name = if i == 1 {
            dir.join("new-script.lua")
        } else {
            dir.join(format!("new-script-{i}.lua"))
        };
        if !name.exists() {
            break name;
        }
        i += 1;
    };

    let _ = std::fs::write(&path, "#!/usr/bin/env lua\n-- New cosmix script\n\nprint(\"hello from cosmix\")\n");
    edit_script(&path);
}

fn open_scripts_folder() {
    let dir = lua_scripts_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = Command::new("xdg-open").arg(&dir).spawn();
}

// ── SNI Tray ──

struct CosmixTray;

impl ksni::Tray for CosmixTray {
    fn id(&self) -> String {
        "cosmix-launcher".into()
    }

    fn title(&self) -> String {
        "Cosmix".into()
    }

    fn icon_name(&self) -> String {
        "application-x-executable".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn menu(&self) -> Vec<KsniMenuItem<Self>> {
        let mut items: Vec<KsniMenuItem<Self>> = Vec::new();

        // Cosmix Apps
        let apps = discover_cosmix_apps();
        for app in apps {
            let exec = app.exec.clone();
            items.push(KsniMenuItem::Standard(ksni::menu::StandardItem {
                label: app.name,
                activate: Box::new(move |_| launch_app(&exec)),
                ..Default::default()
            }));
        }

        items.push(KsniMenuItem::Separator);

        // Lua Scripts
        let scripts = discover_lua_scripts();
        for script in scripts {
            let run_path = script.path.clone();
            let edit_path = script.path.clone();
            let del_path = script.path.clone();

            items.push(KsniMenuItem::SubMenu(ksni::menu::SubMenu {
                label: script.name,
                submenu: vec![
                    KsniMenuItem::Standard(ksni::menu::StandardItem {
                        label: "Run".into(),
                        activate: Box::new(move |_| run_script(&run_path)),
                        ..Default::default()
                    }),
                    KsniMenuItem::Standard(ksni::menu::StandardItem {
                        label: "Edit".into(),
                        activate: Box::new(move |_| edit_script(&edit_path)),
                        ..Default::default()
                    }),
                    KsniMenuItem::Standard(ksni::menu::StandardItem {
                        label: "Delete".into(),
                        activate: Box::new(move |_| delete_script(&del_path)),
                        ..Default::default()
                    }),
                ],
                ..Default::default()
            }));
        }

        items.push(KsniMenuItem::Separator);

        items.push(KsniMenuItem::Standard(ksni::menu::StandardItem {
            label: "New Script...".into(),
            activate: Box::new(|_| new_script()),
            ..Default::default()
        }));

        items.push(KsniMenuItem::Standard(ksni::menu::StandardItem {
            label: "Open Scripts Folder".into(),
            activate: Box::new(|_| open_scripts_folder()),
            ..Default::default()
        }));

        items.push(KsniMenuItem::Separator);

        items.push(KsniMenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_| std::process::exit(0)),
            ..Default::default()
        }));

        items
    }
}

// ── Main ──

fn main() {
    let _handle = CosmixTray.spawn().expect("failed to create tray service");

    // Block forever — the tray runs on a background thread
    loop {
        std::thread::park();
    }
}
