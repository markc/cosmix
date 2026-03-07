use anyhow::{Context, Result};
use std::io::Read;
use std::os::unix::io::{AsFd, AsRawFd, FromRawFd};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

// ── Read state ──

#[derive(Debug, Default)]
struct ClipState {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    selection_offer: Option<ZwlrDataControlOfferV1>,
    mime_types: Vec<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipState {
    fn event(
        state: &mut Self, registry: &wl_registry::WlRegistry,
        event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    state.manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name, version.min(2), qh, (),
                    ));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipState {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self, _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.selection_offer = Some(id);
                state.mime_types.clear();
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if id.is_none() {
                    state.selection_offer = None;
                }
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16, qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ZwlrDataControlOfferV1, _>(()),
            _ => panic!("data_control_device: unknown child opcode {opcode}"),
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self, _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.mime_types.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipState {
    fn event(
        _: &mut Self, _: &ZwlrDataControlSourceV1,
        _: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

// ── Write state ──

struct ClipWriteState {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    content: Vec<u8>,
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipWriteState {
    fn event(
        state: &mut Self, registry: &wl_registry::WlRegistry,
        event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    state.manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name, version.min(2), qh, (),
                    ));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipWriteState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipWriteState {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipWriteState {
    fn event(
        _: &mut Self, _: &ZwlrDataControlDeviceV1,
        _: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}

    fn event_created_child(
        opcode: u16, qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ZwlrDataControlOfferV1, _>(()),
            _ => panic!("data_control_device: unknown child opcode {opcode}"),
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipWriteState {
    fn event(_: &mut Self, _: &ZwlrDataControlOfferV1, _: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipWriteState {
    fn event(
        state: &mut Self, _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                // Compositor is asking us for the clipboard data — write to the fd
                use std::io::Write;
                let raw = fd.as_raw_fd();
                let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
                let _ = file.write_all(&state.content);
                // Drop file to close the fd (signals EOF to reader)
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                // Another app took the clipboard — we're done
                state.done = true;
            }
            _ => {}
        }
    }
}

/// Read the current clipboard text content
pub fn get_clipboard() -> Result<String> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland")?;
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();

    let mut state = ClipState::default();
    display.get_registry(&qh, ());
    eq.roundtrip(&mut state)?;

    let manager = state.manager.as_ref().context("zwlr_data_control_manager not available")?;
    let seat = state.seat.as_ref().context("No seat")?;

    manager.get_data_device(seat, &qh, ());
    eq.roundtrip(&mut state)?;

    let offer = state.selection_offer.as_ref().context("No clipboard selection")?;

    let mime = state.mime_types.iter()
        .find(|m| m.as_str() == "text/plain;charset=utf-8")
        .or_else(|| state.mime_types.iter().find(|m| m.starts_with("text/")))
        .context("Clipboard doesn't contain text")?
        .clone();

    // Create pipe, ask compositor to write clipboard to it
    let (read_pipe, write_pipe) = os_pipe::pipe()?;
    offer.receive(mime, write_pipe.as_fd());
    conn.flush()?;
    drop(write_pipe);

    // Read from pipe — transfer ownership to File to read contents
    let mut content = String::new();
    let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(&read_pipe);
    let mut read_file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    std::mem::forget(read_pipe); // prevent double-close
    read_file.read_to_string(&mut content)?;

    Ok(content)
}

/// Set clipboard text content (native Wayland, no external tools)
/// Forks a background thread to serve clipboard data until replaced.
pub fn set_clipboard(text: &str) -> Result<()> {
    let content = text.as_bytes().to_vec();

    // Fork a child process that holds the clipboard source alive.
    // When another app copies something, the compositor sends Cancelled
    // and the child exits.
    let child_content = content.clone();
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork() failed");
        }
        if pid > 0 {
            // Parent: return immediately, clipboard is set
            return Ok(());
        }
        // Child: detach from parent
        libc::setsid();
    }

    // Child process: set up Wayland connection and serve clipboard
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland")?;
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();

    let mut state = ClipWriteState {
        manager: None,
        seat: None,
        content: child_content,
        done: false,
    };

    display.get_registry(&qh, ());
    eq.roundtrip(&mut state)?;

    let manager = state.manager.as_ref().context("zwlr_data_control_manager not available")?;
    let seat = state.seat.as_ref().context("No seat")?;

    // Create source and offer MIME types
    let source = manager.create_data_source(&qh, ());
    source.offer("text/plain;charset=utf-8".into());
    source.offer("text/plain".into());
    source.offer("UTF8_STRING".into());
    source.offer("STRING".into());

    // Create device and set selection
    let device = manager.get_data_device(seat, &qh, ());
    device.set_selection(Some(&source));
    conn.flush()?;

    // Event loop: serve data until cancelled
    while !state.done {
        eq.blocking_dispatch(&mut state)?;
    }

    // Child exits cleanly
    std::process::exit(0);
}

/// CLI: print clipboard content
pub fn clipboard_get_cmd() -> Result<()> {
    let content = get_clipboard()?;
    print!("{content}");
    Ok(())
}
