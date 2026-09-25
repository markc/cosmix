//! Core tests (ced E0 plan §6.1). In-crate so they can reach the gap
//! buffer's failure injection and the line index.

mod props;
mod unit;

use crate::buffer::{Buffer, Cas, OpSpec, TxnRequest};
use crate::error::{CoreError, ErrorCode};
use crate::origin::{Origin, Via};
use crate::pos::{PosSpec, RangeSpec};

pub(crate) fn o(s: &str) -> Origin {
    s.parse().unwrap()
}

pub(crate) fn via() -> Via {
    Via { from: Some("test".into()), broker_origin: "local".into(), broker_peer: None, broker_service: None }
}

pub(crate) fn buf(s: &str) -> Buffer {
    Buffer::from_bytes(s.as_bytes()).unwrap().0
}

pub(crate) fn text(b: &Buffer) -> String {
    let mut out = String::new();
    b.read(0..b.len(), &mut out);
    out
}

pub(crate) fn ins(at: usize, t: &str) -> OpSpec {
    OpSpec::Insert { at: PosSpec::Offset(at), text: t.into() }
}

pub(crate) fn del(s: usize, e: usize) -> OpSpec {
    OpSpec::Delete { range: RangeSpec::Offsets([s, e]) }
}

pub(crate) fn rep(s: usize, e: usize, t: &str) -> OpSpec {
    OpSpec::Replace { range: RangeSpec::Offsets([s, e]), text: t.into() }
}

pub(crate) fn req(ops: Vec<OpSpec>) -> TxnRequest {
    TxnRequest { ops, cas: Cas::Latest, coalesce: false, cursor: None, op_id: None }
}

pub(crate) fn coalescing(ops: Vec<OpSpec>) -> TxnRequest {
    TxnRequest { coalesce: true, ..req(ops) }
}

/// Apply as `origin`, panicking on refusal.
pub(crate) fn edit(b: &mut Buffer, origin: &str, ops: Vec<OpSpec>) -> crate::buffer::Applied {
    b.apply(req(ops), &o(origin), via(), 0).unwrap()
}

/// Line starts of `s`, recomputed naively.
pub(crate) fn recount(s: &str) -> Vec<u32> {
    let mut v = vec![0u32];
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i as u32 + 1);
        }
    }
    v
}

pub(crate) fn assert_code(r: Result<impl std::fmt::Debug, CoreError>, code: ErrorCode, reason: &str) {
    match r {
        Ok(v) => panic!("expected {code:?} {reason}, got Ok({v:?})"),
        Err(e) => {
            assert_eq!(e.code, code, "{e:?}");
            assert_eq!(e.reason, Some(reason), "{e:?}");
        }
    }
}
