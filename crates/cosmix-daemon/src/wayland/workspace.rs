use anyhow::Result;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WsState},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use super::{State, WorkspaceInfo};

// ── ext_workspace_manager dispatch ──

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { .. } => {}
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                let id = workspace.id().protocol_id();
                state.workspaces.insert(id, WorkspaceInfo::default());
            }
            ext_workspace_manager_v1::Event::Done => {}
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
            _ => panic!("ext_workspace_manager: unknown child opcode {opcode}"),
        }
    }
}

// ── ext_workspace_group_handle dispatch ──

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
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

// ── ext_workspace_handle dispatch ──

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
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
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                info.coordinates = coordinates
                    .chunks(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: ws_state } => {
                if let WEnum::Value(s) = ws_state {
                    info.active = s.contains(WsState::Active);
                    info.urgent = s.contains(WsState::Urgent);
                    info.hidden = s.contains(WsState::Hidden);
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&id);
            }
            _ => {}
        }
    }
}

// ── Public commands ──

pub fn list_workspaces() -> Result<()> {
    let (_conn, _eq, state) = super::connect()?;

    if state.workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!(
        "{:>4} {:<16} {:>6} {:>6} {:>6} {}",
        "#", "NAME", "ACTIVE", "URGENT", "HIDDEN", "COORDS"
    );
    println!(
        "{:>4} {:<16} {:>6} {:>6} {:>6} {}",
        "-", "----", "------", "------", "------", "------"
    );

    let mut entries: Vec<_> = state.workspaces.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for (i, info) in entries.iter().enumerate() {
        let coords = if info.coordinates.is_empty() {
            String::new()
        } else {
            info.coordinates
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };

        println!(
            "{:>4} {:<16} {:>6} {:>6} {:>6} {}",
            i + 1,
            info.name,
            if info.active { "*" } else { "" },
            if info.urgent { "*" } else { "" },
            if info.hidden { "*" } else { "" },
            coords,
        );
    }

    Ok(())
}
