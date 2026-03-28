use std::sync::Arc;

use dioxus::prelude::*;
use serde::Serialize;
use cosmix_ui::menu::{menubar, standard_file_menu, MenuBar};
use cosmix_ui::theme::{ThemeParams, generate_css};

// ── Theme params (loaded from config, refreshed every 30s) ──

static THEME: GlobalSignal<ThemeParams> = Signal::global(|| {
    cosmix_config::store::load()
        .map(|s| ThemeParams {
            hue: s.global.theme_hue,
            dark: s.global.theme_dark,
            font_size: s.global.font_size,
        })
        .unwrap_or_default()
});

fn main() {
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("cosmix-files", 900.0, 640.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── Data ──

#[derive(Clone, Debug, Serialize)]
struct FileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    modified: String,
}

fn read_directory(path: &str) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let meta = entry.metadata().ok();
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%Y-%m-%d %H:%M").to_string()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    let gb = mb / 1024.0;
    format!("{gb:.1} GB")
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

// ── Hub command handling ──

async fn handle_hub_commands(client: Arc<cosmix_client::HubClient>) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        let result = match cmd.command.as_str() {
            "file.list" => handle_file_list(&cmd),
            "file.read" => handle_file_read(&cmd),
            "file.stat" => handle_file_stat(&cmd),
            _ => Err(format!("unknown command: {}", cmd.command)),
        };

        match result {
            Ok(body) => {
                if let Err(e) = client.respond(&cmd, 0, &body).await {
                    tracing::warn!("failed to send response: {e}");
                }
            }
            Err(msg) => {
                let err_body =
                    serde_json::json!({"error": msg}).to_string();
                if let Err(e) = client.respond(&cmd, 10, &err_body).await {
                    tracing::warn!("failed to send error response: {e}");
                }
            }
        }
    }
}

fn handle_file_list(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let path = cmd
        .args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let entries = read_directory(path);
    serde_json::to_string(&entries).map_err(|e| e.to_string())
}

fn handle_file_read(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let path = cmd
        .args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing path argument")?;

    // Reject binary files by checking for null bytes in first 8KB
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let check = &data[..data.len().min(8192)];
    if check.contains(&0) {
        return Err("binary file".into());
    }

    let text = String::from_utf8(data).map_err(|_| "binary file".to_string())?;
    serde_json::to_string(&text).map_err(|e| e.to_string())
}

fn handle_file_stat(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let path = cmd
        .args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing path argument")?;

    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let modified = meta
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_default();

    let stat = serde_json::json!({
        "path": path,
        "is_dir": meta.is_dir(),
        "size": meta.len(),
        "modified": modified,
    });
    Ok(stat.to_string())
}

// ── UI ──

