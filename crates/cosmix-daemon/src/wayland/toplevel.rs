use anyhow::Result;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
    self, ZcosmicToplevelManagerV1,
};

use super::{Geometry, State, ToplevelInfo};

// ── ext_foreign_toplevel_list dispatch ──

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
                state.ext_handles.insert(id, toplevel);
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ExtForeignToplevelHandleV1, _>(()),
            _ => panic!("ext_foreign_toplevel_list: unknown child opcode {opcode}"),
        }
    }
}

// ── ext_foreign_toplevel_handle dispatch ──

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
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                state.ext_handles.remove(&id);
            }
            _ => {}
        }
    }
}

// ── zcosmic_toplevel_info dispatch ──
// User data = () for the info global itself

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_toplevel_info_v1::Event::Toplevel { toplevel } => {
                let id = toplevel.id().protocol_id();
                state.cosmic_handles_by_id.insert(id, toplevel);
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            // v1 toplevel event — creates zcosmic_toplevel_handle_v1
            0 => qh.make_data::<ZcosmicToplevelHandleV1, _>(0u32),
            _ => panic!("zcosmic_toplevel_info: unknown child opcode {opcode}"),
        }
    }
}

// ── zcosmic_toplevel_handle dispatch ──
// User data = u32 (ext_id of the associated ext_foreign_toplevel_handle)

impl Dispatch<ZcosmicToplevelHandleV1, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        ext_id: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let cosmic_id = proxy.id().protocol_id();

        // For v1 path (ext_id=0), store handle and title for later matching
        // For v2+ path (ext_id>0), directly link to ext handle
        if *ext_id != 0 {
            if !state.ext_to_cosmic.contains_key(ext_id) {
                state.ext_to_cosmic.insert(*ext_id, cosmic_id);
                state.cosmic_handles.insert(*ext_id, proxy.clone());
            }
        }

        // Store all info on cosmic_state, keyed by cosmic_id
        let cinfo = state.cosmic_state.entry(cosmic_id).or_default();

        match event {
            zcosmic_toplevel_handle_v1::Event::Title { title } => {
                state.cosmic_titles.insert(cosmic_id, title.clone());
                state.cosmic_handles_by_id.insert(cosmic_id, proxy.clone());
                cinfo.title = title;
            }
            zcosmic_toplevel_handle_v1::Event::AppId { app_id } => {
                cinfo.app_id = app_id;
            }
            zcosmic_toplevel_handle_v1::Event::State { state: wl_state } => {
                cinfo.maximized = false;
                cinfo.minimized = false;
                cinfo.activated = false;
                cinfo.fullscreen = false;
                cinfo.sticky = false;
                for chunk in wl_state.chunks(4) {
                    if chunk.len() == 4 {
                        let val = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        match val {
                            0 => cinfo.maximized = true,
                            1 => cinfo.minimized = true,
                            2 => cinfo.activated = true,
                            3 => cinfo.fullscreen = true,
                            4 => cinfo.sticky = true,
                            _ => {}
                        }
                    }
                }
            }
            zcosmic_toplevel_handle_v1::Event::Geometry { x, y, width, height, .. } => {
                cinfo.geometry = Some(Geometry { x, y, width, height });
            }
            _ => {}
        }
    }
}

// ── zcosmic_toplevel_manager dispatch ──

impl Dispatch<ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZcosmicToplevelManagerV1,
        _event: zcosmic_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── Public commands ──

pub fn list_windows() -> Result<()> {
    let (_conn, _eq, state) = super::connect()?;

    if state.toplevels.is_empty() {
        println!("No windows found.");
        return Ok(());
    }

    println!(
        "{:<30} {:<6} {:<40} {:>12}",
        "APP ID", "STATE", "TITLE", "GEOMETRY"
    );
    println!(
        "{:<30} {:<6} {:<40} {:>12}",
        "------", "-----", "-----", "--------"
    );

    let mut entries: Vec<_> = state.toplevels.values().collect();
    entries.sort_by(|a, b| a.app_id.cmp(&b.app_id));

    for info in entries {
        if info.app_id.is_empty() && info.title.is_empty() {
            continue;
        }

        let mut flags = String::new();
        if info.activated { flags.push('A'); }
        if info.maximized { flags.push('M'); }
        if info.minimized { flags.push('m'); }
        if info.fullscreen { flags.push('F'); }
        if info.sticky { flags.push('S'); }

        let geom = info.geometry.as_ref().map_or_else(
            || String::new(),
            |g| format!("{}x{}+{}+{}", g.width, g.height, g.x, g.y),
        );

        let title = if info.title.len() > 40 {
            format!("{}…", &info.title[..39])
        } else {
            info.title.clone()
        };

        println!("{:<30} {:<6} {:<40} {:>12}", info.app_id, flags, title, geom);
    }

    println!();
    println!("Flags: A=activated M=maximized m=minimized F=fullscreen S=sticky");

    Ok(())
}

