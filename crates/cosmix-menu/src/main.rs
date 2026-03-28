//! cosmix-menu — System tray app launcher for the Cosmix desktop.
//!
//! Uses StatusNotifierItem (SNI) protocol via ksni to register as a tray
//! client with the existing COSMIC desktop watcher.
//!
//! Discovers Cosmix desktop apps from ~/.local/share/applications/
//! (Categories=Cosmix). Click the tray icon to see the menu.

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

    apps.sort_by(|a, b| {
        // Shell always first
        let a_shell = a.exec.contains("cosmix-shell");
        let b_shell = b.exec.contains("cosmix-shell");
        b_shell.cmp(&a_shell).then(a.name.cmp(&b.name))
    });
    apps
}

fn parse_desktop_entry(path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut name = None;
    let mut exec = None;
    let mut categories = String::new();
    let mut no_display = false;
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
            let clean = val
                .split_whitespace()
                .take_while(|s| !s.starts_with('%'))
                .collect::<Vec<_>>()
                .join(" ");
            exec = Some(clean);
        } else if let Some(val) = line.strip_prefix("Categories=") {
            categories = val.to_string();
        } else if let Some(val) = line.strip_prefix("NoDisplay=") {
            no_display = val.trim() == "true";
        }
    }

    if !categories.split(';').any(|c| c == "Cosmix") {
        return None;
    }

    if no_display {
        return None;
    }

    Some(AppEntry {
        name: name?,
        exec: exec?,
    })
}

// ── Action handlers ──

fn launch_app(exec: &str) {
    let parts: Vec<&str> = exec.split_whitespace().collect();
    if let Some((cmd, args)) = parts.split_first() {
        let _ = Command::new(cmd).args(args).spawn();
    }
}

// ── SNI Tray ──

struct CosmixTray;

impl ksni::Tray for CosmixTray {
    fn id(&self) -> String {
        "cosmix-menu".into()
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
