//! Bounded streaming HTTP transfers. No dependency on the daemon layer.
use super::*;
use indexmap::IndexMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

const BUFFER: usize = 64 * 1024;
const MAX_EXACT: u64 = 9_007_199_254_740_991;

fn invalid(message: impl Into<String>) -> MixError {
    MixError::structured("OPTION_INVALID", message)
}

fn text<'a>(value: &'a Value, name: &str) -> MixResult<&'a str> {
    match value {
        Value::String(s) => Ok(s),
        _ => Err(invalid(format!("{name} must be a string"))),
    }
}

fn integer(value: &Value, name: &str) -> MixResult<u64> {
    match value {
        Value::Number(n)
            if n.is_finite() && *n >= 0.0 && *n <= MAX_EXACT as f64 && n.fract() == 0.0 =>
        {
            Ok(*n as u64)
        }
        _ => Err(invalid(format!(
            "{name} must be an exact integer in 0..={MAX_EXACT}"
        ))),
    }
}

fn seconds(value: Option<&Value>, default: f64, name: &str) -> MixResult<Duration> {
    let n = match value {
        None => default,
        Some(Value::Number(n)) => *n,
        _ => return Err(invalid(format!("{name} must be seconds"))),
    };
    if !n.is_finite() || !(0.0..=31_536_000.0).contains(&n) {
        return Err(invalid(format!(
            "{name} must be finite seconds in 0..=31536000"
        )));
    }
    Duration::try_from_secs_f64(n).map_err(|e| invalid(e.to_string()))
}

struct Options {
    map: IndexMap<String, Value>,
    headers: Vec<(String, String)>,
    tls: HttpOpts,
    idle: Duration,
    deadline: Option<Instant>,
}

impl Options {
    fn parse(
        name: &str,
        arg: Option<&Value>,
        allowed: &[&str],
        started: Instant,
    ) -> MixResult<Self> {
        let map = match arg {
            None => IndexMap::new(),
            Some(Value::Map(m)) => (**m).clone(),
            _ => return Err(invalid("file transfer opts must be a map")),
        };
        let common = [
            "headers",
            "idle_timeout",
            "deadline",
            "ssl_verify",
            "ca_file",
            "ca_pem",
        ];
        for key in map.keys() {
            if !common.contains(&key.as_str()) && !allowed.contains(&key.as_str()) {
                return Err(invalid(format!("{name}: unknown option {key}")));
            }
        }
        if map
            .get("headers")
            .is_some_and(|v| !matches!(v, Value::Map(_)))
        {
            return Err(invalid("headers must be a map"));
        }
        let headers = http_headers_from(map.get("headers"))?;
        for (key, value) in &headers {
            if !is_http_token(key) || value.contains(['\r', '\n', '\0']) {
                return Err(invalid("invalid HTTP header"));
            }
            if ["content-length", "transfer-encoding"].contains(&key.to_ascii_lowercase().as_str())
            {
                return Err(invalid(
                    "file transfer owns Content-Length and Transfer-Encoding",
                ));
            }
        }
        let tls_map = map
            .iter()
            .filter(|(k, _)| ["ssl_verify", "ca_file", "ca_pem"].contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let tls = parse_http_opts(name, Some(&Value::map(tls_map)))?;
        let idle = seconds(map.get("idle_timeout"), 30.0, "idle_timeout")?;
        if idle.is_zero() {
            return Err(invalid("idle_timeout must be greater than zero"));
        }
        let duration = seconds(map.get("deadline"), 0.0, "deadline")?;
        let deadline = if duration.is_zero() {
            None
        } else {
            Some(
                started
                    .checked_add(duration)
                    .ok_or_else(|| invalid("deadline out of range"))?,
            )
        };
        Ok(Self {
            map,
            headers,
            tls,
            idle,
            deadline,
        })
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let agent = http_agent_builder(self.tls.insecure, self.tls.ca_agent.as_ref())
            .redirects(0)
            .timeout_connect(self.idle)
            .timeout_read(self.idle)
            .timeout_write(self.idle)
            .build();
        let mut request = agent.request(method, url);
        for (key, value) in &self.headers {
            request = request.set(key, value);
        }
        request
    }
}

fn check_deadline(deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "cooperative HTTP deadline expired",
        ))
    } else {
        Ok(())
    }
}

fn response_map(url: &str) -> IndexMap<String, Value> {
    IndexMap::from([
        ("status".into(), Value::Number(0.0)),
        ("headers".into(), Value::map(IndexMap::new())),
        ("bytes_written".into(), Value::Number(0.0)),
        ("size".into(), Value::Number(0.0)),
        ("blake3".into(), Value::Nil),
        ("body".into(), Value::Nil),
        ("bytes".into(), Value::bytes(Vec::new())),
        ("final_url".into(), Value::String(url.into())),
        ("duration_ms".into(), Value::Number(0.0)),
        ("error_code".into(), Value::Nil),
        ("error".into(), Value::Nil),
    ])
}

