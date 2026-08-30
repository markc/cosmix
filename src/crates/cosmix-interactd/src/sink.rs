//! Delivery abstraction — where an accepted notification is actually shown.
//!
//! The broker decides *whether* and *how* a notification is delivered; a
//! [`NotifySink`] decides *to what*. notify.v1's target is the desktop's
//! freedesktop notification daemon (`org.freedesktop.Notifications`): the
//! default [`FreedesktopSink`] draws real toasts through it via `notify-rust`'s
//! pure-Rust zbus backend (`replaces_id` coalesce + close-by-id + action
//! buttons). [`RecordingSink`] is the explicit headless diagnostic sink: it logs
//! delivery intent rather than drawing a toast and lets the Bus surface be
//! smoke-tested without a desktop session. A missing freedesktop daemon on the
//! default sink resolves the record to `failed`. Pick between the sinks with
//! `serve --sink {freedesktop|recording}`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cosmix_interaction_schema::{IconRef, NotifyHandle, NotifyRequest, Urgency};
use tokio::sync::{mpsc, oneshot};

/// Longest presentation timeout accepted from notify.v1. A request for
/// persistence (`timeout_ms = 0`) and any larger value are clamped to one hour.
pub const MAX_TIMEOUT_MS: u32 = 60 * 60 * 1_000;
const SHOW_DEADLINE: Duration = Duration::from_secs(5);
const CLOSE_DEADLINE: Duration = Duration::from_secs(5);
const LIVENESS_GRACE: Duration = Duration::from_millis(250);

/// An action/close signal observed on the same D-Bus connection that sent the
/// notification. Keeping that connection is required by GNOME Shell.
#[derive(Debug)]
pub enum SinkEvent {
    Action {
        handle: NotifyHandle,
        revision: u64,
        fd_id: u32,
        key: String,
    },
    Closed {
        handle: NotifyHandle,
        revision: u64,
        fd_id: u32,
        expired: bool,
    },
}

/// Separate bounded ingress paths let click overload shed clicks without ever
/// shedding the terminal transition that releases a record and dedupe slot.
#[derive(Debug, Clone)]
pub struct SinkEventSenders {
    actions: mpsc::Sender<SinkEvent>,
    terminals: mpsc::Sender<SinkEvent>,
}

