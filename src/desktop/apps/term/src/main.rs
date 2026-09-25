//! CosMix Term — the lightweight frontend: iced 0.14 on its own winit/Wayland
//! backend (D2/D3), drawing `cosmix-term-core`'s grid through one persistent
//! wgpu texture per visible pane (D7).
//!
//! Tabs and split panes at parity with bterm (T3): the same tab and pane
//! model (`cosmix_term_core::tabs`), the same chords, and the same `term.*`
//! verbs — this binary registers the Bus name `term` and serves the core's
//! surface through `cosmix_term_core::bus`, so a `term.pane.split` from
//! another node and a Ctrl+Shift+E at the keyboard produce the same tree.
//! Runtime font sizing is foot's (T4): Ctrl +/-/0 and Ctrl+wheel, keeping the
//! window and changing the cell count.
//!
//! The two things that are requirements rather than optimisations, because
//! they are what the whole lane is for: the grid is re-rasterised **by damaged
//! row**, and it is rasterised **into one buffer per pane that lives while
//! the pane is on screen**. See `frame.rs` and
//! `cosmix_term_core::raster::render_into`.

mod frame;
mod input;
mod keys;
mod layout;
mod theme;

#[cfg(feature = "wgpu")]
mod wgpu_grid;

#[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
mod cpu_grid;

#[cfg(not(any(feature = "wgpu", feature = "tiny-skia")))]
compile_error!("term needs a renderer: enable the `wgpu` (default) or `tiny-skia` feature");

use cosmix_term_core::{
    bus, config,
    font::FontSize,
    panes::{Geometry, SplitDir},
    session_fd,
    tabs::{self, CompletionNote, Removed, TabSet},
    version::version_request,
    wake::WakeFd,
};
use frame::Painter;
use iced::widget::{Row, button, column, container, mouse_area, row, space, text};
use iced::{Background, Border, Element, Length, Size, Subscription, Task};
use input::Action;
use layout::{Node, Shape};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const DISPLAY_NAME: &str = "CosMix Term";
/// The Bus name and verb namespace this frontend owns (D1). The Bevy frontend
/// is `bterm` / `bterm.*`, so both can run at once — which T5's A/B needs.
const SERVICE: &str = "term";

fn main() {
    // FIRST, before the inherited-fd quarantine, the config read, the Wayland
    // check and the window: Mark's contract (2026-09-21) is that `--version`
    // reports the version and the build hash and does nothing else, whether or
    // not a term is already running and whether or not there is a display.
    // `build_info!()` expands HERE so the sha is this crate's, not the core's.
    if let Some(text) = version_request(
        &std::env::args().collect::<Vec<_>>(),
        cosmix_buildinfo::build_info!(),
    ) {
        println!("{text}");
        return;
    }
    session_fd::quarantine_inherited();
    if std::env::args().any(|arg| arg == "--help") {
        println!(
            "{DISPLAY_NAME}: tabbed Wayland Mix terminal (iced + wgpu frontend)\n\
             Font: TERM_SPIKE_FONT=/path/to/font.ttf, TERM_FONT_PX=<6..48>\n\
             Keys: Ctrl+Shift+T/W new/close tab, Ctrl+PageUp/PageDown change tab,\n\
             \x20     Ctrl+Shift+E/O split side by side/stacked, Ctrl+Shift+X close pane,\n\
             \x20     Ctrl+Shift+arrows move focus, Ctrl+Shift+Q quit,\n\
             \x20     Ctrl+Tab / Ctrl+Shift+Tab next / previous pane (wrap),\n\
             \x20     Ctrl+plus/equal/minus/0 (and Ctrl+wheel) font size\n\
             \x20     Wheel scrolls; Shift forces history; Shift+PageUp/PageDown page history\n\
             \x20     Shift+Home/End history top/bottom (primary screen only)\n\
             \x20     Input returns to bottom; bare Tab goes to the shell\n\
             TERM_NOTIFY=0: no desktop notification when a pane's shell exits\n\
             --version: print version and build hash, and nothing else\n\
             --print-config: print resolved startup settings and exit\n\
             Bus: serves `{SERVICE}` / `{SERVICE}.*`; the Bevy frontend is `bterm`"
        );
        return;
    }
    let settings = resolve_config(
        config::load(
            config::config_path(
                std::env::var_os("XDG_CONFIG_HOME").map(Into::into),
                std::env::var_os("HOME").map(Into::into),
            )
            .as_deref(),
        ),
        std::env::var("TERM_FONT_PX").ok().as_deref(),
        config::selected_term(),
    );
    if std::env::args().any(|arg| arg == "--print-config") {
        println!(
            "{}",
            serde_json::to_string_pretty(&settings).expect("validated config")
        );
        return;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("term requires a native Wayland session");
        std::process::exit(1);
    }
    if let Err(error) = run(settings) {
        eprintln!("term: {error}");
        std::process::exit(1);
    }
}

fn resolve_config(
    mut config: config::Config,
    env_font: Option<&str>,
    term: &'static str,
) -> config::Settings {
    if let Some(px) = env_font
        .and_then(|value| value.parse().ok())
        .filter(|px| config::valid_font(*px))
    {
        config.font_px = px;
    }
    config::Settings { config, term }
}

