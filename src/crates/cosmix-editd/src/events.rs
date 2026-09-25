//! The ordered publisher (plan §4.6).
//!
//! One task publishes `edit.changed` and `edit.props.changed` via
//! `noded topic.publish` with headers `name=<topic>`, `retain=false` (powerd
//! transport). Inner frame command: `edit.changed` / `props.changed`.
//!
//! # Contract: loss is always announced (frozen)
//! - An event whose encoded size exceeds `MAX_EVENT_BYTES` is replaced by
//!   `resync {buffers: [b], reason: "oversized", rev}`.
//! - Pending events are budgeted by `MAX_PUBLISH_QUEUE_BYTES`. A dropped event
//!   (budget or send failure) marks its buffer `resync_pending`; pending
//!   resyncs are published AHEAD of later events and retried with backoff
//!   (`RESYNC_BACKOFF_BASE_MS` ×2, cap `RESYNC_BACKOFF_CAP_MS`) until delivered.
//! - On every supervised-client reconnect edge (`subscribe_state`) publish
//!   `resync {buffers: "all", reason: "reconnect"}`.
//! - `event_seq` is daemon-session monotonic and counts `edit.changed`
//!   frames ONLY (contiguous: 1, 2, 3, …); `edit.props.changed` frames carry
//!   none. Every event carries `epoch`.
//! - The sink awaits noded's `topic.publish` result. For an ordinary
//!   `edit.changed` event, a refusal, a transport failure OR any `dropped`
//!   recipient (a subscriber whose outbound queue was full) is a loss: a
//!   resync is owed for its buffer.
//! - A RESYNC is retried only when noded did not accept it (refusal, transport
//!   failure, timeout). Accepted with `dropped > 0` counts as delivered: the
//!   stalled subscriber that missed it sees its own `event_seq` gap when it
//!   drains, and live events must never queue behind one stalled subscriber.
//! - An owed resync is detached from the pending set while it is in flight, so
//!   a loss recorded meanwhile stays owed; a failed attempt merges it back.
//! - Loss is accounted PER TOPIC. A lost `edit.props.changed` frame (queue
//!   budget, oversize, send failure, drop) counts in `publisher_loss` but owes
//!   no `edit.changed` resync: props consumers heal from `edit.props.get` /
//!   `props.watch`, and props-only subscribers would never see that resync.
//! - Known limit (E0, Wontfix): one awaited noded round trip per frame,
//!   serially. Fine at typing rates; a burst of thousands of frames or a noded
//!   stall (5 s timeout per frame) overflows the queue into resyncs.
//!
//! Mirror rule (documented for clients): apply `edit` events whose `base_rev`
//! equals the mirror's rev, in list order; on a `base_rev` mismatch, an
//! `event_seq` gap, a `resync` naming the buffer (or `all`) or an epoch change,
//! refetch with `edit.get snapshot:true`.
//!
//! `event_seq` is assigned at the send point, so delivered events are strictly
//! increasing in delivery order; a send that fails consumes its number (a gap)
//! and is announced by the resync that follows.

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cosmix_bus::bus::BusMessage;
use cosmix_edit_core::wire::{
    AllTag, BufferId, Event, ResyncEvent, ResyncReason, ResyncTarget, TOPIC_CHANGED, TOPIC_PROPS_CHANGED,
};
use cosmix_props_core::{PropPath, PropValue};
use tokio::sync::Notify;

use crate::limits::{MAX_EVENT_BYTES, MAX_PUBLISH_QUEUE_BYTES, RESYNC_BACKOFF_BASE_MS, RESYNC_BACKOFF_CAP_MS};

/// Buffers owed a `resync`. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct ResyncPending {
    pub buffers: BTreeSet<BufferId>,
    pub all: bool,
}

/// One frame for the sink: the topic and the inner Bus message.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub topic: &'static str,
    pub message: BusMessage,
}

/// Why a publication did not reach every subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// Not accepted: refused, transport failure or timeout.
    Failed(String),
    /// Accepted by noded, but dropped for this many full subscribers.
    Dropped(u64),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Failed(e) => f.write_str(e),
            SinkError::Dropped(n) => write!(f, "dropped for {n} subscriber(s)"),
        }
    }
}

pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>>;

/// Where publications go: the Bus in production, a recorder in tests.
pub trait EventSink: Send + Sync + 'static {
    fn publish<'a>(&'a self, out: &'a Outgoing) -> SinkFuture<'a>;
}

/// The production sink: `noded topic.publish` over the supervised client.
pub struct BusSink(pub Arc<cosmix_client::SupervisedClient>);

impl EventSink for BusSink {
    fn publish<'a>(&'a self, out: &'a Outgoing) -> SinkFuture<'a> {
        Box::pin(async move {
            let headers = std::collections::BTreeMap::from([
                ("name".to_string(), out.topic.to_string()),
                ("retain".to_string(), "false".to_string()),
            ]);
            let wire = out.message.to_wire();
            match tokio::time::timeout(
                Duration::from_secs(5),
                self.0.call_with_headers_raw("noded", "topic.publish", &headers, &wire),
            )
            .await
            {
                Ok(Ok((rc, body, error))) => publish_outcome(rc, &body, error.as_deref()),
                Ok(Err(error)) => Err(SinkError::Failed(error.to_string())),
                Err(_) => Err(SinkError::Failed("topic.publish timed out".to_string())),
            }
        })
    }
}

/// noded's `topic.publish` answer. `rc 0` with `dropped > 0` means some
/// subscriber's outbound queue was full: noded ACCEPTED the frame but that
/// subscriber lost it ([`SinkError::Dropped`]; see the module docs for how
/// events and resyncs treat it). `refused` recipients were never eligible to
/// see it: not a loss.
pub fn publish_outcome(rc: u8, body: &str, error: Option<&str>) -> Result<(), SinkError> {
    if rc != 0 {
        return Err(SinkError::Failed(format!("topic.publish refused (rc {rc}): {}", error.filter(|e| !e.is_empty()).unwrap_or(body))));
    }
    let dropped = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("dropped").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    if dropped > 0 {
        return Err(SinkError::Dropped(dropped));
    }
    Ok(())
}

enum Item {
    Event { buffer: Option<BufferId>, event: Event },
    /// Props frames owe no `edit.changed` resync when lost (module docs).
    Props { message: BusMessage },
}

struct Queued {
    item: Item,
    bytes: usize,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Queued>,
    queue_bytes: usize,
    pending: ResyncPending,
    reconnect: bool,
}

/// The shared publisher handle. Enqueueing never blocks and never awaits.
pub struct Publisher {
    epoch: String,
    state: Mutex<State>,
    wake: Notify,
    event_seq: AtomicU64,
    loss: AtomicU64,
    queue_cap: usize,
}

