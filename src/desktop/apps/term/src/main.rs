//! CosMix Term — the lightweight frontend: iced 0.14 on its own winit/Wayland
//! backend (D2/D3), drawing `cosmix-term-core`'s grid through one persistent
//! wgpu texture (D7).
//!
//! T2 scope, and no more: one window, one tab, PTY in, keys out. Tabs and
//! panes are T3, the `term.*` verb surface is T6, runtime font sizing is T4.
//! The Bus name `term` is reserved for this binary and not yet registered —
//! nothing here dials a broker.
//!
//! The two things that are requirements rather than optimisations, because
//! they are what the whole lane is for: the grid is re-rasterised **by damaged
//! row**, and it is rasterised **into one buffer that lives for the pane's
//! life**. See `frame.rs` and `cosmix_term_core::raster::render_into`.

mod frame;
mod input;
mod keys;
mod theme;

#[cfg(feature = "wgpu")]
mod wgpu_grid;

#[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
mod cpu_grid;

#[cfg(not(any(feature = "wgpu", feature = "tiny-skia")))]
compile_error!("term needs a renderer: enable the `wgpu` (default) or `tiny-skia` feature");

use cosmix_term_core::{
    config, raster, session_fd,
    tabs::{self, TabSet},
    version::version_request,
    wake::WakeFd,
};
use frame::{Frame, Painter};
use iced::widget::container;
use iced::{Element, Length, Size, Subscription, Task};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const DISPLAY_NAME: &str = "CosMix Term";
/// The Bus name and verb namespace this frontend owns (D1). Not registered in
/// T2 — it is here so the two frontends' identities are declared in the same
/// shape and a future T6 cannot quietly pick a different one.
const SERVICE: &str = "term";

/// Same clamps as the Bevy frontend, and for the same reason: a grid wider
/// than 4096 physical pixels exceeds the texture size every GPU is guaranteed
/// to support, and a PTY is not obliged to cope with 10,000 columns.
const MAX_COLS: u16 = 240;
const MAX_ROWS: u16 = 100;
const MAX_TEXTURE: u32 = 4096;

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
            "{DISPLAY_NAME}: Wayland Mix terminal (iced + wgpu frontend)\n\
             Font: TERM_SPIKE_FONT=/path/to/font.ttf, TERM_FONT_PX=<6..48>\n\
             --version: print version and build hash, and nothing else\n\
             --print-config: print resolved startup settings and exit\n\
             Bus: `{SERVICE}` / `{SERVICE}.*` is reserved for this frontend and not yet served (T6)"
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
    let painter = Painter::new(raster::Raster::new(
        1.0,
        settings.config.font_px,
        settings.config.cursor,
    )?);
    let tabs = Arc::new(Mutex::new(TabSet::with_session(settings, None)?));
    let (cleanup, reaper) = tabs::Cleanup::start().map_err(|e| format!("cleanup worker: {e}"))?;

    // One eventfd for the whole frontend: every PTY, resize and pane exit
    // coalesces onto it, and the UI thread learns of all of them in one poll.
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

    let state = State {
        frame: painter.frame(),
        painter,
        tabs: tabs.clone(),
        cleanup: cleanup.clone(),
        settings,
        tokens: theme::tokens(),
        waker,
        window: Size::new(900.0, 560.0),
        grid: (0, 0),
        #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
        cached: None,
    };

    // `BootFn` is `Fn`, not `FnOnce`, and the state is not cloneable — the
    // PTY, the eventfd and the glyph cache each exist exactly once. iced calls
    // boot a single time, so handing it over through a take-once cell is
    // exact rather than defensive; a second call would panic loudly instead of
    // silently booting a second terminal.
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

    // Same teardown ordering constraint as the Bevy frontend: the reaper's
    // loop ends only when the LAST Cleanup sender is dropped, so every clone
    // must go before the join or it hangs forever.
    let removed = tabs.lock().expect("tabs").shutdown();
    cleanup.submit(removed);
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
/// at boot and `exit`. A terminal's concurrency is one PTY, and it is already
/// handled by the `poll(2)` thread and the reaper.
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
    painter: Painter,
    frame: Arc<Mutex<Frame>>,
    settings: config::Settings,
    tokens: cosmix_iced_widgets::Tokens,
    waker: Arc<Waker>,
    /// Logical inner size of the window, as the compositor last reported it.
    window: Size,
    /// Columns and rows the PTY has been told about.
    grid: (u16, u16),
    #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
    cached: Option<(u64, iced::widget::image::Handle)>,
}

#[derive(Debug, Clone)]
enum Message {
    /// Something in the core changed: PTY output, a resize, a pane exit.
    Wake,
    /// Keys to put on the PTY, from the widget tree — NOT from an event
    /// subscription, which drops them under load (see `keys.rs`).
    Keys(Vec<cosmix_term_core::terminal::Key>),
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
            state.repaint();
            if state.tabs.lock().expect("tabs").is_empty() {
                return iced::exit();
            }
        }
        Message::Scale(scale) => state.rescale(scale),
        Message::Keys(keys) => state.send_keys(keys),
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
            iced::window::Event::CloseRequested => return iced::exit(),
            _ => {}
        },
    }
    Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
    let (cell_width, cell_height) = state.painter.logical_cell();
    let (cols, rows) = state.grid;
    let tokens = state.tokens;
    // Sized to the grid exactly, so the texture maps 1:1 to physical pixels
    // and the nearest sampler never resamples a glyph. The leftover strip is
    // the themed surface, which is what a partial cell would otherwise show.
    let grid = renderer(state)
        .width(Length::Fixed(f32::from(cols) * cell_width))
        .height(Length::Fixed(f32::from(rows) * cell_height));
    // The keyboard rides the widget tree, not a subscription: see `keys.rs`.
    let grid = keys::keys(grid, |event| match event {
        iced::keyboard::Event::KeyPressed {
            key,
            text,
            modifiers,
            ..
        } => {
            let keys = input::keys_for(key, text.as_deref(), *modifiers);
            (!keys.is_empty()).then_some(Message::Keys(keys))
        }
        _ => None,
    });
    container(grid)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(tokens.surface.into()),
            ..container::Style::default()
        })
        .into()
}

