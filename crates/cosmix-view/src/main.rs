mod dot;
mod markdown;

use std::collections::HashMap;
use std::path::PathBuf;

use dioxus::prelude::*;
use dioxus::prelude::Key;
use cosmix_ui::app_init::{THEME, use_theme_css, use_theme_poll, use_hub_client, use_hub_handler};
use cosmix_ui::menu::{action_shortcut, amp_action, menubar, standard_file_menu, submenu, MenuBar, Shortcut};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let arg = std::env::args().nth(1);

    // Handle --help / -h
    if arg.as_deref() == Some("--help") || arg.as_deref() == Some("-h") {
        println!("cosmix-view — GFM markdown, DOT graph, and image viewer");
        println!();
        println!("Usage: cosmix-view [file]");
        println!();
        println!("  file    Markdown (.md), DOT graph (.dot/.gv), or image file");
        println!("          If omitted, opens with File > Open (Ctrl+O)");
        std::process::exit(0);
    }

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cosmix_view=info".into()),
        )
        .init();

    let path = arg.map(|a| {
        std::fs::canonicalize(&a).unwrap_or_else(|e| {
            eprintln!("Cannot open {a}: {e}");
            std::process::exit(1);
        })
    });

    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    };

    #[cfg(feature = "desktop")]
    {
        let title = path.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "cosmix-view".into());

        let cfg = cosmix_ui::desktop::window_config(&title, 960.0, 800.0);

        // SAFETY: single-threaded at this point, before Dioxus launch
        if let Some(ref p) = path {
            unsafe { std::env::set_var("COSMIX_VIEW_PATH", p.to_string_lossy().as_ref()); }
        }

        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── Hub command handling ──

static VIEW_REQUEST: GlobalSignal<Option<ViewRequest>> = Signal::global(|| None);
static VIEW_PATH: GlobalSignal<Option<String>> = Signal::global(|| None);

#[derive(Clone, Debug)]
enum ViewRequest {
    OpenFile(String),
    ShowMarkdown(String),
}

fn dispatch_command(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    match cmd.command.as_str() {
        "view.open" => {
            let path = cmd.args.get("path")
                .and_then(|v| v.as_str())
                .ok_or("missing path argument")?;
            *VIEW_REQUEST.write() = Some(ViewRequest::OpenFile(path.to_string()));
            Ok(serde_json::json!({"opened": path}).to_string())
        }
        "view.show-markdown" => {
            let content = cmd.args.get("content")
                .and_then(|v| v.as_str())
                .ok_or("missing content argument")?;
            *VIEW_REQUEST.write() = Some(ViewRequest::ShowMarkdown(content.to_string()));
            Ok(r#"{"status":"ok"}"#.to_string())
        }
        "view.get-path" => {
            let path = VIEW_PATH.read().clone();
            Ok(serde_json::json!({"path": path}).to_string())
        }
        _ => Err(format!("unknown command: {}", cmd.command)),
    }
}

fn is_image(path: &PathBuf) -> bool {
    cosmix_ui::util::is_image(path)
}

fn is_dot(path: &PathBuf) -> bool {
    cosmix_ui::util::is_dot(path)
}

fn mime_from_ext(path: &PathBuf) -> &'static str {
    cosmix_ui::util::mime_from_ext(path)
}

