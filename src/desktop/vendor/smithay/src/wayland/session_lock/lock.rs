//! ext-session-lock lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::renderer::buffer_dimensions;
use crate::utils::Size;
use crate::wayland::compositor::SurfaceAttributes;
use crate::wayland::compositor::{self, BufferAssignment};
use crate::wayland::viewporter::{ViewportCachedState, ViewporterSurfaceState};
use _session_lock::ext_session_lock_surface_v1::ExtSessionLockSurfaceV1;
use _session_lock::ext_session_lock_v1::{Error, ExtSessionLockV1, Request};
use wayland_protocols::ext::session_lock::v1::server::{self as _session_lock, ext_session_lock_surface_v1};
use wayland_server::{backend::ClientId, Client, DataInit, Dispatch, DisplayHandle, Resource};

use crate::wayland::session_lock::surface::{ExtLockSurfaceUserData, LockSurface, LockSurfaceAttributes};
use crate::wayland::session_lock::{SessionLockHandler, SessionLockManagerState};

/// Surface role for ext-session-lock surfaces.
const LOCK_SURFACE_ROLE: &str = "ext_session_lock_surface_v1";

/// [`ExtSessionLockV1`] state.
#[derive(Debug)]
pub struct SessionLockState {
    pub(crate) lock_status: Arc<AtomicBool>,
}

