pub mod protocol;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::daemon::events::EventBus;
use crate::daemon::state::SharedState;
use self::protocol::{IpcRequest, IpcResponse};

pub async fn serve(socket_path: &str, state: SharedState, events: EventBus) -> Result<()> {
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!("IPC listening on {socket_path}");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        let events = events.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state, events).await {
                tracing::debug!("IPC connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    state: SharedState,
    events: EventBus,
) -> Result<()> {
    // Read length prefix (4 bytes big-endian)
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 1_048_576 {
        anyhow::bail!("Request too large: {len} bytes");
    }

    // Read JSON payload
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let request: IpcRequest = serde_json::from_slice(&buf)?;
    let response = dispatch(request, &state, &events).await;

    // Write response
    let resp_bytes = protocol::encode(&response);
    stream.write_all(&resp_bytes).await?;

    Ok(())
}

async fn dispatch(request: IpcRequest, state: &SharedState, _events: &EventBus) -> IpcResponse {
    match request {
        IpcRequest::Ping => IpcResponse::success(serde_json::json!("pong")),

        IpcRequest::ListWindows => {
            let s = state.read().unwrap();
            let mut windows: Vec<serde_json::Value> = s.windows.values()
                .map(|w| {
                    serde_json::json!({
                        "app_id": w.app_id,
                        "title": w.title,
                        "activated": w.activated,
                        "maximized": w.maximized,
                        "minimized": w.minimized,
                        "fullscreen": w.fullscreen,
                        "sticky": w.sticky,
                        "geometry": w.geometry.as_ref().map(|g| serde_json::json!({
                            "x": g.x, "y": g.y, "width": g.width, "height": g.height
                        })),
                    })
                }).collect();
            windows.sort_by(|a, b| {
                let a_id = a["app_id"].as_str().unwrap_or("");
                let b_id = b["app_id"].as_str().unwrap_or("");
                a_id.cmp(b_id)
            });
            IpcResponse::success(serde_json::Value::Array(windows))
        }

        IpcRequest::ListWorkspaces => {
            let s = state.read().unwrap();
            let mut workspaces: Vec<serde_json::Value> = s.workspaces.values().map(|ws| {
                serde_json::json!({
                    "name": ws.name,
                    "active": ws.active,
                    "urgent": ws.urgent,
                    "hidden": ws.hidden,
                    "coordinates": ws.coordinates,
                })
            }).collect();
            workspaces.sort_by(|a, b| {
                let a_name = a["name"].as_str().unwrap_or("");
                let b_name = b["name"].as_str().unwrap_or("");
                a_name.cmp(b_name)
            });
            IpcResponse::success(serde_json::Value::Array(workspaces))
        }

        IpcRequest::GetClipboard => {
            match crate::dbus::clipboard::get_clipboard() {
                Ok(text) => IpcResponse::success(serde_json::Value::String(text)),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::SetClipboard { text } => {
            match crate::dbus::clipboard::set_clipboard(&text) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Activate { query } => {
            match crate::wayland::toplevel::activate_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Close { query } => {
            match crate::wayland::toplevel::close_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Minimize { query } => {
            match crate::wayland::toplevel::minimize_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Maximize { query } => {
            match crate::wayland::toplevel::maximize_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Fullscreen { query } => {
            match crate::wayland::toplevel::fullscreen_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Sticky { query } => {
            match crate::wayland::toplevel::sticky_window(&query) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Notify { summary, body } => {
            match crate::dbus::notify::notify_cmd(&summary, &body) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::RunScript { name, args } => {
            match crate::lua::run_file(&name, &args) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::Status => {
            let uptime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            IpcResponse::success(serde_json::json!({
                "running": true,
                "pid": std::process::id(),
                "timestamp": uptime,
                "version": env!("CARGO_PKG_VERSION"),
            }))
        }

        IpcRequest::TypeText { text, delay_us } => {
            match crate::wayland::virtual_keyboard::type_text(&text, delay_us) {
                Ok(()) => IpcResponse::success(serde_json::json!({
                    "typed": text.len()
                })),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::SendKey { combo, delay_us } => {
            match crate::wayland::virtual_keyboard::send_key(&combo, delay_us) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::ConfigList => {
            match crate::cosmic_config::list_components() {
                Ok(components) => IpcResponse::success(serde_json::json!(components)),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::ConfigKeys { component } => {
            match crate::cosmic_config::list_keys(&component) {
                Ok(keys) => IpcResponse::success(serde_json::json!(keys)),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::ConfigRead { component, key } => {
            match crate::cosmic_config::read_key(&component, &key) {
                Ok(value) => IpcResponse::success(serde_json::Value::String(value)),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::ConfigWrite { component, key, value } => {
            match crate::cosmic_config::write_key(&component, &key, &value) {
                Ok(()) => IpcResponse::ok(),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::DbusCall { service, path, interface, method, args, system } => {
            match crate::dbus::generic::dbus_call(&service, &path, &interface, &method, args.as_deref(), system).await {
                Ok(value) => IpcResponse::success(value),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::CallPort { port, port_command, args } => {
            // Find the port socket from registry
            let socket_path = {
                let s = state.read().unwrap();
                s.port_registry.socket_path(&port)
            };

            match socket_path {
                Some(path) => {
                    let path_str = path.to_string_lossy().to_string();
                    let json_args = args.unwrap_or(serde_json::Value::Null);
                    match cosmix_port::call_port(&path_str, &port_command, json_args).await {
                        Ok(data) => IpcResponse::success(data),
                        Err(e) => IpcResponse::error(&format!("Port call failed: {e}")),
                    }
                }
                None => IpcResponse::error(&format!("Port '{port}' not found in registry")),
            }
        }

        IpcRequest::ListPorts => {
            let s = state.read().unwrap();
            let mut ports: Vec<serde_json::Value> = Vec::new();

            // Built-in ports
            for name in &["clipboard", "windows", "screenshot", "dbus", "config", "mail", "midi", "notify", "input"] {
                ports.push(serde_json::json!({
                    "name": name,
                    "type": "builtin",
                }));
            }

            // App ports from registry
            for info in s.port_registry.ports.values() {
                ports.push(serde_json::json!({
                    "name": info.name,
                    "type": "app",
                    "socket": info.socket.to_string_lossy(),
                    "commands": info.commands,
                }));
            }

            IpcResponse::success(serde_json::Value::Array(ports))
        }

        IpcRequest::Screenshot { path } => {
            let save_path = match path {
                Some(p) => std::path::PathBuf::from(p),
                None => {
                    let dir = crate::wayland::screenshot::screenshots_dir();
                    let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
                    dir.join(format!("Screenshot_{ts}.png"))
                }
            };
            match crate::wayland::screenshot::capture_screenshot(&save_path) {
                Ok(p) => IpcResponse::success(serde_json::json!({
                    "path": p.to_string_lossy(),
                })),
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }

        IpcRequest::ListApps => {
            match crate::desktop::list_apps() {
                Ok(apps) => {
                    let data: Vec<serde_json::Value> = apps.iter().map(|a| {
                        serde_json::json!({
                            "name": a.name,
                            "exec": a.exec,
                            "icon": a.icon,
                            "comment": a.comment,
                            "categories": a.categories,
                            "terminal": a.terminal,
                        })
                    }).collect();
                    IpcResponse::success(serde_json::Value::Array(data))
                }
                Err(e) => IpcResponse::error(&e.to_string()),
            }
        }
    }
}

/// Client: connect to daemon and send a request, get response
pub fn client_request(socket_path: &str, request: &IpcRequest) -> Result<IpcResponse> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut stream = tokio::net::UnixStream::connect(socket_path).await
            .map_err(|e| anyhow::anyhow!("Cannot connect to daemon at {socket_path}: {e}"))?;

        let req_bytes = protocol::encode_request(request);
        stream.write_all(&req_bytes).await?;

        // Read response length
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let response: IpcResponse = serde_json::from_slice(&buf)?;
        Ok(response)
    })
}

/// Try to connect to daemon, returns None if daemon isn't running
pub fn try_daemon(socket_path: &str) -> bool {
    std::path::Path::new(socket_path).exists()
}
