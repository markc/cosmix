use std::sync::{atomic::AtomicBool, Arc, Mutex};

use indexmap::IndexSet;

use crate::{
    utils::{alive_tracker::AliveTracker, IsAlive, Serial},
    wayland::shell::xdg::{XdgShellState, XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE},
};

use wayland_server::protocol::wl_surface::WlSurface;

use wayland_protocols::xdg::shell::server::{
    xdg_positioner::XdgPositioner, xdg_surface, xdg_surface::XdgSurface, xdg_wm_base, xdg_wm_base::XdgWmBase,
};

use wayland_server::{
    backend::ClientId, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use super::{ShellClient, ShellClientData, XdgPositionerUserData, XdgShellHandler, XdgSurfaceUserData};

impl<D> GlobalDispatch<XdgWmBase, (), D> for XdgShellState
where
    D: GlobalDispatch<XdgWmBase, ()>,
    D: Dispatch<XdgWmBase, XdgWmBaseUserData>,
    D: Dispatch<XdgSurface, XdgSurfaceUserData>,
    D: Dispatch<XdgPositioner, XdgPositionerUserData>,
    D: XdgShellHandler,
    D: 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &wayland_server::Client,
        resource: New<XdgWmBase>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let shell = data_init.init(resource, XdgWmBaseUserData::default());

        XdgShellHandler::new_client(state, ShellClient::new(&shell));
    }
}