fn run(settings: config::Settings) -> Result<(), String> {
    // Scale 1.0 to start; the raster is rebuilt at the surface's real
    // fractional scale on the first `Rescaled`, so glyphs are rasterised at
    // physical resolution and the compositor never upscales them.
    let painter = Painter::new(
        1.0,
        FontSize::new(settings.config.font_px),
        settings.config.cursor,
    )?;
    let tabs = Arc::new(Mutex::new(TabSet::with_session(settings, None)?));
    let (cleanup, reaper) = tabs::Cleanup::start().map_err(|e| format!("cleanup worker: {e}"))?;

    // One eventfd for the whole frontend: every PTY, resize, pane exit and
    // Bus mutation coalesces onto it, and the UI thread learns of all of them
    // in one poll.
    let waker = Arc::new(Waker {
        fd: WakeFd::new().map_err(|e| format!("wake descriptor: {e}"))?,
        pending: AtomicBool::new(false),
        sender: Mutex::new(None),
        polling: AtomicBool::new(false),
    });
    tabs.lock().expect("tabs").set_wake(waker.fd.waker());
    WAKER
        .set(waker.clone())
        .map_err(|_| "wake descriptor installed twice".to_owned())?;

    // Completion notifications, exactly as bterm: a pane whose shell exits on
    // its own is reaped on the UI thread and handed to the Bus thread, which
    // emits interact.notify. TERM_NOTIFY=0 drops the sender, so notes are
    // never queued and the Bus task retires its receive branch.
    let notify_enabled = std::env::var("TERM_NOTIFY")
        .map(|value| value != "0")
        .unwrap_or(true);
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
    // The `term.*` surface (D1). With no broker the thread says so once and
    // returns; the terminal works either way.
    let bus = bus::start(SERVICE, tabs.clone(), cleanup.clone(), notify_rx);

    let state = State {
        painter,
        tabs: tabs.clone(),
        cleanup: cleanup.clone(),
        notify: notify_enabled.then_some(notify_tx),
        tokens: theme::tokens(),
        waker,
        window: Size::new(900.0, 560.0),
        shape: Shape::default(),
        grids: HashMap::new(),
        modifiers: iced::keyboard::Modifiers::empty(),
        wheel: 0.0,
        scroll_wheel: 0.0,
        scroll_pane: None,
        pointer: std::cell::Cell::new(None),
    };

    // `BootFn` is `Fn`, not `FnOnce`, and the state is not cloneable — the
    // PTYs, the eventfd and the glyph cache each exist exactly once. iced
    // calls boot a single time, so handing it over through a take-once cell
    // is exact rather than defensive; a second call would panic loudly
    // instead of silently booting a second terminal.
    let state = std::cell::RefCell::new(Some(state));
    let result = iced::application(
        move || {
            (
                state.borrow_mut().take().expect("iced boots once"),
                Task::none(),
            )
        },
        update,
        view,
    )
        .executor::<SingleThread>()
        .title(DISPLAY_NAME)
        .subscription(subscription)
        .theme(iced::Theme::Dark)
        .style(|state: &State, _theme| iced::theme::Style {
            background_color: state.tokens.surface,
            text_color: state.tokens.text,
        })
        .window(iced::window::Settings {
            size: Size::new(900.0, 560.0),
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: format!("dev.cosmix.{SERVICE}"),
                ..Default::default()
            },
            ..Default::default()
        })
        .run();

    // Same teardown ordering as bterm: shut the tabs (which releases the Bus
    // loop through `emptied`), let the Bus thread finish its bounded replies,
    // and only then drop the last Cleanup — the reaper's loop ends only when
    // EVERY sender is gone, and the Bus thread owns one.
    let removed = tabs.lock().expect("tabs").shutdown();
    cleanup.submit(removed);
    let _ = bus.join();
    drop(cleanup);
    let _ = reaper.join();
    result.map_err(|error| error.to_string())
}

/// One background thread for iced's `Task`s, instead of one per core.
///
/// iced's default executor is `futures::executor::ThreadPool::new()`, which
/// sizes itself to `num_cpus` — 18 threads on this workstation, measured, and
/// the whole of T5's thread-budget miss (30 threads against a gate of 24).
/// They are all parked: this frontend's only tasks are `window::scale_factor`
/// at boot and `exit`. A terminal's concurrency is one PTY per pane, and it is
/// already handled by the `poll(2)` thread and the reaper.
///
/// A pool rather than a `LocalPool` because `Executor::spawn` takes `&self`
/// and must not block the UI thread; pool_size(1) is the smallest thing that
/// still satisfies that contract.
struct SingleThread(iced::futures::executor::ThreadPool);

impl iced::Executor for SingleThread {
    fn new() -> Result<Self, iced::futures::io::Error> {
        iced::futures::executor::ThreadPool::builder()
            .pool_size(1)
            .name_prefix("term-task")
            .create()
            .map(Self)
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn_ok(future);
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        iced::futures::executor::block_on(future)
    }
}

type WakeSender = iced::futures::channel::mpsc::UnboundedSender<Message>;

struct Waker {
    fd: WakeFd,
    /// True between "the poll thread published a wake" and "the UI thread
    /// consumed it". Exact coalescing: the thread publishes only on the
    /// false->true edge, so a flood of PTY output cannot grow the queue, and
    /// nothing is ever dropped — a change landing after the UI thread cleared
    /// the flag publishes a fresh wake rather than being swallowed.
    pending: AtomicBool,
    /// Where the poll thread publishes. Swapped, not recreated, when iced
    /// rebuilds the subscription: a second poll thread would race the first
    /// for the same eventfd, and the loser's drain would silently eat wakes
    /// the winner never hears about (cold-review finding, 2026-09-21).
    sender: Mutex<Option<WakeSender>>,
    /// Set once, so exactly one thread ever owns the descriptor.
    polling: AtomicBool,
}

static WAKER: OnceLock<Arc<Waker>> = OnceLock::new();

struct State {
    tabs: Arc<Mutex<TabSet>>,
    cleanup: tabs::Cleanup,
    notify: Option<tokio::sync::mpsc::UnboundedSender<CompletionNote>>,
    painter: Painter,
    tokens: cosmix_iced_widgets::Tokens,
    waker: Arc<Waker>,
    /// Logical inner size of the window, as the compositor last reported it.
    window: Size,
    /// Tabs and the active pane tree as of the last wake — what `view` draws.
    shape: Shape,
    /// Columns and rows each visible pane's PTY has been told about. A pane
    /// missing here has not been sized yet, which forces its first resize.
    grids: HashMap<u64, (u16, u16)>,
    /// Tracked for Ctrl+wheel: a mouse event carries no modifier state.
    modifiers: iced::keyboard::Modifiers,
    /// Fractional Ctrl+wheel travel not yet worth a font step.
    wheel: f32,
    /// History and zoom gestures never share fractional travel.
    scroll_wheel: f32,
    scroll_pane: Option<u64>,
    /// Window coordinates survive a tab change beneath a stationary pointer.
    pointer: std::cell::Cell<Option<iced::Point>>,
}

#[derive(Debug, Clone)]
enum Message {
    /// Something in the core changed: PTY output, a resize, a pane exit, a
    /// Bus mutation.
    Wake,
    /// Keys to put on the PTY, from the widget tree — NOT from an event
    /// subscription, which drops them under load (see `keys.rs`).
    Keys(Vec<cosmix_term_core::terminal::Key>),
    /// A chord the terminal answers itself (tabs, panes, font size).
    Action(Action),
    Modifiers(iced::keyboard::Modifiers),
    SelectTab(u64),
    FocusPane(u64),
    Wheel(u64, iced::mouse::ScrollDelta),
    Pointer,
    Window(iced::window::Event),
    /// The window's device-pixel ratio, answered by the runtime.
    Scale(f32),
}

fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run(wakes),
        // WINDOW events only. Keys go through the widget tree instead,
        // because this path DROPS events under load — see `keys.rs`. Window
        // events survive it: they are rare, and a lost resize is corrected by
        // the next one. `listen_with` already filters RedrawRequested, so
        // this cannot feed itself.
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Window(event) => Some(Message::Window(event)),
            _ => None,
        }),
    ])
}

