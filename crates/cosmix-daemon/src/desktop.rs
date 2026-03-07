use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub icon: String,
    pub comment: String,
    pub categories: String,
    pub terminal: bool,
    pub no_display: bool,
}

pub fn list_apps() -> Result<Vec<DesktopEntry>> {
    let mut apps = Vec::new();
    let dirs = [
        "/usr/share/applications",
        "/usr/local/share/applications",
        &format!(
            "{}/.local/share/applications",
            std::env::var("HOME").unwrap_or_default()
        ),
    ];

    for dir in &dirs {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(dir_path)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Ok(app) = parse_desktop_file(&path) {
                    if !app.no_display && !app.exec.is_empty() {
                        apps.push(app);
                    }
                }
            }
        }
    }

    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps.dedup_by(|a, b| a.name == b.name);
    Ok(apps)
}

fn parse_desktop_file(path: &Path) -> Result<DesktopEntry> {
    let content = std::fs::read_to_string(path)?;
    let mut entry = DesktopEntry {
        name: String::new(),
        exec: String::new(),
        icon: String::new(),
        comment: String::new(),
        categories: String::new(),
        terminal: false,
        no_display: false,
    };

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

        if let Some((key, val)) = line.split_once('=') {
            match key.trim() {
                "Name" => entry.name = val.trim().to_string(),
                "Exec" => {
                    // Strip field codes like %f %u %F %U
                    entry.exec = val
                        .trim()
                        .replace(" %f", "")
                        .replace(" %F", "")
                        .replace(" %u", "")
                        .replace(" %U", "")
                        .replace(" %i", "")
                        .replace(" %c", "")
                        .replace(" %k", "");
                }
                "Icon" => entry.icon = val.trim().to_string(),
                "Comment" => entry.comment = val.trim().to_string(),
                "Categories" => entry.categories = val.trim().to_string(),
                "Terminal" => entry.terminal = val.trim() == "true",
                "NoDisplay" => entry.no_display = val.trim() == "true",
                _ => {}
            }
        }
    }

    Ok(entry)
}