impl SinkEventSenders {
    pub fn new(actions: mpsc::Sender<SinkEvent>, terminals: mpsc::Sender<SinkEvent>) -> Self {
        Self { actions, terminals }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneResolution {
    /// The late call did not yield a handle requiring cleanup.
    NoHandle,
    /// A late handle was captured and closed within the close deadline.
    Closed,
    /// A late handle was captured, but its close result remained ambiguous.
    CloseTimedOut,
}

/// A D-Bus Notify call crossed its deadline after it may have reached the
/// server. Resolving this tombstone waits for the late response and closes any
/// returned handle, so an untracked toast cannot survive the ambiguity.
pub struct ShowTombstone {
    resolution: Pin<Box<dyn Future<Output = TombstoneResolution> + Send>>,
}

impl ShowTombstone {
    pub(crate) fn new(
        resolution: impl Future<Output = TombstoneResolution> + Send + 'static,
    ) -> Self {
        Self {
            resolution: Box::pin(resolution),
        }
    }

    pub async fn resolve(self) -> TombstoneResolution {
        self.resolution.await
    }
}

pub enum ShowOutcome {
    Shown(Option<u32>),
    Ambiguous(ShowTombstone),
}

/// The resolved presentation handed to a sink — everything needed to draw one
/// toast, already reconciled with broker policy (the `origin` is broker-stamped,
/// the `urgency` is post-clamp). A sink never sees the raw request or the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyView {
    /// Broker-stamped, non-spoofable provenance → the freedesktop app-name.
    pub origin: String,
    pub summary: String,
    pub body: Option<String>,
    /// Effective urgency after the broker's remote clamp.
    pub urgency: Urgency,
    /// Resolved catalogue icon name (Lucide only in v1); `None` if unresolved or
    /// of an unwired class (emoji).
    pub icon: Option<String>,
    pub timeout_ms: Option<u32>,
    pub category: Option<String>,
    /// Action buttons to render, as `(key, label)` pairs. The `key` is echoed
    /// back on click; the dispatch target lives in the daemon's route table, not
    /// here (a sink renders buttons, it does not route them).
    pub actions: Vec<(String, String)>,
}

impl NotifyView {
    /// Build a view from a request plus the broker-resolved `origin`/`urgency`.
    /// Used for both a fresh notify and an in-place update (which re-uses the
    /// stored record's origin and any urgency override).
    pub fn render(origin: &str, urgency: Urgency, req: &NotifyRequest) -> Self {
        NotifyView {
            origin: origin.to_string(),
            summary: req.summary.clone(),
            body: req.body.clone(),
            urgency,
            icon: resolve_icon(req.icon.as_ref()),
            timeout_ms: req.timeout_ms.map(|ms| {
                if ms == 0 {
                    MAX_TIMEOUT_MS
                } else {
                    ms.min(MAX_TIMEOUT_MS)
                }
            }),
            category: req.category.clone(),
            actions: req
                .actions
                .iter()
                .map(|a| (a.key.clone(), a.label.clone()))
                .collect(),
        }
    }
}

/// Resolve CTK's Lucide catalogue keys to freedesktop theme icon names. These
/// are semantic theme names rather than Lucide filenames: the desktop chooses
/// the installed artwork. Emoji is rejected on the synchronous request path;
/// returning `None` here is a defensive backstop.
pub(crate) fn resolve_icon(icon: Option<&IconRef>) -> Option<String> {
    match icon {
        Some(IconRef::Lucide(name)) => lucide_theme_icon(name).map(str::to_string),
        Some(IconRef::Emoji(_)) | None => None,
    }
}

fn lucide_theme_icon(key: &str) -> Option<&'static str> {
    LUCIDE_THEME_ICONS
        .iter()
        .find_map(|(catalogue_key, theme_name)| (*catalogue_key == key).then_some(*theme_name))
}

/// Complete CTK Lucide catalogue projected to freedesktop semantic theme names.
/// The cross-workspace drift test below compares these keys with CTK's single
/// checked-in wire catalogue without linking the Bevy toolkit into interactd.
const LUCIDE_THEME_ICONS: &[(&str, &str)] = &[
    ("archive", "package-x-generic"),
    ("arrow-left", "go-previous"),
    ("arrow-right", "go-next"),
    ("arrow-up", "go-up"),
    ("chevron-down", "pan-down-symbolic"),
    ("chevron-right", "pan-end-symbolic"),
    ("chevron-up", "pan-up-symbolic"),
    ("copy", "edit-copy"),
    ("download", "document-save"),
    ("eye", "view-visible"),
    ("eye-off", "view-hidden"),
    ("file", "text-x-generic"),
    ("file-code", "text-x-script"),
    ("file-image", "image-x-generic"),
    ("file-music", "audio-x-generic"),
    ("file-text", "text-x-generic"),
    ("file-video", "video-x-generic"),
    ("folder", "folder"),
    ("folder-open", "folder-open"),
    ("grid", "view-grid-symbolic"),
    ("hard-drive", "drive-harddisk"),
    ("house", "go-home"),
    ("info", "dialog-information"),
    ("list", "view-list-symbolic"),
    ("log-out", "system-log-out"),
    ("menu", "open-menu-symbolic"),
    ("move-horizontal", "transform-move-horizontal-symbolic"),
    ("music", "audio-x-generic"),
    ("panel-left", "sidebar-show-left-symbolic"),
    ("panel-right", "sidebar-show-right-symbolic"),
    ("pin", "view-pin-symbolic"),
    ("pin-off", "view-unpin-symbolic"),
    ("refresh", "view-refresh"),
    ("search", "system-search"),
    ("trash", "user-trash"),
];