/// Find a toplevel by app_id substring match
fn find_toplevel(state: &State, query: &str) -> Option<u32> {
    let query_lower = query.to_lowercase();
    state.toplevels.iter().find_map(|(&ext_id, info)| {
        if info.app_id.to_lowercase().contains(&query_lower)
            || info.title.to_lowercase().contains(&query_lower)
        {
            Some(ext_id)
        } else {
            None
        }
    })
}

pub fn activate_window(query: &str) -> Result<()> {
    let (conn, _eq, state) = super::connect()?;

    let ext_id = find_toplevel(&state, query)
        .ok_or_else(|| anyhow::anyhow!("No window matching '{query}'"))?;

    let cosmic_handle = state
        .cosmic_handles
        .get(&ext_id)
        .ok_or_else(|| anyhow::anyhow!("No cosmic handle for window"))?;

    let seat = state.seat.as_ref()
        .ok_or_else(|| anyhow::anyhow!("No seat available"))?;

    let manager = state.toplevel_manager.as_ref()
        .ok_or_else(|| anyhow::anyhow!("zcosmic_toplevel_manager not available"))?;

    let info = &state.toplevels[&ext_id];
    manager.activate(cosmic_handle, seat);
    conn.flush()?;
    println!("Activated: {} — {}", info.app_id, info.title);

    Ok(())
}

pub fn close_window(query: &str) -> Result<()> {
    let (conn, _eq, state) = super::connect()?;

    let ext_id = find_toplevel(&state, query)
        .ok_or_else(|| anyhow::anyhow!("No window matching '{query}'"))?;

    let cosmic_handle = state.cosmic_handles.get(&ext_id)
        .ok_or_else(|| anyhow::anyhow!("No cosmic handle for window"))?;

    let manager = state.toplevel_manager.as_ref()
        .ok_or_else(|| anyhow::anyhow!("zcosmic_toplevel_manager not available"))?;

    let info = &state.toplevels[&ext_id];
    manager.close(cosmic_handle);
    conn.flush()?;
    println!("Closed: {} — {}", info.app_id, info.title);

    Ok(())
}

pub fn minimize_window(query: &str) -> Result<()> {
    let (conn, _eq, state) = super::connect()?;

    let ext_id = find_toplevel(&state, query)
        .ok_or_else(|| anyhow::anyhow!("No window matching '{query}'"))?;

    let cosmic_handle = state.cosmic_handles.get(&ext_id)
        .ok_or_else(|| anyhow::anyhow!("No cosmic handle for window"))?;

    let manager = state.toplevel_manager.as_ref()
        .ok_or_else(|| anyhow::anyhow!("zcosmic_toplevel_manager not available"))?;

    let info = &state.toplevels[&ext_id];
    if info.minimized {
        manager.unset_minimized(cosmic_handle);
        conn.flush()?;
        println!("Unminimized: {} — {}", info.app_id, info.title);
    } else {
        manager.set_minimized(cosmic_handle);
        conn.flush()?;
        println!("Minimized: {} — {}", info.app_id, info.title);
    }

    Ok(())
}

pub fn maximize_window(query: &str) -> Result<()> {
    let (conn, _eq, state) = super::connect()?;

    let ext_id = find_toplevel(&state, query)
        .ok_or_else(|| anyhow::anyhow!("No window matching '{query}'"))?;

    let cosmic_handle = state.cosmic_handles.get(&ext_id)
        .ok_or_else(|| anyhow::anyhow!("No cosmic handle for window"))?;

    let manager = state.toplevel_manager.as_ref()
        .ok_or_else(|| anyhow::anyhow!("zcosmic_toplevel_manager not available"))?;

    let info = &state.toplevels[&ext_id];
    if info.maximized {
        manager.unset_maximized(cosmic_handle);
        conn.flush()?;
        println!("Unmaximized: {} — {}", info.app_id, info.title);
    } else {
        manager.set_maximized(cosmic_handle);
        conn.flush()?;
        println!("Maximized: {} — {}", info.app_id, info.title);
    }

    Ok(())
}
