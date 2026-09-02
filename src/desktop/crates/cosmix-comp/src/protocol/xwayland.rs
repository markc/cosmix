//! XWayland X-1: one supervised rootless Xwayland generation, one XWM, and
//! normal managed X11 toplevels riding the existing scene/buffer/SSD paths.
//!
//! Scope is deliberately X-1 (design:
//! `~/.ctl/_doc/2026-09-02-arc5-xwayland-x1-design.md`). The refusals are as
//! load-bearing as the features and live in this file so they stay visible:
//!
//! - **Selection bridging is refused.** `allow_selection_access` is always
//!   `false`, `send_selection` is a defensive no-op that drops the fd, and
//!   X selection changes are only logged. X-2 owns bridging — and must
//!   revisit the `draining_wms` teardown proof first: the wm's
//!   selection-transfer calloop sources call `xwm_state` and are never
//!   unregistered, so today they are safe only because these refusals stop
//!   them from ever existing (see `draining_wms`).
//! - **Override-redirect windows are recorded and ignored.** They get no
//!   scene record, no SSD, no focus. X-2 must render them; until then X11
//!   menus/tooltips/dropdowns are absent by design.
//! - **The X11 client scale is held at 1** and RandR primary-output changes
//!   are only logged. X-3 owns scaling.
//! - **Readiness is the one-shot `XWaylandEvent::Ready` display-fd event.**
//!   There is no readiness poll loop. The only timers are a single 60 s
//!   one-shot restart backstop after a crash and a single five-minute
//!   one-shot stability window that restores the retry credit — both events,
//!   never periodic ticks (no-poll law).
//!
//! The renderer stays ignorant of X11: an associated X11 window commits
//! Wayland buffers through the common `commit_new_buffer` path and reaches
//! the renderer as the same `SurfaceSceneSnapshot`/`SurfaceFrame` events as
//! any xdg toplevel.

use super::*;
use smithay::{
    reexports::calloop::RegistrationToken,
    wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, WmWindowProperty, X11Window, XwmId},
    },
};
use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _, path::Path, process::Stdio};

/// One-shot restart backstop after an unexpected XWayland death. This is a
/// bounded recovery delay, not a maintenance interval (no-poll law: any timer
/// is a ≥ 60 s backstop with a stated reason).
const XWAYLAND_RETRY_DELAY: Duration = Duration::from_secs(60);
/// A retried generation must survive this long before the single retry
/// credit is restored; prevents a crash loop from consuming one credit per
/// minute forever.
const XWAYLAND_STABILITY_WINDOW: Duration = Duration::from_secs(300);

/// Pure association/map phase for one normal X11 window. This is the single
/// eligibility authority: a window may present only when it is both
/// associated with a committed `wl_surface` and its map was granted.
/// Transitions are idempotent because Smithay documents that either half of
/// the association can arrive first and callbacks can repeat.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct X11WindowPhase {
    pub(super) associated: bool,
    pub(super) map_requested: bool,
    pub(super) map_notified: bool,
    /// Sticky: comp has written a real granted rectangle for this window at
    /// least once. Unlike `map_requested` this survives `on_unmapped`,
    /// because `granted_geometry` is the window's position memory across
    /// unmap→remap (`x11_map_window_request` reuses it to keep the remap
    /// idempotent) and remains a genuine grant — the surface-swap hand-over
    /// gates on THIS, not on `map_requested`, so an unmapped window's moved
    /// or maximized rect crosses the swap instead of being discarded as raw
    /// hints.
    pub(super) ever_granted: bool,
}

impl X11WindowPhase {
    pub(super) fn on_associated(&mut self) -> bool {
        let first = !self.associated;
        self.associated = true;
        first
    }

    pub(super) fn on_map_requested(&mut self) -> bool {
        let first = !self.map_requested;
        self.map_requested = true;
        self.ever_granted = true;
        first
    }

    pub(super) fn on_map_notify(&mut self) -> bool {
        let first = !self.map_notified;
        self.map_notified = true;
        first
    }

    pub(super) fn on_unmapped(&mut self) {
        self.map_requested = false;
        self.map_notified = false;
    }

    /// Presentation eligibility: association AND a granted map. Buffer
    /// content is the third gate and lives in the surface record.
    pub(super) fn eligible(&self) -> bool {
        self.associated && self.map_requested
    }
}

/// `_MOTIF_WM_HINTS` interpretation, pinned by tests before any gate trusts
/// Firefox CSD behaviour. The decorations *flag* is bit 1 of the flags field;
/// when that flag is set and the decorations field is zero the client refuses
/// window-manager decorations (Smithay surfaces this as
/// `X11Surface::is_decorated()`, "client-side decorated"). Flag unset, or a
/// non-zero decorations value, accepts server-side decorations.
// Consumed by the deterministic tests only: production decoration reads
// `X11Surface::is_decorated()`, never this copy. The pin is two-sided:
// `x11_motif_decoration_interpretation_is_pinned` fixes this copy's truth
// table, and `x11_motif_hints_drive_the_live_decoration_path` drives the
// REAL `is_decorated()` path (via the vendored `set_motif_hints_offline`)
// and asserts it agrees with this copy — so an inversion in a Smithay bump
// fails a test instead of double-decorating Firefox.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn motif_refuses_server_decorations(flags: u32, decorations: u32) -> bool {
    const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
    (flags & MWM_HINTS_DECORATIONS) != 0 && decorations == 0
}

/// Single retry credit for XWayland generations. A failure spends the
/// credit; only a five-minute stable generation restores it; a failure with
/// no credit stays failed until the compositor restarts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum XwaylandRetryDecision {
    Retry,
    StayFailed,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct XwaylandRetryPolicy {
    credit: bool,
}

impl XwaylandRetryPolicy {
    pub(super) fn new() -> Self {
        Self { credit: true }
    }

    pub(super) fn on_failure(&mut self) -> XwaylandRetryDecision {
        if mem::take(&mut self.credit) {
            XwaylandRetryDecision::Retry
        } else {
            XwaylandRetryDecision::StayFailed
        }
    }

    pub(super) fn on_stable(&mut self) {
        self.credit = true;
    }

    #[cfg(test)]
    pub(super) fn has_credit(&self) -> bool {
        self.credit
    }
}

/// Metadata for a normal X11 window between `new_window` and association.
/// The `X11Surface` itself is not retained here: every XWM callback carries
/// it, and association receives the authoritative copy.
#[derive(Debug, Default)]
pub(super) struct PendingX11Window {
    pub(super) phase: X11WindowPhase,
    /// Content rectangle granted at map time (global logical coordinates).
    pub(super) granted_geometry: Option<Rectangle<i32, Logical>>,
}

#[derive(Debug)]
pub(super) enum XwaylandLifecycle {
    /// Feature compiled but no generation running (before first spawn, after
    /// permanent failure with no retry credit, or after orderly shutdown).
    Inert,
    Starting {
        token: RegistrationToken,
        client: Client,
    },
    // `Ready` does not retain the generation's Wayland client for teardown
    // (`Starting` holds one only to hand to `start_wm`): the client gets
    // exactly ONE deliberate kill, and `Drop for XWayland`
    // (vendor/smithay/src/xwayland/xserver.rs:330-336) already delivers
    // `kill_client(client, ConnectionClosed)` when teardown removes `token`.
    // Upstream's `XWaylandClientData::disconnected` panicked on a second
    // kill (`take().unwrap()`); the vendored copy is idempotent now, but
    // only as defense-in-depth for killers comp does not control (the
    // explicit-sync fault path kills by client key, XWayland-blind) — never
    // a licence to double-kill here. The vendor drop still closes the
    // `serial_commit_hook` route into `xwm_state` by construction: it lands
    // either synchronously inside `remove(token)` or at the end of the
    // current calloop iteration (the dispatcher `Rc` cloned for a running
    // callback defers it), and a commit hook can only run inside the wayland
    // source's own `process_events` — a later iteration either way.
    Ready {
        token: RegistrationToken,
        wm: Box<X11Wm>,
        stability_timer: Option<RegistrationToken>,
    },
    RetryArmed {
        timer: RegistrationToken,
    },
    Failed,
}

/// Per-compositor XWayland runtime state, held by `WaylandState`.
pub(super) struct XwaylandRuntime {
    pub(super) generation: u64,
    pub(super) lifecycle: XwaylandLifecycle,
    pub(super) retry: XwaylandRetryPolicy,
    /// The Wayland socket name this compositor serves; keys the per-socket
    /// `DISPLAY` descriptor so nested compositors never race one global file.
    pub(super) socket_name: String,
    pub(super) descriptor_path: Option<PathBuf>,
    pub(super) pending_windows: HashMap<X11Window, PendingX11Window>,
    /// XID → associated `wl_surface` object for normal managed windows.
    pub(super) surfaces_by_xid: HashMap<X11Window, ObjectId>,
    pub(super) xids_by_object: HashMap<ObjectId, X11Window>,
    /// Override-redirect XIDs, recorded for diagnostics/cleanup only.
    /// Deliberately never managed, mapped, or decorated in X-1.
    pub(super) override_redirect_windows: HashSet<X11Window>,
    /// XWMs of torn-down generations, kept alive until their X11 event
    /// source delivers calloop's `Closed` (surfaced as `disconnected`).
    ///
    /// `X11Wm::start_wm` inserts the X11 channel source and discards the
    /// `RegistrationToken`, and `Drop for X11Wm` does not unregister it — so
    /// after a failure teardown that source stays registered and its buffered
    /// events still reach `XwmHandler::xwm_state` with the dead generation's
    /// id. Dropping the wm at teardown would make that an `unreachable!`
    /// panic. Instead the wm drains here: `xwm_state` answers for it, the
    /// gated `XwmHandler` delegates ignore its events, and `disconnected`
    /// drops it — `Closed` is provably the channel's final event (mpsc
    /// reports `Disconnected` only once the queue is drained) and the source
    /// removes itself after delivering it, so nothing can ask for the id
    /// again. The other entry point to `xwm_state`, the per-surface
    /// `serial_commit_hook`, dies with teardown too: removing the XWayland
    /// source token runs `Drop for XWayland`, which kills the generation's
    /// Wayland client, and a killed client dispatches no further requests,
    /// buffered ones included.
    ///
    /// This exhaustiveness argument additionally depends on X-1's selection
    /// refusals: the wm's selection-transfer calloop sources (the incoming
    /// and outgoing `Generic`s in the vendored `XWmSelection`) also capture
    /// the `XwmId` and call `xwm_state`, and `Drop for X11Wm` does NOT
    /// remove them — they can only never exist because
    /// `allow_selection_access` is always `false` and comp never calls
    /// `send_selection`. X-2's clipboard bridge must revisit this drain
    /// before relaxing either refusal.
    pub(super) draining_wms: Vec<X11Wm>,
    /// Keyboard refocus debt: the XID's window held the keyboard when a
    /// surface swap withdrew the displaced record, and the replacement is
    /// not presentable yet. Paid (focus handed back) the moment the XID's
    /// record maps — but ONLY if the keyboard still sits exactly where the
    /// arming fallback parked it; a focus change in the interim is a
    /// deliberate choice by a human or another surface, and yanking the
    /// keyboard back mid-interaction would send keystrokes to the wrong
    /// window, which is worse than the focus drop the debt repairs.
    /// Cleared if the window or generation dies first. Event-driven — set
    /// at the swap, consumed at map, no polling.
    pub(super) refocus: Option<PendingX11Refocus>,
    pub(super) shutting_down: bool,
}

