//! A synthetic input method, for gating IME support.
//!
//! Real IMEs (fcitx5, ibus) are the only other way to exercise
//! `zwp_input_method_v2`, and checking against one is a thing that happens
//! once on a developer's machine and then rots. This binary is the same check,
//! runnable on every gate, on a machine with no IME installed.
//!
//! It does the smallest thing a real IME does that the compositor must get
//! right: bind the manager, take the input method for the seat, and — when the
//! compositor says a text input has been focused — put a candidate window on
//! screen and commit a pre-edit string. If the compositor never activates it,
//! or never paints the popup, that is exactly the user-visible failure an IME
//! would suffer, and the gate sees it.
//!
//! It prints one line per protocol milestone so a harness can assert on
//! progress rather than only on the final pixels — "activated but never
//! painted" and "never activated" are different compositor bugs and must not
//! look the same from outside.

use std::io::Write as _;
use std::os::fd::AsFd;

use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry, wl_seat::WlSeat, wl_shm,
    wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
    zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
};

/// Candidate-window size. Deliberately not 1x1: the gate asserts painted
/// pixels, and a window too small to see is indistinguishable from one that
/// never appeared.
const POPUP_W: i32 = 180;
const POPUP_H: i32 = 48;

#[derive(Default)]
struct App {
    compositor: Option<WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<WlSeat>,
    wm_base: Option<XdgWmBase>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    activation: Option<XdgActivationV1>,
    text_input: Option<ZwpTextInputV3>,
    window: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    toplevel: Option<XdgToplevel>,
    window_configured: bool,
    text_input_enabled: bool,
    manager: Option<ZwpInputMethodManagerV2>,
    input_method: Option<ZwpInputMethodV2>,
    popup: Option<ZwpInputPopupSurfaceV2>,
    popup_surface: Option<WlSurface>,
    /// Serial of the last `done`, which every commit must echo.
    serial: u32,
    activated: bool,
    painted: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match &interface[..] {
            "wl_compositor" => {
                state.compositor = Some(registry.bind::<WlCompositor, _, _>(name, 4, qh, ()));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
            }
            "wl_seat" => {
                state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base = Some(registry.bind::<XdgWmBase, _, _>(name, 1, qh, ()));
            }
            "xdg_activation_v1" => {
                state.activation = Some(registry.bind::<XdgActivationV1, _, _>(name, 1, qh, ()));
            }
            "zwp_text_input_manager_v3" => {
                state.text_input_manager =
                    Some(registry.bind::<ZwpTextInputManagerV3, _, _>(name, 1, qh, ()));
            }
            "zwp_input_method_manager_v2" => {
                state.manager =
                    Some(registry.bind::<ZwpInputMethodManagerV2, _, _>(name, 1, qh, ()));
                println!("IMEPROBE bound zwp_input_method_manager_v2");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for App {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => {
                state.activated = true;
                println!("IMEPROBE activate");
            }
            zwp_input_method_v2::Event::Deactivate => {
                state.activated = false;
                println!("IMEPROBE deactivate");
            }
            zwp_input_method_v2::Event::Done => {
                // The serial every subsequent commit must carry. Tracking it is
                // not optional bookkeeping: a commit with a stale serial is
                // ignored by the compositor, which would look exactly like the
                // compositor dropping our pre-edit.
                state.serial = state.serial.wrapping_add(1);
                if state.activated && !state.painted {
                    state.paint_popup(qh);
                }
            }
            zwp_input_method_v2::Event::Unavailable => {
                println!("IMEPROBE unavailable — another input method already holds this seat");
            }
            _ => {}
        }
    }
}

impl App {
    /// Create the candidate window and put something visible in it.
    fn paint_popup(&mut self, qh: &QueueHandle<Self>) {
        let (Some(compositor), Some(shm), Some(input_method)) =
            (&self.compositor, &self.shm, &self.input_method)
        else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        let popup = input_method.get_input_popup_surface(&surface, qh, ());

        let stride = POPUP_W * 4;
        let size = (stride * POPUP_H) as usize;
        let file =
            match rustix::fs::memfd_create("cosmix-imeprobe", rustix::fs::MemfdFlags::CLOEXEC) {
                Ok(file) => file,
                Err(error) => {
                    println!("IMEPROBE FAILED to create memfd: {error}");
                    return;
                }
            };
        if let Err(error) = rustix::fs::ftruncate(&file, size as u64) {
            println!("IMEPROBE FAILED to size the buffer: {error}");
            return;
        }
        // An opaque, saturated fill. The gate compares frames, so the candidate
        // window has to differ from whatever is behind it by more than noise;
        // a subtle colour would make a real paint look like a failed one.
        let mut mapped = std::fs::File::from(file);
        let pixel = [0x20u8, 0xC0, 0xFF, 0xFF];
        let row = pixel.repeat(POPUP_W as usize);
        for _ in 0..POPUP_H {
            if let Err(error) = mapped.write_all(&row) {
                println!("IMEPROBE FAILED to fill the buffer: {error}");
                return;
            }
        }
        let pool = shm.create_pool(mapped.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            POPUP_W,
            POPUP_H,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, POPUP_W, POPUP_H);
        surface.commit();

        // A pre-edit as well, because a compositor can place the popup and
        // still drop the text — and a user notices the missing text first.
        input_method.set_preedit_string("cosmix-ime".to_string(), 0, "cosmix-ime".len() as i32);
        input_method.commit(self.serial);

        self.popup = Some(popup);
        self.popup_surface = Some(surface);
        self.painted = true;
        println!("IMEPROBE painted candidate window {POPUP_W}x{POPUP_H} and committed a preedit");
    }
}

// The probe is BOTH the input method and the text field it serves.
//
// That is deliberate, not a shortcut: a gate that relied on some other client
// happening to bind text-input AND happening to hold focus would be testing
// the fixture's luck, not the compositor. Owning both ends makes activation
// deterministic — the probe maps a window, takes focus, enables text input,
// and the compositor must then activate the input method.
impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.window_configured = true;
            if let Some(window) = &state.window {
                window.commit();
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for App {
    fn event(
        _: &mut Self,
        _: &XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        state: &mut Self,
        text_input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { .. } => {
                // Keyboard focus reached our window. Enabling here is what
                // makes the compositor activate the input method.
                text_input.enable();
                text_input.set_cursor_rectangle(20, 20, 10, 20);
                text_input.commit();
                state.text_input_enabled = true;
                println!("IMEPROBE text-input entered and enabled");
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                state.text_input_enabled = false;
                println!("IMEPROBE text-input left");
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore WlCompositor);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore WlShmPool);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore WlSeat);
delegate_noop!(App: ignore ZwpInputMethodManagerV2);
delegate_noop!(App: ignore ZwpTextInputManagerV3);
delegate_noop!(App: ignore XdgActivationV1);

/// The probe asks for focus on its OWN window.
///
/// Without this the fixture depends on the compositor volunteering focus to a
/// newly mapped toplevel, which it is under no obligation to do while another
/// window holds it — and a gate that needs the compositor to be feeling
/// generous is not a gate. Using xdg-activation is also honest about what a
/// real application does when it wants attention.
impl Dispatch<XdgActivationTokenV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            let (Some(activation), Some(window)) = (&state.activation, &state.window) else {
                return;
            };
            activation.activate(token, window);
            println!("IMEPROBE requested focus for its own window via xdg-activation");
        }
    }
}
delegate_noop!(App: ignore ZwpInputPopupSurfaceV2);

