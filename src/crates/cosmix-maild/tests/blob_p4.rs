//! P4 storage and JMAP regressions. No broker or external service required.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use base64::Engine;
use cosmix_maild::{
    config::Config,
    db, jmap,
    mailstore::{EmailEnvelope, MailStore},
    runtime::{BuiltMaild, RuntimeOpts, build_runtime},
};
use cosmix_mds::{ContainerAttrs, ContainerId, Flags, Mds, Tags};
use serde_json::{Value, json};

struct Fixture {
    built: BuiltMaild,
    _dir: tempfile::TempDir,
    inbox: ContainerId,
}

struct LocalLane(Option<String>);
#[tokio::test]
async fn html_part_download_is_forced_attachment_and_sandboxed() {
    let f = Fixture::new().await;
    let item = f.deliver(b"Content-Type: text/html\r\nContent-Disposition: attachment; filename*=utf-8''caf%C3%A9%0A.html\r\n\r\n<script>alert(1)</script>");
    let email = get_email(&f, item, Value::Null).await;
    let id = email["attachments"][0]["blobId"].as_str().unwrap();
    let response = jmap::blob_download(State(f.state()), Fixture::headers(1), Path(id.into())).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/html");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-security-policy"], "sandbox");
    assert_eq!(response.headers()["content-disposition"], "attachment; filename*=UTF-8''caf%C3%A9%0A.html");
}

// Exercise the actual typed Bus decoder, including warning-band preservation.
async fn migrate_over_port(f: &Fixture, args: Value) -> (u8, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("port.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let state = f.state();
    let db = state.db.clone();
    let ms = state.mailstore.clone();
    let worker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).await.unwrap();
        let request = cosmix_bus::bus::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        let (rc, body) = cosmix_maild::bus::blobs::migrate(&db, &ms,
            serde_json::from_str(&request.body).unwrap()).await;
        let mut reply = cosmix_bus::bus::BusMessage::new();
        reply.set("rc", &rc.to_string());
        reply.body = body;
        socket.write_all(&reply.to_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let reply = cosmix_bus::call_port_typed(path.to_str().unwrap(), "maild.blob.migrate", args).await.unwrap();
    worker.await.unwrap();
    match reply {
        cosmix_bus::PortReply::Ok { rc, value } => (rc, value),
        other => panic!("page lost through typed Bus: {other:?}"),
    }
}

#[tokio::test]
async fn failed_migration_page_keeps_cursor_and_counts_through_bus() {
    let f = Fixture::new().await;
    let state = f.state();
    for bytes in [b"missing".as_slice(), b"good"] {
        db::blob::store(&state.db.conn, &state.db.blob_dir, 1, bytes).await.unwrap();
    }
    std::fs::remove_file(cosmix_mds::blob::blob_path(&state.db.blob_dir,
        &cosmix_mds::blob::hash_bytes(b"missing"))).unwrap();
    let (rc, page) = migrate_over_port(&f, json!({"apply": true, "limit": 1})).await;
    assert_eq!(rc, 5);
    assert_eq!(page["failed"], true);
    assert_eq!(page["done"], false);
    assert_eq!(page["accounts"]["1"]["missing"], 1);
    assert_eq!(page["errors"][0]["cursor"], page["next"]);
    let (rc, last) = migrate_over_port(&f,
        json!({"apply": true, "limit": 1, "cursor": page["next"]})).await;
    assert_eq!(rc, 0);
    assert_eq!(last["done"], true);
    assert_eq!(last["failed"], false);
    assert_eq!(last["accounts"]["1"]["migrated"], 1);
}

#[tokio::test]
async fn broken_sibling_preserves_body_and_projects_undecodable_attachment() {
    let f = Fixture::new().await;
    for tail in ["AP!8=\r\n--x--\r\n", "AP!8="] {
        let raw = format!("Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=bad.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n{tail}");
        let item = f.deliver(raw.as_bytes());
        let email = get_email(&f, item, Value::Null).await;
        assert_eq!(email["hasAttachment"], true);
        assert_eq!(email["textBody"][0]["partId"], "1.1");
        assert!(email["bodyValues"]["1.1"]["value"].as_str().unwrap().contains("hello"));
        assert_eq!(email["attachments"][0]["undecodable"], true);
        assert!(email["attachments"][0].get("blobId").is_none());
        let args = json!({"account_id": 1, "email_id": item.0.to_string()});
        let (rc, list) = attachment_bus(&f, "maild.attachment.list", args.clone(), &LocalLane(None)).await;
        assert_eq!(rc, 0);
        assert_eq!(list["parts"][1]["undecodable"], true);
        let mut args = args;
        args["part"] = json!("1.2");
        let (rc, reply) = attachment_bus(&f, "maild.attachment.ref", args, &LocalLane(None)).await;
        assert_eq!(rc, 10);
        assert!(reply["error"].as_str().unwrap().starts_with("unreadable:"));
    }
}

impl cosmix_maild::blob_lane::Discovery for LocalLane {
    async fn bind(&self) -> Result<String, String> {
        Ok(self.0.as_ref().expect("unexpected lane discovery").clone())
    }
    async fn quota(&self, owner: &str) -> Result<Value, String> {
        assert!(self.0.is_some(), "unexpected quota discovery");
        assert_eq!(owner, "maild:1");
        Err("test advisory quota unavailable".into())
    }
}

async fn export_server(
    f: &Fixture,
    expected: &[u8],
    wrong_hash: bool,
) -> (LocalLane, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let bytes = expected.to_vec();
    let db = f.state().db.clone();
    let worker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
            assert!(head.len() < 8192);
        }
        let head = String::from_utf8(head).unwrap();
        let length = head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<usize>()
            .unwrap();
        let mut incoming = vec![0; length];
        socket.read_exact(&mut incoming).await.unwrap();
        assert_eq!(incoming, bytes);
        assert!(db.conn.try_lock().is_ok(), "DB mutex held across HTTP");
        let hash = if wrong_hash {
            "0".repeat(64)
        } else {
            blake3::hash(&bytes).to_hex().to_string()
        };
        let body = json!({"blob": format!("b3:{hash}"), "size": bytes.len(),
            "mime": "application/octet-stream", "name": "first-writer", "origin": "alpha"})
        .to_string();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        head
    });
    (LocalLane(Some(bind)), worker)
}