/// `io::Write` that only counts: the encoded size of an event without
/// materialising it (a 64 MiB paste must not be copied just to be measured).
struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encoded JSON size of `value`.
pub fn encoded_len<T: serde::Serialize>(value: &T) -> usize {
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

fn set_seq(event: &mut Event, seq: u64) {
    match event {
        Event::Edit(e) => e.event_seq = seq,
        Event::Cursor(e) => e.event_seq = seq,
        Event::Anchor(e) => e.event_seq = seq,
        Event::Disk(e) => e.event_seq = seq,
        Event::Open(e) => e.event_seq = seq,
        Event::Close(e) => e.event_seq = seq,
        Event::Resync(e) => e.event_seq = seq,
    }
}

fn event_rev(event: &Event) -> Option<u64> {
    match event {
        Event::Edit(e) => Some(e.rev),
        Event::Cursor(e) => Some(e.rev),
        Event::Anchor(e) => Some(e.rev),
        Event::Disk(e) => Some(e.rev),
        Event::Open(e) => Some(e.rev),
        Event::Close(_) | Event::Resync(_) => None,
    }
}

/// Headroom for the `event_seq` digits written at send time.
const SEQ_SLACK: usize = 24;

impl Publisher {
    pub fn new(epoch: &str) -> Arc<Self> {
        Self::with_queue_cap(epoch, MAX_PUBLISH_QUEUE_BYTES)
    }

    /// A publisher with a smaller queue budget (tests).
    pub fn with_queue_cap(epoch: &str, queue_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            epoch: epoch.to_string(),
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            event_seq: AtomicU64::new(0),
            loss: AtomicU64::new(0),
            queue_cap,
        })
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// The last assigned `event_seq`.
    pub fn event_seq(&self) -> u64 {
        self.event_seq.load(Ordering::Acquire)
    }

    /// Frames lost so far (`edit.changed` losses are announced by a resync;
    /// `edit.props.changed` losses are only counted).
    pub fn loss(&self) -> u64 {
        self.loss.load(Ordering::Acquire)
    }

    /// Queue an `edit.changed` event for `buffer` (its `event_seq` is set at
    /// send time). Oversized events become `resync oversized`.
    pub fn event(&self, buffer: Option<&str>, event: Event) {
        let size = encoded_len(&event) + SEQ_SLACK;
        let event = if size > MAX_EVENT_BYTES {
            Event::Resync(ResyncEvent {
                epoch: self.epoch.clone(),
                buffers: ResyncTarget::Buffers(buffer.map(|b| vec![b.to_string()]).unwrap_or_default()),
                reason: ResyncReason::Oversized,
                rev: event_rev(&event),
                event_seq: 0,
            })
        } else {
            event
        };
        let bytes = encoded_len(&event) + SEQ_SLACK;
        self.enqueue(Queued { item: Item::Event { buffer: buffer.map(str::to_string), event }, bytes });
    }

    /// Queue `resync oversized` for an event known to be too large to build.
    pub fn oversized(&self, buffer: &str, rev: u64) {
        let event = Event::Resync(ResyncEvent {
            epoch: self.epoch.clone(),
            buffers: ResyncTarget::Buffers(vec![buffer.to_string()]),
            reason: ResyncReason::Oversized,
            rev: Some(rev),
            event_seq: 0,
        });
        let bytes = encoded_len(&event) + SEQ_SLACK;
        self.enqueue(Queued { item: Item::Event { buffer: Some(buffer.to_string()), event }, bytes });
    }

    /// Queue one SPEC-07 `props.changed` leaf change (`_buffer`: the buffer it
    /// concerns, if any — a lost props frame owes no resync, see module docs).
    pub fn props_changed(&self, _buffer: Option<&str>, path: &PropPath, old: &PropValue, new: &PropValue) {
        let message = cosmix_props_core::publish::build_props_changed_message(path, old, new, "edit");
        if message.body.len() > MAX_EVENT_BYTES {
            // Inputs are bounded so this cannot happen; if it ever does, the
            // frame is counted lost, never published oversized.
            self.loss.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let bytes = message.body.len() + 256;
        self.enqueue(Queued { item: Item::Props { message }, bytes });
    }

    /// A supervised-client reconnect edge: `resync all` goes out first.
    pub fn reconnected(&self) {
        self.state.lock().expect("publisher state").reconnect = true;
        self.wake.notify_one();
    }

    fn enqueue(&self, queued: Queued) {
        {
            let mut state = self.state.lock().expect("publisher state");
            if state.queue_bytes.saturating_add(queued.bytes) > self.queue_cap {
                if let Item::Event { buffer, .. } = &queued.item {
                    Self::mark_lost(&mut state, buffer.as_ref());
                }
                self.loss.fetch_add(1, Ordering::AcqRel);
            } else {
                state.queue_bytes += queued.bytes;
                state.queue.push_back(queued);
            }
        }
        self.wake.notify_one();
    }

    fn mark_lost(state: &mut State, buffer: Option<&BufferId>) {
        match buffer {
            Some(b) => {
                state.pending.buffers.insert(b.clone());
            }
            None => state.pending.all = true,
        }
    }

    fn next_seq(&self) -> u64 {
        self.event_seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Detach the owed resync, if any: from here on a new loss is owed anew,
    /// never erased by this resync's completion.
    fn take_owed(&self) -> Option<(bool, bool, BTreeSet<BufferId>)> {
        let mut state = self.state.lock().expect("publisher state");
        if !state.reconnect && !state.pending.all && state.pending.buffers.is_empty() {
            return None;
        }
        let reconnect = std::mem::take(&mut state.reconnect);
        let all = std::mem::take(&mut state.pending.all);
        Some((reconnect, all, std::mem::take(&mut state.pending.buffers)))
    }

    /// A detached resync that did not get through is owed again (merged with
    /// anything owed since).
    fn restore_owed(&self, reconnect: bool, all: bool, buffers: BTreeSet<BufferId>) {
        let mut state = self.state.lock().expect("publisher state");
        state.reconnect |= reconnect;
        state.pending.all |= all;
        state.pending.buffers.extend(buffers);
    }

    fn event_message(event: &Event) -> BusMessage {
        let mut message = BusMessage::new();
        message.set("command", TOPIC_CHANGED);
        message.body = serde_json::to_string(event).unwrap_or_default();
        message
    }

    /// Publish until the process ends. Waits on enqueue wakes; the only sleep
    /// is the backoff before re-sending an owed resync.
    pub async fn run(self: Arc<Self>, sink: Arc<dyn EventSink>) {
        let base = Duration::from_millis(RESYNC_BACKOFF_BASE_MS);
        let cap = Duration::from_millis(RESYNC_BACKOFF_CAP_MS);
        let mut backoff = base;
        loop {
            // 1. An owed resync goes out ahead of every later event.
            if let Some((reconnect, all, buffers)) = self.take_owed() {
                let seq = self.next_seq();
                let resync = Event::Resync(ResyncEvent {
                    epoch: self.epoch.clone(),
                    buffers: if reconnect || all {
                        ResyncTarget::All(AllTag::All)
                    } else {
                        ResyncTarget::Buffers(buffers.iter().cloned().collect())
                    },
                    reason: if reconnect { ResyncReason::Reconnect } else { ResyncReason::PublisherLoss },
                    rev: None,
                    event_seq: seq,
                });
                let mut message = Self::event_message(&resync);
                message.set("event_seq", &seq.to_string());
                let out = Outgoing { topic: TOPIC_CHANGED, message };
                match sink.publish(&out).await {
                    // Accepted by noded (even if dropped for a stalled
                    // subscriber, which sees its own event_seq gap): done.
                    Ok(()) | Err(SinkError::Dropped(_)) => backoff = base,
                    Err(error) => {
                        self.restore_owed(reconnect, all, buffers);
                        tracing::warn!("cosmix-editd: resync publish failed ({error}); retrying in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(cap);
                    }
                }
                continue;
            }
            // 2. The next queued publication, if any.
            let next = {
                let mut state = self.state.lock().expect("publisher state");
                let next = state.queue.pop_front();
                if let Some(q) = &next {
                    state.queue_bytes -= q.bytes;
                }
                next
            };
            let Some(queued) = next else {
                self.wake.notified().await;
                continue;
            };
            let (buffer, out) = match queued.item {
                Item::Event { buffer, mut event } => {
                    // Only `edit.changed` frames consume a number, so a
                    // subscriber of that topic alone sees no gaps.
                    let seq = self.next_seq();
                    set_seq(&mut event, seq);
                    let mut message = Self::event_message(&event);
                    message.set("event_seq", &seq.to_string());
                    (Some(buffer), Outgoing { topic: TOPIC_CHANGED, message })
                }
                Item::Props { message } => (None, Outgoing { topic: TOPIC_PROPS_CHANGED, message }),
            };
            if let Err(error) = sink.publish(&out).await {
                self.loss.fetch_add(1, Ordering::AcqRel);
                match buffer {
                    // A lost edit.changed event (failed OR dropped): resync owed.
                    Some(buffer) => {
                        tracing::warn!("cosmix-editd: edit.changed publish lost ({error}); resync owed");
                        let mut state = self.state.lock().expect("publisher state");
                        Self::mark_lost(&mut state, buffer.as_ref());
                    }
                    None => tracing::warn!("cosmix-editd: edit.props.changed publish lost ({error})"),
                }
            }
        }
    }
}

/// Test support shared by module tests and `tests/dispatch.rs`.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// A recording sink with injectable failures (`fail_next`: not accepted,
    /// not recorded) and drops (`drop_every`: recorded as accepted, answered
    /// `Dropped(1)`, as noded does for a stalled subscriber).
    #[derive(Default)]
    pub struct RecordingSink {
        pub sent: Mutex<Vec<(String, serde_json::Value)>>,
        pub fail_next: AtomicU64,
        /// Like `fail_next`, counting `edit.changed` frames only.
        pub fail_next_changed: AtomicU64,
        pub drop_every: std::sync::atomic::AtomicBool,
        pub wake: Notify,
    }

    impl EventSink for RecordingSink {
        fn publish<'a>(&'a self, out: &'a Outgoing) -> SinkFuture<'a> {
            Box::pin(async move {
                let failing = self
                    .fail_next
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
                    || (out.topic == TOPIC_CHANGED
                        && self
                            .fail_next_changed
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                            .is_ok());
                if failing {
                    return Err(SinkError::Failed("injected failure".into()));
                }
                let body = serde_json::from_str(&out.message.body).unwrap_or(serde_json::Value::Null);
                self.sent.lock().unwrap().push((out.topic.to_string(), body));
                self.wake.notify_waiters();
                if self.drop_every.load(Ordering::Acquire) {
                    return Err(SinkError::Dropped(1));
                }
                Ok(())
            })
        }
    }

    impl RecordingSink {
        /// Wait (deadline, no sleep loop) until `pred` holds over the sent list.
        pub async fn wait_for(&self, pred: impl Fn(&[(String, serde_json::Value)]) -> bool) -> bool {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let notified = self.wake.notified();
                if pred(&self.sent.lock().unwrap()) {
                    return true;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    return pred(&self.sent.lock().unwrap());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::RecordingSink;
    use super::*;
    use cosmix_edit_core::wire::{CloseEvent, EditEvent, Edit, KindW};

    fn edit_event(buffer: &str, rev: u64, insert: String) -> Event {
        Event::Edit(EditEvent {
            epoch: "9f2c41a7".into(),
            buffer: buffer.into(),
            rev,
            base_rev: rev - 1,
            origin: "agent:x".into(),
            lane: "agent:x".into(),
            kind: KindW::Edit,
            of: None,
            op_id: None,
            edits: vec![Edit { offset: 0, delete: 0, insert }],
            event_seq: 0,
        })
    }

    fn close_event(buffer: &str) -> Event {
        Event::Close(CloseEvent { epoch: "9f2c41a7".into(), buffer: buffer.into(), event_seq: 0 })
    }

    fn start(p: &Arc<Publisher>) -> Arc<RecordingSink> {
        let sink = Arc::new(RecordingSink::default());
        tokio::spawn(p.clone().run(sink.clone()));
        sink
    }

    #[tokio::test]
    async fn events_are_delivered_in_order_with_contiguous_seq() {
        let p = Publisher::new("9f2c41a7");
        let sink = start(&p);
        for rev in 1..=100 {
            p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", rev, "x".into()));
        }
        assert!(sink.wait_for(|s| s.len() == 100).await);
        let sent = sink.sent.lock().unwrap();
        for (i, (topic, body)) in sent.iter().enumerate() {
            assert_eq!(topic, TOPIC_CHANGED);
            assert_eq!(body["event"], "edit");
            assert_eq!(body["rev"], i as u64 + 1);
            assert_eq!(body["base_rev"], i as u64);
            assert_eq!(body["event_seq"], i as u64 + 1);
        }
    }

    #[tokio::test]
    async fn oversized_event_becomes_resync_oversized() {
        let p = Publisher::new("9f2c41a7");
        let sink = start(&p);
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 7, "y".repeat(MAX_EVENT_BYTES)));
        assert!(sink.wait_for(|s| s.len() == 1).await);
        let body = &sink.sent.lock().unwrap()[0].1;
        assert_eq!(body["event"], "resync");
        assert_eq!(body["reason"], "oversized");
        assert_eq!(body["rev"], 7);
        assert_eq!(body["buffers"], serde_json::json!(["b1_9f2c41a7"]));
    }

    #[tokio::test]
    async fn send_failure_owes_a_resync_delivered_before_later_events() {
        let p = Publisher::new("9f2c41a7");
        let sink = Arc::new(RecordingSink::default());
        // First send (the event) fails, then the first resync attempt fails too.
        sink.fail_next.store(2, Ordering::Release);
        tokio::spawn(p.clone().run(sink.clone()));
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 1, "a".into()));
        assert!(sink.wait_for(|s| !s.is_empty()).await);
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 2, "b".into()));
        assert!(sink.wait_for(|s| s.len() == 2).await);
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent[0].1["event"], "resync");
        assert_eq!(sent[0].1["reason"], "publisher_loss");
        assert_eq!(sent[0].1["buffers"], serde_json::json!(["b1_9f2c41a7"]));
        assert_eq!(sent[1].1["rev"], 2);
        assert!(sent[1].1["event_seq"].as_u64() > sent[0].1["event_seq"].as_u64());
        assert_eq!(p.loss(), 1);
    }

    #[tokio::test]
    async fn queue_budget_overflow_is_announced() {
        let p = Publisher::with_queue_cap("9f2c41a7", 400);
        // Not running yet: fill the queue past its budget.
        for i in 0..20 {
            p.event(Some("b2_9f2c41a7"), close_event(&format!("b{i}_9f2c41a7")));
        }
        assert!(p.loss() > 0);
        let sink = start(&p);
        assert!(sink.wait_for(|s| s.iter().any(|(_, b)| b["event"] == "resync")).await);
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent[0].1["event"], "resync", "the owed resync goes first");
        assert_eq!(sent[0].1["buffers"], serde_json::json!(["b2_9f2c41a7"]));
    }

    #[tokio::test]
    async fn props_frames_do_not_consume_event_seq() {
        let p = Publisher::new("9f2c41a7");
        let sink = start(&p);
        let path = PropPath::new("buffers.b1_9f2c41a7.dirty").unwrap();
        for rev in 1..=3 {
            p.props_changed(Some("b1_9f2c41a7"), &path, &false.into(), &true.into());
            p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", rev, "x".into()));
        }
        assert!(sink.wait_for(|s| s.len() == 6).await);
        let sent = sink.sent.lock().unwrap();
        let seqs: Vec<u64> = sent.iter().filter(|(t, _)| t == TOPIC_CHANGED).map(|(_, b)| b["event_seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3], "an edit.changed-only subscriber sees no gaps");
        assert_eq!(p.event_seq(), 3);
    }

    /// Fails the first `fail` publishes; on the first resync it sees, records
    /// a NEW loss for `buffer` (as if a later event dropped mid-flight).
    struct LossDuringResync {
        inner: RecordingSink,
        publisher: Mutex<Option<Arc<Publisher>>>,
        buffer: &'static str,
    }

    impl EventSink for LossDuringResync {
        fn publish<'a>(&'a self, out: &'a Outgoing) -> SinkFuture<'a> {
            Box::pin(async move {
                let is_resync = out.message.body.contains("\"event\":\"resync\"");
                if is_resync && let Some(p) = self.publisher.lock().unwrap().take() {
                    let mut state = p.state.lock().unwrap();
                    Publisher::mark_lost(&mut state, Some(&self.buffer.to_string()));
                }
                self.inner.publish(out).await
            })
        }
    }

    #[tokio::test]
    async fn a_loss_during_a_resync_stays_owed() {
        let p = Publisher::new("9f2c41a7");
        let sink = Arc::new(LossDuringResync {
            inner: RecordingSink::default(),
            publisher: Mutex::new(Some(p.clone())),
            buffer: "b1_9f2c41a7",
        });
        // The first event's send fails: resync owed for b1.
        sink.inner.fail_next.store(1, Ordering::Release);
        tokio::spawn(p.clone().run(sink.clone()));
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 1, "a".into()));
        let resyncs = |s: &[(String, serde_json::Value)]| s.iter().filter(|(_, b)| b["event"] == "resync").count();
        assert!(sink.inner.wait_for(|s| resyncs(s) >= 2).await, "the loss recorded mid-resync was erased");
    }

    #[tokio::test]
    async fn a_failed_resync_is_owed_again() {
        let p = Publisher::new("9f2c41a7");
        let sink = Arc::new(RecordingSink::default());
        // The event and the first two resync attempts fail.
        sink.fail_next.store(3, Ordering::Release);
        tokio::spawn(p.clone().run(sink.clone()));
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 1, "a".into()));
        assert!(sink.wait_for(|s| !s.is_empty()).await);
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent[0].1["event"], "resync");
        assert_eq!(sent[0].1["buffers"], serde_json::json!(["b1_9f2c41a7"]));
    }

    #[test]
    fn publish_outcome_counts_drops_as_loss() {
        assert!(publish_outcome(0, r#"{"seq":4,"delivered":2,"refused":0,"dropped":0,"eligible":2}"#, None).is_ok());
        assert!(publish_outcome(0, r#"{"seq":4,"delivered":1,"refused":1,"dropped":0,"eligible":2}"#, None).is_ok());
        let e = publish_outcome(0, r#"{"seq":5,"delivered":1,"refused":0,"dropped":1,"eligible":2}"#, None);
        assert_eq!(e, Err(SinkError::Dropped(1)));
        let refused = publish_outcome(10, r#"{"error": "payload_too_large", "limit": 1048576}"#, None);
        assert!(matches!(refused, Err(SinkError::Failed(_))), "{refused:?}");
        let refused = publish_outcome(10, "", Some("topic.publish requires 'name' header"));
        assert!(matches!(refused, Err(SinkError::Failed(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn a_stalled_subscriber_does_not_block_live_events() {
        // Every publish is accepted but dropped for one full subscriber.
        let p = Publisher::new("9f2c41a7");
        let sink = Arc::new(RecordingSink::default());
        sink.drop_every.store(true, Ordering::Release);
        tokio::spawn(p.clone().run(sink.clone()));
        for rev in 1..=5 {
            p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", rev, "x".into()));
        }
        let edits = |s: &[(String, serde_json::Value)]| s.iter().filter(|(_, b)| b["event"] == "edit").count();
        assert!(sink.wait_for(|s| edits(s) == 5).await, "live events stalled behind a resync");
        // Each loss owes at most one resync, which is not retried once noded
        // accepted it: the stream goes quiet instead of looping.
        assert!(sink.wait_for(|s| s.last().is_some_and(|(_, b)| b["event"] == "resync")).await);
        tokio::time::sleep(Duration::from_millis(3 * RESYNC_BACKOFF_BASE_MS)).await;
        let sent = sink.sent.lock().unwrap();
        let resyncs = sent.iter().filter(|(_, b)| b["event"] == "resync").count();
        assert!((1..=5).contains(&resyncs), "{resyncs} resyncs for 5 lost events");
        assert!(sent.last().is_some_and(|(_, b)| b["event"] == "resync"), "no retry loop after the last resync");
        assert_eq!(p.loss(), 5);
    }

    #[tokio::test]
    async fn a_lost_props_frame_owes_no_edit_resync() {
        let p = Publisher::new("9f2c41a7");
        let sink = Arc::new(RecordingSink::default());
        sink.fail_next.store(1, Ordering::Release);
        tokio::spawn(p.clone().run(sink.clone()));
        let path = PropPath::new("buffers.b1_9f2c41a7.dirty").unwrap();
        p.props_changed(Some("b1_9f2c41a7"), &path, &false.into(), &true.into());
        p.event(Some("b1_9f2c41a7"), edit_event("b1_9f2c41a7", 1, "x".into()));
        assert!(sink.wait_for(|s| !s.is_empty()).await);
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].1["event"], "edit", "no resync for a props loss");
        assert_eq!(p.loss(), 1);
    }

    #[tokio::test]
    async fn reconnect_publishes_resync_all() {
        let p = Publisher::new("9f2c41a7");
        let sink = start(&p);
        p.reconnected();
        assert!(sink.wait_for(|s| s.len() == 1).await);
        let body = &sink.sent.lock().unwrap()[0].1;
        assert_eq!(body["reason"], "reconnect");
        assert_eq!(body["buffers"], "all");
        assert_eq!(body["epoch"], "9f2c41a7");
    }
}
