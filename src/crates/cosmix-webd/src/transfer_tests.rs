//! Local wire fixtures: real NodedClient discovery and real reqwest lane I/O.
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};

pub(crate) struct FakeLane {
    pub client: Arc<cosmix_client::NodedClient>,
    pub uploads: Arc<AtomicUsize>,
    pub started: Arc<Notify>,
    pub release: Arc<Semaphore>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
#[derive(Clone)]
struct LaneState {
    bytes: Bytes,
    uploads: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Semaphore>,
}
async fn lane(
    State(state): State<LaneState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::PUT {
        state.uploads.fetch_add(1, Ordering::SeqCst);
        state.started.notify_one();
        let _permit = state.release.acquire().await.unwrap();
        let reference = crate::blob_reference::Reference {
            blob: format!("b3:{}", blake3::hash(&body).to_hex()),
            size: body.len() as u64,
            mime: "image/png".into(),
            name: None,
            origin: "alpha".into(),
        };
        return Response::builder()
            .status(StatusCode::CREATED)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&reference).unwrap()))
            .unwrap();
    }
    let size = state.bytes.len() as u64;
    let range =
        crate::blob_lane::expected_range(headers.get("range").and_then(|h| h.to_str().ok()), size);
    let (status, start, len, cr) = match range {
        crate::blob_lane::Range::Full => (200, 0, size, None),
        crate::blob_lane::Range::Partial(start, len) => (
            206,
            start,
            len,
            Some(format!("bytes {start}-{}/{size}", start + len - 1)),
        ),
        crate::blob_lane::Range::Unsatisfiable => (416, 0, 0, Some(format!("bytes */{size}"))),
    };
    let mut response = Response::builder()
        .status(status)
        .header("content-length", len)
        .header("accept-ranges", "bytes")
        .header("content-type", "application/octet-stream")
        .header("set-cookie", "must-not-leak=1");
    if let Some(cr) = cr {
        response = response.header("content-range", cr);
    }
    response
        .body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(state.bytes.slice(start as usize..(start + len) as usize))
        })
        .unwrap()
}
impl FakeLane {
    pub async fn start(bytes: &[u8], present: bool, stalled: bool) -> Self {
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = http.local_addr().unwrap().to_string();
        let state = LaneState {
            bytes: Bytes::copy_from_slice(bytes),
            uploads: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Semaphore::new(if stalled { 0 } else { 8 })),
        };
        let (uploads, started, release) = (
            state.uploads.clone(),
            state.started.clone(),
            state.release.clone(),
        );
        let app = axum::Router::new()
            .route("/blob/{hash}", axum::routing::any(lane))
            .with_state(state);
        let http_task = tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        });
        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", ws.local_addr().unwrap());
        let size = bytes.len();
        let broker_task = tokio::spawn(async move {
            let (socket, _) = ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Ok(text) = message.to_text() else {
                    continue;
                };
                let request = cosmix_bus::bus::parse(text).unwrap();
                let args: serde_json::Value =
                    serde_json::from_str(&request.body).unwrap_or_default();
                let value = match request.get("command").unwrap() {
                    "blob.props.get" => json!({"bind": bind}),
                    "blob.stat" => {
                        json!({"present": present, "size": size, "mime": "image/png", "origin":"alpha", "pins":[]})
                    }
                    "blob.pin" => json!({}),
                    "blob.quota" => {
                        let quota = json!({"used":0,"reserved":0,"limit":100000000});
                        let owner = args["owner"].as_str().unwrap();
                        json!({"owners": {owner: quota.clone()}, "total": quota})
                    }
                    command => panic!("unexpected command {command}"),
                };
                let mut reply = cosmix_bus::bus::BusMessage::new()
                    .with_header("type", "response")
                    .with_header("id", request.get("id").unwrap())
                    .with_header("rc", "0")
                    .with_header("from", "blobd")
                    .with_header("to", request.get("from").unwrap());
                reply.body = value.to_string();
                if socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        reply.to_wire().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let client = Arc::new(
            cosmix_client::NodedClient::connect_anonymous(&url)
                .await
                .unwrap(),
        );
        Self {
            client,
            uploads,
            started,
            release,
            tasks: vec![http_task, broker_task],
        }
    }
}
impl Drop for FakeLane {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
