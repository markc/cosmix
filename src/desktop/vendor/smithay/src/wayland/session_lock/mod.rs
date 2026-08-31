//! Utilities for handling the `ext-session-lock` protocol
//!
//! ## How to use it
//!
//! ### Initialization
//!
//! To initialize this implementation create the [`SessionLockManagerState`] and
//! implement the [`SessionLockHandler`], as shown in this example:
//!
//! ```
//! use smithay::delegate_session_lock;
//! use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
//! use smithay::wayland::session_lock::{
//!     LockSurface, SessionLockManagerState, SessionLockHandler, SessionLocker,
//! };
//!
//! # struct State { session_lock_state: SessionLockManagerState }
//! # let mut display = wayland_server::Display::<State>::new().unwrap();
//! // Create the compositor state
//! let session_lock_state = SessionLockManagerState::new::<State, _>(&display.handle(), |_| true);
//!
//! // Insert the SessionLockManagerState into your state.
//!
//! // Implement the necessary trait.
//! impl SessionLockHandler for State {
//!     fn lock_state(&mut self) -> &mut SessionLockManagerState {
//!         &mut self.session_lock_state
//!     }
//!
//!     fn lock(&mut self, _confirmation: SessionLocker) {
//!         // Lock and clear the screen.
//!
//!         // Call `confirmation.lock()` after a cleared frame was presented on all outputs.
//!
//!         // Dropping `confirmation` will cancel the locking.
//!     }
//!
//!     fn unlock(&mut self) {
//!         // Remove session lock.
//!     }
//!
//!     fn new_surface(&mut self, _surface: LockSurface, _output: WlOutput) {
//!         // Display `LockSurface` on `WlOutput`.
//!     }
//! }
//! delegate_session_lock!(State);
//!
//! // You're now ready to go!
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use _session_lock::ext_session_lock_manager_v1::{ExtSessionLockManagerV1, Request};
use _session_lock::ext_session_lock_surface_v1::ExtSessionLockSurfaceV1;
use _session_lock::ext_session_lock_v1::ExtSessionLockV1;
use wayland_protocols::ext::session_lock::v1::server as _session_lock;
use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New};

mod lock;
mod surface;

pub use lock::SessionLockState;
// cosmix addition: expose the configure record already named by the public
// handler trait so a compositor can keep one configure ledger.
pub use surface::{ExtLockSurfaceUserData, LockSurface, LockSurfaceConfigure, LockSurfaceState};

const MANAGER_VERSION: u32 = 1;

/// State of the [`ExtSessionLockManagerV1`] Global.
#[derive(Debug)]
pub struct SessionLockManagerState {
    // cosmix fix: retain the exact lock-surface owner and originating lock for
    // every output. An aborted generation can then be retired without letting
    // a stale surface destructor remove a newer generation's registration.
    locked_outputs: Vec<LockedOutput>,
}

#[derive(Debug)]
struct LockedOutput {
    output: WlOutput,
    surface: ExtSessionLockSurfaceV1,
    lock: ExtSessionLockV1,
}

impl SessionLockManagerState {
    /// Create new [`ExtSessionLockManagerV1`] global.
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtSessionLockManagerV1, SessionLockManagerGlobalData>,
        D: Dispatch<ExtSessionLockManagerV1, ()>,
        D: Dispatch<ExtSessionLockV1, SessionLockState>,
        D: SessionLockHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let data = SessionLockManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtSessionLockManagerV1, _>(MANAGER_VERSION, data);

        Self {
            locked_outputs: Vec::new(),
        }
    }

    /// Number of outputs retained by Smithay's lock-surface registry.
    // cosmix addition: exposes a narrow invariant probe for the vendored
    // duplicate-registry regressions without exposing the registry itself.
    pub fn locked_output_count(&self) -> usize {
        self.locked_outputs.len()
    }

    pub(crate) fn output_is_locked(&self, output: &WlOutput) -> bool {
        self.locked_outputs
            .iter()
            .any(|entry| &entry.output == output)
    }

    pub(crate) fn register_locked_output(
        &mut self,
        output: WlOutput,
        surface: ExtSessionLockSurfaceV1,
        lock: ExtSessionLockV1,
    ) {
        self.locked_outputs.push(LockedOutput {
            output,
            surface,
            lock,
        });
    }

    pub(crate) fn remove_lock_surface(&mut self, surface: &ExtSessionLockSurfaceV1) {
        // cosmix fix: remove by owning object, not output. A stale surface from
        // an aborted generation must not erase a newer owner's output entry.
        self.locked_outputs
            .retain(|entry| &entry.surface != surface);
    }

    /// Remove only output registrations created by one aborted lock object.
    // cosmix fix: upstream only clears the whole registry on valid unlock;
    // Cosmix also needs generation-aware cleanup when Locking aborts.
    pub fn abort_lock_outputs(&mut self, lock: &ExtSessionLockV1) {
        self.locked_outputs.retain(|entry| &entry.lock != lock);
    }
}

