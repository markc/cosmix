//! Serve loops (`serve.rs`) — tokio.
//!
//! `-> std::io::Result<()>` (not `-> !`): the loop returns `Err` only
//! on a **fatal socket error** (the bound socket/listener is broken).
//! Per-query decode/parse/IO failures are logged-and-continued and
//! never returned — a bad packet does not terminate the loop. The
//! typed result lets the §9 in-process prober spawn the fn as a
//! `tokio::task`, drive the query set, then `JoinHandle::abort()` it.
//!
//! One `store.snapshot()` `Arc` is taken per query and the whole query
//! is answered against that consistent `Arc` (no torn reads).
//! `resolve` is called identically on both transports (transport-blind);
//! the serve loop *computes* the negotiated UDP size and hands it to
//! `wire::encode_udp` — it does not re-implement truncation or pass
//! size into `resolve`. TCP framing: the serve loop owns only the
//! **inbound** length prefix; `wire::encode_tcp` already includes the
//! outbound one (no double-prefixing).
//!
//! ## Observed serve variants (P2 citizen — additive, instruction-safe)
//!
//! `serve_udp_observed` / `serve_tcp_observed` are *additive siblings*
//! of `serve_udp` / `serve_tcp`: identical packet handling, plus a
//! single [`ResponseObserver`] call on the fully-built response right
//! before encode+send (both transports). They back the plan §7
//! `dnsd.stats` rcode canary (the P2 citizen counts response rcodes;
//! the standalone build never observes).
//!
//! The non-observed `serve_udp` / `serve_tcp` / `handle_tcp_conn`
//! bodies are kept **verbatim** — NOT refactored to delegate — because
//! the `--no-default-features` standalone path (plan §1/§9 goal-(c))
//! must stay the committed P1 source verbatim, exactly the "keep the
//! loop verbatim, identity beats DRY" precedent the P1 prompt set for
//! the bind block. The surviving invariant is **source-verbatim** (the
//! standalone path is the committed P1 source, the `_observed` siblings
//! dead-strip out) with serve behaviour proven by the §8 functional
//! prober — NOT any binary-level identity: adding these `_observed`
//! symbols relocates `.rodata`, and `lto = true` redistributes inlined
//! serve code into `main`'s future on benign core growth, so neither
//! byte- nor instruction-multiset identity survives (both abandoned —
//! P2-C and P2-D). The precise twice-reframed form, the verification
//! recipe, and the evidence are pinned in `cosmix-dnsd`'s plan §1
//! ("surviving `--no-default-features` invariant"),
//! `cosmix-dnsd/src/main.rs`'s module doc, and
//! `feedback_lto_inlining_redistribution`; this crate deliberately
//! states it once there rather than re-deriving it. The
//! cost of the verbatim copies is one small duplicated loop scaffold
//! per transport. The primary guarantee is that they are *literal
//! verbatim duplicates* kept in sync by review;
//! `observed_matches_unobserved_wire` (UDP) and
//! `observed_tcp_matches_unobserved_wire` (TCP) are the regression net
//! — they pin the framed *wire* output + observer-call-count on the
//! out-of-zone (REFUSED) decode path of **both** transports, so the
//! copies cannot *silently* diverge there. They do not exhaustively
//! exercise every response branch (EDNS/TC/NOTIMP/FORMERR); those rely
//! on the verbatim-copy discipline, not the tests. The observer is a
//! pure closure (no Cosmix-internal dep), so core stays pure logic and
//! the P1/P2 split still lives only on the `cosmix-dnsd` binary.

use crate::resolver::resolve;
use crate::store::ZoneStore;
use crate::wire;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// Per-response observer for the *citizen* serve variants. Invoked
/// exactly once with the fully-built **resolver-decided** response
/// `Message`, immediately before it is encoded and sent, on **both**
/// transports. "Before encode" is deliberate and contractual: the
/// `wire::encode_*` codec can substitute an on-wire SERVFAIL on a
/// serialization failure, so an observer sees the resolver's rcode,
/// not that codec-failure edge — correct for the §7/§8 REFUSED canary
/// (a zone-load signal), see `cosmix-dnsd`'s `stats` module doc. It is
/// a pure closure (`Arc<dyn Fn>` so the per-connection TCP spawn can
/// cheaply clone it) — no Cosmix-internal Bus/mesh/config type crosses this
/// boundary, so `cosmix-lib-dns` stays pure-logic core and the P1/P2
/// citizen split still lives only on the `cosmix-dnsd` binary. The
/// observer MUST NOT mutate the response (it is `&`-borrowed) and MUST
/// NOT block (it runs inline on the serve path); the P2 citizen's
/// implementation is a handful of atomic increments.
pub type ResponseObserver = Arc<dyn Fn(&hickory_proto::op::Message) + Send + Sync>;

