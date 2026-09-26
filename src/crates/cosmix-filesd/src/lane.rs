//! filesd's blob byte-lane client. Bus discovers the lane; only bytes use HTTP.

use std::io::Read;
use std::time::Duration;

use cosmix_bus::PortReply;
use cosmix_client::NodedClient;
use cosmix_files::{FilesError, atomic, fsops::FsLayer};
use serde_json::{Value, json};

const IO_TIMEOUT: Duration = Duration::from_secs(30);
const PROPS_TIMEOUT: Duration = Duration::from_secs(10);
const REFERENCE_LIMIT: u64 = 64 * 1024;

pub async fn bind(client: &NodedClient, service: &str) -> Result<String, String> {
    let reply = tokio::time::timeout(
        PROPS_TIMEOUT,
        client.call_typed(service, "blob.props.get", json!({"path": "lane"})),
    )
    .await
    .map_err(|_| format!("lane_unavailable: blob.props.get on {service} timed out"))?
    .map_err(|e| format!("lane_unavailable: blob.props.get on {service}: {e}"))?;
    bind_from_reply(&reply)
}

fn checked_bind(bind: &str) -> Result<&str, String> {
    if bind.is_empty() {
        return Err("lane_unavailable: blobd lane not listening".into());
    }
    // blobd publishes a SocketAddr, never a URL, hostname or URL credentials.
    bind.parse::<std::net::SocketAddr>()
        .map_err(|_| "lane_unavailable: invalid lane bind".to_string())?;
    Ok(bind)
}

fn bind_from_reply(reply: &PortReply) -> Result<String, String> {
    match reply {
        PortReply::Ok { value, .. } => {
            let bind = value.get("bind").and_then(Value::as_str)
                .ok_or("lane_unavailable: props lane carries no bind")?;
            checked_bind(bind).map(str::to_string)
        }
        PortReply::AppError { message, .. } => Err(format!("lane_unavailable: {message}")),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(IO_TIMEOUT)
        .timeout_read(IO_TIMEOUT)
        .timeout_write(IO_TIMEOUT)
        .redirects(0)
        .build()
}

fn lane_closed(error: &ureq::Transport) -> bool {
    error.kind() == ureq::ErrorKind::Io
        && std::error::Error::source(error)
            .and_then(|e| e.downcast_ref::<std::io::Error>())
            .is_some_and(|e| matches!(e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::UnexpectedEof))
}

fn http_error(error: ureq::Error, upload: bool) -> String {
    match error {
        ureq::Error::Status(404, _) if !upload =>
            "not_present: blob is not on this node — blob.fetch it first".into(),
        ureq::Error::Status(status, response) => {
            let mut body = String::new();
            let _ = response.into_reader().take(512).read_to_string(&mut body);
            let token = if status == 413 { "quota" } else { "lane" };
            format!("{token}: HTTP {status}: {body}")
        }
        ureq::Error::Transport(error) if upload && lane_closed(&error) => format!(
            "lane: lane closed during upload (refused? check blobd quota): {error}"
        ),
        other => format!("lane: {other}"),
    }
}

/// Accept a bare lowercase hash, a b3 id, or a reference's blob member.
fn blob_hex(value: &Value) -> Result<&str, String> {
    let id = value.as_str().or_else(|| value.get("blob").and_then(Value::as_str))
        .ok_or("invalid blob id")?;
    let hex = id.strip_prefix("b3:").unwrap_or(id);
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("invalid blob id".into());
    }
    Ok(hex)
}

fn parse_reference(body: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("lane: reference is not JSON: {e}"))?;
    let id = value.get("blob").and_then(Value::as_str)
        .filter(|id| id.starts_with("b3:"))
        .ok_or("lane: reference carries no b3 blob id")?;
    blob_hex(&json!(id)).map_err(|_| "lane: invalid reference blob id".to_string())?;
    value.get("size").and_then(Value::as_u64)
        .ok_or("lane: reference carries no size")?;
    Ok(value)
}

