//! `http_post_multipart` — the generic multipart/form-data POST builtin
//! (v0.103.12). Served by a one-shot local listener so the body is
//! inspected end-to-end without any network beyond loopback.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;

#[test]
fn multipart_body_reaches_the_server_well_formed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let cap = captured.clone();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        // Read until the request is complete: a simple content-length parse
        // is enough for our own request (no keep-alive, no chunking).
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..head_end]);
                if let Some(len_line) = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                {
                    let len: usize = len_line.split(':').nth(1).unwrap().trim().parse().unwrap();
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
        }
        *cap.lock().unwrap() = buf;
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        )
        .expect("write response");
    });

    let src = format!(
        "print(http_post_multipart(\"http://{addr}/up\", {{f: \"v\"}}, {{u: {{filename: \"a.txt\", content_type: \"text/plain\", data: string_to_bytes(\"hi\")}}}}).status)\n"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", &src])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix");
    server.join().expect("server thread");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "200");

    let req = String::from_utf8_lossy(&captured.lock().unwrap());
    assert!(
        req.starts_with("POST /up HTTP/1.1"),
        "request line: {req}"
    );
    assert!(
        req.contains("content-type: multipart/form-data; boundary="),
        "content-type header: {req}"
    );
    assert!(
        req.contains("Content-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n"),
        "field part: {req}"
    );
    assert!(
        req.contains(
            "Content-Disposition: form-data; name=\"u\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhi\r\n"
        ),
        "file part: {req}"
    );
}