/// See `XwaylandRuntime::refocus`.
#[derive(Debug)]
pub(super) struct PendingX11Refocus {
    pub(super) xid: X11Window,
    /// What the arming fallback landed the keyboard on (`None` = nothing).
    /// Consumption compares this against the CURRENT focus: equal means the
    /// keyboard is still where comp's own fallback put it and nobody has
    /// made a choice since; different means someone has, and the debt is
    /// dropped silently. Comparing against bare `None` would not work — the
    /// fallback frequently lands on a real window, and that landing is not
    /// a deliberate choice by anyone.
    pub(super) fallback: Option<SeatFocusTarget>,
}

impl XwaylandRuntime {
    pub(super) fn new(socket_name: String) -> Self {
        Self {
            generation: 0,
            lifecycle: XwaylandLifecycle::Inert,
            retry: XwaylandRetryPolicy::new(),
            socket_name,
            descriptor_path: None,
            pending_windows: HashMap::new(),
            surfaces_by_xid: HashMap::new(),
            xids_by_object: HashMap::new(),
            override_redirect_windows: HashSet::new(),
            draining_wms: Vec::new(),
            refocus: None,
            shutting_down: false,
        }
    }

    pub(super) fn wm(&mut self) -> Option<&mut X11Wm> {
        match &mut self.lifecycle {
            XwaylandLifecycle::Ready { wm, .. } => Some(wm),
            _ => None,
        }
    }
}

fn xwayland_descriptor_path(socket_name: &str) -> Option<PathBuf> {
    let dir = env::var_os("XDG_RUNTIME_DIR")?;
    Some(
        PathBuf::from(dir)
            .join("cosmix-comp")
            .join(format!("{socket_name}.xwayland.env")),
    )
}

/// Atomically publish the per-socket `DISPLAY` descriptor: write a mode-0600
/// sibling temporary, fsync, rename over the target. Consumers read it once
/// after the compositor's Wayland socket is ready and pass `DISPLAY`
/// explicitly to each X client; nothing global is set.
fn publish_xwayland_descriptor(
    path: &Path,
    display: u32,
    generation: u64,
) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("descriptor path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(format!("DISPLAY=:{display}\nGENERATION={generation}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        // The failed publication fails the generation; do not also leak the
        // half-written sibling temporary.
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_xwayland_descriptor(path: Option<&PathBuf>) {
    if let Some(path) = path
        && let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "failed to remove XWayland DISPLAY descriptor");
    }
}

/// Map Smithay's X11 resize edge onto the xdg edge enum the interactive
/// pointer machinery already speaks. The enum is used protocol-neutrally as
/// "which edges move"; no xdg object is involved for X11 targets.
fn xdg_edge_for_x11(edge: X11ResizeEdge) -> xdg_toplevel::ResizeEdge {
    match edge {
        X11ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
        X11ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
        X11ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
        X11ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        X11ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
        X11ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
        X11ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
        X11ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
    }
}

/// Clamp a client-requested content size by WM_NORMAL_HINTS min/max and the
/// usable output, with the shared sensible default as fallback for windows
/// that request nothing usable.
pub(super) fn clamp_x11_content_size(
    requested: (i32, i32),
    min_hint: Option<(i32, i32)>,
    max_hint: Option<(i32, i32)>,
    fallback: (i32, i32),
    usable: (i32, i32),
) -> (i32, i32) {
    // Per-axis: a degenerate axis falls back alone — a 1000×1 request keeps
    // its legitimate width and only the height is replaced.
    let requested = (
        if requested.0 > 1 {
            requested.0
        } else {
            fallback.0
        },
        if requested.1 > 1 {
            requested.1
        } else {
            fallback.1
        },
    );
    let min = min_hint.unwrap_or((1, 1));
    let max = max_hint.unwrap_or((i32::MAX, i32::MAX));
    let upper = (
        usable.0.max(1).min(max.0.max(1)),
        usable.1.max(1).min(max.1.max(1)),
    );
    (
        requested.0.clamp(min.0.max(1).min(upper.0), upper.0),
        requested.1.clamp(min.1.max(1).min(upper.1), upper.1),
    )
}

