//! A fake `edit` daemon over the REAL `cosmix_edit_core::Buffer` (ced E1 plan
//! §7.2) — the server side of every mirror test and fixture. Implemented in
//! Stage S (the fixtures are checked against it there); E1d extends it only
//! with the lead's sign-off, because fixtures depend on its behaviour.
//!
//! Faithful to `cosmix-editd` where the mirror can observe it: CAS
//! (`expect_rev` / `base_rev` / latest), attested-looking origins (a claim is
//! honoured as given; the default is `agent:<caller>`), per-origin undo lanes,
//! op_id dedup keyed `(caller, origin, verb, op_id)` over successful replies
//! with an LRU of [`DEDUP_ENTRIES`] (like `actor.rs`), compact mutation replies
//! (the editd builder, copied), `edit.changed` events with a daemon-wide
//! `event_seq`, `resync oversized` past [`FakeEditd::max_event_insert`], and
//! paged-free `edit.get` / `edit.history` (history elides entries whose text
//! exceeds [`FakeEditd::history_elide_over`]). Not modelled: files and disk
//! state (save marks saved; reload takes text from a test hook), anchors,
//! find, holders, props.
//!
//! Test hooks: [`FakeEditd::truncate_next_reply`] (a `reply_truncated`
//! success), [`FakeEditd::evict_dedup`], [`FakeEditd::restart`] (new epoch,
//! buffers restored as recovered), [`FakeEditd::set_disk_text`] (what the next
//! `edit.reload` reads).

use std::collections::{BTreeMap, VecDeque};

use cosmix_edit_core::buffer::{Applied, Buffer, Cas, LaneSel, OpSpec, TxnRequest};
use cosmix_edit_core::error::{CoreError, ErrorCode};
use cosmix_edit_core::history::EntryKind;
use cosmix_edit_core::origin::{Origin, OriginKind, Via};
use cosmix_edit_core::pos::{PosSpec, RangeSpec, SelSpec};
use cosmix_edit_core::wire::{self, Event, KindW};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Successful replies remembered for op_id dedup (editd `DEDUP_ENTRIES`).
pub const DEDUP_ENTRIES: usize = 1024;

/// A reply: `rc` 0 = success, 10 = refusal (body is a `wire::Refusal`).
#[derive(Debug, Clone, PartialEq)]
pub struct FakeReply {
    pub rc: u8,
    pub body: Value,
}

struct FakeBuf {
    buf: Buffer,
    path: Option<String>,
    recovery_id: String,
    recovered: bool,
}

type DedupKey = (String, String, String, String);

pub struct FakeEditd {
    epoch: String,
    bufs: BTreeMap<String, FakeBuf>,
    next: u64,
    event_seq: u64,
    outbox: Vec<Event>,
    dedup: VecDeque<(DedupKey, Value)>,
    disk_text: BTreeMap<String, String>,
    /// An event whose inserted bytes exceed this becomes `resync oversized`
    /// (editd: `MAX_EVENT_BYTES`, 256 KiB).
    pub max_event_insert: usize,
    /// History entries whose text exceeds this are elided (editd: 64 KiB).
    pub history_elide_over: usize,
    /// The next successful mutation reply is compact (`reply_truncated`).
    pub truncate_next_reply: bool,
}

fn via(caller: &str) -> Via {
    Via { from: Some(caller.to_string()), broker_origin: "local".into(), broker_peer: None, broker_service: None }
}

fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, wire::Refusal> {
    serde_json::from_value(args.clone()).map_err(|e| refusal(ErrorCode::InvalidArgument, Some("bad_args"), e.to_string()))
}

fn refusal(code: ErrorCode, reason: Option<&str>, message: impl Into<String>) -> wire::Refusal {
    wire::Refusal {
        error_code: code,
        message: message.into(),
        reason: reason.map(str::to_string),
        buffer: None,
        rev: None,
        context: Default::default(),
    }
}