fn app() -> Element {
    let mut file_path: Signal<Option<PathBuf>> = use_signal(|| {
        let p = std::env::var("COSMIX_VIEW_PATH").ok().map(PathBuf::from);
        if let Some(ref path) = p {
            *VIEW_PATH.write() = Some(path.to_string_lossy().to_string());
        }
        p
    });

    let hub_client = use_hub_client("view");
    use_hub_handler(hub_client, "view", dispatch_command);

    // Poll config every 30s for theme changes
    use_theme_poll(30);

    // Watch for incoming view requests from hub commands
    let mut markdown_content: Signal<Option<String>> = use_signal(|| None);
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let req = VIEW_REQUEST.write().take();
                if let Some(req) = req {
                    match req {
                        ViewRequest::OpenFile(path) => {
                            *VIEW_PATH.write() = Some(path.clone());
                            file_path.set(Some(PathBuf::from(path)));
                            markdown_content.set(None);
                        }
                        ViewRequest::ShowMarkdown(content) => {
                            file_path.set(None);
                            markdown_content.set(Some(content));
                        }
                    }
                }
            }
        });
    });

    let open_file = move || {
        spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter("Markdown", &["md", "markdown"])
                .add_filter("DOT graph", &["dot", "gv"])
                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"])
                .add_filter("All files", &["*"])
                .set_title("Open file")
                .pick_file()
                .await;
            if let Some(handle) = picked {
                file_path.set(Some(handle.path().to_path_buf()));
            }
        });
    };

    let app_menu = menubar(vec![
        standard_file_menu(vec![
            action_shortcut("open", "Open...", Shortcut::ctrl('o')),
        ]),
        submenu("Edit", vec![
            action_shortcut("open-in-editor", "Open in Editor", Shortcut::ctrl('e')),
        ]),
        submenu("Services", vec![
            amp_action("mon-status", "Monitor Status", "mon", "mon.status"),
        ]),
        cosmix_script::user_menu("view"),
    ]);
    let open_for_menu = open_file.clone();

    let onkeydown = move |e: KeyboardEvent| {
        if e.modifiers().ctrl() {
            match e.key() {
                Key::Character(c) if c == "o" => open_file(),
                Key::Character(c) if c == "l" => {
                    if let Some(ref client) = hub_client() {
                        let client = client.clone();
                        spawn(async move {
                            let path = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                            match client.call("files", "file.list", serde_json::json!({"path": path})).await {
                                Ok(result) => tracing::info!("file.list result: {result}"),
                                Err(e) => tracing::warn!("file.list failed: {e}"),
                            }
                        });
                    } else {
                        tracing::debug!("Ctrl+L: hub not connected, ignoring");
                    }
                }
                Key::Character(c) if c == "q" => std::process::exit(0),
                _ => {}
            }
        }
    };

    use_theme_css();
    let fs = THEME.read().font_size;

    let content = if let Some(ref md) = markdown_content() {
        render_markdown_content(md)
    } else {
        match file_path() {
            Some(ref path) if is_image(path) => render_image(path),
            Some(ref path) if is_dot(path) => render_dot_file(path),
            Some(ref path) => render_markdown(path),
            None => render_welcome(),
        }
    };

    rsx! {
        div {
            tabindex: "0",
            onkeydown: onkeydown,
            style: "outline:none; width:100%; height:100vh; display:flex; flex-direction:column; font-size:{fs}px;",
            MenuBar {
                menu: app_menu,
                hub: Some(hub_client),
                on_action: move |id: String| match id.as_str() {
                    "open" => open_for_menu(),
                    "open-in-editor" => {
                        if let Some(ref path) = file_path() {
                            if let Some(ref client) = hub_client() {
                                let client = client.clone();
                                let path_str = path.to_string_lossy().to_string();
                                spawn(async move {
                                    let args = serde_json::json!({ "path": path_str });
                                    match client.call("edit", "edit.open", args).await {
                                        Ok(_) => tracing::info!("Opened {path_str} in editor"),
                                        Err(e) => tracing::warn!("Open in editor failed: {e}"),
                                    }
                                });
                            } else {
                                tracing::warn!("Hub not connected — cannot open in editor");
                            }
                        }
                    }
                    "quit" => std::process::exit(0),
                    "script:reload" => { /* menu rebuilt each render */ }
                    "script:open-folder" => {
                        let dir = cosmix_script::scripts_dir().join("view");
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                    }
                    id if id.starts_with("script:") => {
                        if let Some(ref client) = hub_client() {
                            let client = client.clone();
                            let id = id.to_string();
                            spawn(async move {
                                let mut vars = HashMap::new();
                                if let Some(ref p) = *VIEW_PATH.read() {
                                    vars.insert("CURRENT_FILE".into(), p.clone());
                                }
                                vars.insert("SERVICE_NAME".into(), "view".into());
                                cosmix_script::handle_script_action(&id, "view", &client, &vars).await;
                            });
                        }
                    }
                    _ => {}
                },
            }
            div { style: "flex:1; overflow:auto;",
                {content}
            }
        }
    }
}