fn main() {
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(error) => {
            println!("IMEPROBE FAILED to connect: {error}");
            std::process::exit(2);
        }
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());

    let mut app = App::default();
    // Two roundtrips: the first delivers the globals, the second whatever they
    // emit on bind.
    let _ = queue.roundtrip(&mut app);
    let _ = queue.roundtrip(&mut app);

    let (Some(manager), Some(seat)) = (app.manager.clone(), app.seat.clone()) else {
        // This is the honest negative: a compositor that does not advertise the
        // protocol cannot be tested for it, and saying so beats timing out.
        println!("IMEPROBE FAILED no zwp_input_method_manager_v2 advertised");
        std::process::exit(3);
    };
    app.input_method = Some(manager.get_input_method(&seat, &qh, ()));
    let _ = queue.roundtrip(&mut app);

    // Now the text field half: a real window that takes focus, so the
    // compositor has something to activate the input method FOR.
    let (Some(compositor), Some(wm_base), Some(text_input_manager)) = (
        app.compositor.clone(),
        app.wm_base.clone(),
        app.text_input_manager.clone(),
    ) else {
        println!("IMEPROBE FAILED no xdg_wm_base or zwp_text_input_manager_v3 advertised");
        std::process::exit(4);
    };
    // Create the text-input object BEFORE the window can take focus. The
    // `enter` event is delivered to an existing object or not at all — create
    // it after mapping and a focus that arrives first is simply missed, which
    // looks exactly like the compositor failing to activate.
    app.text_input = Some(text_input_manager.get_text_input(&seat, &qh, ()));
    let window = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&window, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("cosmix imeprobe".to_string());
    window.commit();
    let _ = queue.roundtrip(&mut app);

    // A buffer, so the window actually maps and can take focus. An unmapped
    // surface never receives a text-input enter and the probe would wait
    // forever for an activation that cannot come.
    if let Some(shm) = app.shm.clone() {
        let stride = 200 * 4;
        let size = (stride * 120) as usize;
        // Nested rather than a let-chain: this crate is edition 2021.
        if let Ok(file) =
            rustix::fs::memfd_create("cosmix-imeprobe-win", rustix::fs::MemfdFlags::CLOEXEC)
                .and_then(|file| rustix::fs::ftruncate(&file, size as u64).map(|()| file))
        {
            let mut mapped = std::fs::File::from(file);
            let row = [0x18u8, 0x1C, 0x24, 0xFF].repeat(200);
            let mut ok = true;
            for _ in 0..120 {
                if mapped.write_all(&row).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                let pool = shm.create_pool(mapped.as_fd(), size as i32, &qh, ());
                let buffer =
                    pool.create_buffer(0, 200, 120, stride, wl_shm::Format::Argb8888, &qh, ());
                window.attach(Some(&buffer), 0, 0);
                window.damage(0, 0, 200, 120);
                window.commit();
            }
        }
    }
    app.window = Some(window);
    app.xdg_surface = Some(xdg_surface);
    app.toplevel = Some(toplevel);
    let _ = queue.roundtrip(&mut app);
    // Ask for focus rather than hope for it.
    if let Some(activation) = app.activation.clone() {
        let token = activation.get_activation_token(&qh, ());
        if let Some(surface) = &app.window {
            token.set_surface(surface);
        }
        token.commit();
        let _ = queue.roundtrip(&mut app);
    } else {
        println!(
            "IMEPROBE note: no xdg_activation_v1 — relying on the compositor to volunteer focus"
        );
    }
    println!("IMEPROBE text field mapped, waiting for focus");

    // Serve until killed. The harness focuses a text input, waits for the
    // candidate window, captures, and then tears this down.
    loop {
        if queue.blocking_dispatch(&mut app).is_err() {
            break;
        }
    }
}