impl WaylandState {
    /// Spawn a new XWayland generation. Called at protocol startup and by the
    /// one-shot retry backstop; never from a periodic tick.
    pub(super) fn start_xwayland(&mut self) {
        if self.xwayland.shutting_down {
            return;
        }
        if matches!(
            self.xwayland.lifecycle,
            XwaylandLifecycle::Starting { .. } | XwaylandLifecycle::Ready { .. }
        ) {
            tracing::debug!("ignored XWayland start while a generation is live");
            return;
        }
        self.xwayland.generation = self.xwayland.generation.saturating_add(1);
        let generation = self.xwayland.generation;
        match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            // Inherit the compositor's stdout/stderr so Xwayland output lands
            // in the same journal as the comp log.
            Stdio::inherit(),
            Stdio::inherit(),
            |_| (),
        ) {
            Ok((xwayland, client)) => {
                let token = self.capture_loop_handle.insert_source(
                    xwayland,
                    move |event, (), state: &mut WaylandState| match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => state.xwayland_ready(generation, x11_socket, display_number),
                        XWaylandEvent::Error => {
                            state.fail_xwayland_generation(generation, "startup error");
                        }
                    },
                );
                match token {
                    Ok(token) => {
                        tracing::info!(
                            generation,
                            "XWayland spawned; waiting for display-fd readiness"
                        );
                        self.xwayland.lifecycle = XwaylandLifecycle::Starting { token, client };
                    }
                    Err(error) => {
                        tracing::warn!(generation, %error, "failed to insert XWayland event source");
                        self.fail_xwayland_generation(generation, "event-source insertion failed");
                    }
                }
            }
            Err(error) => {
                // Missing /usr/bin/Xwayland or no free display: native
                // Wayland stays fully usable; this is a degraded capability,
                // not a runtime failure.
                tracing::warn!(generation, %error, "XWayland unavailable; native Wayland unaffected");
                self.fail_xwayland_generation(generation, "spawn failed");
            }
        }
    }

    fn xwayland_ready(
        &mut self,
        generation: u64,
        x11_socket: std::os::unix::net::UnixStream,
        display_number: u32,
    ) {
        if self.xwayland.generation != generation {
            tracing::warn!(
                generation,
                current = self.xwayland.generation,
                "ignored stale XWayland readiness"
            );
            return;
        }
        let XwaylandLifecycle::Starting { token, client } =
            mem::replace(&mut self.xwayland.lifecycle, XwaylandLifecycle::Failed)
        else {
            tracing::warn!(generation, "ignored XWayland readiness outside Starting");
            return;
        };
        let client_for_retry = client.clone();
        match X11Wm::start_wm(self.capture_loop_handle.clone(), x11_socket, client) {
            Ok(wm) => {
                // `DISPLAY` is published only after the XWM owns WM_S0:
                // Smithay accepts no X clients before `start_wm` completes.
                // Publication failure fails the generation: an Xwayland
                // running on a display nothing can discover is not "ready",
                // and logging it as such buries the fault.
                let path = xwayland_descriptor_path(&self.xwayland.socket_name);
                let published = match path.as_deref() {
                    Some(path) => {
                        match publish_xwayland_descriptor(path, display_number, generation) {
                            Ok(()) => true,
                            Err(error) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    %error,
                                    "failed to publish XWayland DISPLAY descriptor"
                                );
                                false
                            }
                        }
                    }
                    None => {
                        tracing::warn!(
                            "XDG_RUNTIME_DIR unset; XWayland DISPLAY descriptor not published"
                        );
                        false
                    }
                };
                if !published {
                    // Hand the wm to the Ready teardown arm so the source is
                    // removed (its `Drop` kills the client) and the wm drains.
                    self.xwayland.lifecycle = XwaylandLifecycle::Ready {
                        token,
                        wm: Box::new(wm),
                        stability_timer: None,
                    };
                    self.fail_xwayland_generation(generation, "descriptor publication failed");
                    return;
                }
                self.xwayland.descriptor_path = path;
                let stability_timer = match self.capture_loop_handle.insert_source(
                    Timer::from_duration(XWAYLAND_STABILITY_WINDOW),
                    move |_, (), state: &mut WaylandState| {
                        state.xwayland_stable(generation);
                        TimeoutAction::Drop
                    },
                ) {
                    Ok(timer) => Some(timer),
                    Err(error) => {
                        // The generation still runs; only the credit restore
                        // is forfeited, and silently forfeiting it hides why
                        // a later crash stays failed.
                        tracing::warn!(
                            generation,
                            %error,
                            "failed to arm XWayland stability window; retry credit will not be restored this generation"
                        );
                        None
                    }
                };
                tracing::info!(
                    generation,
                    display = display_number,
                    socket = %self.xwayland.socket_name,
                    "XWayland ready; XWM started"
                );
                self.xwayland.lifecycle = XwaylandLifecycle::Ready {
                    token,
                    wm: Box::new(wm),
                    stability_timer,
                };
            }
            Err(error) => {
                tracing::warn!(generation, %error, "failed to start XWM");
                self.xwayland.lifecycle = XwaylandLifecycle::Starting {
                    token,
                    client: client_for_retry,
                };
                self.fail_xwayland_generation(generation, "XWM start failed");
            }
        }
    }

    fn xwayland_stable(&mut self, generation: u64) {
        if self.xwayland.generation != generation {
            return;
        }
        if let XwaylandLifecycle::Ready {
            stability_timer, ..
        } = &mut self.xwayland.lifecycle
        {
            *stability_timer = None;
            self.xwayland.retry.on_stable();
            tracing::debug!(
                generation,
                "XWayland generation stable; retry credit restored"
            );
        }
    }

    /// Generation-guarded, duplicate-capable failure teardown. Removes the
    /// descriptor, destroys every X11 scene record through the existing
    /// destruction path, drops the XWayland source and XWM, and arms at most
    /// one 60 s one-shot retry.
    pub(super) fn fail_xwayland_generation(&mut self, generation: u64, reason: &'static str) {
        if self.xwayland.generation != generation {
            return;
        }
        if matches!(
            self.xwayland.lifecycle,
            XwaylandLifecycle::Failed | XwaylandLifecycle::RetryArmed { .. }
        ) {
            return;
        }
        tracing::warn!(generation, reason, "XWayland generation failed");
        self.teardown_xwayland_generation();
        if self.xwayland.shutting_down {
            self.xwayland.lifecycle = XwaylandLifecycle::Failed;
            return;
        }
        match self.xwayland.retry.on_failure() {
            XwaylandRetryDecision::Retry => {
                let timer = self.capture_loop_handle.insert_source(
                    Timer::from_duration(XWAYLAND_RETRY_DELAY),
                    move |_, (), state: &mut WaylandState| {
                        state.retry_xwayland();
                        TimeoutAction::Drop
                    },
                );
                match timer {
                    Ok(timer) => {
                        tracing::info!(
                            generation,
                            delay_secs = XWAYLAND_RETRY_DELAY.as_secs(),
                            "armed one-shot XWayland restart backstop"
                        );
                        self.xwayland.lifecycle = XwaylandLifecycle::RetryArmed { timer };
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to arm XWayland retry timer");
                        self.xwayland.lifecycle = XwaylandLifecycle::Failed;
                    }
                }
            }
            XwaylandRetryDecision::StayFailed => {
                tracing::warn!(
                    generation,
                    "XWayland retry credit exhausted; staying failed until compositor restart"
                );
                self.xwayland.lifecycle = XwaylandLifecycle::Failed;
            }
        }
    }

    fn retry_xwayland(&mut self) {
        if !matches!(
            self.xwayland.lifecycle,
            XwaylandLifecycle::RetryArmed { .. }
        ) {
            return;
        }
        self.xwayland.lifecycle = XwaylandLifecycle::Inert;
        self.start_xwayland();
    }

    /// Shared teardown for failure and orderly shutdown: descriptor first,
    /// then the source/XWM/client, then X records.
    fn teardown_xwayland_generation(&mut self) {
        remove_xwayland_descriptor(self.xwayland.descriptor_path.as_ref());
        self.xwayland.descriptor_path = None;
        let lifecycle = mem::replace(&mut self.xwayland.lifecycle, XwaylandLifecycle::Inert);
        match lifecycle {
            // Removing `token` drops the `XWayland` source, whose `Drop`
            // kills the generation's Wayland client — the only deliberate
            // kill it receives (see the `XwaylandLifecycle::Ready` comment;
            // the vendored `disconnected` tolerates a stray second kill,
            // teardown still never issues one).
            XwaylandLifecycle::Starting { token, client: _ } => {
                self.capture_loop_handle.remove(token);
            }
            XwaylandLifecycle::Ready {
                token,
                wm,
                stability_timer,
            } => {
                self.capture_loop_handle.remove(token);
                if let Some(timer) = stability_timer {
                    self.capture_loop_handle.remove(timer);
                }
                // The X11 event source Smithay registered for this wm has no
                // token we can remove; the wm drains until that source
                // delivers `Closed` (see `draining_wms`). Dropping it here
                // would leave a registered callback that panics in
                // `xwm_state`.
                self.xwayland.draining_wms.push(*wm);
                if self.xwayland.draining_wms.len() > 1 {
                    // No cap: each entry frees itself on its channel's final
                    // `Closed`. More than one at once means a drain is slow
                    // or a `Closed` never arrived — make that visible before
                    // it reads as a leak.
                    tracing::warn!(
                        draining = self.xwayland.draining_wms.len(),
                        "multiple XWMs draining at once"
                    );
                }
            }
            XwaylandLifecycle::RetryArmed { timer } => {
                self.capture_loop_handle.remove(timer);
            }
            XwaylandLifecycle::Inert | XwaylandLifecycle::Failed => {}
        }
        let surfaces = self
            .xwayland
            .surfaces_by_xid
            .values()
            .filter_map(|object| {
                self.surfaces
                    .get(object)
                    .map(|record| record.role.wl_surface().clone())
            })
            .collect::<Vec<_>>();
        // Fall back to another focus target only when an X11 surface of this
        // generation actually held the keyboard; an unconditional fallback
        // would steal focus from an unrelated Wayland surface (and dismiss
        // its popup grabs) on every generation death.
        let held_focus = match self.keyboard.current_focus() {
            Some(SeatFocusTarget::X11(_)) => true,
            Some(SeatFocusTarget::Wayland(focused)) => surfaces.contains(&focused),
            None => false,
        };
        self.xwayland.pending_windows.clear();
        self.xwayland.surfaces_by_xid.clear();
        self.xwayland.xids_by_object.clear();
        self.xwayland.override_redirect_windows.clear();
        self.xwayland.refocus = None;
        for surface in surfaces {
            self.destroy_surface_record(&surface);
        }
        // Leak check: teardown just destroyed every X11 record the maps knew
        // about, so any X11 record still alive was reachable from no map —
        // it will outlive its generation silently and its XID lookups will
        // miss. Make it loud with its birth generation.
        for record in self.surfaces.values() {
            if let SurfaceRole::X11(role) = &record.role {
                tracing::warn!(
                    xid = role.xid,
                    record_generation = role.generation,
                    current_generation = self.xwayland.generation,
                    "X11 record survived generation teardown; surfaces_by_xid leaked it"
                );
            }
        }
        if held_focus {
            self.arbitrate_keyboard_focus(None, true, false);
        }
        self.refresh_chrome_pointer_after_scene_change();
    }

    /// Orderly compositor shutdown: reject further launches, remove the
    /// descriptor, tear down all X records and the XWM/XWayland source.
    pub(super) fn shutdown_xwayland(&mut self) {
        self.xwayland.shutting_down = true;
        self.teardown_xwayland_generation();
        self.xwayland.lifecycle = XwaylandLifecycle::Failed;
    }

    fn x11_role_record_mut(&mut self, xid: X11Window) -> Option<&mut SurfaceRecord> {
        let object = self.xwayland.surfaces_by_xid.get(&xid)?.clone();
        self.surfaces.get_mut(&object)
    }

    /// The one idempotent X geometry authority: applies a granted/notified
    /// global content rectangle to the record without touching the committed
    /// buffer presentation (`layout.width/height` follow pixels, not grants).
    fn apply_x11_geometry(&mut self, xid: X11Window, rect: Rectangle<i32, Logical>) {
        let Some(record) = self.x11_role_record_mut(xid) else {
            return;
        };
        let SurfaceRole::X11(role) = &mut record.role else {
            return;
        };
        role.granted_geometry = rect;
        // Whatever wrote this rect (map grant, configure answer, maximize,
        // fullscreen, notify), it is now real position memory worth carrying
        // across a surface swap.
        role.phase.ever_granted = true;
        let new_origin = (rect.loc.x as f32, rect.loc.y as f32);
        let new_size = (rect.size.w.max(1), rect.size.h.max(1));
        let size_changed = record.configured_size != new_size;
        record.configured_size = new_size;
        let old_origin = record.window_origin;
        if old_origin == new_origin {
            // A size-only grant still changes the scene (SSD chrome and the
            // hit-test box follow `configured_size`); publish it even though
            // nothing moves.
            if size_changed {
                if record.mapped {
                    let id = record.id;
                    let scene = record.scene_snapshot();
                    self.events
                        .push(ProtocolEvent::SurfaceRelayout { id, scene });
                }
                self.invalidate_pointer_hit_test_geometry();
            }
            return;
        }
        record.window_origin = new_origin;
        let offset = record
            .committed_window_geometry
            .map(|geometry| (geometry.x, geometry.y))
            .unwrap_or_default();
        record.layout.x = new_origin.0 - offset.0;
        record.layout.y = new_origin.1 - offset.1;
        let id = record.id;
        let delta = (new_origin.0 - old_origin.0, new_origin.1 - old_origin.1);
        let mapped = record.mapped;
        let scene = record.scene_snapshot();
        if mapped {
            self.events
                .push(ProtocolEvent::SurfaceRelayout { id, scene });
        }
        self.shift_surface_descendants(id, delta);
        self.invalidate_pointer_hit_test_geometry();
    }

    /// Decoration policy for a normal X11 toplevel: SSD when compositor
    /// decoration is enabled, unless Motif hints refuse WM decorations or the
    /// window is fullscreen. Never touches xdg decoration state.
    fn x11_decoration_mode(&self, surface: &X11Surface, fullscreen: bool) -> SceneDecorationMode {
        if !self.decoration.enabled || fullscreen || surface.is_decorated() {
            SceneDecorationMode::ClientSide
        } else {
            SceneDecorationMode::ServerSide
        }
    }

    fn refresh_x11_decoration(&mut self, xid: X11Window) {
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            return;
        };
        let Some((surface, fullscreen)) = self.surfaces.get(&object).and_then(|record| {
            let SurfaceRole::X11(role) = &record.role else {
                return None;
            };
            Some((role.surface.clone(), role.fullscreen))
        }) else {
            return;
        };
        let mode = self.x11_decoration_mode(&surface, fullscreen);
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        if record.committed_decoration == mode {
            return;
        }
        record.committed_decoration = mode;
        sync_toplevel_scene_state(record);
        if record.mapped {
            let id = record.id;
            let scene = record.scene_snapshot();
            self.events
                .push(ProtocolEvent::SurfaceRelayout { id, scene });
        }
        self.invalidate_pointer_hit_test_geometry();
    }

    /// Grant an initial normal geometry: the client's requested origin when
    /// it stated one inside the usable rect, else the cascade; size from the
    /// client's request clamped by hints and the usable output.
    fn choose_initial_x11_geometry(&mut self, window: &X11Surface) -> Rectangle<i32, Logical> {
        let usable = self.usable_output_rect();
        let extents = DecoExtents::of(&self.decoration.theme);
        let server_side =
            self.x11_decoration_mode(window, false) == SceneDecorationMode::ServerSide;
        // Content origin: honour a requested origin at INITIAL placement only
        // (the "ignore client x/y" rule governs windows the compositor has
        // already placed) — `xmessage -center` computes its own centre and
        // creates the window there. Smithay does not expose the
        // WM_NORMAL_HINTS US/PPosition flags, so (0,0) — every X client's
        // default — is indistinguishable from "no preference" and cascades.
        let requested = window.geometry().loc;
        let requested_inside = (requested.x != 0 || requested.y != 0)
            && (requested.x as f32) >= usable.x
            && (requested.y as f32) >= usable.y
            && (requested.x as f32) < usable.x + usable.width
            && (requested.y as f32) < usable.y + usable.height;
        let (mut x, mut y) = if requested_inside {
            (requested.x as f32, requested.y as f32)
        } else {
            // Cascade like a Wayland toplevel; with SSD, keep the outer
            // frame inside the usable rect.
            let cascade = self.next_layout_index % 6;
            self.next_layout_index = self.next_layout_index.saturating_add(1);
            (
                usable.x + CASCADE_ORIGIN + cascade as f32 * CASCADE_STEP,
                usable.y + CASCADE_ORIGIN + cascade as f32 * CASCADE_STEP,
            )
        };
        if server_side {
            x = x.max(usable.x + extents.left);
            y = y.max(usable.y + extents.top);
        }
        let requested = window.geometry().size;
        let fallback = sensible_toplevel_size(
            LogicalOutputRect {
                x: usable.x,
                y: usable.y,
                width: usable.width,
                height: usable.height,
            },
            x,
            y,
        );
        let usable_size = (
            (usable.x + usable.width - x - OUTPUT_MARGIN).max(1.0) as i32,
            (usable.y + usable.height - y - OUTPUT_MARGIN).max(1.0) as i32,
        );
        let size = clamp_x11_content_size(
            (requested.w, requested.h),
            window.min_size().map(Into::into),
            window.max_size().map(Into::into),
            fallback,
            usable_size,
        );
        Rectangle::new((x as i32, y as i32).into(), size.into())
    }

    /// Make an associated, map-granted record eligible and republish retained
    /// content if it has any (remap path); first-time maps wait for the first
    /// buffer commit like any other surface.
    fn make_x11_record_presentable(&mut self, xid: X11Window) {
        let Some(record) = self.x11_role_record_mut(xid) else {
            return;
        };
        let SurfaceRole::X11(role) = &record.role else {
            return;
        };
        if !role.phase.eligible() || record.mapped {
            return;
        }
        if record.buffer_dimensions.is_some() {
            record.mapped = true;
            let id = record.id;
            self.pending_full_upserts.insert(id);
            self.recompute_effective_visibility();
            self.mark_pointer_hit_test_dirty();
            self.consume_pending_x11_refocus();
        }
    }

    /// Hand the keyboard back to a window whose surface swap took it away.
    /// Armed by the displaced-record withdrawal in `x11_associate_window`
    /// when the swapped window held the keyboard; consumed here the moment
    /// its record is presentable again (first buffer commit on the new
    /// surface, or a retained-buffer remap). Cheap no-op while unarmed.
    pub(super) fn consume_pending_x11_refocus(&mut self) {
        let Some(xid) = self.xwayland.refocus.as_ref().map(|pending| pending.xid) else {
            return;
        };
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            // The window died before its replacement presented; the debt
            // dies with it.
            self.xwayland.refocus = None;
            return;
        };
        let Some((mapped, surface)) = self
            .surfaces
            .get(&object)
            .map(|record| (record.mapped, record.role.wl_surface().clone()))
        else {
            self.xwayland.refocus = None;
            return;
        };
        if !mapped {
            // Not presentable yet; stay armed for the mapping commit.
            return;
        }
        // `record.mapped` is the ONLY predicate checked here — not
        // `layout.visible`, not `minimized`, not `role.xid`, not the
        // record's generation. That is sound today because of three facts
        // stated nowhere else, so they are stated here: (1) visibility —
        // every map site runs `recompute_effective_visibility` (which
        // maintains mapped && !minimized ⇒ visible) before this consume,
        // and a record cannot be minimized in the same dispatch that maps
        // it (minimize acts on mapped records); (2) identity —
        // `surfaces_by_xid` only ever maps an xid to a record whose role
        // carries that xid (association writes both together, destroy
        // removes them together), so the record found via the map IS this
        // debt's window; (3) generation — teardown clears the forward map
        // wholesale, so a hit here is always current-generation. A refactor
        // that breaks any of the three breaks this consume silently; move
        // the corresponding check here with it.
        let Some(pending) = self.xwayland.refocus.take() else {
            return;
        };
        if self.keyboard.current_focus() != pending.fallback {
            // A human or another surface made a deliberate focus choice
            // while the debt was pending. Yanking the keyboard back now
            // would send keystrokes to the wrong window, which is worse
            // than the drop the debt repairs. Expected whenever the user
            // interacts during a swap, so drop quietly.
            //
            // Two adjudications worth being precise about:
            // - A DYING fallback target also lands here — but not for the
            //   reason a first read suggests. The working mechanism is the
            //   vendored `X11Surface` `PartialEq`, which requires BOTH
            //   sides alive (xwm/surface.rs:100-107), and DestroyNotify
            //   sets `alive = false` before `destroyed_window` runs — so a
            //   recorded X11 fallback whose window died can never compare
            //   equal, and a false pay is impossible. A vendor bump that
            //   simplifies that `PartialEq` to a plain id comparison would
            //   silently convert every dying-fallback drop into a pay;
            //   this comment is the only comp-side record of the
            //   dependency.
            // - A POPUP over the fallback, scoped to popups that actually
            //   take the keyboard: comp installs `PopupKeyboardGrab` only
            //   when the surface's layer interactivity is not `None`
            //   (handlers.rs:1513-1518) — a `KeyboardInteractivity::None`
            //   layer popup never moves keyboard focus, and a debt pays
            //   with it open. For a keyboard-grabbing popup: open at
            //   consumption reads as different and drops the debt; after
            //   dismissal, focus lands on the grab ROOT — but on the NEXT
            //   input event, not at dismissal: the pointer grab notices
            //   `has_ended()` in its next `motion` (popup/grab.rs:548) or
            //   `button` (:586) and unsets with restore, whose `unset`
            //   (:697-700) runs `unset_keyboard_grab` = the no-restore
            //   two-arg `KeyboardHandle::unset_grab(data)` + explicit
            //   `set_focus(Some(self.root))` (popup/grab.rs:371-381;
            //   keyboard/mod.rs:891-897) — and the keyboard grab's own
            //   `input` path restores the root directly on the next key
            //   (:443-451). A consume in the
            //   dismissal-to-next-event window still sees pre-restore
            //   focus and DROPS — the safe direction; after the restore,
            //   the debt pays exactly when the root is the recorded
            //   fallback. No offline test drives this; these citations
            //   are the verification, read from the vendored source.
            tracing::debug!(
                xid,
                "dropped X11 refocus debt; focus moved deliberately while it was pending"
            );
            return;
        }
        // The pay is BEST-EFFORT: the debt was `take()`n above, and
        // `arbitrate_keyboard_focus` may still decline — including
        // session-lock filtering, an exclusive layer holding the keyboard,
        // and `interaction_focus_root` resolving to nothing. The
        // debt is gone either way and the keyboard stays put — by design:
        // a declined arbitration means a policy says this window must not
        // have the keyboard right now, and a retried debt would be a
        // standing argument with that policy.
        self.arbitrate_keyboard_focus(Some(surface), false, false);
        tracing::debug!(xid, "returned keyboard focus after X11 surface swap");
    }

    /// Mirror the comp scene's authoritative normal-band order into the XWM
    /// stack: filter the bottom→top scene order to associated X11 windows and
    /// hand it to Smithay's stacking synchroniser.
    pub(super) fn sync_xwm_stacking(&mut self) {
        let mut ordered = self
            .surfaces
            .values()
            .filter_map(|record| {
                let SurfaceRole::X11(role) = &record.role else {
                    return None;
                };
                record
                    .mapped
                    .then_some((record.layout.z, role.surface.clone()))
            })
            .collect::<Vec<_>>();
        if ordered.is_empty() {
            return;
        }
        ordered.sort_unstable_by_key(|(key, _)| *key);
        let order = ordered
            .iter()
            .map(|(_, surface)| surface)
            .collect::<Vec<_>>();
        if let Some(wm) = self.xwayland.wm()
            && let Err(error) = wm.update_stacking_order_upwards(order.into_iter())
        {
            tracing::warn!(%error, "failed to mirror stacking order into XWM");
        }
    }

    /// Whether the seat's keyboard focus currently resolves to this X11
    /// window. X11 targets are compared by window id — not through
    /// `X11Surface::wl_surface()` — so the answer does not depend on how far
    /// Smithay's own teardown for the window has progressed when the
    /// callback runs (it clears `wl_surface` only after `unmapped_window`
    /// returns, and never before `destroyed_window`; neither ordering is
    /// load-bearing here).
    fn x11_window_holds_keyboard_focus(&self, xid: X11Window, wl_surface: &WlSurface) -> bool {
        match self.keyboard.current_focus() {
            Some(SeatFocusTarget::X11(surface)) => surface.window_id() == xid,
            Some(SeatFocusTarget::Wayland(focused)) => focused == *wl_surface,
            None => false,
        }
    }

    /// X11 (`_NET_WM_MOVERESIZE`) requests carry a pressed button, not a
    /// Wayland serial: validate that the pointer currently holds an implicit
    /// grab whose focus resolves to this window's surface tree.
    fn x11_pointer_grab_targets(&self, window: &X11Surface) -> bool {
        let Some(surface) = window.wl_surface() else {
            return false;
        };
        if !self.pointer.is_grabbed() {
            return false;
        }
        let Some(start) = self.pointer.grab_start_data() else {
            return false;
        };
        let Some((focus, _)) = start.focus else {
            return false;
        };
        let Some(focus_surface) = focus.owned_surface() else {
            return false;
        };
        canonical_root_surface(&self.popup_manager, &focus_surface)
            == canonical_root_surface(&self.popup_manager, &surface)
    }
}