fn render_welcome() -> Element {
    rsx! {
        document::Style { "{CSS}" }
        div {
            class: "markdown-body",
            style: "display:flex; align-items:center; justify-content:center; min-height:80vh; text-align:center;",
            div {
                h2 { style: "color:var(--fg-muted); font-weight:400;", "cosmix-view" }
                p { style: "color:var(--fg-muted);", "Open a file with File > Open or Ctrl+O" }
                p { style: "color:var(--fg-secondary); font-size:0.85em;", "Supports Markdown, DOT graphs, and images" }
            }
        }
    }
}

fn render_markdown(path: &PathBuf) -> Element {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| format!("Error reading file: {e}"));
    let base_dir = path.parent().map(|p| p.to_path_buf());
    let html = markdown::render_gfm(&content, base_dir.as_ref());

    rsx! {
        document::Style { "{CSS}" }
        div {
            class: "markdown-body",
            dangerous_inner_html: "{html}"
        }
    }
}

fn render_markdown_content(content: &str) -> Element {
    let html = markdown::render_gfm(content, None);
    rsx! {
        document::Style { "{CSS}" }
        div {
            class: "markdown-body",
            dangerous_inner_html: "{html}"
        }
    }
}

fn render_dot_file(path: &PathBuf) -> Element {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| format!("Error reading file: {e}"));

    let svg_html = match dot::render_dot(&content) {
        Ok(svg) => svg,
        Err(e) => format!("<pre>DOT render error: {e}</pre>"),
    };

    let canvas_css = cosmix_ui::canvas::CANVAS_CSS;
    let canvas_js = cosmix_ui::canvas::CANVAS_JS;
    let controls = cosmix_ui::canvas::CANVAS_CONTROLS_TEXT;

    rsx! {
        document::Style { "{CANVAS_BASE_CSS}" }
        document::Style { "{canvas_css}" }
        div {
            class: "pan-canvas",
            div {
                id: "pan-content",
                class: "pan-content",
                dangerous_inner_html: "{svg_html}"
            }
            div { class: "pan-controls",
                "{controls}"
            }
        }
        document::Script { "{canvas_js}" }
    }
}

fn render_image(path: &PathBuf) -> Element {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let path_str = path.to_string_lossy();
    let mime = mime_from_ext(path);
    let data = std::fs::read(path).unwrap_or_default();
    let b64 = STANDARD.encode(&data);
    let src = format!("data:{mime};base64,{b64}");

    let canvas_css = cosmix_ui::canvas::CANVAS_CSS;
    let canvas_js = cosmix_ui::canvas::CANVAS_JS;
    let controls = cosmix_ui::canvas::CANVAS_CONTROLS_TEXT;

    rsx! {
        document::Style { "{CANVAS_BASE_CSS}" }
        document::Style { "{canvas_css}" }
        document::Style { "{IMAGE_CSS}" }
        div {
            class: "pan-canvas",
            div {
                id: "pan-content",
                class: "pan-content",
                img { src: "{src}", alt: "{path_str}" }
            }
            div { class: "pan-controls",
                "{controls}"
            }
        }
        document::Script { "{canvas_js}" }
    }
}

/// Base reset for canvas views (DOT and image). The shared CANVAS_CSS handles
/// the pan/zoom container itself; this sets the page-level background.
const CANVAS_BASE_CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    background: #f0f0f0;
    width: 100%; height: 100%;
    overflow: hidden;
}
"#;

