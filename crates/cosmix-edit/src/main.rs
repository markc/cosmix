//! cosmix-edit — Text editor service for the cosmix appmesh.
//!
//! Registers as "edit" on the hub. Handles:
//! - `edit.open` — open a file by path
//! - `edit.goto` — jump to line number
//! - `edit.compose` — open editor with prefilled content (for email drafts etc.)
//! - `edit.get` — return current editor content
//!
//! Other apps delegate: mail sends `edit.compose`, files sends `edit.open`.

use std::sync::Arc;

use dioxus::prelude::*;
use cosmix_ui::menu::{action_shortcut, menubar, standard_file_menu, separator, submenu, MenuBar, Shortcut};
use cosmix_ui::theme::{ThemeParams, generate_css};

// ── Theme params (loaded from config, updated via hub config.changed) ──

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
        let cfg = cosmix_ui::desktop::window_config("cosmix-edit", 800.0, 600.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── Shared state for hub commands to update the editor ──

static OPEN_REQUEST: GlobalSignal<Option<OpenRequest>> = Signal::global(|| None);

#[derive(Clone, Debug)]
struct OpenRequest {
    path: Option<String>,
    content: Option<String>,
    line: Option<usize>,
}

// ── Hub command handling ──

async fn handle_hub_commands(client: Arc<cosmix_client::HubClient>) {
    // Register for config change notifications
    let _ = client.call(
        "configd",
        "config.watch",
        serde_json::json!({ "watcher": "edit" }),
    ).await;

    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        let result = match cmd.command.as_str() {
            "edit.open" => handle_edit_open(&cmd),
            "edit.goto" => handle_edit_goto(&cmd),
            "edit.compose" => handle_edit_compose(&cmd),
            "edit.get" => Ok(r#"{"status": "ok"}"#.to_string()),
            "config.changed" => {
                if let Ok(settings) = cosmix_config::store::load() {
                    *THEME.write() = ThemeParams {
                        hue: settings.global.theme_hue,
                        dark: settings.global.theme_dark,
                        font_size: settings.global.font_size,
                    };
                }
                Ok(r#"{"status": "ok"}"#.to_string())
            }
            _ => Err(format!("unknown command: {}", cmd.command)),
        };

        match result {
            Ok(body) => {
                if let Err(e) = client.respond(&cmd, 0, &body).await {
                    tracing::warn!("failed to send response: {e}");
                }
            }
            Err(msg) => {
                let err_body = serde_json::json!({"error": msg}).to_string();
                if let Err(e) = client.respond(&cmd, 10, &err_body).await {
                    tracing::warn!("failed to send error response: {e}");
                }
            }
        }
    }
}

fn handle_edit_open(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let path = cmd.args.get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing path argument")?;

    let line = cmd.args.get("line").and_then(|v| v.as_u64()).map(|l| l as usize);

    *OPEN_REQUEST.write() = Some(OpenRequest {
        path: Some(path.to_string()),
        content: None,
        line,
    });

    Ok(serde_json::json!({"opened": path}).to_string())
}

fn handle_edit_goto(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let line = cmd.args.get("line")
        .and_then(|v| v.as_u64())
        .ok_or("missing line argument")? as usize;

    *OPEN_REQUEST.write() = Some(OpenRequest {
        path: None,
        content: None,
        line: Some(line),
    });

    Ok(serde_json::json!({"line": line}).to_string())
}

fn handle_edit_compose(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    let content = cmd.args.get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let path = cmd.args.get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    *OPEN_REQUEST.write() = Some(OpenRequest {
        path,
        content: Some(content),
        line: None,
    });

    Ok(serde_json::json!({"composing": true}).to_string())
}

// ── File I/O ──

fn load_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn save_file(path: &str, content: &str) -> Result<(), String> {
    std::fs::write(path, content).map_err(|e| e.to_string())
}

// ── UI ──

fn app() -> Element {
    let mut content = use_signal(String::new);
    let mut file_path: Signal<Option<String>> = use_signal(|| None);
    let mut modified = use_signal(|| false);
    let mut status_msg = use_signal(String::new);
    let mut line_count = use_signal(|| 1usize);
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);

    // Connect to hub
    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default("edit").await {
                Ok(client) => {
                    let client = Arc::new(client);
                    tracing::info!("connected to cosmix-hub as 'edit'");
                    hub_client.set(Some(client.clone()));
                    tokio::spawn(handle_hub_commands(client));
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });
    });

    // Watch for open requests from hub commands
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let req = OPEN_REQUEST.write().take();
                if let Some(req) = req {
                    if let Some(path) = &req.path {
                        match load_file(path) {
                            Ok(text) => {
                                content.set(text.clone());
                                file_path.set(Some(path.clone()));
                                modified.set(false);
                                line_count.set(text.lines().count().max(1));
                                status_msg.set(format!("Opened {path}"));
                            }
                            Err(e) => {
                                status_msg.set(format!("Error: {e}"));
                            }
                        }
                    }
                    if let Some(text) = &req.content {
                        content.set(text.clone());
                        file_path.set(req.path.clone());
                        modified.set(true);
                        line_count.set(text.lines().count().max(1));
                        status_msg.set("Compose mode".into());
                    }
                }
            }
        });
    });

    // Shared action closures
    let do_open = move || {
        spawn(async move {
            if let Some(path) = cosmix_ui::desktop::pick_file(
                &[("Text files", &["txt", "md", "rs", "toml", "json", "yaml", "sh", "py", "lua"])],
            ).await {
                *OPEN_REQUEST.write() = Some(OpenRequest {
                    path: Some(path.to_string_lossy().to_string()),
                    content: None,
                    line: None,
                });
            }
        });
    };

    let mut do_save = move || {
        if let Some(ref path) = file_path() {
            match save_file(path, &content()) {
                Ok(()) => {
                    modified.set(false);
                    status_msg.set(format!("Saved {path}"));
                }
                Err(e) => status_msg.set(format!("Save error: {e}")),
            }
        } else {
            status_msg.set("No file path — use Save As".into());
        }
    };

    let do_save_as = move || {
        spawn(async move {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Save As");
            if let Some(ref path) = file_path() {
                if let Some(name) = std::path::Path::new(path).file_name() {
                    dialog = dialog.set_file_name(name.to_string_lossy());
                }
            }
            if let Some(handle) = dialog.save_file().await {
                let path = handle.path().to_string_lossy().to_string();
                match save_file(&path, &content()) {
                    Ok(()) => {
                        file_path.set(Some(path.clone()));
                        modified.set(false);
                        status_msg.set(format!("Saved {path}"));
                    }
                    Err(e) => status_msg.set(format!("Save error: {e}")),
                }
            }
        });
    };

    // Menu definition
    let app_menu = menubar(vec![
        standard_file_menu(vec![
            action_shortcut("open", "Open...", Shortcut::ctrl('o')),
            action_shortcut("save", "Save", Shortcut::ctrl('s')),
            action_shortcut("save-as", "Save As...", Shortcut::ctrl_shift('s')),
            separator(),
        ]),
        submenu("View", vec![
            action_shortcut("preview", "Preview in Viewer", Shortcut::ctrl('p')),
        ]),
    ]);

    let mut do_save_action = do_save.clone();
    let do_save_as_action = do_save_as.clone();
    let do_open_action = do_open.clone();

    // Keyboard shortcuts (handles same actions as menu)
    let onkeydown = {
        use dioxus::prelude::ModifiersInteraction;
        move |e: KeyboardEvent| {
            if e.modifiers().ctrl() {
                if let Key::Character(ref c) = e.key() {
                    match c.as_str() {
                        "s" if e.modifiers().shift() => do_save_as(),
                        "s" => do_save(),
                        "o" => do_open(),
                        "q" => std::process::exit(0),
                        _ => {}
                    }
                }
            }
        }
    };

    let title_suffix = if modified() { " *" } else { "" };
    let title = file_path()
        .map(|p| {
            let name = std::path::Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(p.clone());
            format!("{name}{title_suffix}")
        })
        .unwrap_or_else(|| format!("untitled{title_suffix}"));

    let lines = line_count();
    let theme = THEME.read().clone();
    let css = generate_css(&theme);
    let fs = theme.font_size;
    let fs_sm = fs.saturating_sub(2);

    rsx! {
        document::Style { "{css}" }
        div {
            tabindex: "0",
            onkeydown: onkeydown,
            style: "outline:none; width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-family:var(--font-mono); font-size:{fs}px;",

            // Menu bar
            MenuBar {
                menu: app_menu,
                hub: Some(hub_client),
                on_action: move |id: String| match id.as_str() {
                    "open" => do_open_action(),
                    "save" => do_save_action(),
                    "save-as" => do_save_as_action(),
                    "preview" => {
                        if let Some(ref path) = file_path() {
                            if let Some(ref client) = hub_client() {
                                let client = client.clone();
                                let path = path.clone();
                                spawn(async move {
                                    let args = serde_json::json!({ "path": path });
                                    match client.call("view", "view.open", args).await {
                                        Ok(_) => tracing::info!("Opened {path} in viewer"),
                                        Err(e) => tracing::warn!("Preview failed: {e}"),
                                    }
                                });
                            } else {
                                tracing::warn!("Hub not connected — cannot preview");
                            }
                        }
                    }
                    "quit" => std::process::exit(0),
                    _ => {}
                },
            }

            // Title bar
            div {
                style: "padding:3px 12px; background:var(--bg-secondary); border-bottom:1px solid var(--border); font-size:{fs}px; display:flex; align-items:center;",
                span { style: "font-weight:600; font-family:var(--font-sans);", "{title}" }
                if let Some(ref path) = file_path() {
                    span { style: "margin-left:8px; color:var(--fg-muted); font-size:{fs_sm}px; font-family:var(--font-sans);", "{path}" }
                }
            }

            // Editor area with line numbers
            div {
                style: "flex:1; display:flex; overflow:hidden;",

                // Line numbers
                div {
                    style: "width:48px; background:var(--bg-secondary); border-right:1px solid var(--border); padding:8px 4px; text-align:right; color:var(--fg-muted); font-size:{fs_sm}px; line-height:1.5; overflow:hidden; user-select:none;",
                    for i in 1..=lines {
                        div { "{i}" }
                    }
                }

                // Text area
                textarea {
                    style: "flex:1; background:var(--bg-primary); color:var(--fg-primary); border:none; outline:none; padding:8px; font-size:{fs}px; font-family:var(--font-mono); line-height:1.5; resize:none; tab-size:4;",
                    spellcheck: false,
                    value: "{content}",
                    oninput: move |e| {
                        let val = e.value();
                        line_count.set(val.lines().count().max(1));
                        content.set(val);
                        modified.set(true);
                    },
                }
            }

            // Status bar
            div {
                style: "padding:4px 12px; background:var(--bg-secondary); border-top:1px solid var(--border); color:var(--fg-muted); font-size:{fs_sm}px; display:flex; gap:16px; font-family:var(--font-sans);",
                span { "{status_msg}" }
                span { style: "margin-left:auto;", "{lines} lines" }
            }
        }
    }
}
