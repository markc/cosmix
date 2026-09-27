//! HTTP adapter for durable upload sessions. Bytes use the existing bounded
//! lane pump; all filesystem/database work runs on the blocking pool.
use super::*;
use crate::core::store::{UploadCreate, UploadSession};

type Result<T> = crate::core::store::Result<T>;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StoreError::Io(io::Error::other(e.to_string())))?
}

async fn control<T: Send + 'static>(
    lane: &Lane,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = Arc::clone(&lane.controls)
        .try_acquire_owned()
        .map_err(|_| StoreError::Busy("lane control workers"))?;
    blocking(move || {
        let _permit = permit;
        f()
    })
    .await
}

fn text_header(headers: &HeaderMap, name: &str, required: bool) -> Result<Option<String>> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(StoreError::BadRequest(format!("duplicate {name}")));
    }
    match value {
        Some(value) => {
            let value = value
                .to_str()
                .map_err(|_| StoreError::BadRequest(format!("invalid {name}")))?;
            if value.is_empty() {
                return Err(StoreError::BadRequest(format!("empty {name}")));
            }
            Ok(Some(value.into()))
        }
        None if required => Err(StoreError::BadRequest(format!("missing {name}"))),
        None => Ok(None),
    }
}

fn integer(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn create_options(headers: &HeaderMap) -> Result<UploadCreate> {
    let owner = text_header(headers, "x-cosmix-owner", true)?.unwrap();
    let size = text_header(headers, "x-cosmix-size", true)?
        .and_then(|s| integer(&s))
        .ok_or_else(|| StoreError::BadRequest("invalid X-Cosmix-Size".into()))?;
    let expected_hash = text_header(headers, "x-cosmix-expect", false)?
        .map(|s| {
            if !s.is_ascii() {
                return Err(StoreError::BadRequest("invalid X-Cosmix-Expect".into()));
            }
            crate::core::reference::parse_blob_id(&s)
                .ok_or_else(|| StoreError::BadRequest("invalid X-Cosmix-Expect".into()))
        })
        .transpose()?;
    let name = text_header(headers, "x-cosmix-name", false)?;
    let mime = text_header(headers, "x-cosmix-mime", false)?.unwrap_or_else(|| {
        name.as_deref()
            .map(mime::sniff)
            .unwrap_or("application/octet-stream")
            .into()
    });
    Ok(UploadCreate {
        owner,
        size,
        expected_hash,
        name,
        mime,
        key: text_header(headers, "x-cosmix-upload-key", false)?,
    })
}

fn metadata_headers(mut response: Response, s: &UploadSession) -> Response {
    let mut headers = vec![
        ("x-cosmix-offset", s.offset.to_string()),
        ("x-cosmix-size", s.size.to_string()),
        ("x-cosmix-expires", s.expires_at.to_string()),
        ("x-cosmix-state", s.state.clone()),
        ("x-cosmix-owner", s.owner.clone()),
        ("x-cosmix-mime", s.mime.clone()),
    ];
    if let Some(hash) = &s.expected_hash {
        headers.push(("x-cosmix-expect", format!("b3:{hash}")));
    }
    if let Some(hash) = &s.actual_hash {
        headers.push(("x-cosmix-blob", format!("b3:{hash}")));
    }
    if let Some(key) = &s.key {
        headers.push(("x-cosmix-upload-key", key.clone()));
    }
    if let Some(name) = &s.name {
        headers.push(("x-cosmix-name", name.clone()));
    }
    for (name, value) in headers {
        if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }
    no_store(response)
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn error(error: StoreError) -> Response {
    let code = match &error {
        StoreError::UploadMissing => StatusCode::NOT_FOUND,
        StoreError::UploadConflict { .. } => StatusCode::CONFLICT,
        StoreError::UploadLimit => StatusCode::TOO_MANY_REQUESTS,
        StoreError::UploadVerify => StatusCode::UNPROCESSABLE_ENTITY,
        StoreError::QuotaOwner { .. } | StoreError::QuotaTotal { .. } => {
            StatusCode::PAYLOAD_TOO_LARGE
        }
        StoreError::BadRequest(_) => StatusCode::BAD_REQUEST,
        StoreError::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        StoreError::Io(e) => match e.get_ref().and_then(|e| e.downcast_ref::<LaneAbort>()) {
            Some(LaneAbort::Idle | LaneAbort::Deadline) => StatusCode::REQUEST_TIMEOUT,
            Some(LaneAbort::Cap) => StatusCode::BAD_REQUEST,
            _ if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::BrokenPipe =>
            {
                StatusCode::BAD_REQUEST
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = lane_error(code, &error.to_string());
    if let StoreError::UploadConflict { offset, .. } = error {
        response = json_response(
            code,
            serde_json::json!({"error": error.to_string(), "offset": offset}),
        );
        response.headers_mut().insert(
            "x-cosmix-offset",
            offset.to_string().parse().expect("integer header"),
        );
    }
    no_store(response)
}

pub(super) async fn create(State(lane): State<Arc<Lane>>, headers: HeaderMap) -> Response {
    let opts = match create_options(&headers) {
        Ok(opts) => opts,
        Err(e) => return error(e),
    };
    let store = Arc::clone(&lane.store);
    match control(&lane, move || store.upload_create(&opts)).await {
        Ok((s, new)) => {
            let mut response = metadata_headers(
                json_response(
                    if new {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    s.to_json(),
                ),
                &s,
            );
            response.headers_mut().insert(
                header::LOCATION,
                format!("/blob/uploads/{}", s.id)
                    .parse()
                    .expect("UUID location"),
            );
            response
        }
        Err(e) => error(e),
    }
}

pub(super) async fn head(
    State(lane): State<Arc<Lane>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let store = Arc::clone(&lane.store);
    let response = match control(&lane, move || store.upload_status(&id)).await {
        Ok(s) => metadata_headers(Response::new(Body::empty()), &s),
        Err(e) => error(e),
    };
    response.map(|_| Body::empty())
}

pub(super) async fn abort(
    State(lane): State<Arc<Lane>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let store = Arc::clone(&lane.store);
    match control(&lane, move || store.upload_abort(&id)).await {
        Ok(()) => no_store(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap(),
        ),
        Err(e) => error(e),
    }
}

pub(super) async fn commit(
    State(lane): State<Arc<Lane>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let permit = match Arc::clone(&lane.uploads).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return error(StoreError::Busy("lane transfer workers")),
    };
    let store = Arc::clone(&lane.store);
    match blocking(move || {
        let _permit = permit;
        store.upload_commit(&id)
    })
    .await
    {
        Ok((reference, new)) => no_store(reference_response(
            if new {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            &reference,
        )),
        Err(e) => error(e),
    }
}

fn range(headers: &HeaderMap) -> Result<(u64, u64, u64)> {
    let range = text_header(headers, "content-range", true)?.unwrap();
    let parsed = range
        .strip_prefix("bytes ")
        .and_then(|s| s.split_once('/'))
        .and_then(|(window, total)| window.split_once('-').map(|(a, b)| (a, b, total)))
        .and_then(|(a, b, total)| Some((integer(a)?, integer(b)?, integer(total)?)));
    parsed
        .filter(|(a, b, size)| a <= b && b < size)
        .ok_or_else(|| StoreError::BadRequest("expected Content-Range: bytes a-b/size".into()))
}

pub(super) async fn patch(
    State(lane): State<Arc<Lane>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (start, end, total) = match range(&headers) {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    if !headers.contains_key(header::CONTENT_LENGTH) {
        return no_store(lane_error(
            StatusCode::LENGTH_REQUIRED,
            "Content-Length is required",
        ));
    }
    let length = match text_header(&headers, "content-length", true) {
        Ok(s) => s.and_then(|s| integer(&s)),
        Err(e) => return error(e),
    };
    if length != Some(end - start + 1) || headers.contains_key(header::TRANSFER_ENCODING) {
        return error(StoreError::BadRequest(
            "Content-Length must match Content-Range; no Transfer-Encoding".into(),
        ));
    }
    for (name, expected) in [
        ("content-encoding", "identity"),
        ("content-type", "application/octet-stream"),
    ] {
        match text_header(&headers, name, false) {
            Ok(Some(value)) if value != expected => {
                return no_store(lane_error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "PATCH requires unencoded octet-stream bytes",
                ));
            }
            Err(e) => return error(e),
            _ => {}
        }
    }
    let store = Arc::clone(&lane.store);
    let ready = control(&lane, move || {
        let guard = store.upload_begin_patch(&id, start, end, total)?;
        let expires = store.upload_status(&id)?.expires_at;
        Ok((guard, expires))
    })
    .await;
    let (guard, expires) = match ready {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    let permit = match Arc::clone(&lane.uploads).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return error(StoreError::Busy("lane transfer workers")),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let remaining = Duration::from_millis(
        (expires.max(0) as u128)
            .saturating_sub(now)
            .min(u64::MAX as u128) as u64,
    );
    let deadline = tokio::time::Instant::now() + lane.upload_deadline.min(remaining);
    let (tx, rx) = mpsc::channel(PUMP_FRAMES);
    let pump = tokio::spawn(pump_body(body, tx, end - start + 1, deadline));
    let store = Arc::clone(&lane.store);
    let result = blocking(move || {
        // Guard and transfer permit live with the writer, even if the HTTP
        // handler is cancelled while its blocking task is still running.
        let _permit = permit;
        store.upload_append_guarded(&guard, start, end, total, ChannelReader::new(rx))
    })
    .await;
    let _ = pump.await;
    match result {
        Ok(s) => metadata_headers(
            json_response(StatusCode::OK, serde_json::json!({"offset": s.offset})),
            &s,
        ),
        Err(e) => error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{options, test_lane};
    use super::*;

    #[test]
    fn strict_upload_range_grammar() {
        for invalid in [
            "bytes */3",
            "bytes 0-2/*",
            "bytes 0-3/3",
            "bytes -1-2/3",
            "bytes 2-1/3",
            "bytes 0-1/3,2-2/3",
            "bytes +0-2/3",
        ] {
            let mut h = HeaderMap::new();
            h.insert("content-range", invalid.parse().unwrap());
            assert!(range(&h).is_err(), "{invalid}");
        }
        let mut h = HeaderMap::new();
        h.insert("content-range", "bytes 0-2/3".parse().unwrap());
        assert_eq!(range(&h).unwrap(), (0, 2, 3));
    }

    #[tokio::test]
    async fn lane_session_lifecycle_head_conflict_and_commit_replay() {
        let (_dir, store, addr) = test_lane(options()).await;
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/blob/uploads");
        let created = client
            .post(&base)
            .header("X-Cosmix-Owner", "tester")
            .header("X-Cosmix-Size", "3")
            .header("X-Cosmix-Upload-Key", "one")
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 201);
        let s: serde_json::Value = created.json().await.unwrap();
        let url = format!("{base}/{}", s["upload"].as_str().unwrap());
        let again = client
            .post(&base)
            .header("X-Cosmix-Owner", "tester")
            .header("X-Cosmix-Size", "3")
            .header("X-Cosmix-Upload-Key", "one")
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 200);
        assert_eq!(
            client
                .post(format!("{url}/commit"))
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        let head = client.head(&url).send().await.unwrap();
        assert_eq!(head.headers()["x-cosmix-offset"], "0");
        assert_eq!(head.headers()["x-cosmix-state"], "active");
        assert_eq!(head.headers()["cache-control"], "no-store");
        let patch = client
            .patch(&url)
            .header("Content-Range", "bytes 0-2/3")
            .body("abc")
            .send()
            .await
            .unwrap();
        assert_eq!(patch.status(), 200);
        assert_eq!(patch.headers()["x-cosmix-offset"], "3");
        let replay = client
            .patch(&url)
            .header("Content-Range", "bytes 0-2/3")
            .body("abc")
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), 409);
        assert_eq!(replay.headers()["x-cosmix-offset"], "3");
        let first = client.post(format!("{url}/commit")).send().await.unwrap();
        assert_eq!(first.status(), 201);
        let reference: serde_json::Value = first.json().await.unwrap();
        let replay = client.post(format!("{url}/commit")).send().await.unwrap();
        assert_eq!(replay.status(), 200);
        assert_eq!(replay.json::<serde_json::Value>().await.unwrap(), reference);
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
        assert_eq!(client.delete(&url).send().await.unwrap().status(), 204);
        assert_eq!(store.quota_report(None).unwrap().total.used, 3);
    }

    #[tokio::test]
    async fn lane_create_quota_and_patch_framing_refuse_before_bytes() {
        let mut opts = options();
        opts.quota_total_bytes = 3;
        let (_dir, _store, addr) = test_lane(opts).await;
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/blob/uploads");
        assert_eq!(
            client
                .post(&base)
                .header("X-Cosmix-Size", "3")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client
                .post(&base)
                .header("X-Cosmix-Owner", "tester")
                .header("X-Cosmix-Size", "4")
                .send()
                .await
                .unwrap()
                .status(),
            413
        );
        let s = client
            .post(&base)
            .header("X-Cosmix-Owner", "tester")
            .header("X-Cosmix-Size", "3")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let url = format!("{base}/{}", s["upload"].as_str().unwrap());
        assert_eq!(
            client
                .patch(&url)
                .header("Content-Range", "bytes 0-2/3")
                .body("ab")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client.head(&url).send().await.unwrap().headers()["x-cosmix-offset"],
            "0"
        );
        assert_eq!(client.delete(&url).send().await.unwrap().status(), 204);
    }

    #[tokio::test]
    async fn lane_patch_writer_conflict_and_deadline_roll_back_before_retry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_dir, store, addr, _lane) = super::super::test_support::counted_lane_deadline(
            options(),
            Duration::from_millis(200),
        )
        .await;
        let (s, _) = store
            .upload_create(&UploadCreate {
                owner: "tester".into(),
                size: 3,
                expected_hash: None,
                mime: "application/octet-stream".into(),
                name: None,
                key: None,
            })
            .unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/blob/uploads/{}", s.id);
        let guard = store.upload_guard(&s.id).unwrap();
        let refused = client
            .patch(&url)
            .header("Content-Range", "bytes 0-2/3")
            .body("abc")
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 409);
        assert_eq!(refused.headers()["x-cosmix-offset"], "0");
        assert_eq!(client.delete(&url).send().await.unwrap().status(), 409);
        assert_eq!(
            client
                .post(format!("{url}/commit"))
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        drop(guard);
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(format!("PATCH /blob/uploads/{} HTTP/1.1\r\nHost: test\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/3\r\nConnection: close\r\n\r\na",s.id).as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"));
        assert_eq!(store.upload_status(&s.id).unwrap().offset, 0);
        assert_eq!(
            std::fs::metadata(store.blobs_root().join(".uploads").join(&s.id))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            client
                .patch(&url)
                .header("Content-Range", "bytes 0-2/3")
                .body("abc")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }

    #[tokio::test]
    async fn lane_idle_watchdog_emits_abort_not_eof() {
        let body = Body::from_stream(futures_util::stream::pending::<
            std::result::Result<axum::body::Bytes, io::Error>,
        >());
        let (tx, mut rx) = mpsc::channel(PUMP_FRAMES);
        pump_body_with_idle(
            body,
            tx,
            10,
            tokio::time::Instant::now() + Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        match rx.recv().await.unwrap() {
            Frame::Abort(e) => assert!(matches!(
                e.get_ref().and_then(|e| e.downcast_ref::<LaneAbort>()),
                Some(LaneAbort::Idle)
            )),
            _ => panic!("idle body must abort"),
        }
    }

    #[tokio::test]
    async fn lane_empty_commit_hash_mismatch_and_expired_head() {
        let (_dir, store, addr) = test_lane(options()).await;
        let client = reqwest::Client::new();
        let opts = UploadCreate {
            owner: "tester".into(),
            size: 0,
            expected_hash: Some(blob::hash_bytes(b"not empty")),
            mime: "application/octet-stream".into(),
            name: None,
            key: None,
        };
        let (s, _) = store.upload_create(&opts).unwrap();
        let url = format!("http://{addr}/blob/uploads/{}", s.id);
        assert_eq!(
            client
                .post(format!("{url}/commit"))
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
        assert_eq!(
            client.head(&url).send().await.unwrap().headers()["x-cosmix-state"],
            "failed"
        );
        let (empty, _) = store
            .upload_create(&UploadCreate {
                expected_hash: None,
                ..opts
            })
            .unwrap();
        let url = format!("http://{addr}/blob/uploads/{}/commit", empty.id);
        assert_eq!(client.post(&url).send().await.unwrap().status(), 201);
        assert_eq!(client.post(&url).send().await.unwrap().status(), 200);
        let db = rusqlite::Connection::open(store.root().join("blobd.sqlite")).unwrap();
        db.execute(
            "UPDATE upload_sessions SET expires_at=0 WHERE id=?1",
            [&empty.id],
        )
        .unwrap();
        assert_eq!(
            client
                .head(format!("http://{addr}/blob/uploads/{}", empty.id))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
}
