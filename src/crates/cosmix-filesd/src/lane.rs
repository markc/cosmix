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

pub async fn quota(client: &NodedClient, service: &str, owner: &str) -> Result<Value, String> {
    let reply = tokio::time::timeout(PROPS_TIMEOUT,
        client.call_typed(service, "blob.quota", json!({"owner": owner})))
        .await.map_err(|_| format!("lane_unavailable: blob.quota on {service} timed out"))?
        .map_err(|e| format!("lane_unavailable: blob.quota on {service}: {e}"))?;
    match reply {
        PortReply::Ok { value, .. } => Ok(value),
        PortReply::AppError { message, .. } => Err(format!("lane_unavailable: blob.quota: {message}")),
    }
}

pub fn source_length(fs: &FsLayer, args: &Value) -> Result<u64, String> {
    let (file, _, _) = fs.open_blob(super::req(args, "path")?).map_err(super::estr)?;
    Ok(file.metadata().map_err(|e| super::estr(e.into()))?.len())
}

/// blobd citizen::verb_quota returns owners[owner] and total, each with numeric
/// limit, used and reserved. Reservations consume headroom just like pinned data.
pub fn check_quota(value: &Value, owner: &str, length: u64) -> Result<Option<String>, String> {
    let remaining = |row: &Value| -> Result<u64, String> {
        let field = |name| row.get(name).and_then(Value::as_u64)
            .ok_or_else(|| format!("lane_unavailable: invalid blob.quota {name}"));
        Ok(field("limit")?.saturating_sub(field("used")?).saturating_sub(field("reserved")?))
    };
    let room = remaining(&value["owners"][owner])?.min(remaining(&value["total"])?);
    if length > room {
        return Ok(Some(format!("quota: {length} B exceeds remaining {room} B for {owner}")));
    }
    Ok(None)
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

/// Printable ASCII except '%' stays unchanged. blobd stores this verbatim.
fn encoded_name(name: &str) -> Option<String> {
    let mut encoded = String::new();
    for byte in name.bytes() {
        if (0x21..=0x7e).contains(&byte) && byte != b'%' {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        if encoded.len() > 128 { return None; }
    }
    Some(encoded)
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
    let (mut file, name, mime) = fs.open_blob(path).map_err(super::estr)?;
    let before = file.metadata().map_err(|e| super::estr(e.into()))?;
    let length = before.len();
    let modified = before.modified().map_err(|e| super::estr(e.into()))?;
    let bind = checked_bind(bind.map_err(str::to_string)?)?;
    let mut request = agent().post(&format!("http://{bind}/blob"))
        .set("Content-Length", &length.to_string())
        .set("X-Cosmix-Owner", owner)
        .set("X-Cosmix-Mime", a["mime"].as_str().unwrap_or(mime));
    if let Some(name) = encoded_name(a["name"].as_str().unwrap_or(&name)) {
        request = request.set("X-Cosmix-Name", &name);
    }
    let mut body = ExactSource { file: &mut file, remaining: length, shrank: false };
    let response = request.send(&mut body);
    if body.shrank {
        return Err("lane: source shrank during upload".into());
    }
    let response = response.map_err(|e| http_error(e, true))?;
    let after = file.metadata().map_err(|e| format!("lane: stat source after upload: {e}"))?;
    if after.len() != length || after.modified().map_err(|e| format!("lane: stat source mtime: {e}"))? != modified {
        return Err("lane: source changed during upload (blob may already be pinned)".into());
    }
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

/// Unlike Take, a premature source EOF is an error, so ureq aborts its socket
/// rather than waiting for the server's idle timeout with a short request body.
struct ExactSource<'a> {
    file: &'a mut std::fs::File,
    remaining: u64,
    shrank: bool,
}

impl Read for ExactSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() { return Ok(0); }
        let want = self.remaining.min(buf.len() as u64) as usize;
        let n = self.file.read(&mut buf[..want])?;
        if n == 0 {
            self.shrank = true;
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "source shrank during upload"));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// `fs.blob.materialise`: GET only from the selected local store, then verify
/// before publishing under the write jail. This never implicitly blob.fetches.
pub fn materialise(fs: &FsLayer, a: &Value, bind: Result<&str, &str>) -> Result<Value, String> {
    let hex = blob_hex(&a["blob"])?;
    let path = super::req(a, "path")?;
    let overwrite = a["overwrite"].as_bool().unwrap_or(false);
    fs.blob_target(path, overwrite, false).map_err(super::estr)?;
    let bind = checked_bind(bind.map_err(str::to_string)?)?;
    let response = agent().get(&format!("http://{bind}/blob/{hex}"))
        .call().map_err(|e| http_error(e, false))?;
    if response.status() != 200 {
        return Err(format!("lane: expected HTTP 200, got {}", response.status()));
    }
    // default-features=false does not disable gzip enabled by workspace feature
    // unification. ureq strips Content-Encoding AND Content-Length when decoding;
    // that case fails closed at the missing-Content-Length check below. Reject
    // any encoding/framing headers that remain visible as well.
    if response.header("Transfer-Encoding").is_some()
        || response.header("Content-Encoding").is_some()
    {
        return Err("lane: encoded blob response is not supported".into());
    }
    let length = response.header("Content-Length").and_then(|s| s.parse::<u64>().ok())
        .ok_or("lane: missing or invalid Content-Length")?;
    let target = fs.blob_target(path, overwrite, true).map_err(super::estr)?;
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
        serve_once_with(status, bytes, length, || {})
    }

    fn serve_once_with(status: &str, bytes: &[u8], length: usize, before_reply: impl FnOnce() + Send + 'static) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
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
            if status.starts_with("413") {
                // Real admission refusal: never read the request body. A large
                // upload can see EPIPE/reset before ureq reads this response.
                write!(reader.get_mut(), "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
                reader.get_mut().write_all(&bytes).unwrap();
                return (head, Vec::new());
            }
            let mut body = vec![0; body_len];
            reader.read_exact(&mut body).unwrap();
            before_reply();
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
    fn source_shrink_errors_at_eof_without_waiting_for_lane() {
        let dir = Scratch::new();
        let path = dir.0.join("file");
        fs::write(&path, b"long source").unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let length = file.metadata().unwrap().len();
        fs::write(&path, b"x").unwrap();
        let mut source = ExactSource { file: &mut file, remaining: length, shrank: false };
        let error = source.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "source shrank during upload");
        assert!(source.shrank);
    }

    #[test]
    fn changed_source_after_upload_is_not_success() {
        let dir = Scratch::new();
        for replacement in [&b"longer"[..], &b"other"[..]] {
            let path = dir.0.join("file");
            fs::write(&path, b"hello").unwrap();
            let replacement = replacement.to_vec();
            let reply = json!({"blob": format!("b3:{}", content_hash(b"hello")), "size": 5}).to_string();
            let (bind, worker) = serve_once_with("201 Created", reply.as_bytes(), reply.len(), move || {
                fs::write(&path, replacement).unwrap();
                fs::File::options().write(true).open(&path).unwrap()
                    .set_modified(std::time::SystemTime::UNIX_EPOCH).unwrap();
            });
            let result = invoke(&dir.layer(true, vec![]), "fs.blob.ref", json!({"path": "home/file"}), &bind);
            assert!(result.1["error"].as_str().unwrap().contains("pinned"));
            assert_error(result, "lane: source changed during upload");
            worker.join().unwrap();
        }
    }

    #[test]
    fn reference_encodes_names_and_omits_oversized_header() {
        let dir = Scratch::new();
        fs::write(dir.0.join("Résumé.pdf"), b"x").unwrap();
        for (name, expected) in [
            (None, Some("R%C3%A9sum%C3%A9.pdf".to_string())),
            (Some("a\nb% c".to_string()), Some("a%0Ab%25%20c".to_string())),
            (Some("a".repeat(200)), None),
        ] {
            let reply = json!({"blob": format!("b3:{}", content_hash(b"x")), "size": 1, "name": expected}).to_string();
            let (bind, worker) = serve_once("201 Created", reply.as_bytes(), reply.len());
            let (rc, value) = invoke(&dir.layer(true, vec![]), "fs.blob.ref",
                json!({"path": "home/Résumé.pdf", "name": name}), &bind);
            assert_eq!(rc, 0, "{value}");
            let head = worker.join().unwrap().0;
            let header = head.lines().filter_map(|s| s.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("x-cosmix-name"))
                .map(|(_, value)| value.trim().to_string());
            assert_eq!(header, expected);
            assert_eq!(value["name"], json!(expected));
        }
    }

    #[test]
    fn reference_quota_down_and_empty_lane_tokens() {
        let dir = Scratch::new();
        let fs = dir.layer(true, vec![]);
        fs::write(dir.0.join("file"), b"x").unwrap();
        let reply = b"{\"error\":\"quota: filesd-test\"}";
        let (bind, worker) = serve_once("413 Payload Too Large", reply, reply.len());
        let result = invoke(&fs, "fs.blob.ref", json!({"path": "home/file"}), &bind);
        assert_eq!(result.0, 10);
        assert!(result.1["error"].as_str().unwrap().contains("quota"), "{}", result.1);
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
    fn early_quota_refusal_of_large_upload_keeps_quota_hint() {
        let dir = Scratch::new();
        fs::write(dir.0.join("large"), vec![0u8; 32 * 1024 * 1024]).unwrap();
        let reply = b"{\"error\":\"quota: filesd-test\"}";
        let (bind, worker) = serve_once("413 Payload Too Large", reply, reply.len());
        let result = invoke(&dir.layer(true, vec![]), "fs.blob.ref", json!({"path": "home/large"}), &bind);
        assert_eq!(result.0, 10);
        assert!(result.1["error"].as_str().unwrap().contains("quota"), "{}", result.1);
        assert!(worker.join().unwrap().1.is_empty());
    }

    #[test]
    fn quota_preflight_counts_total_owner_and_reservations() {
        let mut quota = json!({"owners": {"filesd-test": {"limit": 100, "used": 20, "reserved": 30}},
            "total": {"limit": 200, "used": 10, "reserved": 0}});
        assert_eq!(check_quota(&quota, "filesd-test", 50).unwrap(), None);
        assert_eq!(check_quota(&quota, "filesd-test", 51).unwrap().unwrap(),
            "quota: 51 B exceeds remaining 50 B for filesd-test");
        quota["total"]["reserved"] = json!(180);
        assert_eq!(check_quota(&quota, "filesd-test", 11).unwrap().unwrap(),
            "quota: 11 B exceeds remaining 10 B for filesd-test");
        assert!(check_quota(&json!({}), "filesd-test", 1).unwrap_err().starts_with("lane_unavailable:"));
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
    fn materialise_404_does_not_create_parents() {
        let dir = Scratch::new();
        let (bind, worker) = serve_once("404 Not Found", b"{}", 2);
        assert_error(invoke(&dir.layer(true, vec![]), "fs.blob.materialise",
            json!({"blob": content_hash(b"absent"), "path": "home/a/b/c/out"}), &bind), "not_present:");
        worker.join().unwrap();
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
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