async fn attachment_bus(
    f: &Fixture,
    command: &str,
    args: Value,
    discovery: &impl cosmix_maild::blob_lane::Discovery,
) -> (u8, Value) {
    let state = f.state();
    let lane = cosmix_maild::blob_lane::Lane::new().unwrap();
    let (rc, body) = cosmix_maild::bus::attachments::dispatch(
        command,
        args,
        &state.db,
        &state.mailstore,
        &lane,
        discovery,
    )
    .await;
    (rc, serde_json::from_str(&body).unwrap())
}

#[tokio::test]
async fn attachment_and_message_exports_are_idempotent_and_retain_bookkeeping() {
    let f = Fixture::new().await;
    let item = f.deliver(MIME);
    let args = json!({"account_id": 1, "email_id": item.0.to_string()});
    let (rc, list) =
        attachment_bus(&f, "maild.attachment.list", args.clone(), &LocalLane(None)).await;
    assert_eq!(rc, 0, "{list}");
    assert_eq!(list["parts"][2]["part"], "1.2");
    assert!(list["parts"][2].get("blob").is_none());
    let mut part_args = args.clone();
    part_args["part"] = json!("1.2");
    part_args["name"] = json!("café.bin");
    let (discovery, server) = export_server(&f, &[0, 255], false).await;
    let (rc, reference) =
        attachment_bus(&f, "maild.attachment.ref", part_args.clone(), &discovery).await;
    assert_eq!(rc, 0, "{reference}");
    let headers = server.await.unwrap().to_ascii_lowercase();
    assert!(headers.starts_with(&format!("put /blob/{} ", blake3::hash(&[0, 255]).to_hex())));
    assert!(headers.contains("x-cosmix-owner: maild:1"));
    assert!(headers.contains("x-cosmix-name: caf%c3%a9.bin"));
    assert_eq!(reference["name"], "first-writer");
    assert_eq!(reference["origin"], "alpha");
    assert_eq!(reference["part"], "1.2");
    part_args["name"] = json!("new hint ignored");
    assert_eq!(
        attachment_bus(
            &f,
            "maild.attachment.ref",
            part_args.clone(),
            &LocalLane(None)
        )
        .await,
        (0, reference.clone())
    );
    let (_, list) =
        attachment_bus(&f, "maild.attachment.list", args.clone(), &LocalLane(None)).await;
    assert_eq!(list["parts"][2]["blob"], reference["blob"]);

    let (discovery, server) = export_server(&f, MIME, false).await;
    let (rc, message_ref) = attachment_bus(&f, "maild.message.ref", args.clone(), &discovery).await;
    assert_eq!(rc, 0, "{message_ref}");
    assert!(
        server
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("x-cosmix-mime: message/rfc822")
    );
    assert_eq!(message_ref["blob"], list["blob"]);
    assert!(message_ref.get("part").is_none());
    assert_eq!(
        attachment_bus(&f, "maild.message.ref", args.clone(), &LocalLane(None)).await,
        (0, message_ref)
    );

    part_args["account_id"] = json!(2);
    let (rc, denied) =
        attachment_bus(&f, "maild.attachment.ref", part_args, &LocalLane(None)).await;
    assert_eq!(rc, 10);
    assert_eq!(denied["error"], "not_found: message or part");
    let state = f.state();
    state.mailstore.delete_email(1, item).unwrap();
    assert_eq!(
        attachment_bus(&f, "maild.message.ref", args, &LocalLane(None))
            .await
            .0,
        10
    );
    for table in ["attachment_refs", "message_refs"] {
        let count: i64 = state
            .db
            .conn
            .lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[tokio::test]
async fn failed_export_validation_or_database_write_never_records_a_reference() {
    let f = Fixture::new().await;
    let item = f.deliver(MIME);
    let args = json!({"account_id": 1, "email_id": item.0.to_string(), "part": "1.2"});
    let (discovery, server) = export_server(&f, &[0, 255], true).await;
    let (rc, reply) = attachment_bus(&f, "maild.attachment.ref", args.clone(), &discovery).await;
    assert_eq!(rc, 10);
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .starts_with("verify_failed:")
    );
    server.await.unwrap();
    let state = f.state();
    state.db.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_reference BEFORE INSERT ON attachment_refs BEGIN SELECT RAISE(FAIL, 'fixture'); END;").unwrap();
    let (discovery, server) = export_server(&f, &[0, 255], false).await;
    let (rc, reply) = attachment_bus(&f, "maild.attachment.ref", args.clone(), &discovery).await;
    assert_eq!(rc, 10);
    assert!(reply["error"].as_str().unwrap().starts_with("unreadable:"));
    server.await.unwrap();
    let count: i64 = state
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM attachment_refs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    state
        .db
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_reference;")
        .unwrap();
    struct Pinned;
    impl cosmix_maild::blob_lane::Discovery for Pinned {
        async fn stat(&self, blob: &str) -> Result<Value, String> {
            assert_eq!(blob, format!("b3:{}", blake3::hash(&[0, 255]).to_hex()));
            Ok(json!({"present": true, "pins": ["maild:1"], "size": 2,
                "mime": "application/octet-stream", "origin": "alpha"}))
        }
        async fn bind(&self) -> Result<String, String> { panic!("retry must not use HTTP") }
        async fn quota(&self, _: &str) -> Result<Value, String> {
            Ok(json!({"owners": {"maild:1": {"limit": 2, "used": 2, "reserved": 0}},
                "total": {"limit": 2, "used": 2, "reserved": 0}}))
        }
    }
    assert_eq!(
        attachment_bus(&f, "maild.attachment.ref", args, &Pinned)
            .await
            .0,
        0
    );
}

#[tokio::test]
async fn list_and_export_refuse_invalid_or_unreadable_messages_before_lane_use() {
    let f = Fixture::new().await;
    let item = f.deliver(MIME);
    for part in ["0", "1.02", "1/2", "é", "1.999"] {
        let (rc, _) = attachment_bus(
            &f,
            "maild.attachment.ref",
            json!({"account_id": 1, "email_id": item.0.to_string(), "part": part}),
            &LocalLane(None),
        )
        .await;
        assert_eq!(rc, 10);
    }
    let args = json!({"account_id": 1, "email_id": item.0.to_string()});
    let state = f.state();
    let hash = state.mailstore.get_email(1, item).unwrap().blob_hash;
    let path = cosmix_mds::blob::blob_path(&state.mailstore.mds().blobs_root(), &hash);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cosmix_maild::attachments::MAX_MESSAGE as u64 + 1)
        .unwrap();
    let (rc, reply) =
        attachment_bus(&f, "maild.attachment.list", args.clone(), &LocalLane(None)).await;
    assert_eq!(rc, 10);
    assert!(reply["error"].as_str().unwrap().starts_with("too_large:"));
    std::fs::remove_file(path).unwrap();
    let (rc, reply) = attachment_bus(&f, "maild.attachment.list", args, &LocalLane(None)).await;
    assert_eq!(rc, 10);
    assert!(reply["error"].as_str().unwrap().starts_with("unreadable:"));
}