impl XWaylandShellHandler for WaylandState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        // Same liveness gate as the XwmHandler delegates: an association for
        // a torn-down generation must not mint a fresh scene record.
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_associate_window(wl_surface, surface);
    }
}

impl WaylandState {
    /// The association point: this is where an X11 window becomes a
    /// scene-capable entity, synchronously, before the same commit can reach
    /// the common buffer path (Smithay runs this from a pre-commit hook, so
    /// the record exists before `CompositorHandler::commit` sees the buffer).
    pub(super) fn x11_associate_window(&mut self, wl_surface: WlSurface, surface: X11Surface) {
        let xid = surface.window_id();
        // Pin the vendor ordering this function's focus-identity capture
        // depends on: smithay repoints `X11Surface::wl_surface()` at the
        // incoming surface BEFORE invoking `surface_associated` (and the
        // offline tests hand-simulate the same ordering via
        // `set_wl_surface_offline`). Every by-window-id focus/grab match
        // below exists because resolution through `wl_surface()` is already
        // flipped here — a smithay bump that moves the assignment would
        // falsify those comments silently; this assert makes it loud.
        debug_assert!(
            surface
                .wl_surface()
                .is_none_or(|current| current == wl_surface),
            "surface_associated ran before smithay repointed X11Surface::wl_surface(); \
             the by-id focus capture below is built on the opposite ordering"
        );
        if surface.is_override_redirect() {
            // X-2 gap: recorded, never managed.
            self.xwayland.override_redirect_windows.insert(xid);
            tracing::debug!(xid, "ignored override-redirect surface association (X-1)");
            return;
        }
        let pending = self.xwayland.pending_windows.remove(&xid);
        // Re-association: the same wl_surface can be associated again (either
        // callback can repeat). The map grant belongs to the XID, so carry
        // the phase over from the existing record when it is this window's —
        // taking it from `pending_windows` alone would silently revoke a
        // granted map the client will never re-request.
        let existing_phase = self.surfaces.get(&wl_surface.id()).and_then(|record| {
            let SurfaceRole::X11(role) = &record.role else {
                return None;
            };
            (role.xid == xid).then_some(role.phase)
        });
        // Same XID re-associating to a DIFFERENT wl_surface: the previous
        // record is displaced, and it carries two obligations.
        //
        // 1. Phase + geometry hand-over. The map grant belongs to the XID
        //    and the client will not re-request it. On this ordering the
        //    incoming surface has no record (`existing_phase` is None) and
        //    `pending_windows[xid]` was consumed by the first association —
        //    without the hand-over the new record starts ineligible
        //    (`map_requested == false`) with geometry from raw X hints, and
        //    `commit_may_map_surface` withholds every buffer forever.
        // 2. Withdrawal. The displaced record must not survive as a live
        //    mapped ghost: unreachable from either XID map, it would keep
        //    compositing, hand `sync_xwm_stacking` two entries for one X11
        //    window, and leak its buffer retention token at teardown.
        //    `destroy_surface_record` withdraws it fully (renderer told via
        //    `SurfaceDestroyed`, backings released, both XID map entries
        //    cleaned) — the same established path `x11_destroyed_window`
        //    uses while the wl_surface is still protocol-alive; the
        //    surface's own later destruction is a no-op on the absent
        //    record.
        let displaced_object = self
            .xwayland
            .surfaces_by_xid
            .get(&xid)
            .filter(|previous| **previous != wl_surface.id())
            .cloned();
        let mut displaced_phase = None;
        let mut displaced_geometry = None;
        if let Some(previous_object) = displaced_object {
            let displaced_surface = self.surfaces.get(&previous_object).and_then(|record| {
                let SurfaceRole::X11(role) = &record.role else {
                    return None;
                };
                if role.xid != xid {
                    return None;
                }
                displaced_phase = Some(role.phase);
                // Hand the geometry over only when comp has ever actually
                // granted one: on a never-granted record `granted_geometry`
                // holds raw X hints under a granted name, and a stale hint
                // must not outrank the incoming surface's current one. The
                // gate is the STICKY `ever_granted`, not `map_requested` —
                // an unmap clears the latter while `granted_geometry` stays
                // the window's position memory for the remap, and
                // discarding it here would reopen a moved/maximized window
                // in the wrong place.
                displaced_geometry =
                    Some(role.granted_geometry).filter(|_| role.phase.ever_granted);
                Some(role.wl_surface.clone())
            });
            match displaced_surface {
                Some(displaced_surface) => {
                    // Focus identity is captured BY WINDOW ID before the
                    // destroy, not by resolving the focus target's surface:
                    // smithay repoints `X11Surface::wl_surface()` at the
                    // incoming surface BEFORE `surface_associated` runs, so
                    // resolution-based matching (`clear_focus_for_surface`
                    // inside the destroy, its pointer arm likewise) can no
                    // longer see that the seat is parked on the displaced
                    // surface.
                    let held_focus = self.x11_window_holds_keyboard_focus(xid, &displaced_surface);
                    let pointer_on_window = match self.pointer.current_focus() {
                        Some(SeatFocusTarget::X11(target)) => target.window_id() == xid,
                        Some(SeatFocusTarget::Wayland(focused)) => focused == displaced_surface,
                        None => false,
                    };
                    // The implicit-grab identity, captured by window id for
                    // the same reason as the two above: the destroy's own
                    // grab arm resolves `start.focus` through the flipped
                    // `wl_surface()` and cannot match, which would leave a
                    // held button routing motion through a dead grab.
                    let pointer_grab_on_window = self
                        .pointer
                        .grab_start_data()
                        .and_then(|start| start.focus)
                        .is_some_and(|(focus, _)| match focus {
                            SeatFocusTarget::X11(target) => target.window_id() == xid,
                            SeatFocusTarget::Wayland(focused) => focused == displaced_surface,
                        });
                    self.destroy_surface_record(&displaced_surface);
                    if held_focus {
                        // Clear explicitly against the captured identity —
                        // AFTER the destroy, so the fallback cannot re-pick
                        // the still-mapped record it is clearing — and owe
                        // the window its focus back the moment the new
                        // surface presents (`consume_pending_x11_refocus`).
                        // The `wl_keyboard.leave` this delivers resolves
                        // against the already-flipped surface — vendor lazy
                        // resolution comp cannot redirect without forking
                        // the X half of focus; the map-time refocus's
                        // `enter` balances it.
                        self.arbitrate_keyboard_focus(None, true, false);
                        // Record where the fallback parked the keyboard
                        // (possibly nowhere): consumption pays the debt only
                        // while focus still sits exactly there — anything
                        // else means a deliberate choice was made in the
                        // interim, and the debt is dropped instead of
                        // stealing the keyboard back.
                        if let Some(previous) = &self.xwayland.refocus {
                            // Single slot by design: any second arming
                            // before the first debt resolves discards the
                            // older debt AND its recorded baseline — at
                            // most one window can owe the user a refocus,
                            // and the newer swap is the newer truth. Logged
                            // for the same-xid overwrite too: the baseline
                            // it discards is the load-bearing half.
                            tracing::debug!(
                                previous_xid = previous.xid,
                                xid,
                                "surface swap discards an outstanding focus debt and its baseline"
                            );
                        }
                        self.xwayland.refocus = Some(PendingX11Refocus {
                            xid,
                            fallback: self.keyboard.current_focus(),
                        });
                    }
                    if pointer_on_window || pointer_grab_on_window {
                        // The destroy's own pointer arms mismatched for the
                        // flipped-resolution reason above; mirror its
                        // teardown protocol explicitly, deferral included.
                        if self.pointer_hit_test_reconciliation_deferred() {
                            if pointer_grab_on_window {
                                self.pointer_grab_teardown_deferred = true;
                            }
                            self.mark_pointer_hit_test_dirty();
                        } else {
                            if pointer_grab_on_window {
                                // WITHOUT focus restore, matching the
                                // reconciler's deferred arm: the record was
                                // destroyed just above, so a restore here
                                // would emit a `wl_pointer.enter` naming the
                                // just-destroyed surface — wire traffic, not
                                // merely transient state — before the
                                // retarget below supplies the correct focus
                                // from a current hit test. The restore is
                                // strictly redundant with that retarget.
                                // (`clear_focus_for_surface`'s grab arm
                                // still uses the restoring `unset_grab`;
                                // of its six callers, only the
                                // `destroy_surface_record` invocation has
                                // this record-already-removed shape, and
                                // only that call should migrate for the
                                // same reason — its own change, not a
                                // rider on this one.)
                                let pointer = self.pointer.clone();
                                pointer.unset_grab_without_focus_restore(
                                    self,
                                    SERIAL_COUNTER.next_serial(),
                                    monotonic_millis(),
                                );
                                tracing::debug!(
                                    xid,
                                    "cancelled pointer grab held on a swapped X11 surface"
                                );
                            }
                            self.retarget_pointer_after_visibility_change();
                        }
                    }
                    // Same scene-change epilogue as the sibling destroy
                    // paths (`x11_unmapped_window`/`x11_destroyed_window`):
                    // chrome hover state must not survive the entity.
                    self.refresh_chrome_pointer_after_scene_change();
                    tracing::debug!(
                        xid,
                        held_focus,
                        "withdrew displaced X11 record on re-association to a new wl_surface"
                    );
                }
                None => {
                    // The forward entry points at an object with no matching
                    // X11 record (already destroyed, or its role replaced).
                    // Hygiene only — no demonstrated failure mode rides on
                    // this arm: drop the stale reverse entry when it still
                    // claims this XID; the insert below repoints the forward
                    // one.
                    if self.xwayland.xids_by_object.get(&previous_object) == Some(&xid) {
                        self.xwayland.xids_by_object.remove(&previous_object);
                    }
                }
            }
        }
        let mut phase = existing_phase
            .or_else(|| pending.as_ref().map(|entry| entry.phase))
            .or(displaced_phase)
            .unwrap_or_default();
        phase.on_associated();
        let granted = pending
            .and_then(|entry| entry.granted_geometry)
            .or(displaced_geometry);
        if granted.is_some() {
            // Covers the pre-map ConfigureRequest grant, whose pending
            // entry records a geometry without a phase transition.
            phase.ever_granted = true;
        }
        let geometry = granted.unwrap_or_else(|| surface.geometry());
        let decoration = self.x11_decoration_mode(&surface, false);
        let title = capped_toplevel_title(&surface.title());
        let class = capped_toplevel_title(&surface.class());
        let object = wl_surface.id();
        let z = self.allocate_stack_key(StackBand::Normal);
        let origin = (geometry.loc.x as f32, geometry.loc.y as f32);
        let configured_size = (geometry.size.w.max(1), geometry.size.h.max(1));
        let layout = SurfaceLayout {
            x: origin.0,
            y: origin.1,
            width: configured_size.0 as f32,
            height: configured_size.1 as f32,
            z,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: false,
            toplevel: None,
        };
        let role = SurfaceRole::X11(Box::new(X11ToplevelRole {
            wl_surface: wl_surface.clone(),
            surface: surface.clone(),
            xid,
            generation: self.xwayland.generation,
            phase,
            granted_geometry: geometry,
            fullscreen: false,
            wl_surface_serial: surface.wl_surface_serial(),
        }));
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(&wl_surface);
        let id = if let Some(record) = self.surfaces.get_mut(&object) {
            let id = record.id;
            // Re-associating a presented record withdraws it: tell the
            // renderer, or it keeps an entity the protocol thinks is gone.
            if record.mapped {
                self.events.push(ProtocolEvent::SurfaceUnmapped { id });
            }
            record.role = role;
            record.mapped = false;
            record.layout = layout;
            record.title = Some(title.clone()).filter(|title| !title.is_empty());
            record.app_id = Some(class.clone()).filter(|class| !class.is_empty());
            record.window_origin = origin;
            record.configured_size = configured_size;
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = decoration;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = true;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                object.clone(),
                SurfaceRecord {
                    id,
                    role,
                    mapped: false,
                    layout,
                    title: Some(title.clone()).filter(|title| !title.is_empty()),
                    app_id: Some(class.clone()).filter(|class| !class.is_empty()),
                    window_origin: origin,
                    configured_size,
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: decoration,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, object.clone());
            id
        };
        self.committed_surface_stacks
            .insert(object.clone(), vec![object.clone()]);
        // Same object, new XID: drop the old XID's forward entry (when it
        // still points at this object), or a later DestroyNotify for the
        // old XID would destroy the live record. (The same-XID/new-object
        // direction is fully handled by the displaced-record withdrawal
        // above.)
        if let Some(previous_xid) = self.xwayland.xids_by_object.get(&object).copied()
            && previous_xid != xid
            && self.xwayland.surfaces_by_xid.get(&previous_xid) == Some(&object)
        {
            self.xwayland.surfaces_by_xid.remove(&previous_xid);
        }
        self.xwayland.surfaces_by_xid.insert(xid, object.clone());
        self.xwayland.xids_by_object.insert(object, xid);
        tracing::info!(
            surface_id = id.0,
            xid,
            surface = ?wl_surface.id(),
            width = configured_size.0,
            height = configured_size.1,
            "X11 window associated with wl_surface"
        );
    }
}

