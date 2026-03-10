use axum::{
    Router, routing::{get, post},
    response::Json,
    extract::Path,
};
use super::AppState;
use crate::ipc;

pub fn router() -> Router<AppState> {
    Router::new()
        // Desktop automation
        .route("/windows", get(list_windows))
        .route("/workspaces", get(list_workspaces))
        .route("/ports", get(list_ports))
        .route("/port/{port}/{cmd}", post(call_port))
        .route("/screenshot", post(screenshot))
        // Mesh
        .route("/mesh/status", get(mesh_status))
        .route("/mesh/peers", get(mesh_peers))
        // Daemon
        .route("/status", get(daemon_status))
        .route("/clips", get(list_clips))
        // Config
        .route("/config/list", get(config_list))
        .route("/config/{component}/{key}", get(config_read))
}

async fn list_windows() -> Json<serde_json::Value> {
    match ipc::call("list_windows", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn list_workspaces() -> Json<serde_json::Value> {
    match ipc::call("list_workspaces", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn list_ports() -> Json<serde_json::Value> {
    match ipc::call("list_ports", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn call_port(
    Path((port, cmd)): Path<(String, String)>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let args = serde_json::json!({
        "port": port,
        "port_command": cmd,
        "args": body.map(|b| b.0),
    });
    match ipc::call("call_port", Some(args)).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!(null))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn screenshot() -> Json<serde_json::Value> {
    match ipc::call("screenshot", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!(null))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn mesh_status() -> Json<serde_json::Value> {
    match ipc::call("mesh_status", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!({"error": "no data"}))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn mesh_peers() -> Json<serde_json::Value> {
    match ipc::call("mesh_peers", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn daemon_status() -> Json<serde_json::Value> {
    match ipc::call("status", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!(null))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn list_clips() -> Json<serde_json::Value> {
    match ipc::call("list_clips", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn config_list() -> Json<serde_json::Value> {
    match ipc::call("config_list", None).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!([]))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn config_read(
    Path((component, key)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let args = serde_json::json!({
        "component": component,
        "key": key,
    });
    match ipc::call("config_read", Some(args)).await {
        Ok(resp) if resp.ok => Json(resp.data.unwrap_or(serde_json::json!(null))),
        Ok(resp) => Json(serde_json::json!({"error": resp.error})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