const MIME: &[u8] = b"Subject: parts\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: multipart/alternative; boundary=y\r\n\r\n--y\r\nContent-Type: text/plain\r\n\r\nhello\r\n--y\r\nContent-Type: text/html\r\n\r\n<b>hello</b>\r\n--y--\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=utf-8''caf%C3%A9.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAP8=\r\n--x--\r\n";

#[tokio::test]
async fn part_downloads_bind_account_item_hash_and_path() {
    let f = Fixture::new().await;
    let item = f.deliver(MIME);
    let email = get_email(&f, item, Value::Null).await;
    let id = email["attachments"][0]["blobId"].as_str().unwrap();
    assert_eq!(f.download(1, id).await, (StatusCode::OK, vec![0, 255]));
    let absent = (StatusCode::NOT_FOUND, b"blob not found".to_vec());
    assert_eq!(f.download(2, id).await, absent);
    let original = cosmix_maild::attachments::PartBlobId::parse(id).unwrap();
    let mut stale = original.clone();
    stale.message_hash = cosmix_mds::BlobHash([0; 32]);
    assert_eq!(f.download(1, &stale.to_string()).await, absent);
    let mut missing_item = original.clone();
    missing_item.item = cosmix_mds::ItemId(uuid::Uuid::nil());
    assert_eq!(f.download(1, &missing_item.to_string()).await, absent);
    let mut missing_part = original;
    missing_part.part = "1.999".into();
    assert_eq!(f.download(1, &missing_part.to_string()).await, absent);
    for bad in [
        "mp1_é",
        "mp1_",
        &format!("{id}_0"),
        &id.replace("_1_2", "_1_02"),
    ] {
        assert_eq!(f.download(1, bad).await.0, StatusCode::BAD_REQUEST);
    }
    let unauth = jmap::blob_download(State(f.state()), HeaderMap::new(), Path(id.to_owned()))
        .await
        .into_response();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    // A whole-message blob still follows the existing CAS route.
    assert_eq!(
        f.download(1, email["blobId"].as_str().unwrap()).await,
        (StatusCode::OK, MIME.to_vec())
    );
    // Every projected text/html body ID is downloadable too.
    for kind in ["textBody", "htmlBody"] {
        let response = f
            .download(1, email[kind][0]["blobId"].as_str().unwrap())
            .await;
        assert_eq!(response.0, StatusCode::OK);
        assert!(String::from_utf8(response.1).unwrap().contains("hello"));
    }
}