/// The XWM behaviour, as inherent methods. The `XwmHandler` impl below
/// delegates 1:1; the split exists because `XwmId` is unconstructible outside
/// a live X11 connection, and the deterministic protocol tests must drive
/// exactly this code.
impl WaylandState {
    pub(super) fn x11_new_window(&mut self, window: X11Surface) {
        let xid = window.window_id();
        tracing::debug!(xid, title = %window.title(), "new X11 window");
        self.xwayland
            .pending_windows
            .insert(xid, PendingX11Window::default());
    }

    pub(super) fn x11_new_override_redirect_window(&mut self, window: X11Surface) {
        // Deliberate X-2 gap: the WM must not manage these; X-1 records them
        // for diagnostics and renders nothing.
        let xid = window.window_id();
        self.xwayland.override_redirect_windows.insert(xid);
        tracing::debug!(
            xid,
            "recorded override-redirect X11 window (ignored in X-1)"
        );
    }

    pub(super) fn x11_map_window_request(&mut self, window: X11Surface) {
        let xid = window.window_id();
        if window.is_override_redirect() {
            return;
        }
        // Idempotent: repeated requests re-grant the same geometry.
        let existing = self
            .xwayland
            .surfaces_by_xid
            .get(&xid)
            .cloned()
            .and_then(|object| {
                self.surfaces.get(&object).and_then(|record| {
                    let SurfaceRole::X11(role) = &record.role else {
                        return None;
                    };
                    Some(role.granted_geometry)
                })
            });
        let pending_granted = self
            .xwayland
            .pending_windows
            .get(&xid)
            .and_then(|entry| entry.granted_geometry);
        let geometry = existing
            .or(pending_granted)
            .unwrap_or_else(|| self.choose_initial_x11_geometry(&window));
        // Failure policy for `configure`/`set_maximized`/`set_fullscreen`
        // (here and in the maximize/fullscreen state functions): comp-side
        // state is recorded even when the X call errors, deliberately.
        // `set_maximized`/`set_fullscreen` bottom out in property writes and
        // surface only stream-level failures (a broken connection), so for
        // them an `Err` means the generation is dying and `disconnected`
        // will destroy every record shortly. `configure` has exactly one
        // per-request refusal on a healthy connection —
        // `UnsupportedForOverrideRedirect` — and a client CAN reach it by
        // unmapping, setting override-redirect, and remapping; that case is
        // closed by `x11_mapped_override_redirect_window` destroying the
        // managed record, so no divergent record persists. Recording
        // unconditionally keeps comp internally coherent for the dying-
        // connection window; skipping on error would instead desynchronise
        // phase from geometry with no client to reconcile against.
        if let Err(error) = window.configure(Some(geometry)) {
            tracing::warn!(xid, %error, "failed to grant initial X11 geometry");
        }
        // The grant is recorded BEFORE the fallible `set_mapped`: the client
        // sent its one MapRequest and will not resend it, so a transient X
        // failure below must not leave the window permanently ineligible.
        if !self.xwayland.surfaces_by_xid.contains_key(&xid) {
            // Not associated yet: record the grant so association starts
            // eligible (either callback order is legal).
            let entry = self.xwayland.pending_windows.entry(xid).or_default();
            entry.phase.on_map_requested();
            entry.granted_geometry = Some(geometry);
        }
        if let Some(record) = self.x11_role_record_mut(xid)
            && let SurfaceRole::X11(role) = &mut record.role
        {
            role.phase.on_map_requested();
            role.granted_geometry = geometry;
            record.window_origin = (geometry.loc.x as f32, geometry.loc.y as f32);
            record.configured_size = (geometry.size.w.max(1), geometry.size.h.max(1));
            let offset = record
                .committed_window_geometry
                .map(|window_geometry| (window_geometry.x, window_geometry.y))
                .unwrap_or_default();
            record.layout.x = record.window_origin.0 - offset.0;
            record.layout.y = record.window_origin.1 - offset.1;
        }
        if let Err(error) = window.set_mapped(true) {
            tracing::warn!(xid, %error, "failed to grant X11 map");
            return;
        }
        self.refresh_x11_decoration(xid);
        self.make_x11_record_presentable(xid);
        tracing::debug!(
            xid,
            x = geometry.loc.x,
            y = geometry.loc.y,
            width = geometry.size.w,
            height = geometry.size.h,
            "granted X11 map request"
        );
    }

