use axum::{Router, routing::get, response::Html, extract::Path};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{id}", get(panel))
}

/// Serve an HTML fragment for a sidebar panel.
async fn panel(Path(id): Path<String>) -> Html<String> {
    let content = match id.as_str() {
        "nav" => include_str!("../templates/panels/nav.html").to_string(),
        "conversations" => include_str!("../templates/panels/conversations.html").to_string(),
        "mailboxes" => include_str!("../templates/panels/mailboxes.html").to_string(),
        "theme" => include_str!("../templates/panels/theme.html").to_string(),
        "mesh" => render_mesh_panel().await,
        _ => "<p>Panel not found</p>".to_string(),
    };
    Html(content)
}

async fn render_mesh_panel() -> String {
    let mut html = String::from(r#"<div class="panel-mesh"><h3>Mesh Peers</h3>"#);

    match crate::ipc::call("mesh_status", None).await {
        Ok(resp) if resp.ok => {
            if let Some(data) = resp.data {
                let connected = data.get("connected_peers").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = data.get("total_peers").and_then(|v| v.as_u64()).unwrap_or(0);
                html.push_str(&format!(
                    r#"<p class="text-sm text-muted">{connected}/{total} connected</p>"#
                ));

                if let Some(peers) = data.get("peers").and_then(|v| v.as_array()) {
                    html.push_str("<ul>");
                    for peer in peers {
                        let name = peer.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let ip = peer.get("wg_ip").and_then(|v| v.as_str()).unwrap_or("");
                        let is_connected = peer.get("connected").and_then(|v| v.as_bool()).unwrap_or(false);
                        let icon = if is_connected { "●" } else { "○" };
                        let color = if is_connected { "var(--success)" } else { "var(--text-muted)" };
                        html.push_str(&format!(
                            r#"<li><span style="color:{color}">{icon}</span> {name} <span class="text-sm text-muted">{ip}</span></li>"#
                        ));
                    }
                    html.push_str("</ul>");
                }
            }
        }
        Ok(resp) => {
            let err = resp.error.unwrap_or_else(|| "Unknown error".into());
            html.push_str(&format!(r#"<p class="text-sm text-muted">{err}</p>"#));
        }
        Err(_) => {
            html.push_str(r#"<p class="text-sm text-muted">Daemon not running</p>"#);
        }
    }

    html.push_str("</div>");
    html
}