/// Publishes a [`Message::Wake`] whenever the core's eventfd fires.
///
/// A dedicated thread blocking in `poll(2)` rather than an async descriptor:
/// the executor here is a futures thread pool with no reactor, and a terminal
/// that is idle must cost nothing — this thread is parked in the kernel until
/// the PTY actually writes.
fn wakes() -> impl iced::futures::Stream<Item = Message> {
    let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
    let Some(waker) = WAKER.get().cloned() else {
        // Only reachable if the wiring order in `run` changes. The window
        // would come up and then never repaint, which reads as a hung shell
        // rather than a broken terminal — so say which it is.
        eprintln!("term: wake descriptor not installed before the event loop; the grid cannot repaint");
        return receiver;
    };
    // Re-arm before publishing anywhere: if a previous subscription was torn
    // down between the thread's `swap(true)` and the UI thread's clear, the
    // flag would be stuck true and every later wake silently suppressed.
    *waker.sender.lock().expect("wake sender") = Some(sender);
    waker.pending.store(false, Ordering::Release);
    // Also re-arm the descriptor: any wake the dropped receiver was holding
    // is gone, so ask for one unconditionally rather than wait for the next
    // PTY byte. A spurious repaint finds no damage and costs nothing.
    waker.fd.waker()();
    if waker.polling.swap(true, Ordering::AcqRel) {
        return receiver; // The one poll thread is already running.
    }
    let spawned = std::thread::Builder::new()
        .name("term-wake".into())
        .spawn(move || {
            loop {
                let mut fds = libc::pollfd {
                    fd: waker.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one live pollfd, blocking indefinitely.
                if unsafe { libc::poll(&mut fds, 1, -1) } < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                // Ready-but-not-readable means the descriptor is broken, not
                // that a wake arrived: `drain` would return false and the loop
                // would re-poll instantly, spinning a core forever. Stop
                // instead, and say so — a terminal that stops repainting is a
                // visible failure; one that pins a core is a mystery.
                if fds.revents & libc::POLLIN == 0 {
                    eprintln!("term: wake descriptor failed (revents {}); repaints have stopped", fds.revents);
                    return;
                }
                // Drain BEFORE publishing, never after: a change that lands
                // while the UI thread reads the grid then leaves the
                // descriptor readable for the next turn (one redundant,
                // damage-free repaint) instead of being lost.
                if !waker.fd.drain() {
                    continue;
                }
                if waker.pending.swap(true, Ordering::AcqRel) {
                    continue; // A wake is already queued; this one coalesces.
                }
                let sender = waker.sender.lock().expect("wake sender").clone();
                match sender {
                    Some(sender) if sender.unbounded_send(Message::Wake).is_ok() => {}
                    // The receiver is gone. Leave `pending` false so the next
                    // subscription is not born latched shut, and keep polling:
                    // this thread owns the descriptor for the process's life.
                    _ => waker.pending.store(false, Ordering::Release),
                }
            }
        });
    if let Err(error) = spawned {
        waker_spawn_failed(&error);
    }
    receiver
}

fn waker_spawn_failed(error: &std::io::Error) {
    // Not a warning to carry on past: with no poll thread the grid never
    // repaints and the window is a frozen picture of the first frame, which
    // looks like a hung shell rather than a failed terminal.
    eprintln!("term: cannot start the wake thread ({error}); the grid would never repaint");
    std::process::exit(1);
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Wake => {
            state.waker.pending.store(false, Ordering::Release);
            return state.sync();
        }
        Message::Scale(scale) => state.rescale(scale),
        Message::Keys(keys) => state.send_keys(keys),
        Message::Action(action) => return state.act(action),
        Message::Modifiers(modifiers) => {
            if modifiers != state.modifiers {
                state.scroll_wheel = 0.0;
            }
            // Letting go of Ctrl ends a Ctrl+wheel gesture: travel short of a
            // step must not carry into the next one and zoom early.
            if !modifiers.control() {
                state.wheel = 0.0;
            }
            state.modifiers = modifiers;
        }
        Message::SelectTab(id) => {
            let mut tabs = state.tabs.lock().expect("tabs");
            tabs.user_activity();
            tabs.select(id);
        }
        Message::FocusPane(id) => {
            let mut tabs = state.tabs.lock().expect("tabs");
            tabs.user_activity();
            tabs.focus(id);
        }
        Message::Pointer => {},
        Message::Wheel(id, delta) => {
            if state.modifiers.control() {
                let steps = input::wheel_steps(&mut state.wheel, delta);
                if steps != 0 {
                    state.zoom(|font| font.step_by(steps));
                }
            } else {
                state.scroll(id, delta);
            }
        }
        Message::Window(event) => match event {
            iced::window::Event::Opened { size, .. } => {
                state.resize(size);
                // Ask rather than wait: winit does not necessarily emit a
                // Rescaled for the scale a surface is BORN at, and a terminal
                // that renders one frame at the wrong scale is a terminal
                // that starts blurry.
                return iced::window::latest()
                    .and_then(iced::window::scale_factor)
                    .map(Message::Scale);
            }
            iced::window::Event::Resized(size) => state.resize(size),
            iced::window::Event::Rescaled(scale) => state.rescale(scale),
            // A release that happens while another window has the keyboard
            // is never delivered; a latched Ctrl would turn every later wheel
            // into a zoom.
            iced::window::Event::Unfocused => {
                state.modifiers = iced::keyboard::Modifiers::empty();
                state.wheel = 0.0;
                state.scroll_wheel = 0.0;
            }
            iced::window::Event::CloseRequested => return iced::exit(),
            _ => {}
        },
    }
    Task::none()
}

/// The keyboard, routed from the widget tree. A terminal chord wins over the
/// shell encoder — without that order, Ctrl+Shift+T would reach the PTY as a
/// Ctrl-T (`input::tests::a_tab_chord_would_otherwise_reach_the_shell_as_a_control_code`).
fn on_key(event: &iced::keyboard::Event) -> Option<Message> {
    on_key_screen(event, false)
}

fn on_key_screen(event: &iced::keyboard::Event, alternate: bool) -> Option<Message> {
    match event {
        iced::keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            text,
            modifiers,
            repeat,
            ..
        } => {
            if let Some(action) = input::action_on_screen(
                input::action_for(key, modified_key, *physical_key, *modifiers), alternate,
            ) {
                // A repeat of a non-repeating chord is swallowed, not passed
                // through: it must not turn into a control code either.
                return (!*repeat || action.repeats()).then_some(Message::Action(action));
            }
            let keys = input::keys_for(key, text.as_deref(), *modifiers);
            (!keys.is_empty()).then_some(Message::Keys(keys))
        }
        iced::keyboard::Event::ModifiersChanged(modifiers) => Some(Message::Modifiers(*modifiers)),
        _ => None,
    }
}

