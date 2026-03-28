//! cosmix-dialog — GUI dialog utility for the cosmix stack.
//!
//! Drop-in replacement for zenity/kdialog, built with Dioxus.
//!
//! Usage:
//!   echo "hello" | cosmix-dialog text-info --title "Output"
//!   cosmix-dialog info --text "Done!" --title "Notice"
//!   cosmix-dialog error --text "Something failed"
//!   cosmix-dialog confirm --text "Delete this file?"    # exit 0=yes, 1=no
//!   cosmix-dialog input --text "Enter name:"            # prints input to stdout

use std::io::Read;
use std::sync::atomic::{AtomicI32, Ordering};

use clap::{Parser, Subcommand};
use cosmix_ui::app_init::use_theme_css;
use dioxus::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── CLI ──

#[derive(Parser)]
#[command(name = "cosmix-dialog", about = "GUI dialog utility for cosmix")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Clone)]
enum Mode {
    /// Display scrollable text from stdin
    TextInfo {
        #[arg(long, default_value = "cosmix-dialog")]
        title: String,
        /// Window width
        #[arg(long, default_value = "600")]
        width: u32,
        /// Window height
        #[arg(long, default_value = "400")]
        height: u32,
    },
    /// Show an info message with OK button
    Info {
        #[arg(long)]
        text: String,
        #[arg(long, default_value = "Info")]
        title: String,
    },
    /// Show an error message with OK button
    Error {
        #[arg(long)]
        text: String,
        #[arg(long, default_value = "Error")]
        title: String,
    },
    /// Yes/No confirmation — exit 0 for yes, 1 for no
    Confirm {
        #[arg(long)]
        text: String,
        #[arg(long, default_value = "Confirm")]
        title: String,
    },
    /// Text input — prints entered text to stdout
    Input {
        #[arg(long, default_value = "")]
        text: String,
        #[arg(long, default_value = "Input")]
        title: String,
        /// Default value for the input field
        #[arg(long, default_value = "")]
        entry_text: String,
    },
}

// ── Shared state for exit code ──

static EXIT_CODE: AtomicI32 = AtomicI32::new(1);

// ── Shared data passed into components ──

static DIALOG_MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
static STDIN_TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

// ── Main ──

fn main() {
    cosmix_ui::desktop::init_linux_env();

    let cli = Cli::parse();

    // Read stdin for text-info mode (must happen before Dioxus takes over)
    if matches!(cli.mode, Mode::TextInfo { .. }) {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        STDIN_TEXT.set(buf).ok();
    }

    let (title, width, height) = match &cli.mode {
        Mode::TextInfo { title, width, height } => (title.clone(), *width, *height),
        Mode::Info { title, .. } => (title.clone(), 400, 200),
        Mode::Error { title, .. } => (title.clone(), 400, 200),
        Mode::Confirm { title, .. } => (title.clone(), 400, 180),
        Mode::Input { title, .. } => (title.clone(), 400, 180),
    };

    DIALOG_MODE.set(cli.mode).ok();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config(&title, width as f64, height as f64);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
    }

    std::process::exit(EXIT_CODE.load(Ordering::Relaxed));
}

// ── Helpers ──

/// Exit the process from an event handler without returning `!`.
fn exit(code: i32) {
    EXIT_CODE.store(code, Ordering::Relaxed);
    // Defer exit to avoid `!` return type in Dioxus event handlers
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::process::exit(code);
    });
}

// ── UI ──

fn app() -> Element {
    use_theme_css();
    let mode = DIALOG_MODE.get().unwrap();

    rsx! {
        {match mode {
            Mode::TextInfo { .. } => text_info_view(),
            Mode::Info { text, .. } => message_view(text, "info"),
            Mode::Error { text, .. } => message_view(text, "error"),
            Mode::Confirm { text, .. } => confirm_view(text),
            Mode::Input { text, entry_text, .. } => input_view(text, entry_text),
        }}
    }
}