/// `fs.blob.ref`: resolve for read before any HTTP, stream the open file and pin
/// it to this filesd service. Name/MIME defaults come from the filesystem layer.
pub fn reference(fs: &FsLayer, a: &Value, bind: Result<&str, &str>, owner: &str) -> Result<Value, String> {
    let path = super::req(a, "path")?;
    let (file, name, mime) = fs.open_blob(path).map_err(super::estr)?;
    let length = file.metadata().map_err(|e| super::estr(e.into()))?.len();
    let bind = checked_bind(bind.map_err(str::to_string)?)?;
    let response = agent().post(&format!("http://{bind}/blob"))
        .set("Content-Length", &length.to_string())
        .set("X-Cosmix-Owner", owner)
        .set("X-Cosmix-Name", a["name"].as_str().unwrap_or(&name))
        .set("X-Cosmix-Mime", a["mime"].as_str().unwrap_or(mime))
        .send(file.take(length))
        .map_err(|e| http_error(e, true))?;
    if response.status() != 201 {
        return Err(format!("lane: expected HTTP 201, got {}", response.status()));
    }
    let mut body = String::new();
    response.into_reader().take(REFERENCE_LIMIT + 1).read_to_string(&mut body)
        .map_err(|e| format!("lane: read reference: {e}"))?;
    if body.len() as u64 > REFERENCE_LIMIT {
        return Err("lane: reference exceeds size limit".into());
    }
    let mut reference = parse_reference(&body)?;
    if reference["size"].as_u64() != Some(length) {
        return Err("lane: reference size differs from uploaded length".into());
    }
    reference["path"] = json!(path);
    Ok(reference)
}

/// `fs.blob.materialise`: GET only from the selected local store, then verify
/// before publishing under the write jail. This never implicitly blob.fetches.
pub fn materialise(fs: &FsLayer, a: &Value, bind: Result<&str, &str>) -> Result<Value, String> {
    let hex = blob_hex(&a["blob"])?;
    let path = super::req(a, "path")?;
    let overwrite = a["overwrite"].as_bool().unwrap_or(false);
    fs.blob_target(path, overwrite).map_err(super::estr)?;
    let bind = checked_bind(bind.map_err(str::to_string)?)?;
    let response = agent().get(&format!("http://{bind}/blob/{hex}"))
        .call().map_err(|e| http_error(e, false))?;
    if response.status() != 200 {
        return Err(format!("lane: expected HTTP 200, got {}", response.status()));
    }
    // Feature unification may enable ureq decompression in a workspace build.
    // The lane's protocol is raw Content-Length bytes; reject alternate framing.
    if response.header("Transfer-Encoding").is_some()
        || response.header("Content-Encoding").is_some()
    {
        return Err("lane: encoded blob response is not supported".into());
    }
    let length = response.header("Content-Length").and_then(|s| s.parse::<u64>().ok())
        .ok_or("lane: missing or invalid Content-Length")?;
    let target = fs.blob_target(path, overwrite).map_err(super::estr)?;
    // ureq exposes exactly the HTTP Content-Length body. Extra wire bytes after
    // that frame are not exposed; land_verified also checks length for any Read.
    let size = atomic::land_verified(&target, response.into_reader(), length, hex, overwrite)
        .map_err(|e| match e {
            FilesError::SourceRead(e) => format!("lane: read blob body: {e}"),
            other => other.to_string(),
        })?;
    Ok(json!({"ok": true, "path": path, "blob": format!("b3:{hex}"), "size": size}))
}