#[tokio::test]
async fn part_downloads_preserve_charset_octets_and_embedded_bytes() {
    let f = Fixture::new().await;
    let raw = b"Content-Type: text/plain; charset=iso-8859-1\r\nContent-Disposition: attachment; filename=cafe.txt\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9";
    let item = f.deliver(raw);
    let email = get_email(&f, item, Value::Null).await;
    let id = email["attachments"][0]["blobId"].as_str().unwrap();
    let response = jmap::blob_download(State(f.state()), Fixture::headers(1), Path(id.into()))
        .await
        .into_response();
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap()
            .as_ref(),
        b"caf\xe9"
    );

    let inner = b"Subject: inner\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\nAP8=";
    let raw = format!(
        "Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=mail.eml\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
        base64::engine::general_purpose::STANDARD.encode(inner)
    );
    let item = f.deliver(raw.as_bytes());
    let email = get_email(&f, item, Value::Null).await;
    let id = email["attachments"][0]["blobId"].as_str().unwrap();
    assert_eq!(email["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(f.download(1, id).await, (StatusCode::OK, inner.to_vec()));
    let mut nested = cosmix_maild::attachments::PartBlobId::parse(id).unwrap();
    nested.part = "1.1".into();
    assert_eq!(
        f.download(1, &nested.to_string()).await,
        (StatusCode::OK, vec![0, 255])
    );
}

#[tokio::test]
async fn part_download_reports_caps_and_unreadable_storage() {
    let f = Fixture::new().await;
    let item = f.deliver(MIME);
    let email = get_email(&f, item, Value::Null).await;
    let id = email["attachments"][0]["blobId"].as_str().unwrap();
    let state = f.state();
    let hash = state.mailstore.get_email(1, item).unwrap().blob_hash;
    let path = cosmix_mds::blob::blob_path(&state.mailstore.mds().blobs_root(), &hash);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cosmix_maild::attachments::MAX_MESSAGE as u64 + 1)
        .unwrap();
    let (status, body) = f.download(1, id).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.starts_with(b"too_large:"));
    // Foreign-account requests never expose the read failure or cap.
    assert_eq!(f.download(2, id).await.0, StatusCode::NOT_FOUND);
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        f.download(1, id).await,
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            b"unreadable: message".to_vec()
        )
    );
}