fn app() -> Element {
    let home = home_dir();
    let mut current_dir = use_signal(|| home.clone());
    let mut entries: Signal<Vec<FileEntry>> = use_signal(|| read_directory(&home));
    let mut selected: Signal<Option<usize>> = use_signal(|| None);

    // Try to connect to hub on mount (non-blocking, silent failure)
    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default("files").await {
                Ok(client) => {
                    let client = Arc::new(client);
                    tracing::info!("connected to cosmix-hub as 'files'");
                    tokio::spawn(handle_hub_commands(client));
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });
    });

    // Poll config every 30s for theme changes
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if let Ok(settings) = cosmix_config::store::load() {
                    let new = ThemeParams {
                        hue: settings.global.theme_hue,
                        dark: settings.global.theme_dark,
                        font_size: settings.global.font_size,
                    };
                    let current = THEME.read();
                    if new.font_size != current.font_size
                        || new.hue != current.hue
                        || new.dark != current.dark
                    {
                        drop(current);
                        *THEME.write() = new;
                    }
                }
            }
        });
    });

    let app_menu = menubar(vec![standard_file_menu(vec![])]);

    let mut navigate = move |path: String| {
        selected.set(None);
        let new_entries = read_directory(&path);
        entries.set(new_entries);
        current_dir.set(path);
    };

    let onkeydown = move |e: KeyboardEvent| {
        if e.modifiers().ctrl() {
            if let Key::Character(ref c) = e.key() {
                if c == "q" {
                    std::process::exit(0);
                }
            }
        }
    };

    let theme = THEME.read().clone();
    let css = generate_css(&theme);
    let fs = theme.font_size;
    let fs_sm = fs.saturating_sub(2);

    // Build breadcrumb segments from current path
    let dir = current_dir();
    let segments: Vec<(String, String)> = {
        let parts: Vec<&str> = dir.split('/').collect();
        let mut segs = Vec::new();
        segs.push(("/".to_string(), "/".to_string()));
        let mut accum = String::new();
        for part in &parts[1..] {
            if part.is_empty() {
                continue;
            }
            accum.push('/');
            accum.push_str(part);
            segs.push((part.to_string(), accum.clone()));
        }
        segs
    };

    rsx! {
        document::Style { "{css}" }
        div {
            tabindex: "0",
            onkeydown: onkeydown,
            style: "outline:none; width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-family:var(--font-sans); font-size:{fs}px;",

            MenuBar {
                menu: app_menu,
                on_action: move |id: String| match id.as_str() {
                    "quit" => std::process::exit(0),
                    _ => {}
                },
            }

            // Breadcrumb bar
            div {
                style: "display:flex; align-items:center; gap:2px; padding:8px 12px; background:var(--bg-secondary); border-bottom:1px solid var(--border); flex-shrink:0; overflow-x:auto; white-space:nowrap;",
                for (i, (label, path)) in segments.iter().enumerate() {
                    if i > 0 {
                        span { style: "color:var(--fg-muted); margin:0 2px;", "/" }
                    }
                    {
                        let path = path.clone();
                        rsx! {
                            span {
                                style: "color:var(--fg-secondary); cursor:pointer; padding:2px 4px; border-radius:var(--radius-sm);",
                                onmouseover: move |_| {},
                                onclick: move |_| navigate(path.clone()),
                                "{label}"
                            }
                        }
                    }
                }
            }

            // File list
            div {
                style: "flex:1; overflow-y:auto;",

                // Header row
                div {
                    style: "display:flex; align-items:center; padding:6px 12px; background:var(--bg-tertiary); border-bottom:1px solid var(--border); color:var(--fg-muted); font-size:{fs_sm}px; font-weight:600; text-transform:uppercase; letter-spacing:0.05em;",
                    span { style: "flex:1; min-width:0;", "Name" }
                    span { style: "width:80px; text-align:right; margin-right:16px;", "Size" }
                    span { style: "width:130px;", "Modified" }
                }

                // Parent directory entry
                {
                    let dir_clone = dir.clone();
                    let has_parent = dir_clone != "/";
                    if has_parent {
                        let parent = std::path::Path::new(&*dir_clone)
                            .parent()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|| "/".to_string());
                        rsx! {
                            div {
                                style: "display:flex; align-items:center; padding:5px 12px; cursor:pointer; border-bottom:1px solid var(--border-muted);",
                                onclick: move |_| navigate(parent.clone()),
                                onmouseenter: move |_| {},
                                span {
                                    style: "margin-right:8px; color:var(--fg-muted);",
                                    dangerous_inner_html: "{ICON_FOLDER}"
                                }
                                span { style: "flex:1; color:var(--fg-secondary);", ".." }
                            }
                        }
                    } else {
                        rsx! {}
                    }
                }

                // File entries
                for (idx, entry) in entries().iter().enumerate() {
                    {
                        let entry_path = entry.path.clone();
                        let entry_is_dir = entry.is_dir;
                        let is_selected = selected() == Some(idx);
                        let bg = if is_selected { "var(--bg-tertiary)" } else { "transparent" };
                        let icon = if entry.is_dir { ICON_FOLDER } else { ICON_FILE };
                        let icon_color = if entry.is_dir { "var(--accent)" } else { "var(--fg-muted)" };
                        let name_color = if entry.is_dir { "var(--fg-primary)" } else { "var(--fg-secondary)" };
                        let size_str = if entry.is_dir { String::new() } else { format_size(entry.size) };
                        let name = entry.name.clone();
                        let modified = entry.modified.clone();

                        rsx! {
                            div {
                                style: "display:flex; align-items:center; padding:5px 12px; cursor:pointer; border-bottom:1px solid var(--border-muted); background:{bg};",
                                onclick: move |_| {
                                    if entry_is_dir {
                                        navigate(entry_path.clone());
                                    } else {
                                        selected.set(Some(idx));
                                    }
                                },
                                span {
                                    style: "margin-right:8px; color:{icon_color};",
                                    dangerous_inner_html: "{icon}"
                                }
                                span { style: "flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; color:{name_color};", "{name}" }
                                span { style: "width:80px; text-align:right; margin-right:16px; color:var(--fg-muted); font-size:{fs_sm}px;", "{size_str}" }
                                span { style: "width:130px; color:var(--fg-muted); font-size:{fs_sm}px;", "{modified}" }
                            }
                        }
                    }
                }
            }

            // Status bar
            div {
                style: "padding:4px 12px; background:var(--bg-secondary); border-top:1px solid var(--border); color:var(--fg-muted); font-size:{fs_sm}px; flex-shrink:0;",
                "{entries().len()} items"
            }
        }
    }
}

// ── Icons ──

const ICON_FOLDER: &str = cosmix_ui::icons::ICON_FOLDER;
const ICON_FILE: &str = cosmix_ui::icons::ICON_FILE_EDIT;
