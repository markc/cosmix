mod dot;
mod markdown;

use dioxus::prelude::*;
use dioxus::prelude::Key;
use std::path::PathBuf;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico"];

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
        use dioxus_desktop::{Config, WindowBuilder};

        let menu = build_menu();
        let title = path.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "cosmix-view".into());

        let cfg = Config::new()
            .with_window(
                WindowBuilder::new()
                    .with_title(&title)
                    .with_inner_size(dioxus_desktop::LogicalSize::new(960.0, 800.0)),
            )
            .with_menu(menu);

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

#[cfg(feature = "desktop")]
fn build_menu() -> dioxus_desktop::muda::Menu {
    use dioxus_desktop::muda::*;

    let menu = Menu::new();

    let file_menu = Submenu::new("&File", true);
    file_menu.append(&MenuItem::with_id("open", "&Open...\tCtrl+O", true, None)).ok();
    file_menu.append(&PredefinedMenuItem::separator()).ok();
    file_menu.append(&MenuItem::with_id("quit", "&Quit\tCtrl+Q", true, None)).ok();

    menu.append(&file_menu).ok();
    menu
}

fn is_image(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

fn is_dot(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dot") || ext.eq_ignore_ascii_case("gv"))
}

fn app() -> Element {
    let mut file_path: Signal<Option<PathBuf>> = use_signal(|| {
        std::env::var("COSMIX_VIEW_PATH").ok().map(PathBuf::from)
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

    #[cfg(feature = "desktop")]
    {
        let open_file = open_file.clone();
        dioxus_desktop::use_muda_event_handler(move |event| {
            match event.id().0.as_str() {
                "open" => open_file(),
                "quit" => std::process::exit(0),
                _ => {}
            }
        });
    }

    let onkeydown = move |e: KeyboardEvent| {
        if e.modifiers().ctrl() {
            match e.key() {
                Key::Character(c) if c == "o" => open_file(),
                Key::Character(c) if c == "q" => std::process::exit(0),
                _ => {}
            }
        }
    };

    let content = match file_path() {
        Some(ref path) if is_image(path) => render_image(path),
        Some(ref path) if is_dot(path) => render_dot_file(path),
        Some(ref path) => render_markdown(path),
        None => render_welcome(),
    };

    rsx! {
        div {
            tabindex: "0",
            onkeydown: onkeydown,
            style: "outline:none; width:100%; height:100%;",
            {content}
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
                h2 { style: "color:#9ca3af; font-weight:400;", "cosmix-view" }
                p { style: "color:#6b7280;", "Open a file with File > Open or Ctrl+O" }
                p { style: "color:#9ca3af; font-size:0.85em;", "Supports Markdown, DOT graphs, and images" }
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

fn render_dot_file(path: &PathBuf) -> Element {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| format!("Error reading file: {e}"));

    let svg_html = match dot::render_dot(&content) {
        Ok(svg) => svg,
        Err(e) => format!("<pre>DOT render error: {e}</pre>"),
    };

    rsx! {
        document::Style { "{DOT_CSS}" }
        div {
            class: "dot-canvas",
            div {
                id: "dot-content",
                class: "dot-content",
                dangerous_inner_html: "{svg_html}"
            }
            div { class: "dot-controls",
                "Scroll: zoom | Drag: pan | Double-click: reset"
            }
        }
        document::Script { "{DOT_JS}" }
    }
}

fn render_image(path: &PathBuf) -> Element {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let path_str = path.to_string_lossy();
    let mime = mime_from_ext(path);
    let data = std::fs::read(path).unwrap_or_default();
    let b64 = STANDARD.encode(&data);
    let src = format!("data:{mime};base64,{b64}");

    rsx! {
        document::Style { "{DOT_CSS}" }
        document::Style { "{IMAGE_CSS}" }
        div {
            class: "dot-canvas",
            div {
                id: "dot-content",
                class: "dot-content",
                img { src: "{src}", alt: "{path_str}" }
            }
            div { class: "dot-controls",
                "Scroll: zoom | Drag: pan | Double-click: reset"
            }
        }
        document::Script { "{DOT_JS}" }
    }
}

fn mime_from_ext(path: &PathBuf) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

const DOT_CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    background: #f0f0f0;
    width: 100%; height: 100%;
    overflow: hidden;
}
.dot-canvas {
    width: 100%; height: 100vh;
    overflow: hidden;
    position: relative;
    cursor: grab;
}
.dot-canvas:active { cursor: grabbing; }
.dot-content {
    transform-origin: 0 0;
    position: absolute;
    top: 0; left: 0;
}
.dot-content svg {
    display: block;
}
.dot-controls {
    position: fixed;
    bottom: 8px; right: 12px;
    font-size: 11px;
    color: #6b7280;
    background: rgba(255,255,255,0.85);
    padding: 3px 10px;
    border-radius: 4px;
    pointer-events: none;
    font-family: system-ui, sans-serif;
}
"#;

const DOT_JS: &str = r#"
(function() {
    let scale = 1, panX = 0, panY = 0;
    let dragging = false, startX = 0, startY = 0, startPanX = 0, startPanY = 0;
    const el = document.getElementById('dot-content');
    const canvas = el.parentElement;

    function apply() {
        el.style.transform = 'translate(' + panX + 'px,' + panY + 'px) scale(' + scale + ')';
    }

    function centerView() {
        const child = el.querySelector('svg') || el.querySelector('img');
        if (!child) return;
        // For images, use naturalWidth/Height; for SVG use bounding rect
        const sw = child.naturalWidth || (child.getBoundingClientRect().width / scale);
        const sh = child.naturalHeight || (child.getBoundingClientRect().height / scale);
        if (!sw || !sh) return;
        const cw = canvas.clientWidth;
        const ch = canvas.clientHeight;
        scale = Math.min(cw / sw, ch / sh, 1) * 0.9;
        panX = (cw - sw * scale) / 2;
        panY = (ch - sh * scale) / 2;
        apply();
    }

    // Center after content is ready
    function initCenter() {
        const img = el.querySelector('img');
        if (img) {
            function tryCenter() {
                if (img.naturalWidth > 0 && img.naturalHeight > 0) {
                    centerView();
                } else {
                    setTimeout(tryCenter, 50);
                }
            }
            img.addEventListener('load', centerView);
            setTimeout(tryCenter, 100);
        } else {
            setTimeout(centerView, 50);
        }
    }
    initCenter();

    canvas.addEventListener('wheel', function(e) {
        e.preventDefault();
        const rect = canvas.getBoundingClientRect();
        const mx = e.clientX - rect.left;
        const my = e.clientY - rect.top;
        const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
        const newScale = Math.max(0.1, Math.min(10, scale * factor));
        panX = mx - (mx - panX) * (newScale / scale);
        panY = my - (my - panY) * (newScale / scale);
        scale = newScale;
        apply();
    }, {passive: false});

    canvas.addEventListener('mousedown', function(e) {
        if (e.button !== 0) return;
        dragging = true;
        startX = e.clientX; startY = e.clientY;
        startPanX = panX; startPanY = panY;
    });
    window.addEventListener('mousemove', function(e) {
        if (!dragging) return;
        panX = startPanX + (e.clientX - startX);
        panY = startPanY + (e.clientY - startY);
        apply();
    });
    window.addEventListener('mouseup', function() { dragging = false; });

    canvas.addEventListener('dblclick', function() {
        centerView();
    });
})();
"#;

const IMAGE_CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    background: #1a1a1a;
    width: 100%; height: 100%;
    overflow: hidden;
}
.dot-canvas { background: #1a1a1a; }
.dot-content img {
    display: block;
    max-width: none;
    max-height: none;
}
.dot-controls { color: #9ca3af; background: rgba(0,0,0,0.6); }
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
    font-size: 16px;
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
