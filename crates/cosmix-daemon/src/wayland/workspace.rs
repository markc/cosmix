use anyhow::{Context, Result};
use std::collections::HashMap;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    protocol::{wl_output, wl_registry},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

#[derive(Debug, Default)]
struct WorkspaceInfo {
    name: String,
    active: bool,
    urgent: bool,
    hidden: bool,
}

#[derive(Debug, Default)]
struct WState {
    workspaces: HashMap<u32, WorkspaceInfo>,
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WState {
    fn event(
        _state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "ext_workspace_manager_v1" {
                registry.bind::<ExtWorkspaceManagerV1, _, _>(name, version.min(1), qh, ());
                tracing::debug!("Bound ext_workspace_manager_v1");
            }
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for WState {
    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group: _ } => {
                tracing::debug!("New workspace group");
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                let id = workspace.id().protocol_id();
                state.workspaces.insert(id, WorkspaceInfo::default());
                tracing::debug!("New workspace: id={id}");
            }
            ext_workspace_manager_v1::Event::Done => {
                state.done = true;
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ExtWorkspaceGroupHandleV1, _>(()),
            1 => qh.make_data::<ExtWorkspaceHandleV1, _>(()),
            _ => panic!("Unknown child-creating opcode: {opcode}"),
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for WState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtWorkspaceGroupHandleV1,
        _event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_output::WlOutput,
        _event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for WState {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let info = state.workspaces.entry(id).or_default();

        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                info.name = name;
            }
            ext_workspace_handle_v1::Event::State { state: ws_state } => {
                if let WEnum::Value(s) = ws_state {
                    info.active = s.contains(State::Active);
                    info.urgent = s.contains(State::Urgent);
                    info.hidden = s.contains(State::Hidden);
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&id);
            }
            _ => {}
        }
    }
}

pub fn list_workspaces() -> Result<()> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland compositor")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = WState::default();

    display.get_registry(&qh, ());

    // Roundtrips to get globals then workspace events
    event_queue.roundtrip(&mut state).context("Wayland roundtrip failed")?;
    event_queue.roundtrip(&mut state).context("Wayland roundtrip failed")?;

    if state.workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("{:<20} {:>6} {:>6} {:>6}", "NAME", "ACTIVE", "URGENT", "HIDDEN");
    println!("{:<20} {:>6} {:>6} {:>6}", "----", "------", "------", "------");
    for info in state.workspaces.values() {
        println!(
            "{:<20} {:>6} {:>6} {:>6}",
            info.name,
            if info.active { "*" } else { "" },
            if info.urgent { "*" } else { "" },
            if info.hidden { "*" } else { "" },
        );
    }

    Ok(())
}