    pub(super) fn x11_map_window_notify(&mut self, window: X11Surface) {
        // Notification only: reconcile, never allocate or publish a second
        // entity.
        let xid = window.window_id();
        if let Some(entry) = self.xwayland.pending_windows.get_mut(&xid) {
            entry.phase.on_map_notify();
        }
        if let Some(record) = self.x11_role_record_mut(xid)
            && let SurfaceRole::X11(role) = &mut record.role
        {
            role.phase.on_map_notify();
        }
    }

    pub(super) fn x11_mapped_override_redirect_window(&mut self, window: X11Surface) {
        let xid = window.window_id();
        // A managed window can leave management at runtime: unmap, set
        // override-redirect, remap. From here on every managed-path X write
        // for this XID is refused per-request
        // (`UnsupportedForOverrideRedirect`) while a surviving managed
        // record would keep accepting maximize/move/resize requests —
        // divergence that outlives the connection until DestroyNotify.
        // Destroy the record: with no record there is nothing to diverge.
        if self.xwayland.surfaces_by_xid.contains_key(&xid) {
            tracing::info!(
                xid,
                "managed X11 window remapped as override-redirect; destroying its managed record"
            );
            self.x11_destroyed_window(window.clone());
        }
        // Deliberate X-2 gap: visually ignored. Logged once per XID so the
        // gate can see the gap without the log flooding.
        if self.xwayland.override_redirect_windows.insert(xid) {
            tracing::info!(
                xid,
                window_type = ?window.window_type(),
                "override-redirect X11 window mapped; not rendered in X-1"
            );
        }
    }