fn failure(map: &mut IndexMap<String, Value>, code: &str, error: impl std::fmt::Display) {
    map.insert("status".into(), Value::Number(0.0));
    map.insert("error_code".into(), Value::String(code.into()));
    map.insert("error".into(), Value::String(error.to_string()));
}

fn io_code(error: &io::Error) -> &'static str {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        "HTTP_TIMEOUT"
    } else {
        "FILE_IO"
    }
}

fn finish(mut map: IndexMap<String, Value>, started: Instant) -> MixResult<Option<Value>> {
    map.insert(
        "duration_ms".into(),
        Value::Number(started.elapsed().as_secs_f64() * 1000.0),
    );
    Ok(Some(Value::map(map)))
}

/// Unlike Read::take, early EOF is an error, never a clean end of body.
struct WindowReader<R> {
    reader: R,
    remaining: u64,
    consumed: u64,
    hash: blake3::Hasher,
    deadline: Option<Instant>,
    short: bool,
}

impl<R: Read> Read for WindowReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        check_deadline(self.deadline)?;
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let count = self.remaining.min(BUFFER as u64).min(buf.len() as u64) as usize;
        let n = self.reader.read(&mut buf[..count])?;
        if n == 0 {
            self.short = true;
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shortened during upload",
            ));
        }
        self.hash.update(&buf[..n]);
        self.remaining -= n as u64;
        self.consumed += n as u64;
        check_deadline(self.deadline)?;
        Ok(n)
    }
}

fn drain_response(
    resp: ureq::Response,
    map: &mut IndexMap<String, Value>,
    deadline: Option<Instant>,
) {
    let status = resp.status();
    http_response_meta_into_map(&resp, map);
    let mut reader = resp.into_reader();
    let mut bytes = Vec::new();
    let mut buf = [0u8; BUFFER];
    let mut over_cap = false;
    let result = (|| -> io::Result<()> {
        loop {
            check_deadline(deadline)?;
            let n = reader.read(&mut buf)?;
            check_deadline(deadline)?;
            if n == 0 {
                return Ok(());
            }
            if bytes.len() as u64 + n as u64 > MAX_HTTP_BODY_BYTES {
                over_cap = true;
                return Err(io::Error::other("response exceeds 64 MiB cap"));
            }
            bytes.extend_from_slice(&buf[..n]);
        }
    })();
    match result {
        Ok(()) => http_body_into_map(status, bytes, map),
        Err(e) => failure(
            map,
            if over_cap {
                "HTTP_BODY_LIMIT"
            } else if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                "HTTP_TIMEOUT"
            } else {
                "HTTP_BODY"
            },
            e,
        ),
    }
}

