//! Source-side drag-and-drop state and own-window echo correlation.
//!
//! This module contains no Wayland objects. [`crate::transport`] owns those
//! objects and drives this machine from callbacks, which keeps the lifecycle,
//! nonce retirement, and writer rules testable without a compositor.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::mime::{MimeError, encode_uri_list};
use crate::types::{ActionMask, DataTransferId, DndAction, SourceId};

pub const URI_LIST_MIME: &str = "text/uri-list";
pub const UTF8_TEXT_MIME: &str = "text/plain;charset=utf-8";
pub const NONCE_MIME_PREFIX: &str = "application/x-cosmix-dnd-";

const NONCE_BYTES: usize = 16;
const MAX_TOMBSTONES: usize = 64;

/// Wall-clock lifecycle limits for one outgoing source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendConfig {
    /// A silently ignored `start_drag` produces no source callback.
    pub start_deadline: Duration,
    /// Maximum silence after the compositor has acknowledged the source.
    pub active_deadline: Duration,
    /// Maximum wait after `dnd_drop_performed` or `dnd_finished`.
    pub finish_deadline: Duration,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            start_deadline: Duration::from_secs(10),
            active_deadline: Duration::from_secs(60),
            finish_deadline: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendConfigError {
    ZeroStartDeadline,
    ZeroActiveDeadline,
    ZeroFinishDeadline,
    UnrepresentableStartDeadline,
    UnrepresentableActiveDeadline,
    UnrepresentableFinishDeadline,
}

impl SendConfig {
    pub fn validate(self, now: Instant) -> Result<Self, SendConfigError> {
        validate_duration(
            now,
            self.start_deadline,
            SendConfigError::ZeroStartDeadline,
            SendConfigError::UnrepresentableStartDeadline,
        )?;
        validate_duration(
            now,
            self.active_deadline,
            SendConfigError::ZeroActiveDeadline,
            SendConfigError::UnrepresentableActiveDeadline,
        )?;
        validate_duration(
            now,
            self.finish_deadline,
            SendConfigError::ZeroFinishDeadline,
            SendConfigError::UnrepresentableFinishDeadline,
        )?;
        Ok(self)
    }
}

fn validate_duration(
    now: Instant,
    duration: Duration,
    zero: SendConfigError,
    unrepresentable: SendConfigError,
) -> Result<(), SendConfigError> {
    if duration.is_zero() {
        return Err(zero);
    }
    if now.checked_add(duration).is_none() {
        return Err(unrepresentable);
    }
    Ok(())
}

fn deadline_after(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration)
        .unwrap_or_else(|| furthest_representable_instant(now, duration))
}

fn furthest_representable_instant(now: Instant, upper_bound: Duration) -> Instant {
    let mut valid_nanos = 0_u128;
    let mut invalid_nanos = upper_bound.as_nanos();
    while valid_nanos + 1 < invalid_nanos {
        let candidate_nanos = valid_nanos + (invalid_nanos - valid_nanos) / 2;
        let candidate = Duration::new(
            (candidate_nanos / 1_000_000_000) as u64,
            (candidate_nanos % 1_000_000_000) as u32,
        );
        if now.checked_add(candidate).is_some() {
            valid_nanos = candidate_nanos;
        } else {
            invalid_nanos = candidate_nanos;
        }
    }
    now.checked_add(Duration::new(
        (valid_nanos / 1_000_000_000) as u64,
        (valid_nanos % 1_000_000_000) as u32,
    ))
    .expect("zero duration is always representable")
}