#[allow(missing_debug_implementations)]
#[doc(hidden)]
pub struct SessionLockManagerGlobalData {
    /// Filter whether the clients can view global.
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl<D> GlobalDispatch<ExtSessionLockManagerV1, SessionLockManagerGlobalData, D> for SessionLockManagerState
where
    D: GlobalDispatch<ExtSessionLockManagerV1, SessionLockManagerGlobalData>,
    D: Dispatch<ExtSessionLockManagerV1, ()>,
    D: Dispatch<ExtSessionLockV1, SessionLockState>,
    D: SessionLockHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ExtSessionLockManagerV1>,
        _global_data: &SessionLockManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &SessionLockManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ExtSessionLockManagerV1, (), D> for SessionLockManagerState
where
    D: GlobalDispatch<ExtSessionLockManagerV1, SessionLockManagerGlobalData>,
    D: Dispatch<ExtSessionLockManagerV1, ()>,
    D: Dispatch<ExtSessionLockV1, SessionLockState>,
    D: SessionLockHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ExtSessionLockManagerV1,
        request: Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            Request::Lock { id } => {
                let lock_state = SessionLockState::new();
                let lock_status = lock_state.lock_status.clone();
                let lock = data_init.init(id, lock_state);
                state.lock(SessionLocker::new(lock, lock_status));
            }
            Request::Destroy => (),
            _ => unreachable!(),
        }
    }
}

/// Handler trait for ext-session-lock.
pub trait SessionLockHandler {
    /// Session lock state.
    fn lock_state(&mut self) -> &mut SessionLockManagerState;

    /// Handle compositor locking requests.
    ///
    /// The [`SessionLocker`] parameter is used to confirm once the session was
    /// locked and no more client data is accessible using the
    /// [`SessionLocker::lock`] method.
    ///
    /// If locking was not possible, dropping the [`SessionLocker`] will
    /// automatically notify the requesting client about the failure.
    fn lock(&mut self, confirmation: SessionLocker);

    /// Handle compositor lock removal.
    fn unlock(&mut self);

    /// Add a new lock surface for an output.
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput);

    /// Add a new lock surface, retaining the lock object that created it.
    ///
    // cosmix addition: a rejected same-client lock object must not be able to
    // create surfaces for the active lock generation.
    fn new_surface_for_lock(&mut self, _lock: ExtSessionLockV1, surface: LockSurface, output: WlOutput) {
        self.new_surface(surface, output);
    }

    /// Whether this wl_surface has any commit or buffer-attach history.
    // cosmix addition: the protocol's AlreadyConstructed rule is broader than
    // the buffer state Smithay can reconstruct from SurfaceAttributes alone.
    fn lock_surface_already_constructed(&self, _surface: &WlSurface) -> bool {
        false
    }

    /// Whether this lock object owns the compositor's active lock generation.
    // cosmix addition: SessionLockState's client-local boolean cannot
    // distinguish an accepted lock object from a rejected same-client one.
    fn lock_object_may_create_surface(&self, _lock: &ExtSessionLockV1) -> bool {
        true
    }

    /// A surface has acknowledged a configure serial.
    fn ack_configure(&mut self, _surface: WlSurface, _configure: LockSurfaceConfigure) {}

    /// A lock-surface protocol object was destroyed.
    ///
    // cosmix addition: ext-session-lock needs to retain the compositor-owned
    // blank while retiring the destroyed client surface immediately.
    fn lock_surface_destroyed(&mut self, _surface: WlSurface) {}

    /// The ext_session_lock_v1 object was destroyed.
    // cosmix addition: destroying the owner object while Locking is legal and
    // must abort the pending lock instead of leaving an un-unlockable epoch.
    fn lock_destroyed(&mut self, _lock: ExtSessionLockV1) {}
}

/// Manage session locking.
///
/// See [`SessionLockHandler::lock`] for more detail.
#[derive(Debug)]
pub struct SessionLocker {
    lock: Option<ExtSessionLockV1>,
    lock_status: Arc<AtomicBool>,
}

impl Drop for SessionLocker {
    fn drop(&mut self) {
        // If the session wasn't locked, we notify clients about the failure.
        if let Some(lock) = self.lock.take() {
            lock.finished();
        }
    }
}

impl SessionLocker {
    fn new(lock: ExtSessionLockV1, lock_status: Arc<AtomicBool>) -> Self {
        Self {
            lock: Some(lock),
            lock_status,
        }
    }

    /// Get the underlying [`ExtSessionLockV1`]
    pub fn ext_session_lock(&self) -> &ExtSessionLockV1 {
        self.lock.as_ref().unwrap()
    }

    /// Notify the client that the session lock was successful.
    pub fn lock(mut self) {
        if let Some(lock) = self.lock.take() {
            self.lock_status.store(true, Ordering::Relaxed);
            lock.locked();
        }
    }
}

#[allow(missing_docs)]
#[macro_export]
macro_rules! delegate_session_lock {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        $crate::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::ExtSessionLockManagerV1: $crate::wayland::session_lock::SessionLockManagerGlobalData
        ] => $crate::wayland::session_lock::SessionLockManagerState);

        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::ExtSessionLockManagerV1: ()
        ] => $crate::wayland::session_lock::SessionLockManagerState);

        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1: $crate::wayland::session_lock::SessionLockState
        ] => $crate::wayland::session_lock::SessionLockManagerState);

        $crate::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_surface_v1::ExtSessionLockSurfaceV1: $crate::wayland::session_lock::ExtLockSurfaceUserData
        ] => $crate::wayland::session_lock::SessionLockManagerState);
    };
}