    pub(super) fn x11_unmapped_window(&mut self, window: X11Surface) {
        let xid = window.window_id();
        // Twin of the association-time ordering pin: smithay nulls
        // `state.wl_surface` only AFTER `unmapped_window` returns
        // (vendored xwm/mod.rs:1715-1719), and this path's
        // resolution-based teardown — `clear_focus_for_surface`'s pointer
        // and grab arms in particular — works ONLY because resolution
        // still succeeds here. A vendor bump that moves that assignment
        // above the callback would silently stop tearing down a pointer
        // grab held on the unmapping window (the same defect the surface-
        // swap path fixed by-id); this assert makes it loud instead.
        //
        // The `record.mapped` gate is load-bearing, not hygiene: a
        // compliant withdraw (XWithdrawWindow, a GTK/Qt menu closing)
        // delivers TWO unmaps — the real UnmapNotify and the ICCCM-4.1.4
        // synthetic one — and the vendor dedupes neither. After the first,
        // smithay has nulled `wl_surface` while comp DELIBERATELY keeps
        // the record and association for the idempotent remap, so on the
        // duplicate a `None` resolution is the correct, supported state
        // (`record.mapped` went false at the first unmap).
        //
        // The two causes of a mapped-but-unresolvable window ARE
        // separable, just not through `wl_surface()` alone: the vendor
        // records `wl_surface_serial` BEFORE both arms of the pairing
        // branch (xwm/mod.rs:2303), so the legal unpaired-serial null
        // (xwm/mod.rs:2320-2328 — WL_SURFACE_SERIAL before its commit, an
        // explicitly supported ordering) carries a serial DIFFERENT from
        // the one this role captured at association (the code compares
        // `!=`; the vendor guarantees no monotonicity), while an ordering
        // flip (a vendor bump moving the null above this callback) leaves
        // it untouched.
        //
        // The flip branch is `warn!`, not `error!`, because the runtime
        // line is the SECONDARY instrument: a flip can only ever arrive
        // via a vendor bump — never at runtime — and the source-text pins
        // in the default suite (`vendored_xwm_serial_recorded_before_
        // unpaired_insert` / `vendored_xwm_unmap_callback_precedes_the_
        // null`) detect that deterministically at bump time. A runtime
        // `error!` would also red the smoke gate's bare ERROR needle on
        // the one UNRULED-OUT legal path: `by_serial` is insert-only
        // (xwayland_shell.rs:299-304), so an X-first-associated window's
        // serial is never registered and a client resending the IDENTICAL
        // serial would land here legally — unproven, but not worth an
        // abortive gate red to find out. Two more scope facts: the
        // classifier's false-negative direction is a flip that lands
        // INSIDE a legal re-serial window (classifies as the legal arm and
        // is missed — coverage is "flip outside a re-serial window"); and
        // `mapped` only becomes true once a buffer has presented, so the
        // pin is silently inert for a window that unmaps before first
        // present — benign (a pointer grab needs hit-test presence, which
        // needs presentation), but a narrowing.
        let pin = self
            .xwayland
            .surfaces_by_xid
            .get(&xid)
            .and_then(|object| self.surfaces.get(object))
            .filter(|record| record.mapped)
            .and_then(|record| record.role.x11())
            .map(|role| (role.wl_surface.id(), role.wl_surface_serial));
        if let Some((expected_surface, association_serial)) = pin {
            let current = window.wl_surface();
            let resolves = current
                .as_ref()
                .is_some_and(|current| current.id() == expected_surface);
            if !resolves {
                let current_serial = window.wl_surface_serial();
                if current_serial != association_serial {
                    tracing::debug!(
                        xid,
                        ?association_serial,
                        ?current_serial,
                        "unmap inside the legal unpaired-serial window; \
                         resolution returns when the commit pairs the new serial"
                    );
                } else {
                    #[cfg(test)]
                    {
                        self.x11_unmap_pin_flip_count += 1;
                    }
                    tracing::warn!(
                        xid,
                        current = ?current.map(|surface| surface.id()),
                        expected = ?expected_surface,
                        "X11 wl_surface resolution diverged from the association with no \
                         new serial: vendored unmap ordering flip (see the source pins), \
                         or the unproven same-serial re-association residual — either way \
                         the resolution-based grab teardown for this unmap did not run"
                    );
                }
            }
        }
        if self
            .xwayland
            .refocus
            .as_ref()
            .is_some_and(|pending| pending.xid == xid)
        {
            // An unmap is the window forfeiting its claim: without this, a
            // debt armed at a swap could park behind the consume's
            // `!record.mapped` early-return for MINUTES (unmap holds
            // `map_requested` false until a remap), and the baseline
            // comparison would not save it — a user who worked elsewhere
            // without changing focus again still matches the recorded
            // fallback, and the eventual remap would yank the keyboard
            // mid-keystroke.
            self.xwayland.refocus = None;
        }
        if let Some(entry) = self.xwayland.pending_windows.get_mut(&xid) {
            entry.phase.on_unmapped();
        }
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            return;
        };
        let Some((wl_surface, was_mapped, id)) =
            self.surfaces.get_mut(&object).and_then(|record| {
                let SurfaceRole::X11(role) = &mut record.role else {
                    return None;
                };
                role.phase.on_unmapped();
                let was_mapped = record.mapped;
                record.mapped = false;
                record.minimized = false;
                Some((role.wl_surface.clone(), was_mapped, record.id))
            })
        else {
            return;
        };
        // Keep the association and retained backing for an idempotent remap;
        // clear focus, grabs and the foreign handle like a Wayland unmap.
        let held_focus = self.x11_window_holds_keyboard_focus(xid, &wl_surface);
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(&wl_surface);
        self.close_foreign_toplevel(&wl_surface);
        self.cancel_chrome_pointer_grab_for_surface(&wl_surface, false);
        self.reset_chrome_pointer_tracking(&object);
        self.minimized_toplevels.retain(|entry| *entry != object);
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| *interactive == wl_surface)
        {
            self.interactive_pointer = None;
        }
        self.recompute_effective_visibility();
        self.clear_focus_for_surface(&wl_surface);
        if was_mapped {
            self.events.push(ProtocolEvent::SurfaceUnmapped { id });
        }
        // Fall back only when this window actually held the keyboard: an
        // unconditional fallback stole focus from unrelated surfaces (an
        // OnDemand layer panel, an idle focus) and its `unset_grab` path
        // dismissed open Wayland popups whenever any background X11 window
        // unmapped. The gated call also guarantees the move-away for the
        // held case even if `clear_focus_for_surface`'s target resolution
        // ever fails, since the gate compares window ids.
        if held_focus {
            self.arbitrate_keyboard_focus(None, true, false);
        }
        self.refresh_chrome_pointer_after_scene_change();
        tracing::debug!(xid, surface_id = id.0, "X11 window unmapped");
    }

    pub(super) fn x11_destroyed_window(&mut self, window: X11Surface) {
        let xid = window.window_id();
        self.xwayland.pending_windows.remove(&xid);
        self.xwayland.override_redirect_windows.remove(&xid);
        if self
            .xwayland
            .refocus
            .as_ref()
            .is_some_and(|pending| pending.xid == xid)
        {
            // The window died before its swapped surface presented; the
            // focus debt dies with it. This clear is also what makes XID
            // reuse safe for the debt: an X server cannot recycle an XID it
            // has not destroyed, DestroyNotify for the destruction lands
            // here before any CreateWindow can reuse the id (the X event
            // stream is ordered), and generation teardown clears the field
            // wholesale — so a pending debt can never be paid to a
            // different window that inherited the XID.
            self.xwayland.refocus = None;
        }
        let Some(object) = self.xwayland.surfaces_by_xid.remove(&xid) else {
            return;
        };
        self.xwayland.xids_by_object.remove(&object);
        let Some(wl_surface) = self
            .surfaces
            .get(&object)
            .map(|record| record.role.wl_surface().clone())
        else {
            return;
        };
        // No cross-generation check here: the `surfaces_by_xid.remove` above
        // is cleared wholesale at teardown, so a callback that gets past it
        // is always current-generation. The leak check for records that
        // outlive their generation lives in `teardown_xwayland_generation`,
        // where the leaked shape is actually reachable.
        let held_focus = self.x11_window_holds_keyboard_focus(xid, &wl_surface);
        // Existing destruction path: exactly once; the later wl_surface
        // destroy is a no-op for the absent record.
        self.destroy_surface_record(&wl_surface);
        // Same gate as `x11_unmapped_window`: the fallback re-focus runs only
        // for a window that held the keyboard.
        if held_focus {
            self.arbitrate_keyboard_focus(None, true, false);
        }
        self.refresh_chrome_pointer_after_scene_change();
        tracing::debug!(xid, "X11 window destroyed");
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn x11_configure_request(
        &mut self,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        let xid = window.window_id();
        if window.is_override_redirect() {
            return;
        }
        // Position policy: an explicit client x/y is honoured only BEFORE
        // first placement (a pre-map ConfigureRequest is how an X client
        // states where it wants to appear); once the compositor has placed
        // the window, client x/y is ignored.
        let current = self
            .xwayland
            .surfaces_by_xid
            .get(&xid)
            .cloned()
            .and_then(|object| {
                self.surfaces.get(&object).and_then(|record| {
                    let SurfaceRole::X11(role) = &record.role else {
                        return None;
                    };
                    Some(role.granted_geometry)
                })
            })
            .or_else(|| {
                self.xwayland
                    .pending_windows
                    .get(&xid)
                    .and_then(|entry| entry.granted_geometry)
            });
        let base = current.unwrap_or_else(|| window.geometry());
        let usable = self.usable_output_rect();
        let origin = if current.is_none() && (x.is_some() || y.is_some()) {
            let requested = (x.unwrap_or(base.loc.x), y.unwrap_or(base.loc.y));
            let inside = (requested.0 as f32) >= usable.x
                && (requested.1 as f32) >= usable.y
                && (requested.0 as f32) < usable.x + usable.width
                && (requested.1 as f32) < usable.y + usable.height;
            if inside { requested.into() } else { base.loc }
        } else {
            base.loc
        };
        let usable_size = (usable.width.max(1.0) as i32, usable.height.max(1.0) as i32);
        let requested = (
            w.map_or(base.size.w, |w| w.min(i32::MAX as u32) as i32),
            h.map_or(base.size.h, |h| h.min(i32::MAX as u32) as i32),
        );
        let size = clamp_x11_content_size(
            requested,
            window.min_size().map(Into::into),
            window.max_size().map(Into::into),
            (base.size.w.max(1), base.size.h.max(1)),
            usable_size,
        );
        let granted = Rectangle::new(origin, size.into());
        let changed = Some(granted) != current;
        let result = if changed {
            window.configure(Some(granted))
        } else {
            // Nothing changed: answer with a synthetic configure so the
            // client still receives a reply.
            window.configure(None)
        };
        if let Err(error) = result {
            tracing::warn!(xid, %error, "failed to answer X11 configure request");
            return;
        }
        if let Some(entry) = self.xwayland.pending_windows.get_mut(&xid) {
            entry.granted_geometry = Some(granted);
        }
        if changed {
            self.apply_x11_geometry(xid, granted);
        }
        match reorder {
            Some(Reorder::Top) => {
                if let Some(surface) = window.wl_surface() {
                    self.raise_surface(&surface);
                    self.sync_xwm_stacking();
                }
            }
            Some(other @ (Reorder::Above(_) | Reorder::Below(_) | Reorder::Bottom)) => {
                // X-1 simplification: the comp scene is the stacking
                // authority and only exposes raise-to-top; relative restacks
                // are refused, not approximated.
                tracing::debug!(xid, reorder = ?other, "ignored non-Top X11 restack request (X-1)");
            }
            None => {}
        }
    }

    pub(super) fn x11_configure_notify(
        &mut self,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<X11Window>,
    ) {
        let xid = window.window_id();
        if window.is_override_redirect() {
            // Diagnostics only in X-1; X-2 must mirror these faithfully.
            tracing::trace!(
                xid,
                ?geometry,
                "override-redirect configure notify (ignored in X-1)"
            );
            return;
        }
        self.apply_x11_geometry(xid, geometry);
        if let Some(above) = above {
            tracing::debug!(xid, above, "ignored X11 sibling restack notify (X-1)");
        }
    }

    pub(super) fn x11_property_notify(&mut self, window: X11Surface, property: WmWindowProperty) {
        let xid = window.window_id();
        match property {
            WmWindowProperty::Title => {
                let title = capped_toplevel_title(&window.title());
                let lock_active = self.session_lock_active();
                let Some(record) = self.x11_role_record_mut(xid) else {
                    return;
                };
                let title = Some(title).filter(|title| !title.is_empty());
                if record.title == title {
                    return;
                }
                record.title = title;
                let publish = record.mapped && !lock_active;
                let event = publish.then(|| ProtocolEvent::SurfaceRelayout {
                    id: record.id,
                    scene: record.scene_snapshot(),
                });
                #[cfg(feature = "bus")]
                let id = record.id;
                if let Some(event) = event {
                    self.events.push(event);
                }
                #[cfg(feature = "bus")]
                self.mark_surface_dirty(id, "wayland.map");
                if let Some(surface) = window.wl_surface() {
                    self.sync_foreign_toplevel(&surface);
                }
            }
            WmWindowProperty::Class => {
                let class = capped_toplevel_title(&window.class());
                let Some(record) = self.x11_role_record_mut(xid) else {
                    return;
                };
                let class = Some(class).filter(|class| !class.is_empty());
                if record.app_id == class {
                    return;
                }
                record.app_id = class;
                #[cfg(feature = "bus")]
                let id = record.id;
                #[cfg(feature = "bus")]
                self.mark_surface_dirty(id, "wayland.map");
                if let Some(surface) = window.wl_surface() {
                    self.sync_foreign_toplevel(&surface);
                }
            }
            WmWindowProperty::MotifHints => {
                self.refresh_x11_decoration(xid);
            }
            WmWindowProperty::Hints
            | WmWindowProperty::NormalHints
            | WmWindowProperty::TransientFor
            | WmWindowProperty::WindowType
            | WmWindowProperty::Protocols
            | WmWindowProperty::StartupId
            | WmWindowProperty::Pid => {
                // Hints are consulted live at the next size/focus decision;
                // TransientFor/WindowType are X-2 relationship inputs.
                tracing::trace!(xid, ?property, "X11 property retained for later decisions");
            }
        }
    }

    pub(super) fn x11_minimize_request(&mut self, window: X11Surface) {
        let Some(surface) = window.wl_surface() else {
            return;
        };
        self.minimize_toplevel(&surface);
        let _ = window.set_suspended(true);
    }

    pub(super) fn x11_unminimize_request(&mut self, window: X11Surface) {
        let xid = window.window_id();
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            return;
        };
        let restored = self.surfaces.get_mut(&object).and_then(|record| {
            (record.mapped && record.minimized).then(|| {
                record.minimized = false;
                record.role.wl_surface().clone()
            })
        });
        let Some(surface) = restored else {
            return;
        };
        self.minimized_toplevels.retain(|entry| *entry != object);
        let _ = window.set_suspended(false);
        self.recompute_effective_visibility();
        self.raise_surface(&surface);
        self.sync_xwm_stacking();
        self.arbitrate_keyboard_focus(Some(surface), false, false);
        self.retarget_pointer_after_visibility_change();
    }

    pub(super) fn x11_resize_request(
        &mut self,
        window: X11Surface,
        _button: u32,
        resize_edge: X11ResizeEdge,
    ) {
        if self.chrome_pointer_grab.is_some() || !self.x11_pointer_grab_targets(&window) {
            tracing::debug!(
                xid = window.window_id(),
                "rejected X11 resize request without matching pointer grab"
            );
            return;
        }
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(record) = self.surfaces.get(&surface.id()) else {
            return;
        };
        if record.committed_maximized {
            return;
        }
        self.interactive_pointer = Some(InteractivePointer::Resize {
            surface,
            edges: xdg_edge_for_x11(resize_edge),
            start_pointer: self.cursor_position,
            start_origin: record.window_origin,
            start_size: record.configured_size,
        });
    }

    pub(super) fn x11_move_request(&mut self, window: X11Surface, _button: u32) {
        if self.chrome_pointer_grab.is_some() || !self.x11_pointer_grab_targets(&window) {
            tracing::debug!(
                xid = window.window_id(),
                "rejected X11 move request without matching pointer grab"
            );
            return;
        }
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(record) = self.surfaces.get(&surface.id()) else {
            return;
        };
        if record.committed_maximized {
            return;
        }
        self.interactive_pointer = Some(InteractivePointer::Move {
            surface,
            start_pointer: self.cursor_position,
            start_origin: record.window_origin,
        });
    }

    pub(super) fn x11_allow_selection_access(&mut self, selection: SelectionTarget) -> bool {
        // Deliberate X-2 gap: no clipboard/primary bridging in X-1.
        //
        // X-2 AUTHOR, BEFORE RETURNING `true` HERE: this refusal is load-
        // bearing for teardown, not just scope. It is the only thing that
        // stops smithay from registering selection-transfer calloop sources
        // that call `xwm_state` and are never unregistered by `Drop for
        // X11Wm` — a wm dropped with a live transfer panics in `xwm_state`.
        // Revisit the `draining_wms` drain proof first.
        tracing::debug!(?selection, "refused X11 selection access (X-1)");
        false
    }

    pub(super) fn x11_send_selection(
        &mut self,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        // Defensive: reachable only if `allow_selection_access` ever returned
        // true, which X-1 never does. Drop the fd, log the invariant breach,
        // never panic (the trait default panics).
        //
        // X-2 AUTHOR: implementing this for real means comp drives outgoing
        // selection transfers, whose calloop sources call `xwm_state` and
        // outlive `Drop for X11Wm` — the `draining_wms` drain proof depends
        // on these transfers never existing. Revisit it before wiring this.
        drop(fd);
        tracing::error!(
            ?selection,
            mime_type,
            "send_selection reached despite X-1 refusing selection access; dropped fd"
        );
    }

    pub(super) fn x11_new_selection(
        &mut self,
        selection: SelectionTarget,
        mime_types: Vec<String>,
    ) {
        // Log-only in X-1; no bridge state is retained.
        tracing::debug!(
            ?selection,
            ?mime_types,
            "X11 selection changed (not bridged in X-1)"
        );
    }

    pub(super) fn x11_cleared_selection(&mut self, selection: SelectionTarget) {
        tracing::debug!(?selection, "X11 selection cleared (not bridged in X-1)");
    }

    pub(super) fn x11_randr_primary_output_change(&mut self, output_name: Option<String>) {
        // Deliberate X-3 gap: scale/output policy untouched; client scale
        // stays 1.
        tracing::debug!(
            ?output_name,
            "X11 RandR primary output changed (ignored in X-1)"
        );
    }
}

