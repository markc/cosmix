use axum::{
    Router, routing::{get, post},
    response::{Html, Json, IntoResponse, Response},
    extract::State,
    http::header,
    Form,
};
use serde::Deserialize;
use tokio::sync::mpsc;
use futures_util::stream::Stream;

use super::{AppState, html_escape};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/send", post(send_message))
        .route("/chat/stream", get(stream_response))
        .route("/chat/sessions", get(list_sessions))
        .route("/chat/sessions", post(create_session))
}

#[derive(Deserialize)]
struct ChatMessage {
    message: String,
}

/// Handle chat message submission — returns an HTML fragment with the user message
/// and triggers an HTMX load for the AI response.
async fn send_message(
    State(_state): State<AppState>,
    session: tower_sessions::Session,
    Form(form): Form<ChatMessage>,
) -> Html<String> {
    let user_name: String = session
        .get("user_name").await.unwrap_or(None)
        .unwrap_or_else(|| "You".into());

    let html = format!(
        r#"<div class="chat-msg chat-msg-user">
            <strong>{}</strong>
            <p>{}</p>
        </div>
        <div class="chat-msg chat-msg-assistant" id="ai-response"
             hx-get="/api/chat/stream?message={}"
             hx-trigger="load"
             hx-swap="innerHTML">
            <strong>Assistant</strong>
            <p class="placeholder">Thinking...</p>
        </div>"#,
        html_escape(&user_name),
        html_escape(&form.message),
        urlencoding::encode(&form.message),
    );
    Html(html)
}

/// Stream the AI response from Ollama via SSE.
async fn stream_response(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let message = params.get("message").cloned().unwrap_or_default();

    let client = reqwest::Client::new();
    let ollama_url = std::env::var("OLLAMA_URL")
        .unwrap_or_else(|_| "http://192.168.2.130:11434".into());
    let model = std::env::var("OLLAMA_MODEL")
        .unwrap_or_else(|_| "qwen3:30b-a3b".into());

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "Respond directly and concisely. Do not use internal thinking or reasoning blocks."
            },
            {"role": "user", "content": message}
        ],
        "stream": true,
        "options": {
            "num_predict": 1024
        }
    });

    let (tx, rx) = mpsc::channel::<Result<String, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let resp = client.post(format!("{ollama_url}/api/chat"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await;

        match resp {
            Ok(resp) => {
                use futures_util::StreamExt;
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            for line in text.lines() {
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                                    if let Some(content) = json
                                        .get("message")
                                        .and_then(|m| m.get("content"))
                                        .and_then(|c| c.as_str())
                                    {
                                        if !content.is_empty() {
                                            let escaped = html_escape(content);
                                            let _ = tx.send(Ok(escaped)).await;
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Ok(format!("Error: {e}"))).await;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::TRANSFER_ENCODING, "chunked")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap()
        .into_response()
}

async fn list_sessions() -> Json<serde_json::Value> {
    // Placeholder -- will read from PostgreSQL
    Json(serde_json::json!([]))
}

async fn create_session() -> Json<serde_json::Value> {
    // Placeholder -- will create in PostgreSQL
    Json(serde_json::json!({"id": "new", "title": "New Chat"}))
}