fn from_core(e: CoreError, bid: &str) -> wire::Refusal {
    let mut context = e.context.clone();
    let rev = context.remove("rev").and_then(|v| v.as_u64());
    context.remove("buffer");
    wire::Refusal {
        error_code: e.code,
        message: e.message,
        reason: e.reason.map(str::to_string),
        buffer: Some(bid.to_string()),
        rev,
        context,
    }
}

fn kind_w(kind: &EntryKind) -> (KindW, Option<[u64; 2]>) {
    match kind {
        EntryKind::Edit => (KindW::Edit, None),
        EntryKind::Undo { of } => (KindW::Undo, Some([*of.start(), *of.end()])),
        EntryKind::Redo { of } => (KindW::Redo, Some([*of.start(), *of.end()])),
        EntryKind::Reload => (KindW::Reload, None),
    }
}

impl FakeEditd {
    pub fn new(epoch: &str) -> Self {
        Self {
            epoch: epoch.to_string(),
            bufs: BTreeMap::new(),
            next: 1,
            event_seq: 0,
            outbox: Vec::new(),
            dedup: VecDeque::new(),
            disk_text: BTreeMap::new(),
            max_event_insert: 256 * 1024,
            history_elide_over: 64 * 1024,
            truncate_next_reply: false,
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// A buffer holding `text`, clean at rev 0, optionally bound to `path`.
    pub fn create(&mut self, path: Option<&str>, text: &str) -> String {
        let (mut buf, _) = Buffer::from_bytes(text.as_bytes()).expect("fixture text");
        buf.mark_saved();
        let bid = format!("b{}_{}", self.next, self.epoch);
        self.next += 1;
        let recovery_id = format!("{:016x}", 0x5f0c_2a9e_1b7d_4c00u64 + self.next);
        self.bufs.insert(bid.clone(), FakeBuf { buf, path: path.map(str::to_string), recovery_id, recovered: false });
        bid
    }

    pub fn text(&self, bid: &str) -> String {
        let b = &self.bufs[bid].buf;
        let mut out = String::new();
        b.read(0..b.len(), &mut out);
        out
    }

    pub fn rev(&self, bid: &str) -> u64 {
        self.bufs[bid].buf.rev()
    }

    /// The buffer ids, in creation order of their numbers.
    pub fn buffers(&self) -> Vec<String> {
        self.bufs.keys().cloned().collect()
    }

    /// `(rev, op_id)` of every retained log entry — for exactly-once checks.
    pub fn log_op_ids(&self, bid: &str) -> Vec<(u64, Option<String>)> {
        self.bufs[bid].buf.history(0, usize::MAX).map(|e| (e.rev, e.op_id.clone())).collect()
    }

    /// Take the events published since the last call, in `event_seq` order.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.outbox)
    }

    /// Forget every cached reply (as if 1,024 other ops had evicted them).
    pub fn evict_dedup(&mut self) {
        self.dedup.clear();
    }

    /// What the next `edit.reload` of `bid` reads from "disk".
    pub fn set_disk_text(&mut self, bid: &str, text: &str) {
        self.disk_text.insert(bid.to_string(), text.to_string());
    }

    /// Daemon restart with recovery: a new epoch; every buffer comes back at
    /// rev 0 with its text (or `text_override`), `recovered: true`, the same
    /// `recovery_id` and path, new id. Dedup, history and events are gone.
    pub fn restart(&mut self, new_epoch: &str, text_override: Option<&str>) {
        let old = std::mem::take(&mut self.bufs);
        self.epoch = new_epoch.to_string();
        self.dedup.clear();
        self.outbox.clear();
        for (_, fb) in old {
            let mut text = String::new();
            fb.buf.read(0..fb.buf.len(), &mut text);
            let text = text_override.map(str::to_string).unwrap_or(text);
            let (buf, _) = Buffer::from_bytes(text.as_bytes()).expect("restored text");
            let bid = format!("b{}_{}", self.next, self.epoch);
            self.next += 1;
            self.bufs.insert(bid, FakeBuf { buf, path: fb.path, recovery_id: fb.recovery_id, recovered: true });
        }
    }

    fn publish(&mut self, mut ev: Event) {
        self.event_seq += 1;
        let seq = self.event_seq;
        match &mut ev {
            Event::Edit(e) => e.event_seq = seq,
            Event::Cursor(e) => e.event_seq = seq,
            Event::Anchor(e) => e.event_seq = seq,
            Event::Disk(e) => e.event_seq = seq,
            Event::Open(e) => e.event_seq = seq,
            Event::Close(e) => e.event_seq = seq,
            Event::Resync(e) => e.event_seq = seq,
        }
        self.outbox.push(ev);
    }

    fn publish_edit(&mut self, bid: &str, applied: &Applied) {
        let fb = &self.bufs[bid];
        let (origin, lane) = fb
            .buf
            .history(applied.rev.saturating_sub(1), 1)
            .find(|e| e.rev == applied.rev)
            .map(|e| (e.origin.to_string(), e.lane.to_string()))
            .unwrap_or_default();
        let inserted: usize = applied.edits.iter().map(|e| e.insert.len()).sum();
        let ev = if inserted > self.max_event_insert {
            Event::Resync(wire::ResyncEvent {
                epoch: self.epoch.clone(),
                buffers: wire::ResyncTarget::Buffers(vec![bid.to_string()]),
                reason: wire::ResyncReason::Oversized,
                rev: Some(applied.rev),
                event_seq: 0,
            })
        } else {
            let (kind, of) = kind_w(&applied.kind);
            Event::Edit(wire::EditEvent {
                epoch: self.epoch.clone(),
                buffer: bid.to_string(),
                rev: applied.rev,
                base_rev: applied.base_rev,
                origin,
                lane,
                kind,
                of,
                op_id: applied.op_id.clone(),
                edits: applied.edits.clone(),
                event_seq: 0,
            })
        };
        self.publish(ev);
    }

    fn span(buf: &Buffer, r: &std::ops::Range<usize>) -> wire::Span {
        wire::Span { start: buf.point(r.start), end: buf.point(r.end) }
    }

    /// editd's compact mutation reply builder (`actor.rs` `mutation_reply`).
    fn mutation_reply(&self, bid: &str, applied: &Applied, origin: &Origin) -> wire::MutationReply {
        let buf = &self.bufs[bid].buf;
        let max = cosmix_edit_core::limits::REPLY_CHANGED_MAX;
        let changed: Vec<wire::Span> = applied.changed.iter().take(max).map(|r| Self::span(buf, r)).collect();
        let envelope = match (applied.changed.iter().map(|r| r.start).min(), applied.changed.iter().map(|r| r.end).max()) {
            (Some(s), Some(e)) => Some(Self::span(buf, &(s..e))),
            _ => None,
        };
        let cursor = buf
            .selections()
            .find(|(o, _)| *o == origin)
            .and_then(|(_, sels)| sels.first().copied())
            .map(|s| buf.point(s.head))
            .or(envelope.map(|s| s.end))
            .or(applied.edits.last().map(|e| buf.point(e.offset.min(buf.len()))));
        wire::MutationReply {
            buffer: bid.to_string(),
            epoch: self.epoch.clone(),
            rev: applied.rev,
            base_rev: applied.base_rev,
            origin: origin.to_string(),
            origin_downgraded: false,
            op_id: applied.op_id.clone(),
            duplicate: false,
            rebased: applied.rebased,
            edit_count: applied.edits.len(),
            inserted_bytes: applied.inserted_bytes,
            deleted_bytes: applied.deleted_bytes,
            changed_span: envelope,
            changed_truncated: applied.changed.len() > max,
            changed,
            cursor,
            dirty: buf.is_dirty(),
            lines: buf.line_count(),
            bytes: buf.len(),
            history_trimmed_to: applied.history_trimmed_to,
        }
    }

    fn lookup(&self, bid: &str) -> Result<(), wire::Refusal> {
        if self.bufs.contains_key(bid) {
            return Ok(());
        }
        let reason = if bid.ends_with(&format!("_{}", self.epoch)) { "unknown_buffer" } else { "epoch_mismatch" };
        let mut r = refusal(ErrorCode::NotFound, Some(reason), format!("no buffer {bid}"));
        r.buffer = Some(bid.to_string());
        Err(r)
    }

    fn origin_of(caller: &str, claim: Option<&str>) -> Result<Origin, wire::Refusal> {
        match claim {
            Some(c) => c.parse::<Origin>().map_err(|e| from_core(e, "")),
            None => Ok(Origin::new(OriginKind::Agent, caller.replace(':', "."))),
        }
    }

    /// Handle one request from `caller` (an attested caller key such as
    /// `local:ced`). Emits events into the outbox.
    pub fn handle(&mut self, caller: &str, verb: &str, args: &Value) -> FakeReply {
        match self.dispatch(caller, verb, args) {
            Ok(body) => FakeReply { rc: 0, body },
            Err(r) => FakeReply { rc: 10, body: serde_json::to_value(r).expect("refusal") },
        }
    }

    fn dispatch(&mut self, caller: &str, verb: &str, args: &Value) -> Result<Value, wire::Refusal> {
        match verb {
            "edit.insert" | "edit.delete" | "edit.replace" | "edit.apply" => self.text_mutation(caller, verb, args),
            "edit.undo" | "edit.redo" => self.undo_redo(caller, verb, args),
            "edit.get" => self.get(args),
            "edit.history" => self.history(args),
            "edit.save" => self.save(args),
            "edit.reload" => self.reload(caller, args),
            "edit.list" => Ok(self.list()),
            "edit.open" => self.open(args),
            "edit.select" => self.select(caller, args),
            "edit.ping" => Ok(json!({"pong": true, "service": "edit", "schema": "edit.v1", "epoch": self.epoch})),
            _ => Err(refusal(ErrorCode::UnknownVerb, None, format!("fake editd: unsupported verb {verb}"))),
        }
    }

    fn dedup_get(&mut self, key: &DedupKey) -> Option<Value> {
        let i = self.dedup.iter().position(|(k, _)| k == key)?;
        let (k, mut v) = self.dedup.remove(i)?;
        self.dedup.push_back((k, v.clone()));
        if let Some(o) = v.as_object_mut() {
            o.insert("duplicate".into(), Value::Bool(true));
        }
        Some(v)
    }

    fn dedup_put(&mut self, key: DedupKey, body: &Value) {
        self.dedup.retain(|(k, _)| k != &key);
        while self.dedup.len() >= DEDUP_ENTRIES {
            self.dedup.pop_front();
        }
        self.dedup.push_back((key, body.clone()));
    }

    fn finish_reply(&mut self, body: Value) -> Value {
        if !self.truncate_next_reply {
            return body;
        }
        self.truncate_next_reply = false;
        let mut compact = serde_json::Map::new();
        for key in ["buffer", "epoch", "rev", "op_id", "duplicate"] {
            if let Some(v) = body.get(key) {
                compact.insert(key.into(), v.clone());
            }
        }
        compact.insert("reply_truncated".into(), Value::Bool(true));
        compact.insert("reply_bytes".into(), json!(5 * 1024 * 1024));
        Value::Object(compact)
    }

    fn text_mutation(&mut self, caller: &str, verb: &str, args: &Value) -> Result<Value, wire::Refusal> {
        let (bid, ops, targs) = match verb {
            "edit.insert" => {
                let r: wire::InsertReq = parse(args)?;
                (r.buffer, vec![OpSpec::Insert { at: r.at, text: r.text }], r.args)
            }
            "edit.delete" => {
                let r: wire::DeleteReq = parse(args)?;
                (r.buffer, vec![OpSpec::Delete { range: r.range }], r.args)
            }
            "edit.replace" => {
                let r: wire::ReplaceReq = parse(args)?;
                (r.buffer, vec![OpSpec::Replace { range: r.range, text: r.text }], r.args)
            }
            _ => {
                let r: wire::ApplyReq = parse(args)?;
                (r.buffer, r.ops, r.args)
            }
        };
        if targs.cas.expect_rev.is_some() && targs.cas.base_rev.is_some() {
            return Err(refusal(ErrorCode::InvalidArgument, Some("both_cas"), "both expect_rev and base_rev"));
        }
        self.lookup(&bid)?;
        let origin = Self::origin_of(caller, targs.meta.origin.as_deref())?;
        let key = targs.meta.op_id.clone().map(|id| (caller.to_string(), origin.to_string(), verb.to_string(), id));
        if let Some(k) = &key
            && let Some(cached) = self.dedup_get(k)
        {
            return Ok(cached);
        }
        let cas = match (targs.cas.expect_rev, targs.cas.base_rev) {
            (Some(r), None) => Cas::ExpectRev(r),
            (None, Some(b)) => Cas::BaseRev(b),
            _ => Cas::Latest,
        };
        let req = TxnRequest { ops, cas, coalesce: targs.coalesce, cursor: targs.cursor, op_id: targs.meta.op_id };
        let applied = self
            .bufs
            .get_mut(&bid)
            .expect("looked up")
            .buf
            .apply(req, &origin, via(caller), 0)
            .map_err(|e| from_core(e, &bid))?;
        self.publish_edit(&bid, &applied);
        let body = serde_json::to_value(self.mutation_reply(&bid, &applied, &origin)).expect("reply");
        if let Some(k) = key {
            self.dedup_put(k, &body);
        }
        Ok(self.finish_reply(body))
    }

    fn undo_redo(&mut self, caller: &str, verb: &str, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::UndoReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let redo = verb == "edit.redo";
        let as_origin = Self::origin_of(caller, r.as_.as_deref())?;
        let key = r.op_id.clone().map(|id| (caller.to_string(), as_origin.to_string(), verb.to_string(), id));
        if let Some(k) = &key
            && let Some(cached) = self.dedup_get(k)
        {
            return Ok(cached);
        }
        let fb = self.bufs.get_mut(&r.buffer).expect("looked up");
        if let Some(expect) = r.expect_rev
            && expect != fb.buf.rev()
        {
            let mut refused = refusal(ErrorCode::Conflict, Some("stale_rev"), format!("expect_rev {expect} is stale"));
            refused.buffer = Some(r.buffer.clone());
            refused.rev = Some(fb.buf.rev());
            return Err(refused);
        }
        let sel = match r.origin.as_deref() {
            None => LaneSel::Own,
            Some("*") => LaneSel::All,
            Some(lane) => LaneSel::Lane(lane.parse::<Origin>().map_err(|e| from_core(e, &r.buffer))?),
        };
        let applied = fb
            .buf
            .undo_redo_op(sel, &as_origin, via(caller), 0, redo, r.op_id.clone())
            .map_err(|e| from_core(e, &r.buffer))?;
        self.publish_edit(&r.buffer, &applied);
        let lane = self.bufs[&r.buffer]
            .buf
            .history(applied.rev.saturating_sub(1), 1)
            .find(|e| e.rev == applied.rev)
            .map(|e| e.lane.to_string())
            .unwrap_or_default();
        let (_, of) = kind_w(&applied.kind);
        let reply = wire::UndoReply {
            mutation: self.mutation_reply(&r.buffer, &applied, &as_origin),
            lane,
            undid: if redo { None } else { of },
            redid: if redo { of } else { None },
        };
        let body = serde_json::to_value(reply).expect("reply");
        if let Some(k) = key {
            self.dedup_put(k, &body);
        }
        Ok(self.finish_reply(body))
    }

    fn get(&mut self, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::GetReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let buf = &self.bufs[&r.buffer].buf;
        let mut text = String::new();
        buf.read(0..buf.len(), &mut text);
        let snapshot = match r.snapshot {
            Some(wire::SnapshotArg::Start(true)) => Some(format!("s{}", buf.rev())),
            Some(wire::SnapshotArg::Token(t)) => Some(t),
            _ => None,
        };
        Ok(serde_json::to_value(wire::GetReply {
            buffer: r.buffer.clone(),
            epoch: self.epoch.clone(),
            rev: buf.rev(),
            text: Some(text),
            lines: None,
            start: buf.point(0),
            end: buf.point(buf.len()),
            bytes_total: buf.len(),
            lines_total: buf.line_count(),
            truncated: false,
            next: None,
            snapshot,
        })
        .expect("reply"))
    }

    fn history(&mut self, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::HistoryReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let buf = &self.bufs[&r.buffer].buf;
        let limit = r.limit.unwrap_or(100);
        let entries: Vec<wire::HistoryEntryW> = buf
            .history(r.since_rev, limit)
            .map(|e| {
                let (kind, of) = kind_w(&e.kind);
                let text_bytes: usize =
                    e.edits.iter().map(|x| x.insert.len()).sum::<usize>() + e.deleted.iter().map(String::len).sum::<usize>();
                let elided = text_bytes > self.history_elide_over;
                wire::HistoryEntryW {
                    rev: e.rev,
                    origin: e.origin.to_string(),
                    lane: e.lane.to_string(),
                    kind,
                    of,
                    op_id: e.op_id.clone(),
                    time: "1970-01-01T00:00:00.000Z".into(),
                    via: wire::ViaW {
                        from: e.via.from.clone(),
                        broker_origin: e.via.broker_origin.clone(),
                        broker_peer: e.via.broker_peer.clone(),
                        broker_service: e.via.broker_service.clone(),
                    },
                    edits: (!elided).then(|| e.edits.clone()),
                    edits_elided: elided,
                    text_bytes,
                }
            })
            .collect();
        let last = entries.last().map(|e| e.rev);
        let truncated = last.is_some_and(|l| l < buf.rev());
        Ok(serde_json::to_value(wire::HistoryReply {
            buffer: r.buffer.clone(),
            rev: buf.rev(),
            oldest_rev: buf.oldest_rev(),
            entries,
            truncated,
            next: if truncated { last } else { None },
        })
        .expect("reply"))
    }

    fn save(&mut self, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::SaveReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let epoch = self.epoch.clone();
        let fb = self.bufs.get_mut(&r.buffer).expect("looked up");
        if let Some(expect) = r.expect_rev
            && expect != fb.buf.rev()
        {
            let mut refused = refusal(ErrorCode::Conflict, Some("stale_rev"), format!("expect_rev {expect} is stale"));
            refused.buffer = Some(r.buffer.clone());
            refused.rev = Some(fb.buf.rev());
            return Err(refused);
        }
        fb.buf.mark_saved();
        fb.recovered = false;
        let path = r.path.clone().or_else(|| fb.path.clone()).unwrap_or_else(|| format!("/fake/{}", r.buffer));
        fb.path = Some(path.clone());
        Ok(serde_json::to_value(wire::SaveReply {
            buffer: r.buffer.clone(),
            epoch,
            path,
            rev: fb.buf.rev(),
            saved_rev: fb.buf.rev(),
            file_bytes: fb.buf.len(),
            disk: wire::DiskState::Clean,
            durable: true,
            warning: None,
        })
        .expect("reply"))
    }

    fn reload(&mut self, caller: &str, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::ReloadReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let Some(disk) = self.disk_text.remove(&r.buffer) else {
            return Ok(json!({"buffer": r.buffer, "rev": self.bufs[&r.buffer].buf.rev(), "unchanged": true}));
        };
        let fb = self.bufs.get_mut(&r.buffer).expect("looked up");
        if fb.buf.is_dirty() && !r.force {
            let mut refused = refusal(ErrorCode::Conflict, Some("dirty"), "buffer is dirty");
            refused.buffer = Some(r.buffer.clone());
            refused.rev = Some(fb.buf.rev());
            return Err(refused);
        }
        let applied = fb.buf.reload_minimal(&disk, via(caller), 0).map_err(|e| from_core(e, &r.buffer))?;
        fb.buf.mark_saved();
        match applied {
            Some(applied) => {
                self.publish_edit(&r.buffer, &applied);
                let disk_origin: Origin = "tool:disk".parse().expect("reserved origin");
                Ok(serde_json::to_value(self.mutation_reply(&r.buffer, &applied, &disk_origin)).expect("reply"))
            }
            None => Ok(json!({"buffer": r.buffer, "rev": self.bufs[&r.buffer].buf.rev(), "unchanged": true})),
        }
    }

    fn summary(&self, bid: &str, fb: &FakeBuf) -> wire::BufferSummary {
        wire::BufferSummary {
            buffer: bid.to_string(),
            path: fb.path.clone(),
            opened_as: fb.path.clone(),
            name: fb.path.as_ref().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()),
            language: "text".into(),
            rev: fb.buf.rev(),
            saved_rev: fb.buf.saved_rev(),
            dirty: fb.buf.is_dirty() || fb.recovered,
            disk: wire::DiskState::Clean,
            lines: fb.buf.line_count(),
            bytes: fb.buf.len(),
            holders: vec![],
            recovery_id: fb.recovery_id.clone(),
            recovered: fb.recovered,
        }
    }

