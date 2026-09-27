//! Bounded streaming HTTP transfers. No dependency on the daemon layer.
use super::*;
use indexmap::IndexMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
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
            if n.is_finite() && (0.0..=MAX_EXACT as f64).contains(n) && n.fract() == 0.0 =>
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
        ("published".into(), Value::Bool(false)),
    ])
}

fn failure(map: &mut IndexMap<String, Value>, code: &str, error: impl std::fmt::Display) {
    map.insert("status".into(), Value::Number(0.0));
    map.insert("error_code".into(), Value::String(code.into()));
    map.insert("error".into(), Value::String(error.to_string()));
}

fn io_code(error: &io::Error) -> &'static str {
    if source_is_timeout(error) {
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
            } else if source_is_timeout(&e) {
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

fn flag(map: &IndexMap<String, Value>, key: &str) -> MixResult<bool> {
    match map.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(invalid(format!("{key} must be a bool"))),
    }
}

fn decimal(s: &str) -> io::Result<u64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(io::Error::other("invalid decimal size"));
    }
    s.parse::<u64>()
        .ok()
        .filter(|n| *n <= MAX_EXACT)
        .ok_or_else(|| io::Error::other("size is not exactly representable"))
}

/// Require one contiguous suffix through the last byte of the representation.
fn suffix_range(value: &str, prefix: u64) -> io::Result<u64> {
    let (window, total) = value
        .strip_prefix("bytes ")
        .and_then(|s| s.split_once('/'))
        .ok_or_else(|| io::Error::other("invalid Content-Range"))?;
    let (start, end) = window
        .split_once('-')
        .ok_or_else(|| io::Error::other("invalid range"))?;
    let (start, end, total) = (decimal(start)?, decimal(end)?, decimal(total)?);
    if start != prefix || start > end || end.checked_add(1) != Some(total) {
        return Err(io::Error::other(
            "Content-Range must cover the complete suffix at the prefix size",
        ));
    }
    Ok(total)
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn staging(path: &Path, private: bool) -> io::Result<(File, Staging)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("destination has no file name"))?;
    let parent = parent(path);
    for _ in 0..16 {
        let tmp = parent.join(format!(
            ".{}.http-{:032x}",
            name.to_string_lossy(),
            rand::random::<u128>()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if private { 0o600 } else { 0o666 });
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((file, Staging(tmp))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not allocate unique staging file"))
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Atomic refusal when the destination exists, including dangling symlinks.
/// On unsupported kernels/filesystems use atomic hard-link creation; never
/// fall back to a check followed by an overwriting rename.
fn publish_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let a = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let b = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: both NUL-terminated paths outlive this syscall.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(error);
        }
    }
    std::fs::hard_link(from, to)
}