impl SessionLockState {
    pub(crate) fn new() -> Self {
        Self {
            lock_status: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<D> Dispatch<ExtSessionLockV1, SessionLockState, D> for SessionLockManagerState
where
    D: Dispatch<ExtSessionLockV1, SessionLockState>,
    D: Dispatch<ExtSessionLockSurfaceV1, ExtLockSurfaceUserData>,
    D: SessionLockHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        lock: &ExtSessionLockV1,
        request: Request,
        data: &SessionLockState,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            Request::GetLockSurface { id, surface, output } => {
                // cosmix fix: bind every lock surface to the exact accepted
                // lock resource. Client identity alone lets a same-client
                // object that already received `finished` occupy outputs for
                // the real owner's generation.
                if !state.lock_object_may_create_surface(lock) {
                    lock.post_error(
                        Error::InvalidUnlock,
                        "Lock object does not own the active lock generation.",
                    );
                    return;
                }
                // Assign surface a role and ensure it never had one before.
                if compositor::give_role(&surface, LOCK_SURFACE_ROLE).is_err() {
                    lock.post_error(Error::Role, "Surface already has a role.");
                    return;
                }

                // cosmix fix: validate before registering the output. The
                // upstream order leaked one locked_outputs entry whenever an
                // AlreadyConstructed request failed because no lock-surface
                // object then existed to run destruction cleanup.
                let has_buffer = compositor::with_states(&surface, |states| {
                    let cached = &states.cached_state;
                    let mut guard = cached.get::<SurfaceAttributes>();
                    let pending = matches!(guard.pending().buffer, Some(BufferAssignment::NewBuffer(_)));
                    let current = matches!(guard.current().buffer, Some(BufferAssignment::NewBuffer(_)));
                    pending || current
                });
                if has_buffer || state.lock_surface_already_constructed(&surface) {
                    lock.post_error(
                        Error::AlreadyConstructed,
                        "Surface was already committed or had a buffer attached.",
                    );
                    return;
                }

                // Ensure output is not already locked.
                let lock_state = state.lock_state();
                if lock_state.output_is_locked(&output) {
                    lock.post_error(Error::DuplicateOutput, "Output is already locked.");
                    return;
                }

                let data = ExtLockSurfaceUserData {
                    surface: surface.downgrade(),
                };
                let lock_surface = data_init.init(id, data);
                state.lock_state().register_locked_output(
                    output.clone(),
                    lock_surface.clone(),
                    lock.clone(),
                );

                // Initialize surface data.
                compositor::with_states(&surface, |states| {
                    let inserted = states.data_map.insert_if_missing_threadsafe(|| {
                        Mutex::new(LockSurfaceAttributes::new(lock_surface.clone()))
                    });

                    if !inserted {
                        let mut attributes = states
                            .data_map
                            .get::<Mutex<LockSurfaceAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        attributes.surface = lock_surface.clone();
                    }
                });

                // Add pre-commit hook for updating surface state.
                compositor::add_pre_commit_hook::<D, _>(&surface, |_state, _dh, surface| {
                    compositor::with_states(surface, |states| {
                        let attributes = states.data_map.get::<Mutex<LockSurfaceAttributes>>();
                        let mut attributes = attributes.unwrap().lock().unwrap();

                        let Some(state) = attributes.last_acked else {
                            attributes.surface.post_error(
                                ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
                                "Committed before the first ack_configure.",
                            );
                            return;
                        };

                        // Verify the attached buffer: ext-session-lock requires no NULL buffers
                        // and an exact dimentions match.
                        // cosmix fix: validate the effective buffer after this
                        // commit, not only an explicit pending assignment. An
                        // empty first commit is NULL, while an empty commit
                        // after a resize retains the old buffer and must still
                        // be checked against the newly acked dimensions.
                        let mut guard = states.cached_state.get::<SurfaceAttributes>();
                        let (pending_buffer, scale, transform) = {
                            let pending = guard.pending();
                            let assignment = pending.buffer.as_ref().map(|assignment| match assignment {
                                BufferAssignment::Removed => None,
                                BufferAssignment::NewBuffer(buffer) => Some(buffer.clone()),
                            });
                            (assignment, pending.buffer_scale, pending.buffer_transform.into())
                        };
                        let effective_buffer = match pending_buffer {
                            Some(buffer) => {
                                attributes.effective_buffer = buffer.clone();
                                buffer
                            }
                            None => attributes.effective_buffer.clone(),
                        };
                        let Some(buffer) = effective_buffer else {
                            attributes.surface.post_error(
                                ext_session_lock_surface_v1::Error::NullBuffer,
                                "Surface commit has no effective buffer.",
                            );
                            return;
                        };
                        if let Some(buf_size) = buffer_dimensions(&buffer) {
                            let viewport = states
                                .data_map
                                .get::<ViewporterSurfaceState>()
                                .map(|v| v.lock().unwrap());
                            let surface_size = if let Some(dest) = viewport.as_ref().and_then(|_| {
                                let mut guard = states.cached_state.get::<ViewportCachedState>();
                                guard.pending().dst
                            }) {
                                Size::from((dest.w as u32, dest.h as u32))
                            } else {
                                let surface_size = buf_size.to_logical(scale, transform);
                                Size::from((surface_size.w as u32, surface_size.h as u32))
                            };

                            if Some(surface_size) != state.size {
                                attributes.surface.post_error(
                                    ext_session_lock_surface_v1::Error::DimensionsMismatch,
                                    "Surface dimensions do not match acked configure.",
                                );
                            }
                        }
                    });
                });
                compositor::add_post_commit_hook::<D, _>(&surface, |_state, _dh, surface| {
                    compositor::with_states(surface, |states| {
                        let attributes = states.data_map.get::<Mutex<LockSurfaceAttributes>>();
                        let mut attributes = attributes.unwrap().lock().unwrap();

                        if let Some(state) = attributes.last_acked {
                            attributes.current = state;
                        }
                    });
                });

                // Call compositor handler.
                let lock_surface = LockSurface::new(surface, lock_surface);
                state.new_surface_for_lock(lock.clone(), lock_surface.clone(), output);

                // Send initial configure when the interface is bound.
                lock_surface.send_configure();
            }
            Request::UnlockAndDestroy => {
                // Ensure session is locked.
                if !data.lock_status.load(Ordering::Relaxed) {
                    lock.post_error(Error::InvalidUnlock, "Session is not locked.");
                    // cosmix fix: a rejected lock object receives `finished`
                    // but remains usable until destroyed. It must not fall
                    // through and unlock the real owner's session.
                    return;
                }

                state.lock_state().locked_outputs.clear();
                state.unlock();
            }
            Request::Destroy => {
                // Ensure session is not locked.
                if data.lock_status.load(Ordering::Relaxed) {
                    lock.post_error(Error::InvalidDestroy, "Cannot destroy session lock while locked.");
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, lock: &ExtSessionLockV1, _data: &SessionLockState) {
        // cosmix addition: bind Locking lifetime to the protocol object.
        state.lock_destroyed(lock.clone());
    }
}