impl<D> Dispatch<XdgWmBase, XdgWmBaseUserData, D> for XdgShellState
where
    D: Dispatch<XdgWmBase, XdgWmBaseUserData>,
    D: Dispatch<XdgSurface, XdgSurfaceUserData>,
    D: Dispatch<XdgPositioner, XdgPositionerUserData>,
    D: XdgShellHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &wayland_server::Client,
        wm_base: &XdgWmBase,
        request: xdg_wm_base::Request,
        data: &XdgWmBaseUserData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, XdgPositionerUserData::default());
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                // xdg_shell: "Creating an xdg_surface from a wl_surface which
                // has a buffer attached or committed is a client error, and any
                // attempts by a client to attach or manipulate a buffer prior
                // to the first xdg_surface.configure call must also be treated
                // as errors." -- and a wl_surface that already carries a role
                // must be refused here with `xdg_wm_base.role`.
                //
                // Two cases, and they are not the same rule.
                //
                // A role from any *other* protocol (subsurface, layer, cursor,
                // drag icon, session lock, …) is always refused: an
                // `xdg_surface` can never legitimately wrap it.
                //
                // An xdg role (`xdg_toplevel` / `xdg_popup`) is refused only
                // while a LIVE `xdg_surface` for this `wl_surface` still
                // exists. The stamp itself is permanent (`set_role` never
                // clears it), but xdg_shell releases the surface for the same
                // role once its role object and `xdg_surface` are destroyed,
                // and Qt's hide→show does exactly that: it destroys both and
                // later asks `get_xdg_surface` for the same `wl_surface`.
                // wlroots, Mutter and KWin accept it; refusing it killed every
                // Qt client that toggled `visible` (TODO-cos, 2026-09-16).
                // `give_role` with the same role stays a no-op, so the fresh
                // wrapper can take the role back.
                //
                // Refusing before `data_init.init` while a wrapper is live is
                // still the point: an initialised duplicate wrapper next to a
                // live one is the shape that once reached the shared
                // per-`wl_surface` geometry and configure serials.
                let refuse = match crate::wayland::compositor::get_role(&surface) {
                    None => false,
                    Some(role) if role == XDG_TOPLEVEL_ROLE || role == XDG_POPUP_ROLE => {
                        XdgSurfaceWrappers::any_live(&surface)
                    }
                    Some(_) => true,
                };
                if refuse {
                    wm_base.post_error(
                        xdg_wm_base::Error::Role,
                        "wl_surface already has an assigned role",
                    );
                    return;
                }
                // Do not assign a role to the surface here
                // xdg_surface is not role, only xdg_toplevel and
                // xdg_popup are defined as roles
                let xdg_surface = data_init.init(
                    id,
                    XdgSurfaceUserData {
                        known_surfaces: data.known_surfaces.clone(),
                        wl_surface: surface.clone(),
                        wm_base: wm_base.clone(),
                        has_active_role: AtomicBool::new(false),
                    },
                );
                XdgSurfaceWrappers::register(&surface, &xdg_surface);
                data.known_surfaces
                    .lock()
                    .unwrap()
                    .insert(xdg_surface.downgrade());
            }
            xdg_wm_base::Request::Pong { serial } => {
                let serial = Serial::from(serial);
                let valid = {
                    let mut guard = data.client_data.lock().unwrap();
                    if guard.pending_ping == Some(serial) {
                        guard.pending_ping = None;
                        true
                    } else {
                        false
                    }
                };
                if valid {
                    XdgShellHandler::client_pong(state, ShellClient::new(wm_base));
                }
            }
            xdg_wm_base::Request::Destroy => {
                if !data.known_surfaces.lock().unwrap().is_empty() {
                    wm_base.post_error(
                        xdg_wm_base::Error::DefunctSurfaces,
                        "xdg_wm_base was destroyed before children",
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, _client_id: ClientId, wm_base: &XdgWmBase, data: &XdgWmBaseUserData) {
        XdgShellHandler::client_destroyed(state, ShellClient::new(wm_base));
        data.alive_tracker.destroy_notify();
    }
}

impl IsAlive for XdgWmBase {
    #[inline]
    fn alive(&self) -> bool {
        let data: &XdgWmBaseUserData = self.data().unwrap();
        data.alive_tracker.alive()
    }
}

/// The `xdg_surface` wrappers created for one `wl_surface`, held weakly in the
/// surface's own data map.
///
/// Per-surface rather than the per-`xdg_wm_base` `known_surfaces`, so a client
/// that binds `xdg_wm_base` twice cannot slip a second live wrapper past the
/// `get_xdg_surface` guard through the other binding.
#[derive(Debug, Default)]
pub(crate) struct XdgSurfaceWrappers(Mutex<Vec<Weak<XdgSurface>>>);

impl XdgSurfaceWrappers {
    fn register(surface: &WlSurface, xdg_surface: &XdgSurface) {
        crate::wayland::compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing_threadsafe(Self::default);
            let wrappers = states.data_map.get::<Self>().unwrap();
            let mut guard = wrappers.0.lock().unwrap();
            guard.retain(|weak| weak.upgrade().is_ok());
            guard.push(xdg_surface.downgrade());
        });
    }

    /// Drop one wrapper from the registry. Called from `xdg_surface.destroy`
    /// so the answer does not depend on when the backend marks the object dead.
    pub(crate) fn unregister(surface: &WlSurface, xdg_surface: &XdgSurface) {
        crate::wayland::compositor::with_states(surface, |states| {
            if let Some(wrappers) = states.data_map.get::<Self>() {
                let gone = xdg_surface.downgrade();
                wrappers
                    .0
                    .lock()
                    .unwrap()
                    .retain(|weak| weak != &gone && weak.upgrade().is_ok());
            }
        });
    }

    fn any_live(surface: &WlSurface) -> bool {
        crate::wayland::compositor::with_states(surface, |states| {
            states.data_map.get::<Self>().is_some_and(|wrappers| {
                let mut guard = wrappers.0.lock().unwrap();
                guard.retain(|weak| weak.upgrade().is_ok());
                !guard.is_empty()
            })
        })
    }
}

/*
 * xdg_shell
 */

/// User data for Xdg Wm Base
#[derive(Default, Debug)]
pub struct XdgWmBaseUserData {
    pub(crate) client_data: Mutex<ShellClientData>,
    known_surfaces: Arc<Mutex<IndexSet<Weak<xdg_surface::XdgSurface>>>>,
    alive_tracker: AliveTracker,
}