    fn list(&self) -> Value {
        let buffers = self.bufs.iter().map(|(bid, fb)| self.summary(bid, fb)).collect();
        serde_json::to_value(wire::ListReply { epoch: self.epoch.clone(), buffers }).expect("reply")
    }

    fn open(&mut self, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::OpenReq = parse(args)?;
        let found = r.path.as_ref().and_then(|p| self.bufs.iter().find(|(_, fb)| fb.path.as_ref() == Some(p)).map(|(b, _)| b.clone()));
        let (bid, reopened, created) = match found {
            Some(bid) => (bid, true, false),
            None if r.path.is_none() || r.create => (self.create(r.path.as_deref(), ""), false, r.path.is_some()),
            None => return Err(refusal(ErrorCode::NotFound, Some("file_not_found"), "no such file in the fake")),
        };
        let fb = &self.bufs[&bid];
        let s = self.summary(&bid, fb);
        Ok(serde_json::to_value(wire::OpenReply {
            buffer: bid.clone(),
            epoch: self.epoch.clone(),
            path: s.path.clone(),
            opened_as: s.opened_as,
            name: s.name,
            language: s.language,
            rev: fb.buf.rev(),
            lines: s.lines,
            bytes: s.bytes,
            eol: cosmix_edit_core::buffer::Eol::Lf,
            bom: false,
            disk: wire::DiskState::Clean,
            reopened,
            created,
            recovery_id: s.recovery_id,
            recovered: s.recovered,
            recovered_from: None,
        })
        .expect("reply"))
    }

