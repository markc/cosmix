//! Wire codec (`wire.rs`) — hickory-proto, **codec only**.
//!
//! No `hickory-server` / `Catalog` / `Authority` / `hickory-resolver` /
//! `hickory-recursor`, no recursion/forwarding. Our model types are
//! converted to/from hickory `Record`s only at this edge (via
//! `rr::RData::to_hickory_record`); the rest of the crate is in model
//! types.
//!
//! **This layer owns size-aware truncation** (the byte size only exists
//! post-encode; `resolve` is not even given `max_payload`, §5).
//! `encode_udp` sets TC=1 and drops sections (ADDITIONAL first, then
//! ANSWER) until it fits. `encode_tcp` returns bytes **already
//! including** the 2-byte length prefix and **never** truncates.
//!
//! **TCP length-prefix single owner:** the prefix is produced exactly
//! once here (`encode_tcp`) and consumed exactly once by
//! `serve_tcp`'s inbound framer. `decode` parses a **bare** DNS
//! message (no framing prefix) — identical on the UDP and TCP paths.

use hickory_proto::ProtoError;
use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};

/// Decode a bare inbound DNS message. Malformed input is an `Err`
/// (the serve loop turns it into FORMERR/drop — never a panic).
pub fn decode(bytes: &[u8]) -> Result<Message, ProtoError> {
    Message::from_bytes(bytes)
}

fn serialize(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    msg.to_bytes()
}

/// Best-effort SERVFAIL fallback if a response fails to serialize
/// (should not happen for well-formed responses; never panic on the
/// serve path). Preserves id + question so the client can correlate.
fn servfail_bytes(msg: &Message) -> Vec<u8> {
    let mut m = Message::new();
    m.set_id(msg.id());
    m.set_message_type(hickory_proto::op::MessageType::Response);
    m.set_op_code(msg.op_code());
    m.set_response_code(ResponseCode::ServFail);
    for q in msg.queries() {
        m.add_query(q.clone());
    }
    m.to_bytes().unwrap_or_default()
}

/// Encode for UDP, owning TC/truncation against `max_payload`.
///
/// If the full message exceeds the limit: drop ADDITIONAL, then (if
/// still over) drop ANSWER, setting TC=1 — the header stays
/// well-formed. The negotiated `max_payload` is computed by the serve
/// loop (no OPT → 512; OPT → `min(advertised, 1232)`), never by the
/// resolver.
pub fn encode_udp(msg: &Message, max_payload: u16) -> Vec<u8> {
    let limit = max_payload as usize;

    let full = match serialize(msg) {
        Ok(b) => b,
        Err(_) => return servfail_bytes(msg),
    };
    if full.len() <= limit {
        return full;
    }

    // Drop ADDITIONAL (keep any echoed OPT so EDNS negotiation holds).
    let mut trimmed = msg.clone();
    let opt = trimmed.extensions().clone();
    trimmed.additionals_mut().clear();
    if let Some(edns) = opt {
        trimmed.set_edns(edns);
    }
    if let Ok(b) = serialize(&trimmed)
        && b.len() <= limit
    {
        return b;
    }

    // Still over → TC=1 and drop ANSWER (truncation per DNS).
    trimmed.set_truncated(true);
    trimmed.answers_mut().clear();
    trimmed.name_servers_mut().clear();
    match serialize(&trimmed) {
        Ok(b) => b,
        Err(_) => servfail_bytes(msg),
    }
}

/// Encode for TCP: 2-byte big-endian length prefix + message, **no
/// truncation**. The returned bytes are written verbatim by
/// `serve_tcp` (it MUST NOT add its own prefix — no double-prefixing).
///
/// A DNS-over-TCP message cannot exceed 65535 bytes (the 2-byte length
/// field). If the body somehow does (it never should for this mesh's
/// tiny zones), fall back to a SERVFAIL — emitting a length prefix that
/// disagrees with the payload would desync the client's stream framer.
pub fn encode_tcp(msg: &Message) -> Vec<u8> {
    let mut body = serialize(msg).unwrap_or_else(|_| servfail_bytes(msg));
    let len = match u16::try_from(body.len()) {
        Ok(l) => l,
        Err(_) => {
            // Unframable on TCP → SERVFAIL (tiny; always fits) so the
            // prefix and payload stay consistent.
            body = servfail_bytes(msg);
            u16::try_from(body.len()).unwrap_or(0)
        }
    };
    let mut out = Vec::with_capacity(2 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    out
}
