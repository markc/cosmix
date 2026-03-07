use anyhow::{Context, Result};
use std::collections::HashMap;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

#[derive(Debug, Default)]
struct ToplevelInfo {
    title: String,
    app_id: String,
}

#[derive(Debug, Default)]
struct State {
    toplevels: HashMap<u32, ToplevelInfo>,
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        _state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => {
                    registry.bind::<ExtForeignToplevelListV1, _, _>(name, version.min(1), qh, ());
                    tracing::debug!("Bound ext_foreign_toplevel_list_v1");
                }
                "wl_seat" => {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                let id = toplevel.id().protocol_id();
                state.toplevels.insert(id, ToplevelInfo::default());
                tracing::debug!("New toplevel: id={id}");
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                state.done = true;
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16,
        _qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        // opcode 0 = toplevel event, which creates an ExtForeignToplevelHandleV1
        if opcode == 0 {
            _qh.make_data::<ExtForeignToplevelHandleV1, _>(())
        } else {
            panic!("Unknown child-creating opcode: {opcode}")
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let info = state.toplevels.entry(id).or_default();

        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                info.title = title;
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                info.app_id = app_id;
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                tracing::debug!("Toplevel done: id={id} app_id={} title={}", info.app_id, info.title);
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                tracing::debug!("Toplevel closed: id={id}");
            }
            _ => {}
        }
    }
}

pub fn list_windows() -> Result<()> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland compositor")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = State::default();

    display.get_registry(&qh, ());

    // Initial roundtrip to get globals
    event_queue.roundtrip(&mut state).context("Wayland roundtrip failed")?;

    // Second roundtrip to get toplevel events
    event_queue.roundtrip(&mut state).context("Wayland roundtrip failed")?;

    if state.toplevels.is_empty() {
        println!("No windows found.");
        println!("(Is ext_foreign_toplevel_list_v1 available in your compositor?)");
        return Ok(());
    }

    println!("{:<30} {}", "APP ID", "TITLE");
    println!("{:<30} {}", "------", "-----");
    for info in state.toplevels.values() {
        if !info.app_id.is_empty() || !info.title.is_empty() {
            println!("{:<30} {}", info.app_id, info.title);
        }
    }

    Ok(())
}
