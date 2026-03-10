use axum::{Router, routing::get, response::Html, extract::Path};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{name}", get(page))
}

/// Serve an HTML fragment for the main content area.
async fn page(Path(name): Path<String>) -> Html<String> {
    let content = match name.as_str() {
        "chat" => include_str!("../templates/pages/chat.html"),
        "mail" => include_str!("../templates/pages/mail.html"),
        "settings" => include_str!("../templates/pages/settings.html"),
        _ => "<h2>Page not found</h2>",
    };
    Html(content.to_string())
}