fn view(state: &State) -> Element<'_, Message> {
    let tokens = state.tokens;
    let scale = state.painter.scale();
    let bounds = layout::content(state.window.width, state.window.height, scale);
    let panes: Element<'_, Message> = match &state.shape.tree {
        Some(tree) => pane_tree(state, tree, bounds, scale),
        None => space().into(),
    };
    // The keyboard rides the widget tree, not a subscription: see `keys.rs`.
    // It wraps the strip as well, so a key pressed while the pointer is over
    // a tab still reaches the terminal.
    let pane_bounds = state.shape.tree.as_ref()
        .map(|tree| layout::panes(tree, bounds, scale)).unwrap_or_default();
    let hovered = move |position: iced::Point| {
        let (id, pane) = pane_bounds.iter().find(|(_, pane)| {
            position.x >= pane.x && position.x < pane.x + pane.w
                && position.y >= pane.y && position.y < pane.y + pane.h
        })?;
        let grid = *state.grids.get(id)?;
        let (col, row) = input::pointer_cell(
            iced::Point::new(position.x - pane.x, position.y - pane.y),
            layout::border(scale), state.painter.logical_cell(), grid,
        );
        Some((*id, col, row))
    };
    let last = std::cell::Cell::new(state.pointer.get().and_then(&hovered));
    let content = keys::keys(column![tab_strip(state, scale), panes], move |event| {
        let message = on_key(event);
        // Ordinary typing must not acquire an extra terminal/grid lock just
        // to decide who owns a scrollback chord.
        if matches!(message, Some(Message::Action(Action::Scroll(_)))) {
            let tabs = state.tabs.lock().expect("tabs");
            if !tabs.is_empty() && tabs.active_terminal().lock().expect("terminal").alternate_screen() {
                return on_key_screen(event, true);
            }
        }
        message
    }).on_pointer(move |position| {
        let cell = hovered(position);
        pointer_message(&state.pointer, &last, cell, position)
    });
    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(tokens.surface.into()),
            ..container::Style::default()
        })
        .into()
}

type HoveredCell = Option<(u64, u16, u16)>;

fn pointer_message(
    pointer: &std::cell::Cell<Option<iced::Point>>,
    last: &std::cell::Cell<HoveredCell>,
    hovered: HoveredCell,
    position: iced::Point,
) -> Option<Message> {
    // Keep pixel coordinates even when no application update is needed. A queued
    // Pointer message must never overwrite a newer coalesced position.
    pointer.set(Some(position));
    (last.replace(hovered) != hovered).then_some(Message::Pointer)
}

/// One button per tab and a `+`, as bterm. Every colour is a design token.
fn tab_strip(state: &State, scale: f32) -> Element<'_, Message> {
    let tokens = state.tokens;
    let tab = |label: String, active: bool| {
        button(text(label).size(13.0))
            .padding([3.0, 12.0])
            .style(move |_theme: &iced::Theme, status: button::Status| {
                let (background, text_color) = if active {
                    (tokens.primary, tokens.primary_text)
                } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                    (tokens.muted_surface, tokens.text)
                } else {
                    (tokens.card, tokens.muted_text)
                };
                button::Style {
                    background: Some(Background::Color(background)),
                    text_color,
                    border: Border {
                        radius: tokens.radius.into(),
                        ..Border::default()
                    },
                    ..button::Style::default()
                }
            })
    };
    let mut strip: Row<'_, Message> = Row::new().spacing(4.0).padding([3.0, 6.0]);
    for label in &state.shape.tabs {
        strip = strip.push(tab(label.title.clone(), label.active).on_press(Message::SelectTab(label.id)));
    }
    strip = strip.push(tab("+".into(), false).on_press(Message::Action(Action::NewTab)));
    container(strip)
        .width(Length::Fill)
        .height(Length::Fixed(layout::strip_height(scale)))
        .style(move |_theme| container::Style {
            background: Some(tokens.card.into()),
            ..container::Style::default()
        })
        .into()
}

/// The active tab's panes as nested rows and columns, split with the same
/// function that sized their PTYs (`layout::split`), so a pane's widget and
/// its grid always agree on its rectangle.
fn pane_tree<'a>(state: &'a State, node: &Node, bounds: Geometry, scale: f32) -> Element<'a, Message> {
    match node {
        Node::Leaf(id) => pane(state, *id, bounds, scale),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = layout::split(*dir, *ratio, bounds, scale);
            let first = pane_tree(state, first, a, scale);
            let second = pane_tree(state, second, b, scale);
            match dir {
                SplitDir::Vertical => row![first, second].into(),
                SplitDir::Horizontal => column![first, second].into(),
            }
        }
    }
}

/// One pane: a border in the focus colour, the themed surface, and the grid
/// sized to its cells exactly, so the texture maps 1:1 to physical pixels and
/// the nearest sampler never resamples a glyph.
fn pane(state: &State, id: u64, bounds: Geometry, scale: f32) -> Element<'_, Message> {
    let tokens = state.tokens;
    let (cell_width, cell_height) = state.painter.logical_cell();
    let grid: Element<'_, Message> = match (state.painter.existing(id), state.grids.get(&id)) {
        (Some(frame), Some(&(cols, rows))) => renderer(state, id, frame)
            .width(Length::Fixed(f32::from(cols) * cell_width))
            .height(Length::Fixed(f32::from(rows) * cell_height))
            .into(),
        // Not sized or not painted yet: the next wake does both.
        _ => space().into(),
    };
    let frame_colour = frame_colour(&state.shape, id, tokens);
    let inner = container(grid)
        .width(Length::Fill)
        .height(Length::Fill)
        // A pane smaller than two columns still gets a two-column PTY; its
        // texture must not paint over the neighbour.
        .clip(true)
        .style(move |_theme| container::Style {
            background: Some(tokens.surface.into()),
            ..container::Style::default()
        });
    let outer = container(inner)
        .padding(layout::border(scale))
        .width(Length::Fixed(bounds.w))
        .height(Length::Fixed(bounds.h))
        .style(move |_theme| container::Style {
            background: Some(frame_colour.into()),
            ..container::Style::default()
        });
    mouse_area(outer)
        .on_press(Message::FocusPane(id))
        .on_scroll(move |delta| Message::Wheel(id, delta))
        .into()
}

/// A pane's border colour. The focus ring marks which pane keys go to, so it
/// shows only when there is a choice: a lone pane wears the plain border, as
/// foot shows nothing at all. The border's WIDTH never changes — that is what
/// keeps focus changes from resizing a PTY — only its colour.
fn frame_colour(shape: &Shape, id: u64, tokens: cosmix_iced_widgets::Tokens) -> iced::Color {
    if id == shape.active_pane && shape.visible().len() > 1 {
        tokens.ring
    } else {
        tokens.border
    }
}

#[cfg(feature = "wgpu")]
fn renderer(
    _state: &State,
    _id: u64,
    frame: Arc<Mutex<frame::Frame>>,
) -> iced::widget::Shader<Message, wgpu_grid::GridProgram> {
    iced::widget::shader(wgpu_grid::GridProgram::new(frame))
}

#[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
fn renderer(
    _state: &State,
    _id: u64,
    frame: Arc<Mutex<frame::Frame>>,
) -> iced::widget::Image<iced::widget::image::Handle> {
    cpu_grid::view(&frame)
}