impl WaylandState {
    /// Whether an XWM callback belongs to the live generation. Buffered
    /// events from a draining (torn-down) wm still arrive until its channel
    /// delivers `Closed`; acting on them would repopulate the maps the
    /// teardown just cleared, so the delegates below drop them.
    fn xwm_event_is_live(&self, xwm: XwmId) -> bool {
        matches!(
            &self.xwayland.lifecycle,
            XwaylandLifecycle::Ready { wm, .. } if wm.id() == xwm
        )
    }
}

/// Thin delegation: every callback body lives in the inherent `x11_*`
/// methods above so the deterministic tests can drive it without a live
/// `XwmId`. Every delegate (except `xwm_state`/`disconnected`, which manage
/// the drain) is gated on the callback's generation being the live one.
///
/// Known blind spot, accepted with the split: because `XwmId` is
/// unconstructible offline, NO test executes this layer — a swapped
/// delegation (say `unmap` wired to `destroy`) would compile and stay
/// green. Each arm was verified by read in review (2026-09-02); the live
/// nested gate is the only executable check. Keep the arms boring and 1:1.
impl XwmHandler for WaylandState {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        if self.xwm_event_is_live(xwm) {
            match &mut self.xwayland.lifecycle {
                XwaylandLifecycle::Ready { wm, .. } => wm,
                _ => unreachable!("xwm_event_is_live guarantees Ready"),
            }
        } else {
            // Unreachable-by-construction fallback: every wm ever started is
            // either live or draining until its channel's final `Closed`
            // event, and the commit-hook route dies with the generation's
            // killed client.
            self.xwayland
                .draining_wms
                .iter_mut()
                .find(|wm| wm.id() == xwm)
                .unwrap_or_else(|| {
                    unreachable!(
                        "XWM callback for a generation that is neither live nor draining; \
                         a wm was dropped before its X11 event source delivered Closed"
                    )
                })
        }
    }

    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_new_window(window);
    }

    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_new_override_redirect_window(window);
    }

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_map_window_request(window);
    }

    fn map_window_notify(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_map_window_notify(window);
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_mapped_override_redirect_window(window);
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_unmapped_window(window);
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_destroyed_window(window);
    }

    #[allow(clippy::too_many_arguments)]
    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_configure_request(window, x, y, w, h, reorder);
    }

    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<X11Window>,
    ) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_configure_notify(window, geometry, above);
    }

    fn property_notify(&mut self, xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_property_notify(window, property);
    }

    fn maximize_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.request_x11_maximized(&window, true);
    }

    fn unmaximize_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.request_x11_maximized(&window, false);
    }

    fn fullscreen_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.request_x11_fullscreen(&window, true);
    }

    fn unfullscreen_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.request_x11_fullscreen(&window, false);
    }

    fn minimize_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_minimize_request(window);
    }

    fn unminimize_request(&mut self, xwm: XwmId, window: X11Surface) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_unminimize_request(window);
    }

    fn resize_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        button: u32,
        resize_edge: X11ResizeEdge,
    ) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_resize_request(window, button, resize_edge);
    }

    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_move_request(window, button);
    }

    fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool {
        if !self.xwm_event_is_live(xwm) {
            return false;
        }
        self.x11_allow_selection_access(selection)
    }

    fn send_selection(
        &mut self,
        xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        if !self.xwm_event_is_live(xwm) {
            // Stale generation: drop the fd, exactly as the live refusal does.
            drop(fd);
            return;
        }
        self.x11_send_selection(selection, mime_type, fd);
    }

    fn new_selection(&mut self, xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_new_selection(selection, mime_types);
    }

    fn cleared_selection(&mut self, xwm: XwmId, selection: SelectionTarget) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_cleared_selection(selection);
    }

    fn randr_primary_output_change(&mut self, xwm: XwmId, output_name: Option<String>) {
        if !self.xwm_event_is_live(xwm) {
            return;
        }
        self.x11_randr_primary_output_change(output_name);
    }

    fn disconnected(&mut self, xwm: XwmId) {
        let generation = self.xwayland.generation;
        if self.xwm_event_is_live(xwm) {
            self.fail_xwayland_generation(generation, "XWM connection closed");
        }
        // `Closed` is the channel's final event and the source removes
        // itself after delivering it, so nothing can ask `xwm_state` for
        // this id again: the drained wm (pushed by the teardown above, or by
        // an earlier one) can now be dropped.
        self.xwayland.draining_wms.retain(|wm| wm.id() != xwm);
    }
}

impl WaylandState {
    /// Publish the scene snapshot of a mapped X11 record. Used by the
    /// maximize/fullscreen state functions when the granted geometry did not
    /// change at all: `apply_x11_geometry` publishes nothing then, but the
    /// scene state (SSD maximize glyph, fullscreen flag) still flipped.
    fn publish_x11_state_relayout(&mut self, xid: X11Window) {
        let Some(record) = self.x11_role_record_mut(xid) else {
            return;
        };
        if !record.mapped {
            return;
        }
        let id = record.id;
        let scene = record.scene_snapshot();
        self.events
            .push(ProtocolEvent::SurfaceRelayout { id, scene });
    }

    /// X11 maximize/unmaximize: save/restore the normal geometry, configure
    /// the content rectangle immediately (no xdg serial), set the EWMH state.
    pub(super) fn request_x11_maximized(&mut self, window: &X11Surface, maximized: bool) {
        let xid = window.window_id();
        let usable = self.usable_output_rect();
        let extents = DecoExtents::of(&self.decoration.theme);
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            return;
        };
        let Some(surface) = self
            .surfaces
            .get(&object)
            .map(|record| record.role.wl_surface().clone())
        else {
            return;
        };
        self.cancel_chrome_pointer_grab_for_surface(&surface, true);
        self.titlebar_click_candidate = None;
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        let server_side = record.committed_decoration == SceneDecorationMode::ServerSide;
        let target = if maximized {
            if record.committed_maximized {
                return;
            }
            record.normal_restore = Some(NormalRestore {
                window_origin: record.window_origin,
                client_size: record.configured_size,
                output: usable,
                server_side,
            });
            let outer = vec2(usable.width, usable.height);
            let content = if server_side {
                extents.content_size_for_window(outer)
            } else {
                outer
            };
            let origin = if server_side {
                (usable.x + extents.left, usable.y + extents.top)
            } else {
                (usable.x, usable.y)
            };
            Rectangle::new(
                (origin.0 as i32, origin.1 as i32).into(),
                (
                    content.x.round().max(1.0) as i32,
                    content.y.round().max(1.0) as i32,
                )
                    .into(),
            )
        } else {
            if !record.committed_maximized {
                return;
            }
            let restore = record.normal_restore.take();
            let (origin, size) = restore
                .map(|restore| (restore.window_origin, restore.client_size))
                .unwrap_or((
                    (usable.x + CASCADE_ORIGIN, usable.y + CASCADE_ORIGIN),
                    record.configured_size,
                ));
            Rectangle::new(
                (origin.0 as i32, origin.1 as i32).into(),
                (size.0.max(1), size.1.max(1)).into(),
            )
        };
        let geometry_unchanged = record.window_origin == (target.loc.x as f32, target.loc.y as f32)
            && record.configured_size == (target.size.w.max(1), target.size.h.max(1));
        record.requested_maximized = maximized;
        record.committed_maximized = maximized;
        sync_toplevel_scene_state(record);
        if let Err(error) = window.configure(Some(target)) {
            tracing::warn!(xid, %error, "failed to configure X11 maximize geometry");
        }
        if let Err(error) = window.set_maximized(maximized) {
            tracing::warn!(xid, %error, "failed to set X11 EWMH maximized state");
        }
        self.apply_x11_geometry(xid, target);
        if geometry_unchanged {
            self.publish_x11_state_relayout(xid);
        }
        tracing::debug!(xid, maximized, "applied X11 maximize state");
    }

    /// X11 fullscreen: configure to the full output, suppress SSD while
    /// fullscreen, restore the prior policy on leave.
    pub(super) fn request_x11_fullscreen(&mut self, window: &X11Surface, fullscreen: bool) {
        let xid = window.window_id();
        let output = self.logical_output_rect();
        let Some(object) = self.xwayland.surfaces_by_xid.get(&xid).cloned() else {
            return;
        };
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        let SurfaceRole::X11(role) = &mut record.role else {
            return;
        };
        if role.fullscreen == fullscreen {
            return;
        }
        role.fullscreen = fullscreen;
        let target = if fullscreen {
            record.normal_restore = record.normal_restore.or(Some(NormalRestore {
                window_origin: record.window_origin,
                client_size: record.configured_size,
                output,
                server_side: record.committed_decoration == SceneDecorationMode::ServerSide,
            }));
            Rectangle::new(
                (output.x as i32, output.y as i32).into(),
                (output.width.max(1.0) as i32, output.height.max(1.0) as i32).into(),
            )
        } else {
            let restore = record.normal_restore.take();
            let (origin, size) = restore
                .map(|restore| (restore.window_origin, restore.client_size))
                .unwrap_or((
                    (output.x + CASCADE_ORIGIN, output.y + CASCADE_ORIGIN),
                    record.configured_size,
                ));
            Rectangle::new(
                (origin.0 as i32, origin.1 as i32).into(),
                (size.0.max(1), size.1.max(1)).into(),
            )
        };
        let geometry_unchanged = record.window_origin == (target.loc.x as f32, target.loc.y as f32)
            && record.configured_size == (target.size.w.max(1), target.size.h.max(1));
        if let Err(error) = window.configure(Some(target)) {
            tracing::warn!(xid, %error, "failed to configure X11 fullscreen geometry");
        }
        if let Err(error) = window.set_fullscreen(fullscreen) {
            tracing::warn!(xid, %error, "failed to set X11 EWMH fullscreen state");
        }
        self.apply_x11_geometry(xid, target);
        if geometry_unchanged {
            self.publish_x11_state_relayout(xid);
        }
        self.refresh_x11_decoration(xid);
        tracing::debug!(xid, fullscreen, "applied X11 fullscreen state");
    }
}

smithay::delegate_xwayland_shell!(WaylandState);