/// EDNS payload ceiling (matches the resolver's advertised value).
const EDNS_MAX: u16 = 1232;
/// Bare-DNS UDP size when the query carried no OPT.
const UDP_NO_EDNS: u16 = 512;

fn negotiate_udp(req: &hickory_proto::op::Message) -> u16 {
    match req.extensions() {
        Some(edns) => edns.max_payload().min(EDNS_MAX),
        None => UDP_NO_EDNS,
    }
}

pub async fn serve_udp(sock: UdpSocket, store: Arc<dyn ZoneStore>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            // Fatal: the bound socket is broken — propagate so the
            // caller can join with a typed error.
            Err(e) => return Err(e),
        };
        let req = match wire::decode(&buf[..n]) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "dropping malformed UDP query");
                continue;
            }
        };
        let snap = store.snapshot();
        let max_payload = negotiate_udp(&req);
        let resp = resolve(&snap, &req);
        let bytes = wire::encode_udp(&resp, max_payload);
        if let Err(e) = sock.send_to(&bytes, peer).await {
            tracing::debug!(%peer, error = %e, "UDP send failed; continuing");
        }
    }
}

pub async fn serve_tcp(listener: TcpListener, store: Arc<dyn ZoneStore>) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => return Err(e), // fatal listener error
        };
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_conn(stream, store).await {
                tracing::debug!(%peer, error = %e, "TCP connection ended on error");
            }
        });
    }
}

async fn handle_tcp_conn(mut stream: TcpStream, store: Arc<dyn ZoneStore>) -> std::io::Result<()> {
    loop {
        // Inbound 2-byte length prefix — consumed exactly once here;
        // the prefix-stripped body goes to `wire::decode`.
        let mut len_buf = [0u8; 2];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return Ok(()); // clean close / idle
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            return Ok(());
        }
        let req = match wire::decode(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, "dropping malformed TCP query");
                continue; // keep the connection; do not terminate
            }
        };
        let snap = store.snapshot();
        let resp = resolve(&snap, &req);
        // `encode_tcp` already includes the 2-byte prefix — write
        // verbatim, do NOT add another (single-owner rule).
        let out = wire::encode_tcp(&resp);
        if stream.write_all(&out).await.is_err() {
            return Ok(());
        }
    }
}

// ── Observed variants (P2 citizen) ───────────────────────────────────
// Verbatim copies of the loops above with one added `obs(&resp)` call
// before encode+send. Kept as copies (not a shared helper) so the
// non-observed standalone path stays the committed P1 source verbatim
// (see the module doc — source-verbatim + the §8 functional prober is
// the invariant; no binary-level identity survives once these symbols
// join the shared core under `lto = true`). `observed_matches_unobserved_wire` (UDP) +
// `observed_tcp_matches_unobserved_wire` (TCP) are the regression net
// on the REFUSED decode path of both transports; the verbatim-copy
// discipline (not the tests) is what covers every other branch.

/// As [`serve_udp`], plus one [`ResponseObserver`] call per response
/// (the only difference). Used by the P2 `cosmix-dnsd` citizen for the
/// §7 `dnsd.stats` rcode canary; never reached by `--no-default-features`.
pub async fn serve_udp_observed(
    sock: UdpSocket,
    store: Arc<dyn ZoneStore>,
    obs: ResponseObserver,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let req = match wire::decode(&buf[..n]) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "dropping malformed UDP query");
                continue;
            }
        };
        let snap = store.snapshot();
        let max_payload = negotiate_udp(&req);
        let resp = resolve(&snap, &req);
        obs(&resp);
        let bytes = wire::encode_udp(&resp, max_payload);
        if let Err(e) = sock.send_to(&bytes, peer).await {
            tracing::debug!(%peer, error = %e, "UDP send failed; continuing");
        }
    }
}

/// As [`serve_tcp`], plus one [`ResponseObserver`] call per response.
/// `obs` is cloned (Arc) into each per-connection task. Used by the P2
/// citizen; never reached by `--no-default-features`.
pub async fn serve_tcp_observed(
    listener: TcpListener,
    store: Arc<dyn ZoneStore>,
    obs: ResponseObserver,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let store = Arc::clone(&store);
        let obs = Arc::clone(&obs);
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_conn_observed(stream, store, obs).await {
                tracing::debug!(%peer, error = %e, "TCP connection ended on error");
            }
        });
    }
}