/// Apply a tab or pane chord to the tab set, returning what to tear down.
///
/// The same `TabSet` calls bterm's keyboard handler makes, so the two
/// frontends do the same thing for the same chord. Font chords are not tab
/// operations and never reach here.
fn apply(tabs: &mut TabSet, action: Action) -> Vec<Removed> {
    if tabs.is_empty() {
        return Vec::new();
    }
    tabs.user_activity();
    match action {
        Action::NewTab => {
            if let Err(error) = tabs.open() {
                eprintln!("new tab: {error}");
            }
            Vec::new()
        }
        Action::CloseTab => {
            let id = tabs.active_id();
            tabs.close(id).1.into_iter().collect()
        }
        Action::Quit => tabs.shutdown(),
        Action::Split(dir) => {
            if let Err(error) = tabs.split_active(dir) {
                eprintln!("split pane: {error}");
            }
            Vec::new()
        }
        Action::ClosePane => tabs.close_active().1.into_iter().collect(),
        Action::Focus(direction) => {
            tabs.focus_dir(direction);
            Vec::new()
        }
        Action::Cycle { forward } => {
            tabs.cycle(forward);
            Vec::new()
        }
        Action::CyclePane { forward } => {
            let ids: Vec<_> = tabs.leaves().iter().map(|pane| pane.id).collect();
            if let Some(id) = input::cycle_pane(&ids, tabs.active_tab().active_pane, forward) {
                tabs.focus(id);
            }
            Vec::new()
        }
        Action::Scroll(request) => {
            tabs.active_terminal()
                .lock()
                .expect("terminal")
                .scroll_view(request);
            Vec::new()
        }
        Action::FontIncrease | Action::FontDecrease | Action::FontReset => Vec::new(),
    }
}

impl State {
    fn scroll(&mut self, id: u64, delta: iced::mouse::ScrollDelta) {
        if self.scroll_pane != Some(id) {
            self.scroll_wheel = 0.0;
            self.scroll_pane = Some(id);
        }
        let Some(position) = self.pointer.get() else {
            return;
        };
        let Some(tree) = &self.shape.tree else {
            return;
        };
        let scale = self.painter.scale();
        let bounds = layout::content(self.window.width, self.window.height, scale);
        let Some((_, pane)) = layout::panes(tree, bounds, scale)
            .into_iter()
            .find(|(pane, _)| *pane == id)
        else {
            return;
        };
        let Some(&grid) = self.grids.get(&id) else {
            return;
        };
        let lines = input::scroll_steps(&mut self.scroll_wheel, delta, self.painter.logical_cell().1);
        if lines == 0 {
            return;
        }
        let (col, row) = input::pointer_cell(
            iced::Point::new(position.x - pane.x, position.y - pane.y),
            layout::border(scale),
            self.painter.logical_cell(),
            grid,
        );
        let tabs = self.tabs.lock().expect("tabs");
        let Some(terminal) = tabs.pane_by_id(id) else {
            return;
        };
        tabs.user_activity();
        drop(tabs);
        let terminal = terminal.lock().expect("terminal");
        let mods = cosmix_term_core::terminal::MouseModifiers {
            shift: self.modifiers.shift(),
            alt: self.modifiers.alt(),
            ctrl: self.modifiers.control(),
        };
        if !terminal.mouse_scroll(col, row, lines, mods) {
            terminal.scroll_wheel(lines, mods);
        }
    }

    /// Everything a wake can mean, in one place: reap exited shells, follow
    /// the tab set's shape (a Bus verb may have changed it), size any pane
    /// that needs it, and repaint each visible pane by its damaged rows.
    fn sync(&mut self) -> Task<Message> {
        let (removed, notes) = self.tabs.lock().expect("tabs").reap_exited();
        self.cleanup.submit(removed);
        if let Some(notify) = &self.notify {
            for note in notes {
                let _ = notify.send(note);
            }
        }
        let shape = {
            let tabs = self.tabs.lock().expect("tabs");
            if tabs.is_empty() {
                return iced::exit();
            }
            Shape::of(&tabs)
        };
        let visible = shape.visible();
        self.painter.retain(&visible);
        self.grids.retain(|id, _| visible.contains(id));
        self.shape = shape;
        self.relayout();
        self.repaint();
        Task::none()
    }

    /// Rasterise every visible pane by its damaged rows (all rows, for a
    /// frame that was invalidated or is new).
    fn repaint(&mut self) {
        let terminals: Vec<_> = {
            let tabs = self.tabs.lock().expect("tabs");
            self.shape
                .visible()
                .into_iter()
                .filter_map(|id| tabs.pane_by_id(id).map(|terminal| (id, terminal)))
                .collect()
        };
        for (id, terminal) in terminals {
            let snapshot = terminal.lock().expect("terminal").grid_snapshot();
            #[allow(unused_variables)]
            let painted = self
                .painter
                .repaint(id, &snapshot.screen, &snapshot.dirty_rows);
            #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
            if painted {
                let frame = self.painter.frame(id);
                cpu_grid::refresh(&frame);
            }
        }
    }