/// Validated real-path payload owned by one outgoing transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingPayload {
    paths: Vec<PathBuf>,
    uri_list: Arc<[u8]>,
    plain_text: Arc<[u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutgoingPayloadError {
    Empty,
    NotAbsolute(PathBuf),
    NotReal {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    Uri(MimeError),
}

impl OutgoingPayload {
    /// Captures a real-path export payload at the `Dragging -> Exporting`
    /// handoff. Every path must exist at that point; virtual resources are not
    /// part of the v1 contract.
    pub fn from_paths(paths: Vec<PathBuf>) -> Result<Self, OutgoingPayloadError> {
        if paths.is_empty() {
            return Err(OutgoingPayloadError::Empty);
        }
        for path in &paths {
            if !path.is_absolute() {
                return Err(OutgoingPayloadError::NotAbsolute(path.clone()));
            }
            std::fs::symlink_metadata(path).map_err(|error| OutgoingPayloadError::NotReal {
                path: path.clone(),
                kind: error.kind(),
            })?;
        }
        let uri_list = encode_uri_list(&paths)
            .map_err(OutgoingPayloadError::Uri)?
            .into_bytes()
            .into();
        let mut plain_text = Vec::new();
        for path in &paths {
            plain_text.extend_from_slice(path.to_string_lossy().as_bytes());
            plain_text.push(b'\n');
        }
        Ok(Self {
            paths,
            uri_list,
            plain_text: plain_text.into(),
        })
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    fn bytes_for(&self, mime: &str, nonce: &TransferNonce) -> Option<Arc<[u8]>> {
        match mime {
            URI_LIST_MIME => Some(Arc::clone(&self.uri_list)),
            UTF8_TEXT_MIME => Some(Arc::clone(&self.plain_text)),
            candidate if candidate == nonce.mime_type() => {
                Some(Arc::<[u8]>::from(nonce.token().as_bytes()))
            }
            _ => None,
        }
    }
}

/// Token-safe per-transfer nonce carried in a private MIME essence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransferNonce(String);

impl TransferNonce {
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; NONCE_BYTES];
        getrandom::fill(&mut bytes)?;
        let mut token = String::with_capacity(NONCE_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
        }
        Ok(Self(token))
    }

    #[cfg(test)]
    fn for_test(token: &str) -> Self {
        assert!(valid_token(token));
        Self(token.into())
    }

    pub fn token(&self) -> &str {
        &self.0
    }

    pub fn mime_type(&self) -> String {
        format!("{NONCE_MIME_PREFIX}{}", self.0)
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        let token = mime.strip_prefix(NONCE_MIME_PREFIX)?;
        valid_token(token).then(|| Self(token.into()))
    }
}

fn valid_token(token: &str) -> bool {
    token.len() == NONCE_BYTES * 2
        && token
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceLookupError {
    Unknown,
    Tombstoned,
    AlreadyAttached,
    IncomingMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EchoCorrelation {
    pub outgoing: DataTransferId,
    pub incoming: DataTransferId,
    pub source: SourceId,
}

#[derive(Clone, Debug)]
struct NonceEntry {
    outgoing: DataTransferId,
    source: SourceId,
    incoming: Option<DataTransferId>,
    outgoing_terminal: bool,
    incoming_terminal: bool,
}

/// Single-live-entry nonce registry with bounded retired tombstones.
#[derive(Debug, Default)]
pub struct NonceRegistry {
    live: BTreeMap<TransferNonce, NonceEntry>,
    tombstones: BTreeSet<TransferNonce>,
    tombstone_order: VecDeque<TransferNonce>,
}

impl NonceRegistry {
    /// Registration is deliberately separate from `start_drag`; the transport
    /// calls this first so an immediate own-window Enter can correlate.
    pub fn register(
        &mut self,
        nonce: TransferNonce,
        outgoing: DataTransferId,
        source: SourceId,
    ) -> Result<(), SendError> {
        if !self.live.is_empty() {
            return Err(SendError::OutgoingAlreadyActive);
        }
        self.tombstones.remove(&nonce);
        self.tombstone_order.retain(|candidate| candidate != &nonce);
        self.live.insert(
            nonce,
            NonceEntry {
                outgoing,
                source,
                incoming: None,
                outgoing_terminal: false,
                incoming_terminal: false,
            },
        );
        Ok(())
    }

    pub(crate) fn register_before_start<T>(
        &mut self,
        nonce: TransferNonce,
        outgoing: DataTransferId,
        source: SourceId,
        start: impl FnOnce() -> T,
    ) -> Result<T, SendError> {
        self.register(nonce, outgoing, source)?;
        Ok(start())
    }

    pub(crate) fn attach_offered_echo(
        &mut self,
        offered_mimes: &[String],
        outgoing: DataTransferId,
        incoming: DataTransferId,
    ) -> Result<EchoCorrelation, NonceLookupError> {
        let nonce = offered_mimes
            .iter()
            .find_map(|mime| TransferNonce::from_mime(mime))
            .ok_or(NonceLookupError::Unknown)?;
        // Validate before mutating. `attach_echo` writes `entry.incoming`, and
        // the caller rejects a mismatch without ever driving `incoming_terminal`
        // for it — an entry attached to a transfer that will never terminate
        // stays live forever, wedging every future drag on
        // `OutgoingAlreadyActive`.
        match self.live.get(&nonce) {
            // `AlreadyAttached` keeps its precedence over the outgoing
            // comparison: a second echo for a nonce is that, whichever transfer
            // the caller thought it belonged to. A *terminal* incoming half is
            // not "already attached" — see `attach_echo` for why re-entry after
            // a non-dropping leave must be allowed to correlate again.
            Some(entry) if entry.incoming.is_some() && !entry.incoming_terminal => {
                return Err(NonceLookupError::AlreadyAttached);
            }
            Some(entry) if entry.outgoing == outgoing => {}
            Some(_) => return Err(NonceLookupError::Unknown),
            None => {
                return Err(if self.tombstones.contains(&nonce) {
                    NonceLookupError::Tombstoned
                } else {
                    NonceLookupError::Unknown
                });
            }
        }
        self.attach_echo(&nonce, incoming)
    }

    pub fn attach_echo(
        &mut self,
        nonce: &TransferNonce,
        incoming: DataTransferId,
    ) -> Result<EchoCorrelation, NonceLookupError> {
        let Some(entry) = self.live.get_mut(nonce) else {
            return Err(if self.tombstones.contains(nonce) {
                NonceLookupError::Tombstoned
            } else {
                NonceLookupError::Unknown
            });
        };
        // A *live* incoming half owns the correlation and a second echo for it is
        // a protocol error. A terminal one does not: leaving the source window
        // without dropping ends the incoming transfer while the outgoing drag
        // continues, and the entry stays live precisely because the drag has not
        // finished. Re-entering the same window then produces a fresh, perfectly
        // legitimate echo — refusing it lost the drop through both routes, since
        // the in-app session was already consumed by the export.
        if entry.incoming.is_some() && !entry.incoming_terminal {
            return Err(NonceLookupError::AlreadyAttached);
        }
        entry.incoming = Some(incoming);
        entry.incoming_terminal = false;
        Ok(EchoCorrelation {
            outgoing: entry.outgoing,
            incoming,
            source: entry.source,
        })
    }

    pub fn correlation_for_incoming(&self, incoming: DataTransferId) -> Option<EchoCorrelation> {
        self.live.values().find_map(|entry| {
            (entry.incoming == Some(incoming)).then_some(EchoCorrelation {
                outgoing: entry.outgoing,
                incoming,
                source: entry.source,
            })
        })
    }

    /// No echo means immediate retirement. With an echo, the entry remains
    /// live until that incoming half is terminal too.
    pub fn outgoing_terminal(&mut self, outgoing: DataTransferId) {
        let nonce = self
            .live
            .iter()
            .find_map(|(nonce, entry)| (entry.outgoing == outgoing).then(|| nonce.clone()));
        let Some(nonce) = nonce else {
            return;
        };
        let retire = {
            let entry = self.live.get_mut(&nonce).expect("found above");
            entry.outgoing_terminal = true;
            entry.incoming.is_none() || entry.incoming_terminal
        };
        if retire {
            self.retire(nonce);
        }
    }

    pub fn incoming_terminal(&mut self, incoming: DataTransferId) -> Result<(), NonceLookupError> {
        let nonce = self
            .live
            .iter()
            .find_map(|(nonce, entry)| (entry.incoming == Some(incoming)).then(|| nonce.clone()));
        let Some(nonce) = nonce else {
            return Err(NonceLookupError::IncomingMismatch);
        };
        let retire = {
            let entry = self.live.get_mut(&nonce).expect("found above");
            entry.incoming_terminal = true;
            entry.outgoing_terminal
        };
        if retire {
            self.retire(nonce);
        }
        Ok(())
    }

    pub fn lookup(&self, nonce: &TransferNonce) -> Result<DataTransferId, NonceLookupError> {
        self.live
            .get(nonce)
            .map(|entry| entry.outgoing)
            .ok_or_else(|| {
                if self.tombstones.contains(nonce) {
                    NonceLookupError::Tombstoned
                } else {
                    NonceLookupError::Unknown
                }
            })
    }

    pub fn live_nonce_for(&self, outgoing: DataTransferId) -> Option<&TransferNonce> {
        self.live
            .iter()
            .find_map(|(nonce, entry)| (entry.outgoing == outgoing).then_some(nonce))
    }

    fn retire(&mut self, nonce: TransferNonce) {
        self.live.remove(&nonce);
        if self.tombstones.insert(nonce.clone()) {
            self.tombstone_order.push_back(nonce);
        }
        while self.tombstone_order.len() > MAX_TOMBSTONES {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutgoingPhase {
    Starting,
    Active,
    DropPerformed,
    Finishing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutgoingTerminalReason {
    Completed,
    CompositorCancelled,
    StartIgnored,
    ActiveDeadlineExpired,
    FinishDeadlineExpired,
    WriterSpawnFailed,
    WriterFailed,
    UnsupportedMime,
    SourceProxyDead,
    SeatRemoved,
    /// The seat driving this drag lost its pointer capability while the drag
    /// was live. The seat itself survives, so its data device and any incoming
    /// transfer on it are untouched — only the pointer this drag was started
    /// from is gone.
    ///
    /// Distinct from [`Self::SeatRemoved`] because a caller may need to tell
    /// them apart, but it shares the property that matters to one: the press
    /// this drag escalated can no longer be released, because the pointer that
    /// held it no longer exists. A caller holding button-release-gated state
    /// for the gesture must drop it on both.
    PointerCapabilityLost,
    WindowTeardown,
    WaylandConnectionLost,
    QueueOverflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutgoingEvent {
    StartRequested {
        transfer_id: DataTransferId,
        source: SourceId,
        nonce_mime: String,
    },
    ActionChanged {
        transfer_id: DataTransferId,
        action: Option<DndAction>,
    },
    DropPerformed {
        transfer_id: DataTransferId,
    },
    DataSent {
        transfer_id: DataTransferId,
        mime_type: String,
    },
    Terminal {
        transfer_id: DataTransferId,
        reason: OutgoingTerminalReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    OutgoingAlreadyActive,
    NoHeldGrab,
    /// More than one pointer-capable seat is present, so the held grab cannot
    /// be attributed to the gesture that asked to escalate.
    ///
    /// A toolkit above this crate sees one logical mouse — winit collapses
    /// every Wayland seat into a single pointer — while the grab belongs to
    /// exactly one seat, whichever pressed first. Starting the drag anyway
    /// would run it on that seat's serial and cursor while consuming the other
    /// seat's gesture, losing its drop. Refusing is the only honest answer
    /// available without seat-aware input plumbing.
    AmbiguousSeat,
    WrongSeat,
    InvalidActions,
    InvalidPayload(OutgoingPayloadError),
    RandomNonce,
    StaleTransfer {
        active: DataTransferId,
        received: DataTransferId,
    },
    NoActiveTransfer,
    InvalidTransition,
    /// Undrained terminal events have reached [`MAX_PENDING_TERMINALS`].
    ///
    /// A terminal must never be dropped, so a consumer that starts drags
    /// without draining is refused rather than silently losing the record of
    /// how its earlier transfers ended.
    UndrainedTerminals,
}

/// Ceiling on reserved-but-undrained outgoing terminal events.
///
/// One drag ends before the next begins, so a consumer draining at any sane
/// cadence never approaches this; reaching it means the consumer has stopped
/// draining entirely.
pub const MAX_PENDING_TERMINALS: usize = 8;

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SendError {}

/// State of one single-use `wl_data_source`.
#[derive(Debug)]
pub(crate) struct OutgoingTransfer {
    id: DataTransferId,
    payload: OutgoingPayload,
    nonce: TransferNonce,
    phase: OutgoingPhase,
    deadline: Instant,
    config: SendConfig,
    live_writers: usize,
    compositor_finished: bool,
    terminal: Option<OutgoingTerminalReason>,
}

impl OutgoingTransfer {
    pub(crate) fn new(
        id: DataTransferId,
        payload: OutgoingPayload,
        nonce: TransferNonce,
        actions: ActionMask,
        now: Instant,
        config: SendConfig,
    ) -> Result<Self, SendError> {
        if actions.is_empty() {
            return Err(SendError::InvalidActions);
        }
        Ok(Self {
            id,
            payload,
            nonce,
            phase: OutgoingPhase::Starting,
            deadline: deadline_after(now, config.start_deadline),
            config,
            live_writers: 0,
            compositor_finished: false,
            terminal: None,
        })
    }

    pub(crate) fn id(&self) -> DataTransferId {
        self.id
    }

    pub(crate) fn phase(&self) -> OutgoingPhase {
        self.phase
    }

    pub(crate) fn terminal(&self) -> Option<OutgoingTerminalReason> {
        self.terminal
    }

    fn expire_first(&mut self, now: Instant) -> bool {
        if self.terminal.is_some() {
            return true;
        }
        if now < self.deadline {
            return false;
        }
        let reason = match self.phase {
            OutgoingPhase::Starting => OutgoingTerminalReason::StartIgnored,
            OutgoingPhase::Active => OutgoingTerminalReason::ActiveDeadlineExpired,
            OutgoingPhase::DropPerformed | OutgoingPhase::Finishing => {
                OutgoingTerminalReason::FinishDeadlineExpired
            }
        };
        self.terminal = Some(reason);
        true
    }

    fn acknowledge(&mut self, now: Instant) -> Result<(), SendError> {
        if self.expire_first(now) {
            return Err(SendError::InvalidTransition);
        }
        if self.phase == OutgoingPhase::Starting {
            self.phase = OutgoingPhase::Active;
        }
        self.deadline = deadline_after(
            now,
            if matches!(
                self.phase,
                OutgoingPhase::DropPerformed | OutgoingPhase::Finishing
            ) {
                self.config.finish_deadline
            } else {
                self.config.active_deadline
            },
        );
        Ok(())
    }

    pub(crate) fn accepted(&mut self, now: Instant) -> Result<(), SendError> {
        self.acknowledge(now)
    }

    pub(crate) fn action(&mut self, now: Instant) -> Result<(), SendError> {
        self.acknowledge(now)
    }

    pub(crate) fn begin_send(
        &mut self,
        mime: &str,
        now: Instant,
    ) -> Result<Arc<[u8]>, OutgoingTerminalReason> {
        if self.acknowledge(now).is_err() {
            return Err(self
                .terminal
                .unwrap_or(OutgoingTerminalReason::UnsupportedMime));
        }
        let Some(bytes) = self.payload.bytes_for(mime, &self.nonce) else {
            self.terminal = Some(OutgoingTerminalReason::UnsupportedMime);
            return Err(OutgoingTerminalReason::UnsupportedMime);
        };
        self.live_writers += 1;
        Ok(bytes)
    }

    pub(crate) fn writer_spawn_failed(&mut self, now: Instant) {
        if self.expire_first(now) {
            return;
        }
        self.live_writers = self.live_writers.saturating_sub(1);
        self.terminal = Some(OutgoingTerminalReason::WriterSpawnFailed);
    }

    pub(crate) fn writer_finished(
        &mut self,
        success: bool,
        now: Instant,
    ) -> Option<OutgoingTerminalReason> {
        if self.expire_first(now) {
            return self.terminal;
        }
        self.live_writers = self.live_writers.saturating_sub(1);
        if !success {
            self.terminal = Some(OutgoingTerminalReason::WriterFailed);
        } else if self.compositor_finished && self.live_writers == 0 {
            self.terminal = Some(OutgoingTerminalReason::Completed);
        }
        self.terminal
    }

    pub(crate) fn dropped(&mut self, now: Instant) -> Result<(), SendError> {
        self.acknowledge(now)?;
        self.phase = OutgoingPhase::DropPerformed;
        self.deadline = deadline_after(now, self.config.finish_deadline);
        Ok(())
    }

    pub(crate) fn finished(&mut self, now: Instant) -> Option<OutgoingTerminalReason> {
        if self.expire_first(now) {
            return self.terminal;
        }
        self.compositor_finished = true;
        if self.live_writers == 0 {
            self.terminal = Some(OutgoingTerminalReason::Completed);
        } else {
            self.phase = OutgoingPhase::Finishing;
            self.deadline = deadline_after(now, self.config.finish_deadline);
        }
        self.terminal
    }

    pub(crate) fn cancel(&mut self, reason: OutgoingTerminalReason, now: Instant) {
        if self.expire_first(now) {
            return;
        }
        self.terminal = Some(reason);
    }

    pub(crate) fn check_deadline(&mut self, now: Instant) -> Option<OutgoingTerminalReason> {
        self.expire_first(now);
        self.terminal
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::mime::parse_uri_list;

    fn nonce() -> TransferNonce {
        TransferNonce::for_test("0123456789abcdef0123456789abcdef")
    }

    #[test]
    fn an_echo_offered_against_a_foreign_outgoing_attaches_nothing() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(7))
            .unwrap();
        assert_eq!(
            registry.attach_offered_echo(
                &[nonce().mime_type()],
                DataTransferId(9),
                DataTransferId(2)
            ),
            Err(NonceLookupError::Unknown)
        );
        // The rejected offer is never driven through `incoming_terminal`, so an
        // attachment here would keep the entry live forever and every later
        // drag would fail with `OutgoingAlreadyActive`.
        assert_eq!(registry.correlation_for_incoming(DataTransferId(2)), None);
        registry.outgoing_terminal(DataTransferId(1));
        assert!(
            registry
                .register(nonce(), DataTransferId(3), SourceId(8))
                .is_ok()
        );
    }

    #[test]
    fn a_second_echo_reports_already_attached_whichever_outgoing_is_named() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(7))
            .unwrap();
        registry
            .attach_offered_echo(&[nonce().mime_type()], DataTransferId(1), DataTransferId(2))
            .unwrap();
        for outgoing in [DataTransferId(1), DataTransferId(9)] {
            assert_eq!(
                registry.attach_offered_echo(&[nonce().mime_type()], outgoing, DataTransferId(3)),
                Err(NonceLookupError::AlreadyAttached)
            );
        }
        // The first echo is still the attached one.
        assert_eq!(
            registry
                .correlation_for_incoming(DataTransferId(2))
                .map(|correlation| correlation.outgoing),
            Some(DataTransferId(1))
        );
    }

    fn transfer(now: Instant) -> OutgoingTransfer {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "cosmix-wl-dnd-send-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"x").unwrap();
        let payload = OutgoingPayload::from_paths(vec![path.clone()]).unwrap();
        let transfer = OutgoingTransfer::new(
            DataTransferId(1),
            payload,
            nonce(),
            ActionMask::COPY | ActionMask::MOVE,
            now,
            SendConfig::default(),
        )
        .unwrap();
        fs::remove_file(path).unwrap();
        transfer
    }

    #[test]
    fn outgoing_all_actions_is_valid_but_an_empty_mask_is_not() {
        let path =
            std::env::temp_dir().join(format!("cosmix-wl-dnd-send-actions-{}", std::process::id()));
        fs::write(&path, b"x").unwrap();
        let payload = OutgoingPayload::from_paths(vec![path.clone()]).unwrap();
        let now = Instant::now();

        assert!(
            OutgoingTransfer::new(
                DataTransferId(1),
                payload.clone(),
                nonce(),
                ActionMask::ALL,
                now,
                SendConfig::default(),
            )
            .is_ok()
        );
        assert!(matches!(
            OutgoingTransfer::new(
                DataTransferId(2),
                payload,
                nonce(),
                ActionMask::NONE,
                now,
                SendConfig::default(),
            ),
            Err(SendError::InvalidActions)
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn uri_generation_round_trips_with_crlf_and_percent_encoding() {
        let root = std::env::temp_dir().join(format!("cosmix wl dnd #{}", std::process::id()));
        fs::write(&root, b"x").unwrap();
        let payload = OutgoingPayload::from_paths(vec![root.clone()]).unwrap();
        let body = std::str::from_utf8(&payload.uri_list).unwrap();
        assert!(body.ends_with("\r\n"));
        assert!(body.contains("%20"));
        assert!(body.contains("%23"));
        assert_eq!(parse_uri_list(body).unwrap(), vec![root.clone()]);
        fs::remove_file(root).unwrap();
    }

    #[test]
    fn outgoing_without_echo_retires_immediately() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        registry.outgoing_terminal(DataTransferId(1));
        assert_eq!(registry.lookup(&nonce()), Err(NonceLookupError::Tombstoned));
    }

    #[test]
    fn echo_entry_retires_only_after_both_halves_are_terminal() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        assert_eq!(
            registry.attach_echo(&nonce(), DataTransferId(3)).unwrap(),
            EchoCorrelation {
                outgoing: DataTransferId(1),
                incoming: DataTransferId(3),
                source: SourceId(2),
            }
        );
        registry.outgoing_terminal(DataTransferId(1));
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
        registry.incoming_terminal(DataTransferId(3)).unwrap();
        assert_eq!(registry.lookup(&nonce()), Err(NonceLookupError::Tombstoned));
    }

    #[test]
    fn incoming_first_still_waits_for_outgoing_terminal() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        registry.attach_echo(&nonce(), DataTransferId(3)).unwrap();
        registry.incoming_terminal(DataTransferId(3)).unwrap();
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
        registry.outgoing_terminal(DataTransferId(1));
        assert_eq!(registry.lookup(&nonce()), Err(NonceLookupError::Tombstoned));
    }

    /// Leaving the source window without dropping ends the *incoming* half while
    /// the outgoing drag continues, so the entry stays live. Re-entering must be
    /// allowed to correlate again: refusing it lost the drop through both routes,
    /// because the in-app session was already consumed by the export.
    #[test]
    fn re_entry_after_a_non_dropping_leave_correlates_again() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        registry
            .attach_offered_echo(&[nonce().mime_type()], DataTransferId(1), DataTransferId(3))
            .unwrap();
        // Leave without dropping. The outgoing drag has not finished, so the
        // entry survives.
        registry.incoming_terminal(DataTransferId(3)).unwrap();
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
        // Re-enter: a fresh incoming id, the same nonce.
        assert_eq!(
            registry
                .attach_offered_echo(&[nonce().mime_type()], DataTransferId(1), DataTransferId(4))
                .unwrap(),
            EchoCorrelation {
                outgoing: DataTransferId(1),
                incoming: DataTransferId(4),
                source: SourceId(2),
            }
        );
        // The correlation moved wholesale — the stale incoming half no longer
        // resolves, so a late event for it cannot be routed into the live drag.
        assert_eq!(registry.correlation_for_incoming(DataTransferId(3)), None);
        assert_eq!(
            registry
                .correlation_for_incoming(DataTransferId(4))
                .map(|correlation| correlation.outgoing),
            Some(DataTransferId(1))
        );
        // And the re-attached half is live again: retirement waits for it.
        registry.outgoing_terminal(DataTransferId(1));
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
        registry.incoming_terminal(DataTransferId(4)).unwrap();
        assert_eq!(registry.lookup(&nonce()), Err(NonceLookupError::Tombstoned));
    }

    #[test]
    fn late_and_unknown_nonces_are_distinct_rejections() {
        let unknown = TransferNonce::for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        registry.outgoing_terminal(DataTransferId(1));
        assert_eq!(
            registry.attach_echo(&nonce(), DataTransferId(4)),
            Err(NonceLookupError::Tombstoned)
        );
        assert_eq!(
            registry.attach_echo(&unknown, DataTransferId(4)),
            Err(NonceLookupError::Unknown)
        );
    }

    #[test]
    fn second_outgoing_registration_is_structurally_rejected() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        let other = TransferNonce::for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(
            registry.register(other, DataTransferId(3), SourceId(4)),
            Err(SendError::OutgoingAlreadyActive)
        );
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
    }

    #[test]
    fn nonce_is_live_before_start_and_an_immediate_echo_correlates() {
        let mut registry = NonceRegistry::default();
        let offered = vec![nonce().mime_type()];
        let correlation = registry
            .register_before_start(nonce(), DataTransferId(1), SourceId(2), || {
                // Models an echo queued immediately by start_drag.
            })
            .and_then(|()| {
                registry
                    .attach_offered_echo(&offered, DataTransferId(1), DataTransferId(3))
                    .map_err(|_| SendError::InvalidTransition)
            })
            .unwrap();
        assert_eq!(correlation.source, SourceId(2));
    }

    #[test]
    fn registration_failure_never_invokes_the_start_request() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        let other = TransferNonce::for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut started = false;

        assert_eq!(
            registry
                .register_before_start(other, DataTransferId(3), SourceId(4), || started = true,),
            Err(SendError::OutgoingAlreadyActive)
        );
        assert!(!started);
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
    }

    #[test]
    fn non_echo_offer_during_export_is_rejected_without_touching_live_entry() {
        let mut registry = NonceRegistry::default();
        registry
            .register(nonce(), DataTransferId(1), SourceId(2))
            .unwrap();
        assert_eq!(
            registry.attach_offered_echo(
                &[URI_LIST_MIME.into(), UTF8_TEXT_MIME.into()],
                DataTransferId(1),
                DataTransferId(3),
            ),
            Err(NonceLookupError::Unknown)
        );
        assert_eq!(registry.lookup(&nonce()), Ok(DataTransferId(1)));
    }

    #[test]
    fn ignored_start_expires_without_a_source_callback() {
        let now = Instant::now();
        let mut transfer = transfer(now);
        assert_eq!(
            transfer.check_deadline(now + SendConfig::default().start_deadline),
            Some(OutgoingTerminalReason::StartIgnored)
        );
    }

    #[test]
    fn every_live_source_phase_has_a_wall_clock_terminal_deadline() {
        let config = SendConfig::default();

        let now = Instant::now();
        let mut active = transfer(now);
        active.accepted(now).unwrap();
        assert_eq!(
            active.check_deadline(now + config.active_deadline),
            Some(OutgoingTerminalReason::ActiveDeadlineExpired)
        );

        let now = Instant::now();
        let mut dropped = transfer(now);
        dropped.dropped(now).unwrap();
        assert_eq!(
            dropped.check_deadline(now + config.finish_deadline),
            Some(OutgoingTerminalReason::FinishDeadlineExpired)
        );

        let now = Instant::now();
        let mut finishing = transfer(now);
        finishing.begin_send(URI_LIST_MIME, now).unwrap();
        assert_eq!(finishing.finished(now), None);
        assert_eq!(finishing.phase(), OutgoingPhase::Finishing);
        assert_eq!(
            finishing.check_deadline(now + config.finish_deadline),
            Some(OutgoingTerminalReason::FinishDeadlineExpired)
        );
    }

    #[test]
    fn drop_does_not_prevent_late_send_or_finish() {
        let now = Instant::now();
        let mut transfer = transfer(now);
        transfer.dropped(now + Duration::from_millis(1)).unwrap();
        assert_eq!(transfer.phase(), OutgoingPhase::DropPerformed);
        assert!(
            transfer
                .begin_send(URI_LIST_MIME, now + Duration::from_millis(2))
                .is_ok()
        );
        assert_eq!(transfer.finished(now + Duration::from_millis(3)), None);
        assert_eq!(
            transfer.writer_finished(true, now + Duration::from_millis(4)),
            Some(OutgoingTerminalReason::Completed)
        );
    }

    #[test]
    fn expired_deadline_cannot_be_rearmed_by_a_late_callback() {
        let now = Instant::now();
        let mut transfer = transfer(now);
        let late = now + SendConfig::default().start_deadline;
        assert_eq!(transfer.accepted(late), Err(SendError::InvalidTransition));
        assert_eq!(
            transfer.terminal(),
            Some(OutgoingTerminalReason::StartIgnored)
        );
    }

    #[test]
    fn unsupported_send_is_terminal() {
        let now = Instant::now();
        let mut transfer = transfer(now);
        assert_eq!(
            transfer.begin_send("application/octet-stream", now),
            Err(OutgoingTerminalReason::UnsupportedMime)
        );
        assert_eq!(
            transfer.terminal(),
            Some(OutgoingTerminalReason::UnsupportedMime)
        );
    }
}
