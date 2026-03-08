mod cosmic_config;
mod daemon;
mod dbus;
mod desktop;
mod dialog;
mod ipc;
mod lua;
mod mail;
mod pipewire;
mod wayland;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    // Suppress MESA warnings by redirecting stderr to /dev/null before GPU init
    if command == "dialog" || command == "d" {
        suppress_stderr();
    }

    // Quiet logging for dialog mode (iced/wgpu are very noisy)
    let default_level = if command == "dialog" || command == "d" {
        tracing::Level::ERROR
    } else if command == "daemon" {
        tracing::Level::INFO
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(default_level.into()),
        )
        .init();

    // Daemon mode
    if command == "daemon" {
        let config = daemon::config::DaemonConfig::load()?;
        let d = daemon::Daemon::new(config);
        return d.run();
    }

    // Try daemon-routed execution for supported commands
    if let Some(result) = try_via_daemon(command, &args) {
        return result;
    }

    // Direct execution (fallback or commands that don't go through daemon)
    run_direct(command, &args)
}

/// Try routing command through daemon IPC
fn try_via_daemon(command: &str, args: &[String]) -> Option<Result<()>> {
    let config = match daemon::config::DaemonConfig::load() {
        Ok(c) => c,
        Err(_) => return None,
    };
    if !ipc::try_daemon(&config.daemon.socket) {
        return None;
    }

    let request = match command {
        "list-windows" | "lw" => Some(ipc::protocol::IpcRequest::ListWindows),
        "list-workspaces" | "lws" => Some(ipc::protocol::IpcRequest::ListWorkspaces),
        "clipboard" | "cb" => Some(ipc::protocol::IpcRequest::GetClipboard),
        "activate" | "a" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Activate { query })
        }
        "close" | "c" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Close { query })
        }
        "minimize" | "min" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Minimize { query })
        }
        "maximize" | "max" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Maximize { query })
        }
        "fullscreen" | "fs" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Fullscreen { query })
        }
        "sticky" | "st" => {
            let query = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::Sticky { query })
        }
        "notify" | "n" => {
            let summary = args.get(2)?.to_string();
            let body = args.get(3).cloned().unwrap_or_default();
            Some(ipc::protocol::IpcRequest::Notify { summary, body })
        }
        "type" | "t" => {
            let text = args.get(2)?.to_string();
            let delay_us = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5000);
            Some(ipc::protocol::IpcRequest::TypeText { text, delay_us })
        }
        "key" | "k" => {
            let combo = args.get(2)?.to_string();
            let delay_us = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5000);
            Some(ipc::protocol::IpcRequest::SendKey { combo, delay_us })
        }
        "config-list" | "cl" => Some(ipc::protocol::IpcRequest::ConfigList),
        "config-keys" | "ck" => {
            let component = args.get(2)?.to_string();
            Some(ipc::protocol::IpcRequest::ConfigKeys { component })
        }
        "config-read" | "cr" => {
            let component = args.get(2)?.to_string();
            let key = args.get(3)?.to_string();
            Some(ipc::protocol::IpcRequest::ConfigRead { component, key })
        }
        "config-write" | "cw" => {
            let component = args.get(2)?.to_string();
            let key = args.get(3)?.to_string();
            let value = args.get(4)?.to_string();
            Some(ipc::protocol::IpcRequest::ConfigWrite { component, key, value })
        }
        "dbus" => {
            let service = args.get(2)?.to_string();
            let path = args.get(3)?.to_string();
            let interface = args.get(4)?.to_string();
            let method = args.get(5)?.to_string();
            let dbus_args: Option<Vec<serde_json::Value>> = args.get(6)
                .and_then(|s| serde_json::from_str(s).ok());
            Some(ipc::protocol::IpcRequest::DbusCall { service, path, interface, method, args: dbus_args, system: false })
        }
        "dbus-system" => {
            let service = args.get(2)?.to_string();
            let path = args.get(3)?.to_string();
            let interface = args.get(4)?.to_string();
            let method = args.get(5)?.to_string();
            let dbus_args: Option<Vec<serde_json::Value>> = args.get(6)
                .and_then(|s| serde_json::from_str(s).ok());
            Some(ipc::protocol::IpcRequest::DbusCall { service, path, interface, method, args: dbus_args, system: true })
        }
        "status" => Some(ipc::protocol::IpcRequest::Status),
        "ping" => Some(ipc::protocol::IpcRequest::Ping),
        _ => None,
    };

    let request = request?;

    Some(match ipc::client_request(&config.daemon.socket, &request) {
        Ok(response) => {
            if response.ok {
                if let Some(data) = response.data {
                    match &data {
                        serde_json::Value::String(s) => println!("{s}"),
                        serde_json::Value::Array(arr) => {
                            println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
                        }
                        other => println!("{}", serde_json::to_string_pretty(&other).unwrap_or_default()),
                    }
                }
                Ok(())
            } else {
                Err(anyhow::anyhow!("{}", response.error.unwrap_or_else(|| "Unknown error".into())))
            }
        }
        Err(_) => {
            // Daemon connection failed, fall through to direct
            return None;
        }
    })
}