async fn get_email(f: &Fixture, id: cosmix_mds::ItemId, properties: Value) -> Value {
    let state = f.state();
    let result = jmap::email::get(
        &state.db,
        &state.mailstore,
        1,
        json!({"ids": [id.0.to_string()], "properties": properties,
            "fetchTextBodyValues": true, "fetchHTMLBodyValues": true}),
    )
    .await
    .unwrap();
    result["list"][0].clone()
}

#[tokio::test]
async fn mime_projection_has_one_part_scheme_and_honours_properties() {
    let f = Fixture::new().await;
    let id = f.deliver(MIME);
    let email = get_email(&f, id, Value::Null).await;
    assert_eq!(email["hasAttachment"], true);
    assert_eq!(email["attachments"][0]["partId"], "1.2");
    assert_eq!(email["attachments"][0]["name"], "café.bin");
    assert_eq!(email["attachments"][0]["size"], 2);
    assert_eq!(email["textBody"][0]["partId"], "1.1.1");
    assert_eq!(email["htmlBody"][0]["partId"], "1.1.2");
    for kind in ["textBody", "htmlBody"] {
        let path = email[kind][0]["partId"].as_str().unwrap();
        assert!(
            email["bodyValues"][path]["value"]
                .as_str()
                .unwrap()
                .contains("hello")
        );
        let blob = cosmix_maild::attachments::PartBlobId::parse(
            email[kind][0]["blobId"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(blob.part, path);
        assert_eq!(blob.item, id);
    }
    let filtered = get_email(&f, id, json!(["attachments"])).await;
    assert_eq!(filtered.as_object().unwrap().len(), 2); // id is mandatory
    assert_eq!(filtered["attachments"], email["attachments"]);
    let plain = f.deliver(b"Subject: plain\r\n\r\nhello");
    let email = get_email(&f, plain, Value::Null).await;
    assert_eq!(email["hasAttachment"], false);
    assert_eq!(email["attachments"], json!([]));
    assert_eq!(email["textBody"][0]["partId"], "1");
}

#[tokio::test]
async fn unreadable_and_over_cap_messages_project_unknown_not_false() {
    let f = Fixture::new().await;
    let id = f.deliver(MIME);
    let state = f.state();
    let hash = state.mailstore.get_email(1, id).unwrap().blob_hash;
    let path = cosmix_mds::blob::blob_path(&state.mailstore.mds().blobs_root(), &hash);
    // A sparse oversized fixture exercises the pre-read cap without allocating 64 MiB.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cosmix_maild::attachments::MAX_MESSAGE as u64 + 1)
        .unwrap();
    let email = get_email(&f, id, Value::Null).await;
    assert!(email["hasAttachment"].is_null());
    assert!(email.get("attachments").is_none());
    assert!(email.get("bodyValues").is_none());
    std::fs::remove_file(path).unwrap();
    let email = get_email(&f, id, Value::Null).await;
    assert!(email["hasAttachment"].is_null());
    assert!(email.get("attachments").is_none());
}

async fn migrate(f: &Fixture, args: Value) -> (u8, Value) {
    let state = f.state();
    let (rc, body) = cosmix_maild::bus::blobs::migrate(&state.db, &state.mailstore, args).await;
    (rc, serde_json::from_str(&body).unwrap())
}

fn item_count(f: &Fixture) -> i64 {
    let state = f.state();
    state
        .mailstore
        .mds()
        .with_set_tx(&cosmix_maild::mailstore::account_id_to_setid(1), |tx| {
            tx.tx()
                .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))
        })
        .unwrap()
}