fn text_info_view() -> Element {
    let text = STDIN_TEXT.get().map(|s| s.as_str()).unwrap_or("");

    rsx! {
        div {
            style: "width:100%;height:100vh;display:flex;flex-direction:column;background:var(--bg-primary);color:var(--fg-primary);font-family:monospace;",

            // Scrollable text area
            pre {
                style: "flex:1;margin:0;padding:12px;overflow:auto;font-size:13px;line-height:1.5;white-space:pre-wrap;word-wrap:break-word;",
                "{text}"
            }

            // Button bar
            div {
                style: "padding:8px 12px;background:var(--bg-secondary);border-top:1px solid var(--border);display:flex;justify-content:flex-end;",
                button {
                    style: "{BTN_STYLE}",
                    onclick: move |_| {
                        exit(0);
                    },
                    "OK"
                }
            }
        }
    }
}

fn message_view(text: &str, kind: &str) -> Element {
    let icon_color = if kind == "error" { "#ef4444" } else { "#60a5fa" };
    let icon = if kind == "error" { "✕" } else { "ℹ" };

    rsx! {
        div {
            style: "width:100%;height:100vh;display:flex;flex-direction:column;background:var(--bg-primary);color:var(--fg-primary);font-family:sans-serif;",

            div {
                style: "flex:1;display:flex;align-items:center;padding:24px;gap:16px;",
                span { style: "font-size:32px;color:{icon_color};", "{icon}" }
                span { style: "font-size:14px;line-height:1.5;", "{text}" }
            }

            div {
                style: "padding:8px 12px;background:var(--bg-secondary);border-top:1px solid var(--border);display:flex;justify-content:flex-end;",
                button {
                    style: "{BTN_STYLE}",
                    onclick: move |_| {
                        exit(0);
                    },
                    "OK"
                }
            }
        }
    }
}

fn confirm_view(text: &str) -> Element {
    rsx! {
        div {
            style: "width:100%;height:100vh;display:flex;flex-direction:column;background:var(--bg-primary);color:var(--fg-primary);font-family:sans-serif;",

            div {
                style: "flex:1;display:flex;align-items:center;padding:24px;gap:16px;",
                span { style: "font-size:32px;color:#f59e0b;", "?" }
                span { style: "font-size:14px;line-height:1.5;", "{text}" }
            }

            div {
                style: "padding:8px 12px;background:var(--bg-secondary);border-top:1px solid var(--border);display:flex;justify-content:flex-end;gap:8px;",
                button {
                    style: "{BTN_STYLE}",
                    onclick: move |_| {
                        exit(1);
                    },
                    "No"
                }
                button {
                    style: "{BTN_PRIMARY_STYLE}",
                    onclick: move |_| {
                        exit(0);
                    },
                    "Yes"
                }
            }
        }
    }
}

fn input_view(text: &str, default: &str) -> Element {
    let mut value = use_signal(|| default.to_string());

    let submit = move |_| {
        print!("{}", value());
        exit(0);
    };

    rsx! {
        div {
            style: "width:100%;height:100vh;display:flex;flex-direction:column;background:var(--bg-primary);color:var(--fg-primary);font-family:sans-serif;",

            div {
                style: "flex:1;display:flex;flex-direction:column;justify-content:center;padding:24px;gap:12px;",
                if !text.is_empty() {
                    label { style: "font-size:14px;", "{text}" }
                }
                input {
                    style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-primary);padding:8px 12px;border-radius:4px;font-size:14px;outline:none;",
                    value: "{value}",
                    autofocus: true,
                    oninput: move |e| value.set(e.value()),
                    onkeypress: move |e| {
                        if e.key() == Key::Enter {
                            print!("{}", value());
                            exit(0);
                        }
                    },
                }
            }

            div {
                style: "padding:8px 12px;background:var(--bg-secondary);border-top:1px solid var(--border);display:flex;justify-content:flex-end;gap:8px;",
                button {
                    style: "{BTN_STYLE}",
                    onclick: move |_| {
                        exit(1);
                    },
                    "Cancel"
                }
                button {
                    style: "{BTN_PRIMARY_STYLE}",
                    onclick: submit,
                    "OK"
                }
            }
        }
    }
}

// ── Theme ──

const BTN_STYLE: &str = "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-muted);padding:6px 16px;border-radius:var(--radius-sm);cursor:pointer;font-size:13px;";
const BTN_PRIMARY_STYLE: &str = "background:var(--accent);border:1px solid var(--accent-hover);color:var(--accent-fg);padding:6px 16px;border-radius:var(--radius-sm);cursor:pointer;font-size:13px;";