    fn select(&mut self, caller: &str, args: &Value) -> Result<Value, wire::Refusal> {
        let r: wire::SelectReq = parse(args)?;
        self.lookup(&r.buffer)?;
        let origin = Self::origin_of(caller, r.meta.origin.as_deref())?;
        let mut sels = Vec::new();
        for s in &r.ranges {
            let (a, h) = match s {
                SelSpec::Directed { anchor: PosSpec::Offset(a), head: PosSpec::Offset(h) } => (*a, *h),
                SelSpec::Range(RangeSpec::Offsets([a, b])) => (*a, *b),
                _ => return Err(refusal(ErrorCode::InvalidArgument, Some("bad_args"), "fake select takes offsets only")),
            };
            sels.push(cosmix_edit_core::anchor::Selection { anchor: a, head: h });
        }
        let epoch = self.epoch.clone();
        let fb = self.bufs.get_mut(&r.buffer).expect("looked up");
        fb.buf.set_selections(&origin, sels.clone()).map_err(|e| from_core(e, &r.buffer))?;
        let rev = fb.buf.rev();
        let points: Vec<wire::SelectionW> =
            sels.iter().map(|s| wire::SelectionW { anchor: fb.buf.point(s.anchor), head: fb.buf.point(s.head) }).collect();
        self.publish(Event::Cursor(wire::CursorEvent {
            epoch,
            buffer: r.buffer.clone(),
            rev,
            origin: origin.to_string(),
            selections: sels.iter().map(|s| wire::OffsetSelection { anchor: s.anchor, head: s.head }).collect(),
            event_seq: 0,
        }));
        Ok(serde_json::to_value(wire::SelectionsReply {
            buffer: r.buffer,
            rev,
            origin: origin.to_string(),
            origin_downgraded: false,
            selections: points,
        })
        .expect("reply"))
    }
}