pub(super) fn put(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("http_put_file", &args, 2, 3)?;
    let started = Instant::now();
    let url = text(&args[0], "url")?;
    let path = text(&args[1], "path")?;
    let opts = Options::parse("http_put_file", args.get(2), &["method", "range"], started)?;
    let method = opts
        .map
        .get("method")
        .map(|v| text(v, "method"))
        .transpose()?
        .unwrap_or("PUT");
    if !["PUT", "POST", "PATCH"].contains(&method) {
        return Err(invalid("method must be PUT, POST or PATCH"));
    }
    let mut map = response_map(url);
    let result = (|| -> io::Result<(File, u64)> {
        check_deadline(opts.deadline)?;
        let file = File::open(path)?;
        let md = file.metadata()?;
        if !md.is_file() || md.len() > MAX_EXACT {
            return Err(io::Error::other(
                "source must be a regular file with exactly representable size",
            ));
        }
        Ok((file, md.len()))
    })();
    let (mut file, size) = match result {
        Ok(v) => v,
        Err(e) => {
            failure(&mut map, io_code(&e), e);
            return finish(map, started);
        }
    };
    let (start, length) = match opts.map.get("range") {
        None => (0, size),
        Some(Value::Map(range)) => {
            if range.len() != 2 {
                return Err(invalid("range requires only start and inclusive end"));
            }
            let start = integer(
                range
                    .get("start")
                    .ok_or_else(|| invalid("range.start required"))?,
                "range.start",
            )?;
            let end = integer(
                range
                    .get("end")
                    .ok_or_else(|| invalid("range.end required"))?,
                "range.end",
            )?;
            if start > end || end >= size {
                return Err(invalid("range is outside file"));
            }
            (start, end - start + 1)
        }
        _ => return Err(invalid("range must be {start,end}")),
    };
    map.insert("size".into(), Value::Number(length as f64));
    if let Err(e) = file.seek(SeekFrom::Start(start)) {
        failure(&mut map, "FILE_IO", e);
        return finish(map, started);
    }
    let mut reader = WindowReader {
        reader: file,
        remaining: length,
        consumed: 0,
        hash: blake3::Hasher::new(),
        deadline: opts.deadline,
        short: false,
    };
    let response = opts
        .request(method, url)
        .set("Content-Length", &length.to_string())
        .send(&mut reader);
    map.insert(
        "bytes_written".into(),
        Value::Number(reader.consumed as f64),
    );
    // Preserve transport classification unless the source itself ended early.
    if reader.short {
        failure(&mut map, "HTTP_SHORT_READ", "file shortened during upload");
        return finish(map, started);
    }
    match response {
        Ok(resp) | Err(ureq::Error::Status(_, resp)) => {
            if reader.remaining != 0 {
                failure(
                    &mut map,
                    "HTTP_SHORT_READ",
                    "request did not consume its declared window",
                );
            } else {
                drain_response(resp, &mut map, opts.deadline);
            }
        }
        Err(e) => failure(&mut map, http_transport_error_code(&e), e),
    }
    if matches!(map.get("error"), Some(Value::Nil)) {
        map.insert(
            "blake3".into(),
            Value::String(reader.hash.finalize().to_hex().to_string()),
        );
    }
    finish(map, started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mix-http-file-{}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn map(value: Option<Value>) -> Rc<IndexMap<String, Value>> {
        let Some(Value::Map(map)) = value else {
            panic!("expected map")
        };
        map
    }

    fn server(
        response: &'static str,
        pause: Duration,
    ) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
                assert!(head.len() < 65536);
            }
            let head = String::from_utf8(head).unwrap();
            let len = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; len];
            socket.read_exact(&mut body).unwrap();
            std::thread::sleep(pause);
            let _ = socket.write_all(response.as_bytes());
            (head, body)
        });
        (url, handle)
    }

    #[test]
    fn upload_exact_inclusive_window_and_redirect_is_returned() {
        let dir = Temp::new();
        let path = dir.0.join("source");
        std::fs::write(&path, b"0123456789").unwrap();
        let (url, server) = server(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /never\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            Duration::ZERO,
        );
        let opts = Value::map(IndexMap::from([
            ("method".into(), Value::String("PATCH".into())),
            (
                "range".into(),
                Value::map(IndexMap::from([
                    ("start".into(), Value::Number(2.0)),
                    ("end".into(), Value::Number(5.0)),
                ])),
            ),
        ]));
        let result = map(put(vec![
            Value::String(url),
            Value::String(path.to_string_lossy().into()),
            opts,
        ])
        .unwrap());
        assert!(matches!(result["status"], Value::Number(307.0)));
        assert!(matches!(result["bytes_written"], Value::Number(4.0)));
        let (head, body) = server.join().unwrap();
        assert!(head.starts_with("PATCH /file "));
        assert!(head.to_ascii_lowercase().contains("content-length: 4\r\n"));
        assert_eq!(body, b"2345");
    }

    #[test]
    fn upload_short_read_and_cooperative_deadline_refuse() {
        let mut reader = WindowReader {
            reader: io::Cursor::new(b"ab"),
            remaining: 3,
            consumed: 0,
            hash: blake3::Hasher::new(),
            deadline: None,
            short: false,
        };
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 2);
        assert_eq!(
            reader.read(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(reader.short);
        assert!(check_deadline(Some(Instant::now())).is_err());
        for number in [-1.0, 0.5, f64::NAN, f64::INFINITY, 9_007_199_254_740_992.0] {
            assert!(integer(&Value::Number(number), "window").is_err());
        }
    }

    #[test]
    fn upload_socket_idle_timeout() {
        let dir = Temp::new();
        let path = dir.0.join("source");
        std::fs::write(&path, b"a").unwrap();
        let (url, server) = server(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            Duration::from_millis(300),
        );
        let opts = Value::map(IndexMap::from([(
            "idle_timeout".into(),
            Value::Number(0.03),
        )]));
        let result = map(put(vec![
            Value::String(url),
            Value::String(path.to_string_lossy().into()),
            opts,
        ])
        .unwrap());
        assert!(matches!(result["status"], Value::Number(0.0)));
        assert!(matches!(&result["error_code"],Value::String(s) if s == "HTTP_TIMEOUT"));
        server.join().unwrap();
    }

    #[test]
    fn upload_contract_requires_file_read_without_options() {
        let info = builtin_info_of("http_put_file").unwrap();
        assert_eq!(info.capability, CapabilityClass::Network);
        assert_eq!(info.contract.required_caps, &[CapabilityClass::FsRead]);
        assert!(info.contract.accepts_arity(2));
    }
}
