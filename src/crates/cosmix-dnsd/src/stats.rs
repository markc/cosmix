//! Response-rcode counters (citizen build only) — backs the plan §7
//! `dnsd.stats` action and its **`REFUSED` canary**.
//!
//! v0 is deliberately coarse: one monotone `AtomicU64` per rcode class
//! the v0 resolver can emit (`resolver.rs` emits exactly NoError /
//! NXDomain / Refused / FormErr / NotImp), plus an `other` catch-all so
//! a future/unexpected rcode is still counted, never silently dropped.
//! No per-name / per-qtype dimension, no histogram — the §7 contract is
//! "query counts by rcode (esp. the `REFUSED` canary)", nothing finer.
//!
//! Why this lives in the citizen, not `cosmix-lib-dns`: the core stays
//! pure-logic with no stats state (plan §1; the `--no-default-features`
//! standalone never counts). The serve path observes responses through
//! the additive [`cosmix_dns::ResponseObserver`] seam; this struct is
//! the citizen's observer payload, shared `Arc` between every serve
//! task and the Bus `dnsd.stats` reader.
//!
//! **What is counted: the resolver-decided rcode, observed
//! pre-encode.** The observer fires on the fully-built response
//! *before* `wire::encode_udp`/`encode_tcp`. Those encoders can, on a
//! serialization failure, substitute a SERVFAIL on the wire
//! (`wire.rs` `servfail_bytes` fallback) — a distinct failure class
//! (codec, not zone load). That rare substitution is **intentionally
//! not** reflected here: the §8 canary's question is "did this node's
//! *resolver* go authoritative-for-nothing", a zone-load decision, and
//! REFUSED is resolver-decided and never an encode artifact. So
//! pre-encode is the *correct* observation point for this contract,
//! not a gap — `dnsd.stats` is resolver-response-rcode counts, not
//! on-wire-byte rcode counts, and the two differ only in the
//! codec-failure edge that this canary deliberately does not track.
//!
//! The `REFUSED` canary (plan §4/§7): the v0 resolver REFUSES only
//! out-of-zone / wrong-class queries and **never an in-zone name**, so
//! a `refused` count climbing on a node that should be authoritative is
//! the agreed signal that its zone load is wrong (it has gone
//! authoritative-for-nothing) — the exact failure the §8 cross-node
//! parity check exists to catch.

use hickory_proto::op::{Message, ResponseCode};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotone per-rcode response counters. All loads/stores are
/// `Relaxed`: these are independent statistical counters with no
/// cross-counter invariant to protect (a reader may see counts from
/// slightly different instants — acceptable for a health canary, and
/// far cheaper than seq-cst on the hot serve path).
#[derive(Default)]
pub struct DnsStats {
    noerror: AtomicU64,
    nxdomain: AtomicU64,
    refused: AtomicU64,
    formerr: AtomicU64,
    notimp: AtomicU64,
    other: AtomicU64,
}

impl DnsStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observer body: classify one fully-built response and bump the
    /// matching counter. Runs inline on the serve path (UDP + every TCP
    /// query) — a single relaxed atomic add, no allocation, no block.
    pub fn record(&self, resp: &Message) {
        let bucket = match resp.response_code() {
            ResponseCode::NoError => &self.noerror,
            ResponseCode::NXDomain => &self.nxdomain,
            ResponseCode::Refused => &self.refused,
            ResponseCode::FormErr => &self.formerr,
            ResponseCode::NotImp => &self.notimp,
            _ => &self.other,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    /// `dnsd.stats` reply payload. `total` is summed from the same
    /// relaxed loads (not a separate counter) so it is always exactly
    /// the sum of the buckets in this snapshot — no skew between
    /// `total` and the parts. `refused` is the canary the §8 script
    /// keys on; it is named explicitly in the wire shape, not derived.
    pub fn snapshot_json(&self) -> serde_json::Value {
        let noerror = self.noerror.load(Ordering::Relaxed);
        let nxdomain = self.nxdomain.load(Ordering::Relaxed);
        let refused = self.refused.load(Ordering::Relaxed);
        let formerr = self.formerr.load(Ordering::Relaxed);
        let notimp = self.notimp.load(Ordering::Relaxed);
        let other = self.other.load(Ordering::Relaxed);
        let total = noerror + nxdomain + refused + formerr + notimp + other;
        serde_json::json!({
            "noerror": noerror,
            "nxdomain": nxdomain,
            "refused": refused,
            "formerr": formerr,
            "notimp": notimp,
            "other": other,
            "total": total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Message;

    fn resp(rc: ResponseCode) -> Message {
        let mut m = Message::new();
        m.set_response_code(rc);
        m
    }

    #[test]
    fn record_buckets_each_rcode_and_total_is_the_sum() {
        let s = DnsStats::new();
        s.record(&resp(ResponseCode::NoError));
        s.record(&resp(ResponseCode::NoError));
        s.record(&resp(ResponseCode::NXDomain));
        s.record(&resp(ResponseCode::Refused));
        s.record(&resp(ResponseCode::FormErr));
        s.record(&resp(ResponseCode::NotImp));
        // ServFail is not a v0-resolver rcode → the `other` catch-all,
        // proving an unexpected rcode is counted, never dropped.
        s.record(&resp(ResponseCode::ServFail));

        let j = s.snapshot_json();
        assert_eq!(j["noerror"], 2);
        assert_eq!(j["nxdomain"], 1);
        assert_eq!(j["refused"], 1);
        assert_eq!(j["formerr"], 1);
        assert_eq!(j["notimp"], 1);
        assert_eq!(j["other"], 1);
        assert_eq!(j["total"], 7, "total must equal the sum of buckets");
    }

    #[test]
    fn refused_canary_is_isolated() {
        // The canary must move ONLY on Refused — a NoError/NXDomain
        // storm must not inflate it (else the §8 signal is useless).
        let s = DnsStats::new();
        for _ in 0..50 {
            s.record(&resp(ResponseCode::NoError));
        }
        s.record(&resp(ResponseCode::Refused));
        assert_eq!(s.snapshot_json()["refused"], 1);
    }
}
