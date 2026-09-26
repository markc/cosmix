//! Exactly one diagnostic input, bounded before decoding or lane use. The
//! legacy message_b64 carrier is the explicit compatibility exception to
//! reference-only Bus traffic; new clients should use blob references.

use crate::blob_lane::{Discovery, Lane, blob_hex};
use base64::Engine as _;
use serde_json::Value;
use std::future::Future;

pub trait MessageLoader: Sync {
    fn load(&self, args: &Value) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

pub struct Input<'a, D> {
    pub discovery: &'a D,
    pub max_message_size: usize,
}

enum Source<'a> {
    Legacy(&'a str),
    Blob(&'a Value),
}

fn source(args: &Value) -> Result<Source<'_>, String> {
    let args = args
        .as_object()
        .ok_or("invalid_arguments: expected object")?;
    match (args.get("message_b64"), args.get("blob")) {
        (Some(value), None) => value
            .as_str()
            .map(Source::Legacy)
            .ok_or_else(|| "invalid_arguments: message_b64 must be a string".into()),
        (None, Some(value)) => {
            blob_hex(value)?;
            Ok(Source::Blob(value))
        }
        _ => Err("invalid_arguments: exactly one of message_b64 or blob is required".into()),
    }
}

fn decode(value: &str, cap: usize) -> Result<Vec<u8>, String> {
    if value.len() > cap.div_ceil(3).saturating_mul(4) {
        return Err("too_large: message exceeds max_message_size".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|e| format!("message_b64 decode: {e}"))?;
    if bytes.len() > cap {
        return Err("too_large: message exceeds max_message_size".into());
    }
    Ok(bytes)
}

impl<D: Discovery> MessageLoader for Input<'_, D> {
    async fn load(&self, args: &Value) -> Result<Vec<u8>, String> {
        match source(args)? {
            Source::Legacy(value) => decode(value, self.max_message_size),
            Source::Blob(value) => {
                Lane::new()?
                    .fetch(self.discovery, value, self.max_message_size)
                    .await
            }
        }
    }
}

#[cfg(test)]
pub(crate) struct Legacy {
    pub cap: usize,
}
#[cfg(test)]
impl Default for Legacy {
    fn default() -> Self {
        Self {
            cap: 25 * 1024 * 1024,
        }
    }
}
#[cfg(test)]
impl MessageLoader for Legacy {
    async fn load(&self, args: &Value) -> Result<Vec<u8>, String> {
        match source(args)? {
            Source::Legacy(value) => decode(value, self.cap),
            Source::Blob(_) => Err("lane_unavailable: no test lane".into()),
        }
    }
}

#[cfg(test)]
pub(crate) struct TestLane {
    bind: String,
    worker: Option<tokio::task::JoinHandle<()>>,
}
#[cfg(test)]
impl TestLane {
    pub async fn new(bytes: &[u8], status: u16) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let bytes = bytes.to_vec();
        let worker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(head.starts_with(b"GET /blob/"));
            let reply = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let _ = socket.write_all(&bytes).await;
        });
        Self {
            bind,
            worker: Some(worker),
        }
    }
    pub async fn finish(mut self) {
        self.worker.take().unwrap().await.unwrap();
    }
}
#[cfg(test)]
impl Drop for TestLane {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}
#[cfg(test)]
impl Discovery for TestLane {
    async fn bind(&self) -> Result<String, String> {
        Ok(self.bind.clone())
    }
    async fn quota(&self, _: &str) -> Result<Value, String> {
        panic!("diagnostic GET must not ask quota")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct NoLane;
    impl Discovery for NoLane {
        async fn bind(&self) -> Result<String, String> {
            panic!("invalid/legacy input must not discover a lane")
        }
        async fn quota(&self, _: &str) -> Result<Value, String> {
            panic!("GET never asks quota")
        }
    }

    #[tokio::test]
    async fn exclusive_input_validation_and_decoded_bound_precede_lane_use() {
        let input = Input {
            discovery: &NoLane,
            max_message_size: 2,
        };
        for args in [
            json!({}),
            json!({"blob": null, "message_b64": ""}),
            json!({"message_b64": null}),
            json!({"message_b64": "", "blob": "bad"}),
        ] {
            assert!(
                input
                    .load(&args)
                    .await
                    .unwrap_err()
                    .starts_with("invalid_arguments:")
            );
        }
        assert_eq!(
            input.load(&json!({"blob": "b3:é"})).await.unwrap_err(),
            "invalid blob id"
        );
        assert_eq!(
            input.load(&json!({"message_b64": "AP8="})).await.unwrap(),
            [0, 255]
        );
        for encoded in ["AP8B", "AP8BAP8B"] {
            assert_eq!(
                input
                    .load(&json!({"message_b64": encoded}))
                    .await
                    .unwrap_err(),
                "too_large: message exceeds max_message_size"
            );
        }
        assert!(
            input
                .load(&json!({"message_b64": "****"}))
                .await
                .unwrap_err()
                .starts_with("message_b64 decode:")
        );
    }

    #[tokio::test]
    async fn blob_load_is_local_hash_verified_and_uses_the_configured_cap() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        struct Local(String);
        impl Discovery for Local {
            async fn bind(&self) -> Result<String, String> {
                Ok(self.0.clone())
            }
            async fn quota(&self, _: &str) -> Result<Value, String> {
                panic!("GET never asks quota")
            }
        }
        for (bytes, cap, expected) in [
            (b"message".as_slice(), 7, None),
            (b"message".as_slice(), 6, Some("too_large:")),
            (b"corrupt".as_slice(), 7, Some("verify_failed:")),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local = Local(listener.local_addr().unwrap().to_string());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
                let _ = socket.write_all(bytes).await;
            });
            let input = Input {
                discovery: &local,
                max_message_size: cap,
            };
            let args = json!({"blob": {"blob": format!("b3:{}", blake3::hash(b"message").to_hex()), "origin": "beta"}});
            let result = input.load(&args).await;
            if let Some(prefix) = expected {
                assert!(result.unwrap_err().starts_with(prefix));
            } else {
                assert_eq!(result.unwrap(), b"message");
            }
            server.await.unwrap();
        }
    }
}