#[tokio::test]
async fn legacy_migration_preserves_uuid_pins_and_queue_without_deleting() {
    let f = Fixture::new().await;
    let state = f.state();
    let bytes = b"Subject: legacy\r\n\r\nold upload\r\n";
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, bytes)
        .await
        .unwrap();
    state.db.conn.lock().unwrap().execute(
        "INSERT INTO smtp_queue (from_addr, to_addrs, blob_id) VALUES ('sender@example.test', '[]', ?1)",
        [id.to_string()],
    ).unwrap();
    let hash = cosmix_mds::blob::hash_bytes(bytes);
    let before = item_count(&f);
    let (rc, dry) = migrate(&f, json!({})).await;
    assert_eq!(rc, 0, "{dry}");
    assert_eq!(dry["accounts"]["1"]["planned"], 1);
    assert_eq!(dry["done"], true);
    assert_eq!(item_count(&f), before);
    assert!(!state.mailstore.mds().blob_exists(&hash).unwrap());
    assert!(matches!(
        state
            .mailstore
            .resolve_blob_ref(1, cosmix_maild::mailstore::BlobId(id))
            .unwrap(),
        cosmix_maild::mailstore::BlobRefLookup::NotFound
    ));

    let (rc, applied) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{applied}");
    assert_eq!(applied["accounts"]["1"]["migrated"], 1);
    assert_eq!(item_count(&f), before + 1);
    assert!(
        matches!(state.mailstore.resolve_blob_ref(1, cosmix_maild::mailstore::BlobId(id)).unwrap(),
        cosmix_maild::mailstore::BlobRefLookup::Found(h) if h == hash)
    );
    assert_eq!(
        f.download(1, &id.to_string()).await,
        (StatusCode::OK, bytes.to_vec())
    );
    assert_eq!(
        f.download(2, &id.to_string()).await.0,
        StatusCode::NOT_FOUND
    );
    let queue_hash: String = state
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT blob_hash FROM smtp_queue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queue_hash, cosmix_mds::blob::hex(&hash));
    let (rc, again) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{again}");
    assert_eq!(again["accounts"]["1"]["already_migrated"], 1);
    assert_eq!(item_count(&f), before + 1);
    cosmix_maild::mailstore::expiry::sweep_blocking(state.mailstore.mds(), i64::MAX / 2).unwrap();
    state.mailstore.mds().gc(false).unwrap(); // no zero-ref candidates: no quiescence sleep
    assert_eq!(item_count(&f), before + 1);
    assert_eq!(state.mailstore.mds().get_blob(&hash).unwrap(), bytes);
    assert_eq!(
        db::blob::load(&state.db.conn, &state.db.blob_dir, id)
            .await
            .unwrap()
            .unwrap(),
        bytes
    );
    let imported = jmap::email::import(
        &state.db,
        &state.mailstore,
        1,
        json!({"emails": {"old": {"blobId": id.to_string(),
            "mailboxIds": {(f.inbox.0.to_string()): true}}}}),
    )
    .await
    .unwrap();
    assert!(imported["created"]["old"]["id"].is_string(), "{imported}");
}

#[tokio::test]
async fn migration_pages_and_refusals_keep_bad_rows_untouched() {
    let f = Fixture::new().await;
    let state = f.state();
    for bytes in [b"one".as_slice(), b"two", b"three"] {
        db::blob::store(&state.db.conn, &state.db.blob_dir, 1, bytes)
            .await
            .unwrap();
    }
    let (rc, first) = migrate(&f, json!({"limit": 2})).await;
    assert_eq!(rc, 0);
    assert_eq!(first["done"], false);
    let (rc, last) = migrate(&f, json!({"limit": 2, "cursor": first["next"]})).await;
    assert_eq!(rc, 0);
    assert_eq!(last["done"], true);
    assert_eq!(last["accounts"]["1"]["planned"], 1);
    assert_eq!(item_count(&f), 0);
    let two = cosmix_mds::blob::hash_bytes(b"two");
    std::fs::write(
        cosmix_mds::blob::blob_path(&state.db.blob_dir, &two),
        b"bad",
    )
    .unwrap();
    let three = cosmix_mds::blob::hash_bytes(b"three");
    std::fs::remove_file(cosmix_mds::blob::blob_path(&state.db.blob_dir, &three)).unwrap();
    let (rc, failed) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 5);
    assert_eq!(failed["accounts"]["1"]["migrated"], 1);
    assert_eq!(failed["accounts"]["1"]["corrupt"], 1);
    assert_eq!(failed["accounts"]["1"]["missing"], 1);
    assert_eq!(item_count(&f), 1);
    assert!(!state.mailstore.mds().blob_exists(&two).unwrap());
    for args in [
        json!({"limit": 0}),
        json!({"limit": 501}),
        json!({"cursor": -1}),
        json!({"apply": "yes"}),
        json!({"unexpected": true}),
        json!([]),
    ] {
        let (rc, reply) = migrate(&f, args).await;
        assert_eq!(rc, 10);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid_arguments:")
        );
    }
}