/// Direct execution (no daemon)
fn run_direct(command: &str, args: &[String]) -> Result<()> {
    match command {
        // Query
        "list-windows" | "lw" => wayland::toplevel::list_windows()?,
        "list-workspaces" | "lws" => wayland::workspace::list_workspaces()?,
        "clipboard" | "cb" => dbus::clipboard::clipboard_get_cmd()?,

        // Window control
        "activate" | "a" => {
            let query = require_arg(args, 2, "cosmix activate <query>");
            wayland::toplevel::activate_window(query)?;
        }
        "close" | "c" => {
            let query = require_arg(args, 2, "cosmix close <query>");
            wayland::toplevel::close_window(query)?;
        }
        "minimize" | "min" => {
            let query = require_arg(args, 2, "cosmix minimize <query>");
            wayland::toplevel::minimize_window(query)?;
        }
        "maximize" | "max" => {
            let query = require_arg(args, 2, "cosmix maximize <query>");
            wayland::toplevel::maximize_window(query)?;
        }
        "fullscreen" | "fs" => {
            let query = require_arg(args, 2, "cosmix fullscreen <query>");
            wayland::toplevel::fullscreen_window(query)?;
        }
        "sticky" | "st" => {
            let query = require_arg(args, 2, "cosmix sticky <query>");
            wayland::toplevel::sticky_window(query)?;
        }

        // Input injection
        "type" | "t" => {
            let text = require_arg(args, 2, "cosmix type <text> [delay_us]");
            let delay_us: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5000);
            wayland::virtual_keyboard::type_text(text, delay_us)?;
        }
        "key" | "k" => {
            let combo = require_arg(args, 2, "cosmix key <combo> [delay_us]");
            let delay_us: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5000);
            wayland::virtual_keyboard::send_key(combo, delay_us)?;
        }

        // Notifications
        "notify" | "n" => {
            let summary = require_arg(args, 2, "cosmix notify <summary> [body]");
            let body = args.get(3).map(|s| s.as_str()).unwrap_or("");
            dbus::notify::notify_cmd(summary, body)?;
        }

        // Dialogs
        "dialog" | "d" => {
            let dialog_args: Vec<String> = args[2..].to_vec();
            dialog::dialog_cmd(&dialog_args)?;
        }

        // COSMIC config
        "config-list" | "cl" => {
            let components = cosmic_config::list_components()?;
            for c in &components {
                println!("{c}");
            }
        }
        "config-keys" | "ck" => {
            let component = require_arg(args, 2, "cosmix config-keys <component>");
            let keys = cosmic_config::list_keys(component)?;
            for k in &keys {
                println!("{k}");
            }
        }
        "config-read" | "cr" => {
            let component = require_arg(args, 2, "cosmix config-read <component> <key>");
            let key = require_arg(args, 3, "cosmix config-read <component> <key>");
            let value = cosmic_config::read_key(component, key)?;
            print!("{value}");
        }
        "config-write" | "cw" => {
            let component = require_arg(args, 2, "cosmix config-write <component> <key> <value>");
            let key = require_arg(args, 3, "cosmix config-write <component> <key> <value>");
            let value = require_arg(args, 4, "cosmix config-write <component> <key> <value>");
            cosmic_config::write_key(component, key, value)?;
        }

        // Generic D-Bus
        "dbus" => {
            let service = require_arg(args, 2, "cosmix dbus <service> <path> <interface> <method> [args_json]");
            let path = require_arg(args, 3, "cosmix dbus <service> <path> <interface> <method> [args_json]");
            let interface = require_arg(args, 4, "cosmix dbus <service> <path> <interface> <method> [args_json]");
            let method = require_arg(args, 5, "cosmix dbus <service> <path> <interface> <method> [args_json]");
            let dbus_args: Option<Vec<serde_json::Value>> = args.get(6)
                .and_then(|s| serde_json::from_str(s).ok());
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(dbus::generic::dbus_call(service, path, interface, method, dbus_args.as_deref(), false))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "dbus-system" => {
            let service = require_arg(args, 2, "cosmix dbus-system <service> <path> <interface> <method> [args_json]");
            let path = require_arg(args, 3, "cosmix dbus-system <service> <path> <interface> <method> [args_json]");
            let interface = require_arg(args, 4, "cosmix dbus-system <service> <path> <interface> <method> [args_json]");
            let method = require_arg(args, 5, "cosmix dbus-system <service> <path> <interface> <method> [args_json]");
            let dbus_args: Option<Vec<serde_json::Value>> = args.get(6)
                .and_then(|s| serde_json::from_str(s).ok());
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(dbus::generic::dbus_call(service, path, interface, method, dbus_args.as_deref(), true))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "dbus-list" => {
            let service = require_arg(args, 2, "cosmix dbus-list <service> [path]");
            let path = args.get(3).map(|s| s.as_str()).unwrap_or("/");
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(dbus::generic::dbus_introspect(service, path, false))?;
            println!("{result}");
        }

        // Lua scripting
        "run" | "r" => {
            let path = require_arg(args, 2, "cosmix run <script> [args...]");
            let script_args: Vec<String> = args[3..].to_vec();
            lua::run_file(path, &script_args)?;
        }
        "shell" | "sh" => lua::run_shell()?,

        // Daemon control
        "status" => {
            println!("No daemon running (direct mode)");
        }
        "ping" => {
            println!("No daemon running (direct mode)");
        }

        _ => print_help(),
    }

    Ok(())
}

