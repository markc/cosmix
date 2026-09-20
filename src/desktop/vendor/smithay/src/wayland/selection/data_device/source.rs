use std::sync::Mutex;
use tracing::error;

use wayland_server::{
    backend::ClientId,
    protocol::wl_data_source::{self},
    protocol::{wl_data_device_manager::DndAction, wl_data_source::WlDataSource},
    Dispatch, DisplayHandle, Resource,
};

use crate::utils::{alive_tracker::AliveTracker, IsAlive};

use super::{DataDeviceHandler, DataDeviceState};

/// The metadata describing a data source
#[derive(Debug, Clone)]
pub struct SourceMetadata {
    /// The MIME types supported by this source
    pub mime_types: Vec<String>,
    /// The Drag'n'Drop actions supported by this source
    pub dnd_action: DndAction,
}

impl Default for SourceMetadata {
    fn default() -> Self {
        Self {
            mime_types: Vec::new(),
            dnd_action: DndAction::None,
        }
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct DataSourceUserData {
    pub(crate) inner: Mutex<SourceMetadata>,
    alive_tracker: AliveTracker,
}

impl DataSourceUserData {
    pub(super) fn new() -> Self {
        Self {
            inner: Default::default(),
            alive_tracker: Default::default(),
        }
    }
}

impl<D> Dispatch<WlDataSource, DataSourceUserData, D> for DataDeviceState
where
    D: Dispatch<WlDataSource, DataSourceUserData>,
    D: DataDeviceHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &wayland_server::Client,
        _resource: &WlDataSource,
        request: wl_data_source::Request,
        data: &DataSourceUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        let mut data = data.inner.lock().unwrap();

        match request {
            wl_data_source::Request::Offer { mime_type } => {
                data.mime_types.push(mime_type);
            }
            wl_data_source::Request::SetActions { dnd_actions } => match dnd_actions {
                wayland_server::WEnum::Value(dnd_actions) => {
                    data.dnd_action = dnd_actions;
                }
                wayland_server::WEnum::Unknown(action) => {
                    error!("Unknown dnd_action: {:?}", action);
                }
            },
            wl_data_source::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &WlDataSource, data: &DataSourceUserData) {
        data.alive_tracker.destroy_notify();

        // cosmix patch: Destroy and client disconnect must cancel matching active drags immediately.
        // Inspect the existing seat grabs; release with_grab's lock before unsetting the grab.
        // Downcast through Any to avoid imposing WaylandFocus bounds on source dispatch.
        let seats = state.seat_state().seats.clone();
        for seat in seats {
            if let Some(pointer) = seat.get_pointer() {
                // cosmix patch assumption: this only recognises a BARE `DnDGrab`. If a
                // future feature ever interposes a wrapper grab around DnD (logging, an
                // accessibility layer, gesture arbitration — anything that boxes a
                // `DnDGrab` inside another `PointerGrab` impl rather than installing it
                // directly), this downcast silently misses it, `matches` stays `Some(false)`
                // or `None`, and the zombie-drag bug this file exists to fix comes back
                // with no error signal. A wrapper author must either forward `as_any()`/
                // `has_source()` through to the inner grab, or extend this match.
                let matches = pointer.with_grab(|_, grab| {
                    grab.as_any()
                        .downcast_ref::<super::dnd_grab::DnDGrab<D>>()
                        .is_some_and(|grab| grab.has_source(resource))
                });
                if matches == Some(true) {
                    // cosmix patch: restore focus (unlike the other unset_grab_without_focus_restore
                    // call sites in comp, which pair it with their own explicit reconcile
                    // immediately after — mod.rs:8032/8048, :12618, :12630). This vendored
                    // Dispatch::destroyed() hook has no way to call back into comp's reconcile
                    // logic, so without_focus_restore here would leave the pointer with no
                    // focus for the rest of the drag: e.g. Escape mid-drag destroys the source,
                    // this unsets the grab with focus still None, and the button release the
                    // user is still holding is swallowed (PointerInnerHandle::button no-ops
                    // with no focus) — the source client never sees it.
                    // restore_focus re-enters compositor code (cursor_image, PointerTarget
                    // enter/leave) while PointerHandle's inner Mutex is held, same as every
                    // ordinary DnD release already does via this same public unset_grab; the
                    // incremental risk here is doing so from inside Dispatch::destroyed()
                    // rather than from pointer-event processing. No deadlock is possible unless
                    // that reentered code calls back into this same PointerHandle, which normal
                    // SeatHandler impls do not do.
                    pointer.unset_grab(
                        state,
                        crate::utils::SERIAL_COUNTER.next_serial(),
                        0,
                    );
                }
            }
            if let Some(touch) = seat.get_touch() {
                // cosmix patch assumption: same bare-`DnDGrab`-only caveat as above.
                let matches = touch.with_grab(|_, grab| {
                    grab.as_any()
                        .downcast_ref::<super::dnd_grab::DnDGrab<D>>()
                        .is_some_and(|grab| grab.has_source(resource))
                });
                if matches == Some(true) {
                    touch.unset_grab(state);
                }
            }
        }
    }
}

impl IsAlive for WlDataSource {
    #[inline]
    fn alive(&self) -> bool {
        let data: &DataSourceUserData = self.data().unwrap();
        data.alive_tracker.alive()
    }
}

/// Access the metadata of a data source
pub fn with_source_metadata<T, F: FnOnce(&SourceMetadata) -> T>(
    source: &WlDataSource,
    f: F,
) -> Result<T, crate::utils::UnmanagedResource> {
    match source.data::<DataSourceUserData>() {
        Some(data) => Ok(f(&data.inner.lock().unwrap())),
        None => Err(crate::utils::UnmanagedResource),
    }
}