/// Image-specific overrides: dark background for contrast.
const IMAGE_CSS: &str = r#"
html, body, #main { background: #1a1a1a; }
.pan-canvas { background: #1a1a1a; }
.pan-controls { color: #9ca3af; background: rgba(0,0,0,0.6); }
"#;

const CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    background: #ffffff;
    width: 100%; height: 100%;
}
.markdown-body {
    max-width: 860px;
    margin: 0 auto;
    padding: 40px 32px 80px;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    line-height: 1.6;
    color: #1f2937;
    word-wrap: break-word;
}

/* Headings */
.markdown-body h1 {
    font-size: 2em;
    font-weight: 700;
    margin: 0.67em 0;
    padding-bottom: 0.3em;
    border-bottom: 1px solid #e5e7eb;
}
.markdown-body h2 {
    font-size: 1.5em;
    font-weight: 600;
    margin-top: 1.5em;
    margin-bottom: 0.5em;
    padding-bottom: 0.3em;
    border-bottom: 1px solid #e5e7eb;
}
.markdown-body h3 { font-size: 1.25em; font-weight: 600; margin-top: 1.5em; margin-bottom: 0.5em; }
.markdown-body h4 { font-size: 1em; font-weight: 600; margin-top: 1.5em; margin-bottom: 0.5em; }
.markdown-body h5 { font-size: 0.875em; font-weight: 600; margin-top: 1.5em; margin-bottom: 0.5em; }
.markdown-body h6 { font-size: 0.85em; font-weight: 600; margin-top: 1.5em; margin-bottom: 0.5em; color: #6b7280; }

/* Paragraphs */
.markdown-body p { margin-top: 0; margin-bottom: 16px; }

/* Links */
.markdown-body a { color: #2563eb; text-decoration: none; }
.markdown-body a:hover { text-decoration: underline; }

/* Bold, italic, strikethrough */
.markdown-body strong { font-weight: 600; }
.markdown-body del { text-decoration: line-through; color: #9ca3af; }

/* Blockquotes */
.markdown-body blockquote {
    margin: 0 0 16px;
    padding: 0 16px;
    border-left: 4px solid #d1d5db;
    color: #6b7280;
}
.markdown-body blockquote > :first-child { margin-top: 0; }
.markdown-body blockquote > :last-child { margin-bottom: 0; }

/* Code — inline */
.markdown-body code {
    font-family: "JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Consolas, "Liberation Mono", Menlo, monospace;
    font-size: 0.875em;
    padding: 0.2em 0.4em;
    background: #f3f4f6;
    border-radius: 4px;
}

/* Code — fenced blocks */
.markdown-body pre {
    margin: 0 0 16px;
    padding: 16px;
    background: #f8f9fa;
    border: 1px solid #e5e7eb;
    border-radius: 6px;
    overflow-x: auto;
    line-height: 1.2;
}
.markdown-body pre code {
    font-family: monospace;
    font-size: 0.9em;
    padding: 0;
    background: transparent;
    border-radius: 0;
    white-space: pre;
    word-wrap: normal;
}

/* Lists */
.markdown-body ul, .markdown-body ol {
    margin-top: 0;
    margin-bottom: 16px;
    padding-left: 2em;
}
.markdown-body li { margin-top: 0.25em; }
.markdown-body li + li { margin-top: 0.25em; }

/* Task lists */
.markdown-body li input[type="checkbox"] {
    margin-right: 0.5em;
    vertical-align: middle;
}
.markdown-body ul.task-list {
    list-style: none;
    padding-left: 1.5em;
}

/* Tables */
.markdown-body table {
    border-collapse: collapse;
    border-spacing: 0;
    width: auto;
    margin-bottom: 16px;
    display: block;
    overflow-x: auto;
}
.markdown-body table th {
    font-weight: 600;
    background: #f9fafb;
}
.markdown-body table th,
.markdown-body table td {
    padding: 8px 16px;
    border: 1px solid #d1d5db;
}
.markdown-body table tr:nth-child(even) {
    background: #f9fafb;
}

/* Horizontal rules */
.markdown-body hr {
    border: none;
    border-top: 2px solid #e5e7eb;
    margin: 24px 0;
}

/* Images */
.markdown-body img {
    max-width: 100%;
    height: auto;
    border-radius: 4px;
    margin: 8px 0;
}

/* Footnotes */
.markdown-body .footnote-definition {
    font-size: 0.875em;
    margin-bottom: 8px;
    display: flex;
    gap: 8px;
}
.markdown-body .footnote-definition sup {
    min-width: 1.5em;
}

/* Definition lists */
.markdown-body dt { font-weight: 600; margin-top: 8px; }
.markdown-body dd { margin-left: 2em; margin-bottom: 8px; }

/* Inline DOT diagrams */
.markdown-body .dot-diagram {
    margin: 16px 0;
    text-align: center;
}
.markdown-body .dot-diagram svg {
    max-width: 100%;
    height: auto;
}
.markdown-body .dot-error {
    color: #dc2626;
    background: #fef2f2;
    border: 1px solid #fecaca;
}
"#;