#[tokio::test]
async fn migration_retries_after_queue_failure_without_an_extra_hold() {
    let f = Fixture::new().await;
    let state = f.state();
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, b"queued")
        .await
        .unwrap();
    {
        let conn = state.db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO smtp_queue (from_addr, blob_id) VALUES ('sender@example.test', ?1)",
            [id.to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_queue BEFORE UPDATE ON smtp_queue \
            BEGIN SELECT RAISE(ABORT, 'test queue failure'); END;",
        )
        .unwrap();
    }
    assert_eq!(migrate(&f, json!({"apply": true})).await.0, 5);
    assert_eq!(item_count(&f), 1);
    state
        .db
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_queue")
        .unwrap();
    let (rc, result) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 0, "{result}");
    assert_eq!(item_count(&f), 1);
    let hash: Option<String> = state
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT blob_hash FROM smtp_queue", [], |r| r.get(0))
        .unwrap();
    assert!(hash.is_some());
}

#[tokio::test]
async fn migration_refuses_conflicting_uuid_alias_before_copy() {
    let f = Fixture::new().await;
    let state = f.state();
    let id = db::blob::store(&state.db.conn, &state.db.blob_dir, 1, b"legacy")
        .await
        .unwrap();
    let set = cosmix_maild::mailstore::account_id_to_setid(1);
    state
        .mailstore
        .mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute(
                    "INSERT INTO blob_refs (blob_id, account_id, blob_hash, created_at) \
            VALUES (?1, 1, ?2, 0)",
                    rusqlite::params![id.to_string(), "a".repeat(64)],
                )
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    let (rc, result) = migrate(&f, json!({"apply": true})).await;
    assert_eq!(rc, 5);
    assert_eq!(result["accounts"]["1"]["conflicting"], 1);
    assert_eq!(item_count(&f), 0);
    assert!(
        !state
            .mailstore
            .mds()
            .blob_exists(&cosmix_mds::blob::hash_bytes(b"legacy"))
            .unwrap()
    );
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = |name| dir.path().join(name).to_string_lossy().into_owned();
        let cfg = Config {
            database_path: path("mail.db"),
            blob_dir: path("legacy"),
            mds_dir: path("mds"),
            spam_db_dir: Some(path("spam")),
            rule_stats_dir: Some(path("rules")),
            smtp_inbound: None,
            smtp_smtps: None,
            imap_imaps: None,
            spam_enabled: Some(false),
            ..Config::default()
        };
        let built = build_runtime(
            &cfg,
            RuntimeOpts {
                enable_bus: false,
                disable_outbound_delivery: true,
                ..RuntimeOpts::default()
            },
        )
        .await
        .unwrap();
        let password = bcrypt::hash("test", 4).unwrap();
        for id in [1, 2] {
            built
                .app_state
                .db
                .conn
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, format!("user{id}@example.test"), password],
                )
                .unwrap();
            built.app_state.mailstore.ensure_account_set(id).unwrap();
        }
        let ms = &built.app_state.mailstore;
        let inbox = ms
            .mds()
            .create_container(
                &cosmix_maild::mailstore::account_id_to_setid(1),
                None,
                "Inbox",
                ContainerAttrs {
                    special_use: Some("\\Inbox".into()),
                    subscribed: true,
                    extra: json!({}),
                },
            )
            .unwrap();
        Self {
            built,
            _dir: dir,
            inbox,
        }
    }

    fn state(&self) -> Arc<jmap::AppState> {
        self.built.app_state.clone()
    }

    fn headers(account: i32) -> HeaderMap {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("user{account}@example.test:test"));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Basic {token}").parse().unwrap());
        headers
    }

    async fn download(&self, account: i32, id: &str) -> (StatusCode, Vec<u8>) {
        let response =
            jmap::blob_download(State(self.state()), Self::headers(account), Path(id.into()))
                .await
                .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 70 * 1024 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    fn deliver(&self, bytes: &[u8]) -> cosmix_mds::ItemId {
        let ms = &self.built.app_state.mailstore;
        let hash = ms.mds().put_blob(bytes).unwrap();
        ms.create_email(
            1,
            self.inbox,
            hash,
            EmailEnvelope {
                from: "sender@example.test".into(),
                to: vec!["user1@example.test".into()],
                cc: vec![],
                bcc: vec![],
                reply_to: vec![],
                subject: "P4".into(),
                date: 0,
                message_id: None,
            },
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap()
        .0
    }
}

#[tokio::test]
async fn upload_without_legacy_write_downloads_and_imports() {
    let f = Fixture::new().await;
    let bytes = b"From: sender@example.test\r\nSubject: P4\r\n\r\nupload\r\n";
    let response = jmap::blob_upload(
        State(f.state()),
        Fixture::headers(1),
        Path("1".into()),
        axum::body::Bytes::from_static(bytes),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    let id = value["blobId"].as_str().unwrap();
    let hash = blake3::hash(bytes).to_hex().to_string();
    for download_id in [id, hash.as_str()] {
        assert_eq!(
            f.download(1, download_id).await,
            (StatusCode::OK, bytes.to_vec())
        );
        assert_eq!(f.download(2, download_id).await.0, StatusCode::NOT_FOUND);
    }
    let count: i64 = f
        .state()
        .db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        std::fs::read_dir(&f.state().db.blob_dir).unwrap().count(),
        0
    );
    let state = f.state();
    let imported = jmap::email::import(
        &state.db,
        &state.mailstore,
        1,
        json!({"emails": {"new": {"blobId": id,
            "mailboxIds": {(f.inbox.0.to_string()): true}}}}),
    )
    .await
    .unwrap();
    assert!(imported["created"]["new"]["id"].is_string(), "{imported}");
}

#[tokio::test]
async fn delivered_hash_and_legacy_fallback_are_account_scoped() {
    let f = Fixture::new().await;
    let bytes = b"Subject: delivered\r\n\r\nbody\r\n";
    f.deliver(bytes);
    let hash = blake3::hash(bytes).to_hex().to_string();
    assert_eq!(f.download(1, &hash).await, (StatusCode::OK, bytes.to_vec()));
    assert_eq!(f.download(2, &hash).await.0, StatusCode::NOT_FOUND);
    let state = f.state();
    let legacy = db::blob::store(&state.db.conn, &state.db.blob_dir, 2, bytes)
        .await
        .unwrap();
    assert_eq!(f.download(2, &hash).await.0, StatusCode::OK);
    assert_eq!(f.download(2, &legacy.to_string()).await.0, StatusCode::OK);
    assert_eq!(
        f.download(1, &legacy.to_string()).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn expired_dangling_and_unowned_cas_do_not_grant_hash_access() {
    let f = Fixture::new().await;
    let ms = &f.built.app_state.mailstore;
    let hash = ms.mds().put_blob(b"unowned").unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    assert!(!ms.owns_blob_hash(99, &hash).unwrap());
    let alias = ms.create_blob_ref(1, hash, 7, 1).unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    let set = cosmix_maild::mailstore::account_id_to_setid(1);
    ms.mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute("UPDATE blob_refs SET expires_at = ?1", [i64::MAX])
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert!(ms.owns_blob_hash(1, &hash).unwrap());
    std::fs::remove_file(cosmix_mds::blob::blob_path(&ms.mds().blobs_root(), &hash)).unwrap();
    assert!(!ms.owns_blob_hash(1, &hash).unwrap());
    assert_ne!(f.download(1, &alias.to_string()).await.0, StatusCode::OK);
    ms.mds()
        .with_set_tx(&set, |tx| {
            tx.tx()
                .execute_batch("DROP TABLE blob_refs")
                .map_err(|e| cosmix_mds::Error::Other(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.download(1, &cosmix_mds::blob::hex(&hash)).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