pub(super) fn get(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("http_get_file", &args, 2, 3)?;
    let started = Instant::now();
    let url = text(&args[0], "url")?;
    let path = Path::new(text(&args[1], "path")?);
    let opts = Options::parse(
        "http_get_file",
        args.get(2),
        &["append", "overwrite", "expect_blake3", "max_bytes"],
        started,
    )?;
    let append = flag(&opts.map, "append")?;
    let overwrite = flag(&opts.map, "overwrite")?;
    if append && overwrite {
        return Err(invalid("append and overwrite are mutually exclusive"));
    }
    let expected = opts
        .map
        .get("expect_blake3")
        .map(|v| text(v, "expect_blake3"))
        .transpose()?;
    if expected.is_some_and(|s| {
        s.len() != 64
            || !s
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }) {
        return Err(invalid("expect_blake3 must be 64 lowercase hex digits"));
    }
    if append && expected.is_none() {
        return Err(invalid("append requires expect_blake3 for the whole file"));
    }
    let maximum = opts
        .map
        .get("max_bytes")
        .map(|v| integer(v, "max_bytes"))
        .transpose()?
        .unwrap_or(MAX_EXACT);
    if opts
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Range") || k.eq_ignore_ascii_case("Accept-Encoding"))
    {
        return Err(invalid("download owns Range and Accept-Encoding"));
    }
    let mut map = response_map(url);
    let mut count = 0u64;
    let mut received = 0u64;
    let mut code = "FILE_IO";
    let result = (|| -> io::Result<()> {
        check_deadline(opts.deadline)?;
        // A replacement must not expose a private target's prefix or incoming
        // bytes before verification. Apply its final mode only at publication.
        let private = match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let (mut file, tmp) = staging(path, private)?;
        let mut hash = blake3::Hasher::new();
        let mut buf = [0u8; BUFFER];
        if append {
            let md = std::fs::symlink_metadata(path)?;
            if !md.is_file() {
                return Err(io::Error::other("append prefix must be a regular file"));
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut source = options.open(path)?;
            if !source.metadata()?.is_file() {
                return Err(io::Error::other("append prefix must be regular"));
            }
            loop {
                check_deadline(opts.deadline)?;
                let n = source.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if n as u64 > maximum.saturating_sub(count) {
                    code = "HTTP_BODY_LIMIT";
                    return Err(io::Error::other("prefix exceeds max_bytes"));
                }
                file.write_all(&buf[..n])?;
                hash.update(&buf[..n]);
                count += n as u64;
            }
        }
        let prefix = count;
        let mut req = opts.request("GET", url).set("Accept-Encoding", "identity");
        if append {
            req = req.set("Range", &format!("bytes={prefix}-"));
        }
        check_deadline(opts.deadline)?;
        let response = match req.call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => {
                code = http_transport_error_code(&e);
                return Err(io::Error::other(e));
            }
        };
        let status = response.status();
        http_response_meta_into_map(&response, &mut map);
        if !(200..300).contains(&status) {
            // Error and redirect bodies may be inspected but never published.
            drain_response(response, &mut map, opts.deadline);
            return Ok(());
        }
        // With gzip enabled (including via feature unification), ureq decodes
        // and removes BOTH encoding and length headers. Identity-framed file
        // transfers must therefore require the length, not just inspect CE.
        code = "HTTP_IDENTITY_FRAMING";
        if matches!(status, 200 | 206) && response.header("Content-Length").is_none() {
            return Err(io::Error::other(
                "identity framing requires Content-Length; encoded or unframed response refused",
            ));
        }
        if response
            .header("Content-Encoding")
            .is_some_and(|s| !s.eq_ignore_ascii_case("identity"))
        {
            return Err(io::Error::other(
                "identity framing required; Content-Encoding is not identity",
            ));
        }
        code = "HTTP_RANGE";
        let total = if append {
            if status != 206 || response.all("Content-Range").len() != 1 {
                return Err(io::Error::other(
                    "append requires 206 with one Content-Range",
                ));
            }
            Some(suffix_range(
                response.header("Content-Range").unwrap_or(""),
                prefix,
            )?)
        } else {
            if status != 200 && status != 204 {
                return Err(io::Error::other("download requires 200 or 204"));
            }
            if response.header("Content-Range").is_some() {
                return Err(io::Error::other("unexpected Content-Range"));
            }
            None
        };
        let lengths = response.all("Content-Length");
        if lengths.len() > 1 {
            return Err(io::Error::other("ambiguous Content-Length"));
        }
        let length = lengths.first().map(|s| decimal(s)).transpose()?;
        if let (Some(total), Some(length)) = (total, length)
            && total - prefix != length
        {
            return Err(io::Error::other("Content-Length differs from range"));
        }
        let final_size = total.or(length);
        if final_size.is_some_and(|n| n > maximum) {
            code = "HTTP_BODY_LIMIT";
            return Err(io::Error::other("final size exceeds max_bytes"));
        }
        let mut reader = response.into_reader();
        loop {
            code = "HTTP_BODY";
            check_deadline(opts.deadline)?;
            let n = reader.read(&mut buf)?;
            check_deadline(opts.deadline)?;
            if n == 0 {
                break;
            }
            if n as u64 > maximum.saturating_sub(count) {
                code = "HTTP_BODY_LIMIT";
                return Err(io::Error::other("final size exceeds max_bytes"));
            }
            code = "FILE_IO";
            file.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            count += n as u64;
            received += n as u64;
        }
        code = "HTTP_SHORT_READ";
        if final_size.is_some_and(|n| count != n) {
            return Err(io::Error::other("body length differs from declared size"));
        }
        let actual = hash.finalize().to_hex().to_string();
        code = "HTTP_HASH_MISMATCH";
        if expected.is_some_and(|want| want != actual) {
            return Err(io::Error::other("BLAKE3 mismatch"));
        }
        code = "FILE_IO";
        if append || overwrite {
            match std::fs::symlink_metadata(path) {
                Ok(md) if md.is_file() => file.set_permissions(md.permissions())?,
                Ok(_) => return Err(io::Error::other("replacement target is not a regular file")),
                Err(e) if e.kind() == io::ErrorKind::NotFound && !append => {}
                Err(e) => return Err(e),
            }
        }
        file.sync_all()?;
        check_deadline(opts.deadline)?;
        if append || overwrite {
            std::fs::rename(&tmp.0, path)?;
        } else {
            publish_noreplace(&tmp.0, path)?;
        }
        map.insert("published".into(), Value::Bool(true));
        drop(tmp);
        File::open(parent(path))?.sync_all()?;
        map.insert("status".into(), Value::Number(status as f64));
        map.insert("blake3".into(), Value::String(actual));
        Ok(())
    })();
    map.insert("bytes_written".into(), Value::Number(received as f64));
    map.insert("size".into(), Value::Number(count as f64));
    if let Err(e) = result {
        let code = if source_is_timeout(&e) {
            "HTTP_TIMEOUT"
        } else if e.kind() == io::ErrorKind::AlreadyExists {
            "FILE_EXISTS"
        } else {
            code
        };
        failure(&mut map, code, e);
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
        // `Value` implements Drop, so the payload cannot be moved out of the
        // pattern; borrow it and clone the Rc.
        let Some(Value::Map(ref map)) = value else {
            panic!("expected map")
        };
        map.clone()
    }

    fn server(
        response: impl AsRef<[u8]>,
        pause: Duration,
    ) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let response = response.as_ref().to_vec();
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
            let _ = socket.write_all(&response);
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
    fn upload_socket_read_idle_timeout() {
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

    #[cfg(unix)]
    #[test]
    fn upload_socket_idle_timeout() {
        use std::os::fd::AsRawFd;
        let dir = Temp::new();
        let path = dir.0.join("source");
        let length = 64 * 1024 * 1024;
        File::create(&path).unwrap().set_len(length).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            // Keep the receive window small enough that a sparse 64 MiB file
            // cannot fit in kernel buffers after the server stops reading.
            let size: libc::c_int = 4096;
            // SAFETY: size is valid for the supplied pointer and socklen.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 65536);
            }
            socket.read_exact(&mut [0]).unwrap();
            // No response and no more body reads. The bounded fallback makes
            // removal of timeout_write fail the elapsed assertion, not hang.
            let _ = stalled.recv_timeout(Duration::from_secs(5));
        });
        let opts = Value::map(IndexMap::from([(
            "idle_timeout".into(),
            Value::Number(0.03),
        )]));
        let started = Instant::now();
        let result = map(put(vec![
            Value::String(url),
            Value::String(path.to_string_lossy().into()),
            opts,
        ])
        .unwrap());
        let elapsed = started.elapsed();
        let _ = release.send(());
        server.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(2),
            "write did not time out: {elapsed:?}"
        );
        assert!(matches!(result["status"], Value::Number(0.0)));
        assert!(
            matches!(&result["error_code"], Value::String(s) if s == "HTTP_TIMEOUT"),
            "unexpected error_code: {:?}; error: {:?}",
            result["error_code"],
            result["error"]
        );
        assert!(
            matches!(result["bytes_written"], Value::Number(n) if n > 0.0 && n < length as f64)
        );
    }

    #[test]
    fn wrapped_transport_io_errors_keep_timeout_and_closed_classification() {
        for (kind, expected) in [
            (io::ErrorKind::TimedOut, "HTTP_TIMEOUT"),
            (io::ErrorKind::WouldBlock, "HTTP_TIMEOUT"),
            (io::ErrorKind::BrokenPipe, "HTTP_TRANSPORT"),
            (io::ErrorKind::ConnectionReset, "HTTP_TRANSPORT"),
            (io::ErrorKind::InvalidInput, "HTTP_TRANSPORT"),
        ] {
            // The same classifier handles PUT send errors and GET call errors.
            let error = ureq::Error::from(io::Error::other(io::Error::from(kind)));
            assert_eq!(
                http_transport_error_code(&error),
                expected,
                "{kind:?}: {error}"
            );
        }
    }

    #[test]
    fn upload_contract_requires_file_read_without_options() {
        let info = builtin_info_of("http_put_file").unwrap();
        assert_eq!(info.capability, CapabilityClass::Network);
        assert_eq!(info.contract.required_caps, &[CapabilityClass::FsRead]);
        assert!(info.contract.accepts_arity(2));
    }

    fn download(
        url: String,
        path: &Path,
        opts: IndexMap<String, Value>,
    ) -> Rc<IndexMap<String, Value>> {
        map(get(vec![
            Value::String(url),
            Value::String(path.to_string_lossy().into()),
            Value::map(opts),
        ])
        .unwrap())
    }

    fn hash_option(bytes: &[u8]) -> IndexMap<String, Value> {
        IndexMap::from([(
            "expect_blake3".into(),
            Value::String(blake3::hash(bytes).to_hex().to_string()),
        )])
    }

    #[test]
    #[cfg(unix)]
    fn replacement_staging_stays_private_while_prefix_and_body_are_in_flight() {
        use std::os::unix::fs::PermissionsExt;
        for append in [false, true] {
            let dir = Temp::new();
            let path = dir.0.join("target");
            std::fs::write(&path, b"secret").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/file", listener.local_addr().unwrap());
            let parent = dir.0.clone();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let temp = std::fs::read_dir(&parent)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".target.http-")
                    })
                    .unwrap();
                assert_eq!(
                    std::fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                if append {
                    assert_eq!(std::fs::read(&temp).unwrap(), b"secret");
                }
                let status = if append { 206 } else { 200 };
                let range = if append {
                    "Content-Range: bytes 6-7/8\r\n"
                } else {
                    ""
                };
                socket.write_all(format!("HTTP/1.1 {status} OK\r\nContent-Length: 2\r\n{range}Connection: close\r\n\r\na").as_bytes()).unwrap();
                let until = Instant::now() + Duration::from_secs(5);
                let expected_len = if append { 7 } else { 1 };
                while std::fs::metadata(&temp).unwrap().len() < expected_len {
                    assert!(Instant::now() < until, "body byte was not staged");
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(
                    std::fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                socket.write_all(b"b").unwrap();
            });
            let expected: &[u8] = if append { b"secretab" } else { b"ab" };
            let mut opts = hash_option(expected);
            opts.insert(
                if append { "append" } else { "overwrite" }.into(),
                Value::Bool(true),
            );
            let result = download(url, &path, opts);
            server.join().unwrap();
            assert!(
                matches!(result["published"], Value::Bool(true)),
                "{result:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn download_new_file_uses_creation_umask_and_overwrite_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Temp::new();
        let baseline = dir.0.join("normal-create");
        File::create(&baseline).unwrap();
        let expected = std::fs::metadata(&baseline).unwrap().permissions().mode() & 0o777;
        let path = dir.0.join("download");
        let (url, server) = server(
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na",
            Duration::ZERO,
        );
        let result = download(url, &path, IndexMap::new());
        server.join().unwrap();
        assert!(matches!(result["published"], Value::Bool(true)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            expected
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let (url, worker) = self::server(
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb",
            Duration::ZERO,
        );
        let result = download(
            url,
            &path,
            IndexMap::from([("overwrite".into(), Value::Bool(true))]),
        );
        worker.join().unwrap();
        assert!(matches!(result["published"], Value::Bool(true)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn download_verified_and_atomic_no_replace() {
        let dir = Temp::new();
        let path = dir.0.join("target");
        for exists in [false, true] {
            let (url, srv) = server(
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                Duration::ZERO,
            );
            let result = download(url, &path, hash_option(b"hello"));
            srv.join().unwrap();
            if exists {
                assert!(matches!(&result["error_code"],Value::String(s) if s == "FILE_EXISTS"));
                assert!(matches!(result["published"], Value::Bool(false)));
            } else {
                assert!(matches!(result["status"], Value::Number(200.0)));
                assert!(matches!(result["published"], Value::Bool(true)));
                assert_eq!(std::fs::read(&path).unwrap(), b"hello");
                // A later no-replace request must preserve different bytes.
                std::fs::write(&path, b"existing").unwrap();
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"existing");
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn download_hash_length_limit_and_error_never_replace_destination() {
        let dir = Temp::new();
        let path = dir.0.join("target");
        std::fs::write(&path, b"old").unwrap();
        for (response, max) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nwrong",
                100.0,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nhello",
                100.0,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                4.0,
            ),
            (
                "HTTP/1.1 404 Not Found\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                100.0,
            ),
        ] {
            let (url, srv) = server(response, Duration::ZERO);
            let mut opts = hash_option(b"hello");
            opts.insert("overwrite".into(), Value::Bool(true));
            opts.insert("max_bytes".into(), Value::Number(max));
            let result = download(url, &path, opts);
            srv.join().unwrap();
            assert!(matches!(result["published"], Value::Bool(false)));
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn download_append_hashes_prefix_and_validates_206_and_final_limit() {
        let dir = Temp::new();
        let path = dir.0.join("target");
        for (response, max, ok) in [
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\nContent-Length: 3\r\nConnection: close\r\n\r\nllo",
                5.0,
                true,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nllo",
                5.0,
                false,
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/4\r\nContent-Length: 3\r\nConnection: close\r\n\r\nllo",
                5.0,
                false,
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\nContent-Length: 3\r\nConnection: close\r\n\r\nllo",
                4.0,
                false,
            ),
        ] {
            std::fs::write(&path, b"he").unwrap();
            let (url, srv) = server(response, Duration::ZERO);
            let mut opts = hash_option(b"hello");
            opts.insert("append".into(), Value::Bool(true));
            opts.insert("max_bytes".into(), Value::Number(max));
            let result = download(url, &path, opts);
            let (head, _) = srv.join().unwrap();
            assert!(head.to_ascii_lowercase().contains("range: bytes=2-\r\n"));
            assert!(matches!(result["published"],Value::Bool(b) if b == ok));
            assert_eq!(
                std::fs::read(&path).unwrap(),
                if ok { &b"hello"[..] } else { &b"he"[..] }
            );
            if ok {
                assert!(matches!(result["bytes_written"], Value::Number(3.0)));
                assert!(matches!(result["size"], Value::Number(5.0)));
            }
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        }
        assert!(
            get(vec![
                Value::String("http://unused".into()),
                Value::String(path.to_string_lossy().into()),
                Value::map(IndexMap::from([("append".into(), Value::Bool(true))]))
            ])
            .is_err()
        );
        for bad in [
            "bytes 2-3/5",
            "bytes 2-4/*",
            "bytes -2-4/5",
            "bytes 2-9007199254740992/9007199254740993",
        ] {
            assert!(suffix_range(bad, 2).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn download_no_replace_refuses_dangling_symlink() {
        let dir = Temp::new();
        let from = dir.0.join("stage");
        let to = dir.0.join("target");
        std::fs::write(&from, b"new").unwrap();
        std::os::unix::fs::symlink("missing", &to).unwrap();
        assert_eq!(
            publish_noreplace(&from, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(
            std::fs::symlink_metadata(to)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn download_gzip_200_and_206_refuse_without_publishing() {
        // Valid gzip member for "llo": stored DEFLATE block, CRC32 and ISIZE.
        let gzip = [
            31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 1, 3, 0, 252, 255, 108, 108, 111, 52, 179, 201, 170,
            3, 0, 0, 0,
        ];
        let dir = Temp::new();
        let path = dir.0.join("target");
        for append in [false, true] {
            if append {
                std::fs::write(&path, b"he").unwrap();
            }
            let (status, range) = if append {
                (206, "Content-Range: bytes 2-4/5\r\n")
            } else {
                (200, "")
            };
            let mut wire = format!("HTTP/1.1 {status} Test\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n{range}Connection: close\r\n\r\n",gzip.len()).into_bytes();
            wire.extend_from_slice(&gzip);
            let (url, srv) = server(wire, Duration::ZERO);
            let mut opts = hash_option(if append { b"hello" } else { b"llo" });
            opts.insert("append".into(), Value::Bool(append));
            let result = download(url, &path, opts);
            let (head, _) = srv.join().unwrap();
            assert!(
                head.to_ascii_lowercase()
                    .contains("accept-encoding: identity\r\n")
            );
            assert!(
                matches!(&result["error_code"],Value::String(s) if s == "HTTP_IDENTITY_FRAMING")
            );
            assert!(matches!(result["published"], Value::Bool(false)));
            assert!(matches!(&result["error"],Value::String(s) if s.contains("identity framing")));
            if append {
                assert_eq!(std::fs::read(&path).unwrap(), b"he");
            } else {
                assert!(!path.exists());
            }
            assert_eq!(
                std::fs::read_dir(&dir.0).unwrap().count(),
                usize::from(append)
            );
        }
    }

    #[test]
    fn download_missing_identity_length_refuses_before_publication() {
        let dir = Temp::new();
        let path = dir.0.join("target");
        let (url, srv) = server(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nbody",
            Duration::ZERO,
        );
        let result = download(url, &path, IndexMap::new());
        srv.join().unwrap();
        assert!(matches!(&result["error_code"],Value::String(s) if s == "HTTP_IDENTITY_FRAMING"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