pub fn verb(fs: &FsLayer, verb: &str, args: &Value, bind: &Result<String, String>, owner: &str) -> (u8, String) {
    let bind = bind.as_ref().map(String::as_str).map_err(String::as_str);
    let result = match verb {
        "fs.blob.ref" => reference(fs, args, bind, owner),
        "fs.blob.materialise" => materialise(fs, args, bind),
        _ => Err(format!("unknown verb: {verb}")),
    };
    match result {
        Ok(value) => (0, value.to_string()),
        Err(error) => (10, super::jerr(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_files::{fsops::Place, hash::content_hash};
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "filesd_lane_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn layer(&self, writable: bool, allow: Vec<String>) -> FsLayer {
            FsLayer::new(vec![Place {
                id: "home".into(), label: "Home".into(), group: "places".into(),
                icon: "folder".into(), root: self.0.clone(), writable, order: 0,
                allow, deny: Vec::new(),
            }], self.0.join(".Trash"))
        }

        fn no_temp(&self) {
            assert!(fs::read_dir(&self.0).unwrap().all(|e| {
                !e.unwrap().file_name().to_string_lossy().contains(".tmp.")
            }));
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// One HTTP exchange, with the received request returned by joining the
    /// thread. The caller can deliberately lie about length to test truncation.
    fn serve_once(status: &str, bytes: &[u8], length: usize) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let status = status.to_string();
        let bytes = bytes.to_vec();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                let blank = line == "\r\n";
                head.push_str(&line);
                if blank { break; }
            }
            let body_len = head.lines().filter_map(|s| s.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap()).unwrap_or(0);
            let mut body = vec![0; body_len];
            reader.read_exact(&mut body).unwrap();
            write!(reader.get_mut(), "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
            reader.get_mut().write_all(&bytes).unwrap();
            (head, body)
        });
        (bind, worker)
    }

    fn invoke(fs: &FsLayer, command: &str, args: Value, bind: &str) -> (u8, Value) {
        let (rc, body) = verb(fs, command, &args, &Ok(bind.into()), "filesd-test");
        (rc, serde_json::from_str(&body).unwrap())
    }

    fn assert_error(result: (u8, Value), token: &str) {
        assert_eq!(result.0, 10, "{}", result.1);
        assert!(result.1["error"].as_str().unwrap().starts_with(token), "{}", result.1);
    }

    #[test]
    fn reference_streams_headers_and_adds_path() {
        let dir = Scratch::new();
        let fs = dir.layer(false, vec![]); // read-only places can be sources
        let bytes = b"hello";
        fs::write(dir.0.join("hello.txt"), bytes).unwrap();
        let id = format!("b3:{}", content_hash(bytes));
        let reply = json!({"blob": id, "size": 5, "mime": "text/plain", "name": "hello.txt", "origin": "alpha", "instance": "two"}).to_string();
        let (bind, worker) = serve_once("201 Created", reply.as_bytes(), reply.len());
        let (rc, value) = invoke(&fs, "fs.blob.ref", json!({"path": "home/hello.txt"}), &bind);
        assert_eq!(rc, 0, "{value}");
        assert_eq!(value["blob"], id);
        assert_eq!(value["path"], "home/hello.txt");
        assert_eq!(value["instance"], "two");
        let (head, body) = worker.join().unwrap();
        let head = head.to_ascii_lowercase();
        for expected in ["post /blob http/1.1", "content-length: 5", "x-cosmix-owner: filesd-test", "x-cosmix-name: hello.txt", "x-cosmix-mime: text/plain"] {
            assert!(head.contains(expected), "{head}");
        }
        assert!(!head.contains("transfer-encoding"));
        assert_eq!(body, bytes);
    }

    #[test]
    fn reference_honours_name_and_mime_overrides() {
        let dir = Scratch::new();
        fs::write(dir.0.join("file"), b"x").unwrap();
        let reply = json!({"blob": format!("b3:{}", content_hash(b"x")), "size": 1}).to_string();
        let (bind, worker) = serve_once("201 Created", reply.as_bytes(), reply.len());
        let (rc, value) = invoke(&dir.layer(true, vec![]), "fs.blob.ref", json!({"path": "home/file", "name": "x.png", "mime": "image/png"}), &bind);
        assert_eq!(rc, 0, "{value}");
        let head = worker.join().unwrap().0.to_ascii_lowercase();
        assert!(head.contains("x-cosmix-name: x.png"));
        assert!(head.contains("x-cosmix-mime: image/png"));
    }

    #[test]
    fn reference_quota_down_and_empty_lane_tokens() {
        let dir = Scratch::new();
        let fs = dir.layer(true, vec![]);
        fs::write(dir.0.join("file"), b"x").unwrap();
        let reply = b"{\"error\":\"quota: filesd-test\"}";
        let (bind, worker) = serve_once("413 Payload Too Large", reply, reply.len());
        let result = invoke(&fs, "fs.blob.ref", json!({"path": "home/file"}), &bind);
        assert!(result.1["error"].as_str().unwrap().contains("quota: filesd-test"));
        assert_error(result, "quota:");
        worker.join().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let down = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert_error(invoke(&fs, "fs.blob.ref", json!({"path": "home/file"}), &down), "lane:");
        assert_error(invoke(&fs, "fs.blob.ref", json!({"path": "home/file"}), ""), "lane_unavailable:");
        assert!(bind_from_reply(&PortReply::Ok { rc: 0, value: json!({"bind": ""}) }).unwrap_err().starts_with("lane_unavailable:"));
        assert!(bind_from_reply(&PortReply::AppError { rc: 10, message: "absent".into() }).unwrap_err().starts_with("lane_unavailable:"));
    }

    #[test]
    fn reference_refusals_happen_before_http() {
        let dir = Scratch::new();
        fs::create_dir(dir.0.join("folder")).unwrap();
        fs::write(dir.0.join("file"), b"secret").unwrap();
        let plain = dir.layer(true, vec![]);
        let scoped = dir.layer(true, vec!["folder/*".into()]);
        let prefix = dir.layer(true, vec!["file/*".into()]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        for (fs, path, token) in [
            (&plain, "home/folder", "bad request:"),
            (&plain, "unknown/file", "denied:"),
            (&plain, "home", "denied:"),
            (&plain, "home/", "denied:"),
            (&plain, "home/missing", "not found:"),
            (&scoped, "home/file", "denied:"),
            (&prefix, "home/file", "denied:"),
        ] {
            assert_error(invoke(fs, "fs.blob.ref", json!({"path": path}), &bind), token);
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.0.join("file"), dir.0.join("folder/link")).unwrap();
            assert_error(invoke(&scoped, "fs.blob.ref", json!({"path": "home/folder/link"}), &bind), "denied:");
            assert_error(invoke(&plain, "fs.blob.ref", json!({"path": "home/folder/link"}), &bind), "bad request:");
            fs::hard_link(dir.0.join("file"), dir.0.join("folder/hard")).unwrap();
            assert_error(invoke(&plain, "fs.blob.ref", json!({"path": "home/folder/hard"}), &bind), "bad request:");
        }
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn materialise_verifies_and_lands_and_creates_parents() {
        let dir = Scratch::new();
        let fs = dir.layer(true, vec![]);
        let bytes = b"blob bytes";
        let hex = content_hash(bytes);
        let (bind, worker) = serve_once("200 OK", bytes, bytes.len());
        let (rc, value) = invoke(&fs, "fs.blob.materialise", json!({"blob": {"blob": format!("b3:{hex}")}, "path": "home/sub/out"}), &bind);
        assert_eq!(rc, 0, "{value}");
        assert_eq!(value, json!({"ok": true, "path": "home/sub/out", "blob": format!("b3:{hex}"), "size": bytes.len()}));
        let landed = fs::read(dir.0.join("sub/out")).unwrap();
        assert_eq!(content_hash(&landed), hex);
        assert_eq!(fs::read_dir(dir.0.join("sub")).unwrap().count(), 1);
        dir.no_temp();
        assert!(worker.join().unwrap().0.starts_with(&format!("GET /blob/{hex} HTTP/1.1")));
    }

    #[test]
    fn materialise_not_present_wrong_hash_and_short_body_leave_no_residue() {
        let dir = Scratch::new();
        let fs = dir.layer(true, vec![]);
        let hex = content_hash(b"right");
        for (status, bytes, len, token) in [
            ("404 Not Found", &b"{}"[..], 2, "not_present:"),
            ("200 OK", &b"wrong"[..], 5, "verify_failed:"),
            ("200 OK", &b"rig"[..], 5, "lane:"),
        ] {
            let (bind, worker) = serve_once(status, bytes, len);
            assert_error(invoke(&fs, "fs.blob.materialise", json!({"blob": hex, "path": "home/out"}), &bind), token);
            worker.join().unwrap();
            assert!(!dir.0.join("out").exists());
            dir.no_temp();
        }
    }

    #[test]
    fn materialise_refusals_happen_before_http() {
        let dir = Scratch::new();
        fs::write(dir.0.join("exists"), b"keep").unwrap();
        let rw = dir.layer(true, vec![]);
        let ro = dir.layer(false, vec![]);
        let scoped = dir.layer(true, vec!["allowed/*".into()]);
        let hex = content_hash(b"x");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        for (fs, path, token) in [(&rw, "home/exists", "exists:"), (&ro, "home/out", "denied:"), (&rw, "home", "denied:"), (&scoped, "home/out", "denied:"), (&scoped, "home/allowed", "denied:")] {
            assert_error(invoke(fs, "fs.blob.materialise", json!({"blob": hex, "path": path}), &bind), token);
        }
        for blob in [Value::Null, json!(42), json!({}), json!("bad"), json!("A".repeat(64))] {
            assert_error(invoke(&rw, "fs.blob.materialise", json!({"blob": blob, "path": "home/out"}), &bind), "invalid blob id");
        }
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(dir.0.join("exists")).unwrap(), b"keep");
        dir.no_temp();
    }

    #[test]
    fn malformed_references_are_rejected() {
        for body in ["null".into(), "{}".into(), "not json".into(),
            json!({"blob": "0".repeat(64), "size": 0}).to_string(),
            json!({"blob": format!("b3:{}", "A".repeat(64)), "size": 0}).to_string(),
            json!({"blob": format!("b3:{}", "0".repeat(64))}).to_string(),
        ] {
            assert!(parse_reference(&body).unwrap_err().starts_with("lane:"));
        }
    }
}