fn require_arg<'a>(args: &'a [String], idx: usize, usage: &str) -> &'a str {
    args.get(idx).map(|s| s.as_str()).unwrap_or_else(|| {
        eprintln!("Usage: {usage}");
        std::process::exit(1);
    })
}

/// Redirect fd 2 (stderr) to /dev/null before GPU libraries load.
/// This silences MESA driver warnings that bypass tracing/log.
fn suppress_stderr() {
    use std::os::unix::io::AsRawFd;
    if let Ok(devnull) = std::fs::File::open("/dev/null") {
        unsafe {
            libc::dup2(devnull.as_raw_fd(), 2);
        }
    }
}

fn print_help() {
    eprintln!("cosmix — ARexx for COSMIC");
    eprintln!();
    eprintln!("Usage: cosmix <command> [args]");
    eprintln!();
    eprintln!("Query:");
    eprintln!("  list-windows  (lw)      List all toplevel windows");
    eprintln!("  list-workspaces (lws)   List all workspaces");
    eprintln!("  clipboard (cb)          Print clipboard text");
    eprintln!();
    eprintln!("Window control:");
    eprintln!("  activate (a) <query>    Activate/focus a window");
    eprintln!("  close (c) <query>       Close a window");
    eprintln!("  minimize (min) <query>  Toggle minimize");
    eprintln!("  maximize (max) <query>  Toggle maximize");
    eprintln!("  fullscreen (fs) <query> Toggle fullscreen");
    eprintln!("  sticky (st) <query>     Toggle sticky (all workspaces)");
    eprintln!();
    eprintln!("Input injection (native Wayland, no external tools):");
    eprintln!("  type (t) <text> [delay_us]   Type text into focused window");
    eprintln!("  key (k) <combo> [delay_us]   Send key combo (e.g. ctrl+v, enter)");
    eprintln!();
    eprintln!("Notifications:");
    eprintln!("  notify (n) <summary> [body]  Send desktop notification");
    eprintln!();
    eprintln!("Dialogs (native iced):");
    eprintln!("  dialog message <title> [body]       Show message");
    eprintln!("  dialog input <prompt>               Text input -> stdout");
    eprintln!("  dialog confirm <question>           Yes/No -> exit code");
    eprintln!("  dialog list <title> <items...>      Selection -> stdout");
    eprintln!();
    eprintln!("COSMIC config:");
    eprintln!("  config-list (cl)                      List config components");
    eprintln!("  config-keys (ck) <component>          List keys for component");
    eprintln!("  config-read (cr) <component> <key>    Read config value");
    eprintln!("  config-write (cw) <component> <key> <value>  Write config value");
    eprintln!();
    eprintln!("D-Bus:");
    eprintln!("  dbus <service> <path> <iface> <method> [args_json]  Call session bus");
    eprintln!("  dbus-system <service> <path> <iface> <method> [args_json]  Call system bus");
    eprintln!("  dbus-list <service> [path]            Introspect D-Bus service");
    eprintln!();
    eprintln!("Lua scripting:");
    eprintln!("  run (r) <script>        Execute a Lua script");
    eprintln!("  shell (sh)              Interactive Lua REPL");
    eprintln!();
    eprintln!("Daemon:");
    eprintln!("  daemon                  Start the persistent daemon");
    eprintln!("  status                  Show daemon status");
    eprintln!("  ping                    Check if daemon is running");
    eprintln!();
    eprintln!("Query matches against app_id and title (case-insensitive, substring).");
}