/// Why a delivery attempt failed. The bin records the notification either way
/// (as [`crate::state`] `Failed`) — a sink error never crashes the daemon.
/// [`RecordingSink`] never fails; [`FreedesktopSink`] constructs both variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// No notification daemon is present on the bus.
    NoDaemon,
    /// The backend rejected the request (transport/bus error).
    Backend(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::NoDaemon => write!(f, "no notification daemon present"),
            SinkError::Backend(e) => write!(f, "notification backend error: {e}"),
        }
    }
}

impl std::error::Error for SinkError {}

/// A delivery target for accepted notifications. `Send` so the bounded delivery
/// worker can own it on the multithreaded tokio runtime.
#[async_trait]
pub trait NotifySink: Send {
    /// Show (or, when `replaces` is `Some`, re-render in place) a notification.
    ///
    /// [`ShowOutcome::Shown`] carries the freedesktop numeric id when the sink
    /// has one. [`ShowOutcome::Ambiguous`] carries a tombstone that must resolve
    /// before the delivery is failed and its dedupe slot retired.
    async fn show(
        &mut self,
        handle: &NotifyHandle,
        revision: u64,
        view: &NotifyView,
        replaces: Option<&NotifyHandle>,
    ) -> Result<ShowOutcome, SinkError>;

    /// Close a live notification (from `interact.dismiss`).
    async fn close(&mut self, handle: &NotifyHandle) -> Result<(), SinkError>;

    /// Forget a terminal notification and drop any connection/listener retained
    /// for it. This is local bookkeeping and never performs D-Bus I/O.
    fn retire(&mut self, handle: &NotifyHandle);
}

/// Explicit headless diagnostic sink: logs delivery intent to stderr (captured
/// by journald) instead of drawing a desktop toast.
#[derive(Debug, Default)]
pub struct RecordingSink {
    /// Handles currently "shown" — lets the fallback path model close/replace
    /// without a real bus.
    shown: HashSet<String>,
}

#[async_trait]
impl NotifySink for RecordingSink {
    async fn show(
        &mut self,
        handle: &NotifyHandle,
        _revision: u64,
        view: &NotifyView,
        replaces: Option<&NotifyHandle>,
    ) -> Result<ShowOutcome, SinkError> {
        match replaces {
            Some(prev) => eprintln!(
                "cosmix-interactd: [notify] replace {} <- {} origin={} urgency={:?} actions={} summary={:?}",
                handle.as_str(),
                prev.as_str(),
                view.origin,
                view.urgency,
                view.actions.len(),
                view.summary,
            ),
            None => eprintln!(
                "cosmix-interactd: [notify] show {} origin={} urgency={:?} actions={} summary={:?}",
                handle.as_str(),
                view.origin,
                view.urgency,
                view.actions.len(),
                view.summary,
            ),
        }
        self.shown.insert(handle.0.clone());
        // No bus, no numeric id — the daemon spawns no action listener.
        Ok(ShowOutcome::Shown(None))
    }

    async fn close(&mut self, handle: &NotifyHandle) -> Result<(), SinkError> {
        eprintln!("cosmix-interactd: [notify] close {}", handle.as_str());
        self.shown.remove(&handle.0);
        Ok(())
    }

    fn retire(&mut self, handle: &NotifyHandle) {
        self.shown.remove(handle.as_str());
    }
}

/// Delivers real desktop toasts through the freedesktop notification daemon
/// (`org.freedesktop.Notifications`) via notify-rust's pure-Rust zbus backend.
///
/// It keeps the live [`notify_rust::NotificationHandle`] per opaque handle so a
/// coalescing replace can reuse the freedesktop `replaces_id` (in-place update)
/// and [`close`](NotifySink::close) can retract the toast. Action *buttons* are
/// rendered here (via `notify_rust::Notification::action`). The sink subscribes
/// with `NotificationHandle::wait_for_action_async` on the handle's own sending
/// connection, which works with GNOME's directed signals as well as Plasma's.
#[derive(Debug)]
pub struct FreedesktopSink {
    /// Live notifications by opaque handle → the notify-rust handle (which owns
    /// the zbus connection and the freedesktop numeric id).
    live: Arc<Mutex<HashMap<String, LiveNotification>>>,
    events: SinkEventSenders,
}