#[cfg(feature = "wgpu")]
fn renderer(state: &State) -> iced::widget::Shader<Message, wgpu_grid::GridProgram> {
    iced::widget::shader(wgpu_grid::GridProgram::new(state.frame.clone()))
}

#[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
fn renderer(state: &State) -> iced::widget::Image<iced::widget::image::Handle> {
    cpu_grid::view(state.cached.as_ref().map(|(_, handle)| handle))
}

impl State {
    /// One PTY read, one damage-bounded raster, and nothing at all when the
    /// grid did not change — which is the case a `Wake` usually is, because a
    /// wake is also fired for resizes and Bus mutations.
    fn repaint(&mut self) {
        let (removed, _notes) = self.tabs.lock().expect("tabs").reap_exited();
        self.cleanup.submit(removed);
        let tabs = self.tabs.lock().expect("tabs");
        if tabs.is_empty() {
            return;
        }
        let terminal = tabs.active_terminal();
        drop(tabs);
        let terminal = terminal.lock().expect("terminal");
        let snapshot = terminal.grid_snapshot();
        drop(terminal);
        #[allow(unused_variables)]
        let painted = self
            .painter
            .repaint(&snapshot.screen, &snapshot.dirty_rows);
        #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
        if painted {
            self.cached = cpu_grid::refresh(self.cached.take(), &self.frame);
        }
    }

    fn resize(&mut self, window: Size) {
        // `self.grid == (0, 0)` means no layout has happened yet, and it must
        // force one even when the size is unchanged. Otherwise a compositor
        // that grants exactly the requested 900x560 on a scale-1 output takes
        // BOTH early returns — this one and `rescale`'s — and the grid stays
        // zero-sized: a terminal window with nothing in it, forever
        // (cold-review finding, 2026-09-21; invisible here only because the
        // nested harness tiles and never grants the requested size).
        if self.window == window && self.grid != (0, 0) {
            return;
        }
        self.window = window;
        self.relayout();
    }

    fn rescale(&mut self, scale: f32) {
        if (scale - self.painter.scale()).abs() < 0.01 {
            return;
        }
        match raster::Raster::new(scale, self.settings.config.font_px, self.settings.config.cursor)
        {
            Ok(raster) => self.painter.replace_raster(raster),
            // Keep the old raster: a terminal at the wrong scale is legible,
            // and a terminal with no raster is not a terminal.
            Err(error) => {
                eprintln!("term: raster rebuild at scale {scale}: {error}");
                return;
            }
        }
        // The cell size changed under the same window, so the column count
        // did too; `relayout` forces the PTY resize that repaints everything.
        self.grid = (0, 0);
        self.relayout();
    }

    /// Recompute the grid from the window and tell the PTY, if it moved.
    fn relayout(&mut self) {
        let (logical_width, logical_height) = self.painter.logical_cell();
        let (cell_width, cell_height) = self.painter.cell();
        if cell_width == 0 || cell_height == 0 {
            return;
        }
        let cols = ((self.window.width / logical_width.max(1.0)) as u16)
            .clamp(2, MAX_COLS.min((MAX_TEXTURE / cell_width) as u16));
        let rows = ((self.window.height / logical_height.max(1.0)) as u16)
            .clamp(1, MAX_ROWS.min((MAX_TEXTURE / cell_height) as u16));
        if (cols, rows) == self.grid {
            return;
        }
        let tabs = self.tabs.lock().expect("tabs");
        // Record the new grid only once it has reached a PTY. Recording it
        // first and then bailing on an empty tab set would leave `self.grid`
        // describing a resize nothing was told about, and the equality guard
        // above would suppress the retry.
        if tabs.is_empty() {
            return;
        }
        self.grid = (cols, rows);
        let id = tabs.active_tab().active_pane;
        let terminal = tabs.active_terminal();
        drop(tabs);
        // Physical pixels to the PTY: ioctl TIOCSWINSZ's ws_xpixel is what a
        // full-screen program asks for when it wants real geometry.
        terminal.lock().expect("terminal").resize(
            cols,
            rows,
            cols * cell_width as u16,
            rows * cell_height as u16,
        );
        self.tabs.lock().expect("tabs").resized(id, cols, rows);
        // `Terminal::resize` fires the wake, so the repaint arrives as the
        // next `Message::Wake` rather than being duplicated here.
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
        let terminal = tabs.active_terminal();
        drop(tabs);
        let terminal = terminal.lock().expect("terminal");
        let at = Instant::now();
        for key in keys {
            if let Err(error) = terminal.listener.key(key, at) {
                eprintln!("term input: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