async fn handle_tcp_conn_observed(
    mut stream: TcpStream,
    store: Arc<dyn ZoneStore>,
    obs: ResponseObserver,
) -> std::io::Result<()> {
    loop {
        let mut len_buf = [0u8; 2];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return Ok(());
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            return Ok(());
        }
        let req = match wire::decode(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, "dropping malformed TCP query");
                continue;
            }
        };
        let snap = store.snapshot();
        let resp = resolve(&snap, &req);
        obs(&resp);
        let out = wire::encode_tcp(&resp);
        if stream.write_all(&out).await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ZoneName, ZoneSnapshot};
    use crate::snapshot::SnapshotHash;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Trivial `ZoneStore` with an empty (no-zones) snapshot — every
    /// query is out-of-zone ⇒ REFUSED, which is all this parity test
    /// needs (it asserts wire-byte equality + observer call count, not
    /// resolver content).
    struct EmptyStore(Arc<ZoneSnapshot>);
    impl ZoneStore for EmptyStore {
        fn snapshot(&self) -> Arc<ZoneSnapshot> {
            Arc::clone(&self.0)
        }
    }
    fn empty_store() -> Arc<dyn ZoneStore> {
        Arc::new(EmptyStore(Arc::new(ZoneSnapshot {
            zones: BTreeMap::<ZoneName, _>::new(),
            config_hash: SnapshotHash(0),
        })))
    }

    fn query(name: &str) -> Vec<u8> {
        use hickory_proto::op::{Message, MessageType, OpCode, Query};
        use hickory_proto::rr::{Name, RecordType};
        use hickory_proto::serialize::binary::BinEncodable;
        let mut m = Message::new();
        m.set_id(0x1234)
            .set_message_type(MessageType::Query)
            .set_op_code(OpCode::Query)
            .set_recursion_desired(false);
        m.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
        m.to_bytes().unwrap()
    }

    /// The observed UDP loop must emit byte-identical wire output to
    /// the unobserved one on the out-of-zone REFUSED decode path
    /// (regression net against silent copy drift — see the module doc;
    /// not an exhaustive per-branch proof) and call the observer
    /// exactly once per query.
    #[tokio::test]
    async fn observed_matches_unobserved_wire() {
        // Plain path.
        let plain = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let plain_addr = plain.local_addr().unwrap();
        let s1 = empty_store();
        let t1 = tokio::spawn(async move { serve_udp(plain, s1).await });

        // Observed path.
        let obsd = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let obsd_addr = obsd.local_addr().unwrap();
        let s2 = empty_store();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = Arc::clone(&calls);
        let obs: ResponseObserver = Arc::new(move |_resp| {
            calls_c.fetch_add(1, Ordering::SeqCst);
        });
        let t2 = tokio::spawn(async move { serve_udp_observed(obsd, s2, obs).await });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let q = query("nope.example.");

        client.send_to(&q, plain_addr).await.unwrap();
        let mut b1 = vec![0u8; 65_535];
        let n1 = client.recv(&mut b1).await.unwrap();

        client.send_to(&q, obsd_addr).await.unwrap();
        let mut b2 = vec![0u8; 65_535];
        let n2 = client.recv(&mut b2).await.unwrap();

        assert_eq!(
            &b1[..n1],
            &b2[..n2],
            "observed serve loop must emit byte-identical wire to the unobserved one"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "observer must fire exactly once per query"
        );

        t1.abort();
        t2.abort();
    }

    /// TCP counterpart. `serve_tcp_observed` / `handle_tcp_conn_observed`
    /// is a *separate* verbatim copy from the UDP pair (and the more
    /// drift-prone one — it owns the 2-byte length-prefix framing), so
    /// it needs its own parity pin. Asserts the framed wire bytes are
    /// identical to the unobserved TCP loop and the observer fires
    /// exactly once per query.
    #[tokio::test]
    async fn observed_tcp_matches_unobserved_wire() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        async fn ask(addr: std::net::SocketAddr, q: &[u8]) -> Vec<u8> {
            let mut s = TcpStream::connect(addr).await.unwrap();
            let len = u16::try_from(q.len()).unwrap().to_be_bytes();
            s.write_all(&len).await.unwrap();
            s.write_all(q).await.unwrap();
            let mut pfx = [0u8; 2];
            s.read_exact(&mut pfx).await.unwrap();
            let mut resp = vec![0u8; u16::from_be_bytes(pfx) as usize];
            s.read_exact(&mut resp).await.unwrap();
            resp
        }

        let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plain_addr = plain.local_addr().unwrap();
        let s1 = empty_store();
        let t1 = tokio::spawn(async move { serve_tcp(plain, s1).await });

        let obsd = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let obsd_addr = obsd.local_addr().unwrap();
        let s2 = empty_store();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = Arc::clone(&calls);
        let obs: ResponseObserver = Arc::new(move |_resp| {
            calls_c.fetch_add(1, Ordering::SeqCst);
        });
        let t2 = tokio::spawn(async move { serve_tcp_observed(obsd, s2, obs).await });

        let q = query("nope.example.");
        let r1 = ask(plain_addr, &q).await;
        let r2 = ask(obsd_addr, &q).await;

        assert_eq!(
            r1, r2,
            "observed TCP serve loop must emit byte-identical framed wire to the unobserved one"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "observer must fire exactly once per TCP query"
        );

        t1.abort();
        t2.abort();
    }
}
