pub mod toplevel;
pub mod workspace;

use std::collections::HashMap;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_output, wl_registry, wl_seat},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1;

// Re-export for use in submodules
pub(crate) use wayland_client;

#[derive(Debug, Default)]
pub struct ToplevelInfo {
    pub title: String,
    pub app_id: String,
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
    pub sticky: bool,
    pub geometry: Option<Geometry>,
}

#[derive(Debug, Clone)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Default)]
pub struct WorkspaceInfo {
    pub name: String,
    pub active: bool,
    pub urgent: bool,
    pub hidden: bool,
    pub coordinates: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct State {
    // Protocol objects
    pub seat: Option<wl_seat::WlSeat>,
    pub toplevel_info: Option<ZcosmicToplevelInfoV1>,
    pub toplevel_manager: Option<ZcosmicToplevelManagerV1>,

    // Data
    pub toplevels: HashMap<u32, ToplevelInfo>,
    pub workspaces: HashMap<u32, WorkspaceInfo>,

    // Ext handle → cosmic handle mapping
    pub ext_handles: HashMap<u32, ExtForeignToplevelHandleV1>,
    pub cosmic_handles: HashMap<u32, ZcosmicToplevelHandleV1>,  // keyed by ext_id
    pub cosmic_handles_by_id: HashMap<u32, ZcosmicToplevelHandleV1>,  // keyed by cosmic protocol_id
    pub cosmic_titles: HashMap<u32, String>,  // cosmic_id → title (for matching)
    pub cosmic_state: HashMap<u32, ToplevelInfo>,  // cosmic_id → state/geometry (before matching)
    pub ext_to_cosmic: HashMap<u32, u32>,
}

pub fn connect() -> anyhow::Result<(Connection, wayland_client::EventQueue<State>, State)> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("Failed to connect to Wayland compositor: {e}"))?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = State::default();

    display.get_registry(&qh, ());

    // Roundtrip 1: bind globals
    event_queue.roundtrip(&mut state)?;
    // Roundtrip 2: receive toplevel/workspace events
    event_queue.roundtrip(&mut state)?;

    // Roundtrip 3: receive cosmic toplevel handles (v1 deprecated event path)
    event_queue.roundtrip(&mut state)?;

    // Match cosmic handles to ext handles by title, transfer state/geometry
    let cosmic_by_title: HashMap<String, u32> = state
        .cosmic_titles
        .iter()
        .map(|(&cid, title)| (title.clone(), cid))
        .collect();
    let ext_ids: Vec<u32> = state.toplevels.keys().copied().collect();
    for ext_id in ext_ids {
        let title = state.toplevels.get(&ext_id).map(|i| i.title.clone()).unwrap_or_default();
        if let Some(&cosmic_id) = cosmic_by_title.get(&title) {
            if !state.ext_to_cosmic.contains_key(&ext_id) {
                state.ext_to_cosmic.insert(ext_id, cosmic_id);
                if let Some(handle) = state.cosmic_handles_by_id.get(&cosmic_id) {
                    state.cosmic_handles.insert(ext_id, handle.clone());
                }
                // Transfer state/geometry from cosmic_state to toplevels
                if let Some(cinfo) = state.cosmic_state.get(&cosmic_id) {
                    if let Some(info) = state.toplevels.get_mut(&ext_id) {
                        info.maximized = cinfo.maximized;
                        info.minimized = cinfo.minimized;
                        info.activated = cinfo.activated;
                        info.fullscreen = cinfo.fullscreen;
                        info.sticky = cinfo.sticky;
                        info.geometry = cinfo.geometry.clone();
                    }
                }
            }
        }
    }

    Ok((conn, event_queue, state))
}

// ── Registry dispatch ──

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" => {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ());
                    state.seat = Some(seat);
                }
                "ext_foreign_toplevel_list_v1" => {
                    registry.bind::<ExtForeignToplevelListV1, _, _>(name, version.min(1), qh, ());
                }
                "ext_workspace_manager_v1" => {
                    registry.bind::<ExtWorkspaceManagerV1, _, _>(name, version.min(1), qh, ());
                }
                "zcosmic_toplevel_info_v1" => {

                    // Bind at v1 to use deprecated toplevel event which directly creates handles
                    let info = registry.bind::<ZcosmicToplevelInfoV1, _, _>(name, 1, qh, ());
                    state.toplevel_info = Some(info);
                }
                "zcosmic_toplevel_manager_v1" => {

                    let mgr = registry.bind::<ZcosmicToplevelManagerV1, _, _>(name, version.min(4), qh, ());
                    state.toplevel_manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

// ── Stub dispatches for protocols that send events we don't need to handle deeply ──

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self, _proxy: &wl_seat::WlSeat, _event: wl_seat::Event,
        _data: &(), _conn: &Connection, _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        _state: &mut Self, _proxy: &wl_output::WlOutput, _event: wl_output::Event,
        _data: &(), _conn: &Connection, _qh: &QueueHandle<Self>,
    ) {}
}