#[derive(Debug)]
struct LiveNotification {
    revision: u64,
    handle: Arc<notify_rust::NotificationHandle>,
    cancel: Option<oneshot::Sender<()>>,
}

impl LiveNotification {
    fn cancel_listener(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl Drop for LiveNotification {
    fn drop(&mut self) {
        self.cancel_listener();
    }
}

impl FreedesktopSink {
    pub fn new(events: SinkEventSenders) -> Self {
        Self {
            live: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    fn build(view: &NotifyView) -> notify_rust::Notification {
        let mut n = notify_rust::Notification::new();
        // app-name is the broker-stamped, non-spoofable origin.
        n.appname(&view.origin);
        n.summary(&escape_markup(&view.summary));
        if let Some(body) = &view.body {
            n.body(&escape_markup(body));
        }
        if let Some(icon) = &view.icon {
            // Best-effort: a freedesktop *theme* icon name. Lucide catalogue keys
            // mostly won't resolve until a Lucide→freedesktop icon bridge (or
            // shipping Lucide SVGs as image paths) lands; an unknown name simply
            // shows no icon rather than erroring.
            n.icon(icon);
        }
        if let Some(category) = &view.category {
            n.hint(notify_rust::Hint::Category(category.clone()));
        }
        for (key, label) in &view.actions {
            // `key` is echoed back verbatim on click and matched against the
            // route table; `label` is the button text.
            n.action(key, label);
        }
        n.urgency(map_urgency(view.urgency));
        n.timeout(map_timeout(view.timeout_ms));
        n
    }
}

fn escape_markup(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[async_trait]
impl NotifySink for FreedesktopSink {
    async fn show(
        &mut self,
        handle: &NotifyHandle,
        revision: u64,
        view: &NotifyView,
        replaces: Option<&NotifyHandle>,
    ) -> Result<ShowOutcome, SinkError> {
        let mut notif = Self::build(view);
        // Coalesce: replace the prior toast in place via its freedesktop id
        // (`replaces_id`). More reliable on Plasma than notify-rust's
        // handle.update(), which that server only amends. The replacement gets
        // its own listener on the new sending connection; insertion below
        // cancels and drops the superseded handle and connection.
        if let Some(prev) = replaces
            && let Some(existing) = self
                .live
                .lock()
                .expect("notification handles poisoned")
                .get(prev.as_str())
        {
            notif.id(existing.handle.id());
        }
        // The D-Bus call continues in an owned task if the five-second deadline
        // expires. Keeping the receiver alive lets a tombstone capture and close
        // a late successful handle instead of leaving an untracked toast.
        let (result_tx, mut result_rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = notif.show_async().await.map_err(map_err);
            let _ = result_tx.send(result);
        });
        let result = match tokio::time::timeout(SHOW_DEADLINE, &mut result_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SinkError::Backend(
                "notification delivery task ended without a result".into(),
            )),
            Err(_) => {
                return Ok(ShowOutcome::Ambiguous(ShowTombstone::new(async move {
                    if let Ok(Ok(handle)) = result_rx.await {
                        if close_with_deadline(handle.close_async(), CLOSE_DEADLINE).await {
                            TombstoneResolution::Closed
                        } else {
                            TombstoneResolution::CloseTimedOut
                        }
                    } else {
                        TombstoneResolution::NoHandle
                    }
                })));
            }
        };
        match result {
            Ok(shown) => {
                let id = shown.id();
                let shown = Arc::new(shown);
                let listener_handle = Arc::clone(&shown);
                let closer_handle = Arc::clone(&shown);
                let events = self.events.clone();
                let event_handle = handle.clone();
                let cleanup_handle = handle.clone();
                let live = Arc::clone(&self.live);
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let liveness_deadline = listener_liveness_deadline(view.timeout_ms);
                // Publish the generation before the listener starts so an
                // immediate close/action cannot retire an empty map and then be
                // followed by a leaked insertion.
                self.live
                    .lock()
                    .expect("notification handles poisoned")
                    .insert(
                        handle.0.clone(),
                        LiveNotification {
                            revision,
                            handle: shown,
                            cancel: Some(cancel_tx),
                        },
                    );
                tokio::spawn(async move {
                    let (response_tx, response_rx) = oneshot::channel();
                    let wait = listener_handle.wait_for_action_async(move |response| {
                        use notify_rust::NotificationResponse;
                        let response = match response {
                            NotificationResponse::Action(key) => {
                                ListenerResponse::Action(key.clone())
                            }
                            NotificationResponse::Default | NotificationResponse::Reply(_) => {
                                ListenerResponse::Action("default".to_string())
                            }
                            NotificationResponse::Closed(reason) => ListenerResponse::Closed {
                                expired: *reason == notify_rust::CloseReason::Expired,
                            },
                        };
                        let _ = response_tx.send(response);
                    });
                    let exit =
                        await_listener_exit(cancel_rx, response_rx, wait, liveness_deadline).await;
                    match exit {
                        ListenerExit::Cancelled => {}
                        ListenerExit::Response(response) => {
                            forward_response(&events, &event_handle, revision, id, response).await;
                        }
                        ListenerExit::EndedWithoutSignal => {
                            eprintln!(
                                "cosmix-interactd: [notify] desktop signal stream ended for {}; retiring notification",
                                event_handle.as_str()
                            );
                            emit_terminal(
                                &events.terminals,
                                SinkEvent::Closed {
                                    handle: event_handle.clone(),
                                    revision,
                                    fd_id: id,
                                    expired: false,
                                },
                            )
                            .await;
                        }
                        ListenerExit::LivenessDeadline => {
                            if !close_with_deadline(closer_handle.close_async(), CLOSE_DEADLINE)
                                .await
                            {
                                eprintln!(
                                    "cosmix-interactd: [notify] liveness close timed out for {}; retiring ambiguous desktop handle",
                                    event_handle.as_str()
                                );
                            }
                            emit_terminal(
                                &events.terminals,
                                SinkEvent::Closed {
                                    handle: event_handle.clone(),
                                    revision,
                                    fd_id: id,
                                    expired: true,
                                },
                            )
                            .await;
                        }
                    }
                    let mut live = live.lock().expect("notification handles poisoned");
                    if live
                        .get(cleanup_handle.as_str())
                        .is_some_and(|entry| entry.revision == revision)
                    {
                        live.remove(cleanup_handle.as_str());
                    }
                });
                // A successful replacement gets a new sending connection and a
                // listener attached to that same connection. Dropping the old
                // entry cancels its listener and releases its connection.
                Ok(ShowOutcome::Shown(Some(id)))
            }
            Err(e) => Err(e),
        }
    }

    async fn close(&mut self, handle: &NotifyHandle) -> Result<(), SinkError> {
        let shown = self
            .live
            .lock()
            .expect("notification handles poisoned")
            .remove(handle.as_str());
        if let Some(mut shown) = shown {
            shown.cancel_listener();
            shown.handle.close_async().await;
        }
        Ok(())
    }

    fn retire(&mut self, handle: &NotifyHandle) {
        self.live
            .lock()
            .expect("notification handles poisoned")
            .remove(handle.as_str());
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ListenerResponse {
    Action(String),
    Closed { expired: bool },
}

#[derive(Debug, PartialEq, Eq)]
enum ListenerExit {
    Cancelled,
    Response(ListenerResponse),
    EndedWithoutSignal,
    LivenessDeadline,
}

async fn await_listener_exit<F>(
    mut cancel: oneshot::Receiver<()>,
    response: oneshot::Receiver<ListenerResponse>,
    wait: F,
    deadline: Duration,
) -> ListenerExit
where
    F: Future<Output = ()>,
{
    tokio::pin!(wait);
    tokio::pin!(response);
    tokio::select! {
        _ = &mut cancel => ListenerExit::Cancelled,
        result = &mut response => match result {
            Ok(response) => ListenerExit::Response(response),
            Err(_) => ListenerExit::EndedWithoutSignal,
        },
        _ = &mut wait => match response.await {
            Ok(response) => ListenerExit::Response(response),
            Err(_) => ListenerExit::EndedWithoutSignal,
        },
        _ = tokio::time::sleep(deadline) => ListenerExit::LivenessDeadline,
    }
}

fn listener_liveness_deadline(timeout_ms: Option<u32>) -> Duration {
    Duration::from_millis(u64::from(timeout_ms.unwrap_or(MAX_TIMEOUT_MS))) + LIVENESS_GRACE
}

async fn close_with_deadline<F>(close: F, deadline: Duration) -> bool
where
    F: Future<Output = ()>,
{
    tokio::time::timeout(deadline, close).await.is_ok()
}

async fn forward_response(
    events: &SinkEventSenders,
    handle: &NotifyHandle,
    revision: u64,
    fd_id: u32,
    response: ListenerResponse,
) {
    match response {
        ListenerResponse::Action(key) => {
            let action = SinkEvent::Action {
                handle: handle.clone(),
                revision,
                fd_id,
                key,
            };
            if let Err(error) = events.actions.try_send(action) {
                eprintln!(
                    "cosmix-interactd: [notify] desktop action queue full/closed; dropped click and forced terminal retirement: {error}"
                );
                emit_terminal(
                    &events.terminals,
                    SinkEvent::Closed {
                        handle: handle.clone(),
                        revision,
                        fd_id,
                        expired: false,
                    },
                )
                .await;
            }
        }
        ListenerResponse::Closed { expired } => {
            emit_terminal(
                &events.terminals,
                SinkEvent::Closed {
                    handle: handle.clone(),
                    revision,
                    fd_id,
                    expired,
                },
            )
            .await;
        }
    }
}

/// Terminal transitions use an awaited, dedicated bounded channel. Backpressure
/// may delay cleanup, but it cannot discard the transition or strand props.
async fn emit_terminal(events: &mpsc::Sender<SinkEvent>, event: SinkEvent) {
    if let Err(error) = events.send(event).await {
        eprintln!(
            "cosmix-interactd: [notify] terminal signal dispatcher stopped during shutdown: {error}"
        );
    }
}

fn map_urgency(u: Urgency) -> notify_rust::Urgency {
    match u {
        Urgency::Low => notify_rust::Urgency::Low,
        Urgency::Normal => notify_rust::Urgency::Normal,
        Urgency::Critical => notify_rust::Urgency::Critical,
    }
}

/// `None` → daemon default; explicit values have already been clamped by
/// [`NotifyView::render`].
fn map_timeout(timeout_ms: Option<u32>) -> notify_rust::Timeout {
    match timeout_ms {
        None => notify_rust::Timeout::Default,
        Some(ms) => notify_rust::Timeout::Milliseconds(ms),
    }
}

/// Classify a notify-rust error. Its `kind` is private, so a missing daemon is
/// detected from the Display text (best-effort) — everything else is `Backend`.
fn map_err(e: notify_rust::error::Error) -> SinkError {
    let msg = e.to_string();
    let low = msg.to_ascii_lowercase();
    if low.contains("serviceunknown")
        || low.contains("was not provided")
        || low.contains("namehasnoowner")
    {
        SinkError::NoDaemon
    } else {
        SinkError::Backend(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_timeouts_are_clamped_to_one_hour() {
        for (requested, expected) in [
            (Some(0), Some(MAX_TIMEOUT_MS)),
            (Some(MAX_TIMEOUT_MS + 1), Some(MAX_TIMEOUT_MS)),
            (Some(12_345), Some(12_345)),
            (None, None),
        ] {
            let mut request = NotifyRequest::new("test");
            request.timeout_ms = requested;
            let view = NotifyView::render("musicd", Urgency::Normal, &request);
            assert_eq!(view.timeout_ms, expected, "requested={requested:?}");
        }
    }

    #[test]
    fn lucide_catalogue_resolves_to_freedesktop_theme_names() {
        assert_eq!(
            resolve_icon(Some(&IconRef::Lucide("music".into()))).as_deref(),
            Some("audio-x-generic")
        );
        assert_eq!(
            resolve_icon(Some(&IconRef::Lucide("refresh".into()))).as_deref(),
            Some("view-refresh")
        );
        assert_eq!(
            resolve_icon(Some(&IconRef::Lucide("not-real".into()))),
            None
        );
        assert_eq!(resolve_icon(Some(&IconRef::Emoji("wave".into()))), None);
    }

    #[test]
    fn freedesktop_mapping_matches_ctk_shared_lucide_catalogue() {
        let shared: Vec<_> = include_str!("lucide-catalogue.txt")
            .lines()
            .filter(|line| !line.is_empty())
            .collect();
        let mapped: Vec<_> = LUCIDE_THEME_ICONS.iter().map(|(key, _)| *key).collect();
        assert_eq!(mapped, shared);
    }

    #[test]
    fn freedesktop_summary_and_body_escape_markup() {
        let mut request = NotifyRequest::new("Build <done> & checked");
        request.body =
            Some(r#"Open <a href="https://example.invalid">report</a> & inspect"#.into());
        let view = NotifyView::render("musicd", Urgency::Normal, &request);
        let notification = FreedesktopSink::build(&view);
        assert_eq!(notification.summary, "Build &lt;done&gt; &amp; checked");
        assert_eq!(
            notification.body,
            r#"Open &lt;a href="https://example.invalid"&gt;report&lt;/a&gt; &amp; inspect"#
        );
    }

    #[tokio::test]
    async fn recording_sink_releases_terminal_handles() {
        let mut sink = RecordingSink::default();
        let handle = NotifyHandle("n1".into());
        let view = NotifyView::render("musicd", Urgency::Normal, &NotifyRequest::new("test"));
        let result = sink.show(&handle, 1, &view, None).await.unwrap();
        assert!(matches!(result, ShowOutcome::Shown(None)));
        assert!(sink.shown.contains("n1"));
        sink.retire(&handle);
        assert!(!sink.shown.contains("n1"));
    }

    #[tokio::test]
    async fn readiness_gap_is_bounded_by_one_shot_liveness_deadline() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        let wait = async move {
            let _keep_callback_alive = response_tx;
            std::future::pending::<()>().await;
        };
        let exit =
            await_listener_exit(cancel_rx, response_rx, wait, Duration::from_millis(1)).await;
        assert_eq!(exit, ListenerExit::LivenessDeadline);
    }

    #[tokio::test]
    async fn click_overload_drops_click_but_enqueues_mandatory_terminal() {
        let (action_tx, mut action_rx) = mpsc::channel(1);
        let (terminal_tx, mut terminal_rx) = mpsc::channel(1);
        let senders = SinkEventSenders::new(action_tx.clone(), terminal_tx);
        action_tx
            .send(SinkEvent::Action {
                handle: NotifyHandle("n-old".into()),
                revision: 1,
                fd_id: 1,
                key: "old".into(),
            })
            .await
            .unwrap();

        forward_response(
            &senders,
            &NotifyHandle("n-new".into()),
            2,
            22,
            ListenerResponse::Action("new".into()),
        )
        .await;

        assert!(matches!(
            action_rx.recv().await,
            Some(SinkEvent::Action { handle, .. }) if handle.as_str() == "n-old"
        ));
        assert!(matches!(
            terminal_rx.recv().await,
            Some(SinkEvent::Closed { handle, revision: 2, fd_id: 22, .. })
                if handle.as_str() == "n-new"
        ));
    }

    #[tokio::test]
    async fn listener_exit_without_callback_is_terminal() {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        drop(response_tx);
        let exit =
            await_listener_exit(cancel_rx, response_rx, async {}, Duration::from_secs(1)).await;
        assert_eq!(exit, ListenerExit::EndedWithoutSignal);
    }

    #[tokio::test]
    async fn close_deadline_releases_a_hung_tombstone_operation() {
        assert!(!close_with_deadline(std::future::pending::<()>(), Duration::from_millis(1)).await);
    }
}