    fn act(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::FontIncrease => self.zoom(FontSize::increase),
            Action::FontDecrease => self.zoom(FontSize::decrease),
            Action::FontReset => self.zoom(FontSize::reset),
            _ => {
                let removed = apply(&mut self.tabs.lock().expect("tabs"), action);
                self.cleanup.submit(removed);
                if action == Action::Quit {
                    return iced::exit();
                }
                // Every other mutation notifies the wake, and the next
                // `sync` picks up the new shape.
            }
        }
        Task::none()
    }

    /// foot's behaviour: the window keeps its size and the grid reflows to
    /// the new cell. Every visible pane is re-rasterised (they share the
    /// glyph cache) and every PTY is told its new size.
    fn zoom(&mut self, change: impl FnOnce(&mut FontSize) -> bool) {
        match self.painter.zoom(change) {
            Ok(true) => self.reflow(),
            Ok(false) => {}
            // Keep the old raster: a terminal at the old size is legible, and
            // a terminal with no raster is not a terminal.
            Err(error) => eprintln!("term: font resize: {error}"),
        }
    }

    fn resize(&mut self, window: Size) {
        // An unsized grid must force a layout even when the size is
        // unchanged. Otherwise a compositor that grants exactly the requested
        // 900x560 on a scale-1 output takes BOTH early returns — this one and
        // `rescale`'s — and no pane is ever sized: a terminal window with
        // nothing in it, forever (cold-review finding, 2026-09-21).
        if self.window == window && !self.grids.is_empty() {
            return;
        }
        self.window = window;
        self.relayout();
    }

    fn rescale(&mut self, scale: f32) {
        match self.painter.set_scale(scale) {
            Ok(true) => self.reflow(),
            Ok(false) => {}
            // Keep the old raster: a terminal at the wrong scale is legible,
            // and a terminal with no raster is not a terminal.
            Err(error) => eprintln!("term: raster rebuild at scale {scale}: {error}"),
        }
    }

    /// The cell size changed under the same window: forget every pane's
    /// grid so `relayout` resizes them all, then repaint NOW. The PTY resize
    /// is synchronous, so the snapshots already have the new size; waiting
    /// for the next wake instead would let `view` lay out the new grid size
    /// over the old surface, which keeps its dimensions through `invalidate`
    /// — one stretched frame per zoom step (review finding).
    fn reflow(&mut self) {
        self.grids.clear();
        self.relayout();
        self.repaint();
    }

    /// Size every visible pane from the window and tell each PTY that moved.
    fn relayout(&mut self) {
        let Some(tree) = &self.shape.tree else {
            return; // Nothing synced yet; the first wake lays out.
        };
        let scale = self.painter.scale();
        let cell = self.painter.cell();
        if cell.0 == 0 || cell.1 == 0 {
            return;
        }
        let bounds = layout::content(self.window.width, self.window.height, scale);
        let placed = layout::panes(tree, bounds, scale);
        let mut resize = Vec::new();
        {
            let mut tabs = self.tabs.lock().expect("tabs");
            if tabs.is_empty() {
                return;
            }
            for (id, geometry) in &placed {
                // Logical px relative to the pane area — the frame bterm
                // reports through `term.panes`, and what `focus_dir` reads.
                tabs.geometry(
                    *id,
                    Geometry {
                        x: geometry.x - bounds.x,
                        y: geometry.y - bounds.y,
                        ..*geometry
                    },
                );
                let grid = layout::grid(*geometry, cell, scale);
                if self.grids.get(id) != Some(&grid)
                    && let Some(terminal) = tabs.pane_by_id(*id)
                {
                    resize.push((*id, grid, terminal));
                }
            }
        }
        // Record a grid only once it has reached its PTY: recording first and
        // then failing to find the pane would leave `grids` describing a
        // resize nothing was told about, and the equality check would
        // suppress the retry. Terminal locks are taken without the set lock,
        // as everywhere else in this frontend.
        for (id, (cols, rows), terminal) in resize {
            // Physical pixels to the PTY: ioctl TIOCSWINSZ's ws_xpixel is
            // what a full-screen program asks for when it wants real geometry.
            terminal.lock().expect("terminal").resize(
                cols,
                rows,
                cols * cell.0 as u16,
                rows * cell.1 as u16,
            );
            self.tabs.lock().expect("tabs").resized(id, cols, rows);
            self.grids.insert(id, (cols, rows));
        }
    }

    fn send_keys(&mut self, keys: Vec<cosmix_term_core::terminal::Key>) {
        if keys.is_empty() {
            return;
        }
        let tabs = self.tabs.lock().expect("tabs");
        if tabs.is_empty() {
            return;
        }
        tabs.user_activity();
        // The focused pane of the active tab.
        let terminal = tabs.active_terminal();
        drop(tabs);
        let terminal = terminal.lock().expect("terminal");
        let at = Instant::now();
        for key in keys {
            if let Err(error) = terminal.key(key, at) {
                eprintln!("term input: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_term_core::panes::Direction;

    #[test]
    fn an_env_font_size_overrides_the_config_only_when_it_is_valid() {
        let base = config::Config {
            font_px: 13.0,
            ..config::Config::default()
        };
        assert_eq!(resolve_config(base, Some("18.5"), "xterm").config.font_px, 18.5);
        for rejected in ["5.9", "48.1", "", "eighteen", "nan", "inf"] {
            assert_eq!(
                resolve_config(base, Some(rejected), "xterm").config.font_px,
                13.0,
                "accepted {rejected}"
            );
        }
        assert_eq!(resolve_config(base, None, "xterm").config.font_px, 13.0);
    }

    fn press(
        key: iced::keyboard::Key,
        modified: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text: Option<&str>,
        repeat: bool,
    ) -> iced::keyboard::Event {
        iced::keyboard::Event::KeyPressed {
            key,
            modified_key: modified,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers,
            text: text.map(Into::into),
            repeat,
        }
    }

    fn character(c: &str) -> iced::keyboard::Key {
        iced::keyboard::Key::Character(c.into())
    }

    #[test]
    fn scroll_chords_and_repeats_never_reach_the_shell() {
        use cosmix_term_core::terminal::ScrollRequest;
        use iced::keyboard::{Key, Modifiers, key::Named};
        for (named, request) in [
            (Named::PageUp, ScrollRequest::PageUp),
            (Named::PageDown, ScrollRequest::PageDown),
            (Named::Home, ScrollRequest::Top),
            (Named::End, ScrollRequest::Bottom),
        ] {
            for repeat in [false, true] {
                assert!(matches!(
                    on_key(&press(Key::Named(named), Key::Named(named), Modifiers::SHIFT, None, repeat)),
                    Some(Message::Action(Action::Scroll(actual))) if actual == request
                ));
                assert!(matches!(
                    on_key(&press(Key::Named(named), Key::Named(named), Modifiers::empty(), None, repeat)),
                    Some(Message::Keys(_))
                ));
                assert!(matches!(
                    on_key_screen(&press(Key::Named(named), Key::Named(named), Modifiers::SHIFT, None, repeat), true),
                    Some(Message::Keys(_))
                ));
            }
        }
    }

    #[test]
    fn pointer_motion_emits_only_for_a_new_pane_or_cell() {
        let pointer = std::cell::Cell::new(None);
        let last = std::cell::Cell::new(None);
        assert!(pointer_message(&pointer, &last, Some((1, 0, 0)), iced::Point::new(2.0, 2.0)).is_some());
        for pixel in 3..8 {
            assert!(pointer_message(&pointer, &last, Some((1, 0, 0)), iced::Point::new(pixel as f32, 2.0)).is_none());
        }
        assert_eq!(pointer.get(), Some(iced::Point::new(7.0, 2.0)));
        // A narrower cell after zoom/layout uses the newest pixel, not x=2.
        assert_eq!(input::pointer_cell(pointer.get().unwrap(), 0.0, (4.0, 4.0), (80, 24)), (1, 0));
        assert!(pointer_message(&pointer, &last, Some((1, 1, 0)), iced::Point::new(9.0, 2.0)).is_some());
        assert!(pointer_message(&pointer, &last, Some((2, 1, 0)), iced::Point::new(90.0, 2.0)).is_some());
        assert!(pointer_message(&pointer, &last, None, iced::Point::ORIGIN).is_some());
        assert!(pointer_message(&pointer, &last, None, iced::Point::ORIGIN).is_none());
    }

    /// `on_key` is the dispatcher that decides chord versus shell and
    /// filters repeats; review finding: nothing exercised it.
    #[test]
    fn the_dispatcher_puts_chords_before_the_shell_and_filters_repeats() {
        use iced::keyboard::Modifiers;
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let tab = |repeat| on_key(&press(character("t"), character("T"), ctrl_shift, Some("T"), repeat));

        assert!(matches!(tab(false), Some(Message::Action(Action::NewTab))));
        // A held Ctrl+Shift+T is one tab: the repeat is swallowed, and it is
        // NOT handed to the encoder, which would send Ctrl-T to the shell.
        assert!(tab(true).is_none(), "a repeated tab chord must be swallowed");

        // Font steps repeat when held, as in foot.
        let grow = |repeat| on_key(&press(character("="), character("="), Modifiers::CTRL, None, repeat));
        assert!(matches!(grow(false), Some(Message::Action(Action::FontIncrease))));
        assert!(matches!(grow(true), Some(Message::Action(Action::FontIncrease))));

        // Not a chord: the shell gets it, repeats included.
        for repeat in [false, true] {
            assert!(matches!(
                on_key(&press(character("a"), character("a"), Modifiers::empty(), Some("a"), repeat)),
                Some(Message::Keys(keys)) if keys.len() == 1
            ));
        }
        // Ctrl+T without Shift is the shell's Ctrl-T, not a chord.
        assert!(matches!(
            on_key(&press(character("t"), character("t"), Modifiers::CTRL, None, false)),
            Some(Message::Keys(_))
        ));
        // Modifier state is tracked for Ctrl+wheel.
        assert!(matches!(
            on_key(&iced::keyboard::Event::ModifiersChanged(Modifiers::CTRL)),
            Some(Message::Modifiers(modifiers)) if modifiers.control()
        ));
    }

    #[test]
    fn the_focus_ring_shows_only_when_there_is_more_than_one_pane() {
        let tokens = theme::tokens();
        assert_ne!(tokens.ring, tokens.border, "the test needs two distinct tokens");
        let lone = Shape {
            tabs: Vec::new(),
            tree: Some(Node::Leaf(7)),
            active_pane: 7,
        };
        assert_eq!(frame_colour(&lone, 7, tokens), tokens.border);

        let split = Shape {
            tree: Some(Node::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Leaf(7)),
                second: Box::new(Node::Leaf(8)),
            }),
            ..lone
        };
        assert_eq!(frame_colour(&split, 7, tokens), tokens.ring);
        assert_eq!(frame_colour(&split, 8, tokens), tokens.border);
    }

    /// A real `State` with a PTY-backed tab set, no window and no Bus.
    fn test_state() -> (State, std::thread::JoinHandle<()>) {
        let (cleanup, reaper) = tabs::Cleanup::start().expect("cleanup worker");
        let waker = Arc::new(Waker {
            fd: WakeFd::new().expect("eventfd"),
            pending: AtomicBool::new(false),
            sender: Mutex::new(None),
            polling: AtomicBool::new(false),
        });
        let tabs = Arc::new(Mutex::new(layout::test_tabs()));
        tabs.lock().unwrap().set_wake(waker.fd.waker());
        let state = State {
            painter: Painter::new(1.0, FontSize::new(13.0), config::Cursor::Underline)
                .expect("a monospace font"),
            tabs,
            cleanup,
            notify: None,
            tokens: theme::tokens(),
            waker,
            window: Size::new(900.0, 560.0),
            shape: Shape::default(),
            grids: HashMap::new(),
            modifiers: iced::keyboard::Modifiers::empty(),
            wheel: 0.0,
            scroll_wheel: 0.0,
            scroll_pane: None,
            pointer: std::cell::Cell::new(None),
        };
        (state, reaper)
    }

    /// Review finding: a zoom used to relayout and then only WAKE, so the
    /// next `view` laid the new grid size over the old surface — one
    /// stretched frame per step. After the zoom returns, every visible frame
    /// must already be exactly its grid in the new cell size.
    #[test]
    fn a_zoom_repaints_before_the_next_view() {
        let (mut state, reaper) = test_state();
        let _ = state.sync();
        let _ = state.act(Action::Split(SplitDir::Vertical));
        let _ = state.sync();
        assert_eq!(state.shape.visible().len(), 2);

        let before = state.painter.cell();
        state.zoom(|font| font.step_by(6));
        let cell = state.painter.cell();
        assert_ne!(cell, before, "six steps must change the cell");
        for id in state.shape.visible() {
            let (cols, rows) = state.grids[&id];
            let frame = state.painter.existing(id).expect("a visible pane has a frame");
            let frame = frame.lock().unwrap();
            assert_eq!(
                (frame.surface().width(), frame.surface().height()),
                (u32::from(cols) * cell.0, u32::from(rows) * cell.1),
                "pane {id} still holds the pre-zoom surface"
            );
        }

        let removed = state.tabs.lock().unwrap().shutdown();
        state.cleanup.submit(removed);
        drop(state);
        reaper.join().unwrap();
    }

    /// Review finding: wheel travel short of a step survived letting go of
    /// Ctrl, so the next Ctrl+wheel gesture zoomed early.
    #[test]
    fn releasing_ctrl_forgets_partial_wheel_travel() {
        use iced::keyboard::Modifiers;
        use iced::mouse::ScrollDelta;
        let (mut state, reaper) = test_state();
        let _ = state.sync();
        let start = state.painter.font().current();
        let travel = |fraction: f32| {
            Message::Wheel(0, ScrollDelta::Pixels {
                x: 0.0,
                y: input::PIXELS_PER_STEP * fraction,
            })
        };

        let _ = update(&mut state, Message::Modifiers(Modifiers::CTRL));
        let _ = update(&mut state, travel(0.75));
        assert_eq!(state.painter.font().current(), start, "three quarters is not a step");
        let _ = update(&mut state, Message::Modifiers(Modifiers::empty()));
        let _ = update(&mut state, Message::Modifiers(Modifiers::CTRL));
        let _ = update(&mut state, travel(0.5));
        assert_eq!(
            state.painter.font().current(),
            start,
            "a new gesture of half a step zoomed: the old three quarters carried over"
        );
        let _ = update(&mut state, travel(0.5));
        assert_ne!(state.painter.font().current(), start, "a whole step within one gesture zooms");

        let removed = state.tabs.lock().unwrap().shutdown();
        state.cleanup.submit(removed);
        drop(state);
        reaper.join().unwrap();
    }

    #[test]
    fn wheel_targets_the_hovered_pane_without_focus_or_zoom_travel_leaking() {
        use iced::keyboard::Modifiers;
        use iced::mouse::ScrollDelta;
        let (mut state, reaper) = test_state();
        let _ = state.sync();
        let left = state.shape.active_pane;
        let _ = state.act(Action::Split(SplitDir::Vertical));
        let _ = state.sync();
        let right = state.shape.active_pane;
        assert_ne!(left, right);
        let terminal = state.tabs.lock().unwrap().pane_by_id(left).unwrap();
        fill_history(&terminal);
        state.pointer.set(Some(iced::Point::new(10.0, 40.0)));
        let half = ScrollDelta::Pixels { x: 0.0, y: state.painter.logical_cell().1 / 2.0 };
        let _ = update(&mut state, Message::Wheel(left, half));
        assert_eq!(state.scroll_pane, Some(left));
        assert_eq!(state.scroll_wheel, 0.5);
        assert_eq!(active_pane(&state.tabs.lock().unwrap()), right);
        let _ = update(&mut state, Message::Wheel(left, half));
        assert_eq!(terminal.lock().unwrap().display_offset(), 1, "two half-cell deltas scroll the hovered pane");
        let other = state.tabs.lock().unwrap().pane_by_id(right).unwrap();
        assert_eq!(other.lock().unwrap().display_offset(), 0);
        assert_eq!(active_pane(&state.tabs.lock().unwrap()), right);
        let _ = update(&mut state, Message::Wheel(left, ScrollDelta::Lines { x: 0.0, y: -1.0 }));
        assert_eq!(terminal.lock().unwrap().display_offset(), 0);
        let _ = update(&mut state, Message::Modifiers(Modifiers::CTRL));
        let _ = update(&mut state, Message::Wheel(left, ScrollDelta::Pixels { x: 0.0, y: input::PIXELS_PER_STEP / 2.0 }));
        assert_eq!(state.wheel, 0.5);
        assert_eq!(state.scroll_wheel, 0.0);
        let _ = update(&mut state, Message::Modifiers(Modifiers::empty()));
        let _ = update(&mut state, Message::Wheel(left, half));
        assert_eq!(state.wheel, 0.0);
        assert_eq!(state.scroll_wheel, 0.5);
        // A different pane starts a new accumulation, even without movement
        // (for example a tab change beneath the pointer).
        let _ = update(&mut state, Message::Wheel(right, half));
        assert_eq!(state.scroll_pane, Some(right));
        assert_eq!(state.scroll_wheel, 0.5);
        let removed = state.tabs.lock().unwrap().shutdown();
        state.cleanup.submit(removed);
        drop(state);
        reaper.join().unwrap();
    }

    fn fill_history(terminal: &Arc<Mutex<cosmix_term_core::terminal::Terminal>>) {
        let text = (0..120).map(|n| format!("history-{n}\r\n")).collect::<String>();
        let mut terminal = terminal.lock().unwrap();
        let screen = terminal.grid_snapshot().screen;
        // Replace the live pane with an isolated grid, keeping its dimensions.
        // Dropping the old terminal stops its reader; shell startup output can
        // never race the fixture or the subsequent pixel comparisons.
        *terminal = cosmix_term_core::terminal::Terminal::from_test_vt(
            screen.cols,
            screen.rows,
            text.as_bytes(),
        );
    }

    #[test]
    fn viewport_pixels_match_fresh_painter_after_up_and_down() {
        use cosmix_term_core::terminal::ScrollRequest;
        let (mut state, reaper) = test_state();
        let _ = state.sync();
        let id = state.shape.active_pane;
        let terminal = state.tabs.lock().unwrap().pane_by_id(id).unwrap();
        fill_history(&terminal);
        #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
        let frame = state.painter.frame(id);
        let mut paint = || {
            let snapshot = terminal.lock().unwrap().grid_snapshot();
            state.painter.repaint(id, &snapshot.screen, &snapshot.dirty_rows);
            let mut fresh = Painter::new(state.painter.scale(), state.painter.font(), config::Cursor::Underline).unwrap();
            fresh.repaint(id, &snapshot.screen, &vec![true; snapshot.screen.rows]);
            assert_eq!(state.painter.frame(id).lock().unwrap().surface().rgba(), fresh.frame(id).lock().unwrap().surface().rgba());
            // Keep a handle alive across the next paint to exercise tiny-skia's
            // copy/rebind path as well as the wgpu arm's persistent Vec.
            #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
            cpu_grid::refresh(&state.painter.frame(id));
        };
        paint();
        #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
        let _retained = cpu_grid::view(&frame);
        terminal.lock().unwrap().scroll_view(ScrollRequest::PageUp);
        assert!(terminal.lock().unwrap().display_offset() > 0);
        paint();
        terminal.lock().unwrap().scroll_view(ScrollRequest::Bottom);
        assert_eq!(terminal.lock().unwrap().display_offset(), 0);
        paint();
        let removed = state.tabs.lock().unwrap().shutdown();
        state.cleanup.submit(removed);
        drop(state);
        reaper.join().unwrap();
    }

    fn active_pane(tabs: &TabSet) -> u64 {
        tabs.active_tab().active_pane
    }

    /// T3 parity at the model: each chord drives the tab set the way bterm's
    /// keyboard handler does, on a real PTY-backed `TabSet`.
    #[test]
    fn chords_drive_the_tab_set_like_bterm() {
        let mut tabs = layout::test_tabs();
        let first_tab = tabs.active_id();
        let left = active_pane(&tabs);

        assert!(apply(&mut tabs, Action::Split(SplitDir::Vertical)).is_empty());
        assert_eq!(tabs.leaves().len(), 2);
        let right = active_pane(&tabs);
        assert_ne!(right, left, "a split focuses the new pane");

        apply(&mut tabs, Action::CyclePane { forward: true });
        assert_eq!(active_pane(&tabs), left);
        apply(&mut tabs, Action::CyclePane { forward: false });
        assert_eq!(active_pane(&tabs), right);
        assert_eq!(tabs.active_id(), first_tab);

        apply(&mut tabs, Action::Focus(Direction::Left));
        assert_eq!(active_pane(&tabs), left);
        apply(&mut tabs, Action::Focus(Direction::Right));
        assert_eq!(active_pane(&tabs), right);

        apply(&mut tabs, Action::NewTab);
        assert_eq!(tabs.list().len(), 2);
        assert_ne!(tabs.active_id(), first_tab, "a new tab is selected");
        apply(&mut tabs, Action::Cycle { forward: true });
        assert_eq!(tabs.active_id(), first_tab, "cycling wraps back to the first tab");

        let removed = apply(&mut tabs, Action::ClosePane);
        assert_eq!(removed.len(), 1, "the closed pane's terminal is torn down");
        assert_eq!(tabs.leaves().len(), 1);
        assert_eq!(active_pane(&tabs), left, "focus falls to the sibling");

        let removed = apply(&mut tabs, Action::CloseTab);
        assert_eq!(removed.len(), 1);
        assert_eq!(tabs.list().len(), 1);

        assert!(apply(&mut tabs, Action::FontIncrease).is_empty(), "not a tab operation");
        assert_eq!(tabs.list().len(), 1);

        let removed = apply(&mut tabs, Action::Quit);
        assert!(!removed.is_empty());
        assert!(tabs.is_empty());
        assert!(apply(&mut tabs, Action::NewTab).is_empty(), "nothing opens after quit");
        assert!(tabs.is_empty());
    }
}
