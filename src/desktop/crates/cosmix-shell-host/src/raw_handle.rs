//! The only unsafe boundary in `cosmix-shell-host`.
//!
//! # Invariants
//!
//! - The display pointer comes from the retained SCTK [`Connection`] and the
//!   window pointer comes from the retained `wl_surface`; neither pointer may
//!   be used after that owner is dropped.
//! - Construction, Bevy extraction/rendering, handle removal and final drop
//!   all occur on the runner thread recorded by the owner. Pipelined rendering
//!   is disabled so Bevy does not extend use onto a second render thread.
//! - The panel removes `RawHandleWrapper`, disables its camera and runs one
//!   complete Bevy update before dropping this owner. Inside the owner the
//!   `wl_surface` is declared before the connection and therefore drops first.

#![allow(unsafe_code)]

use std::error::Error;
use std::ffi::c_void;
use std::fmt::{Display, Formatter};
use std::ptr::NonNull;
use std::thread::{self, ThreadId};

use bevy::window::{RawHandleWrapper, WindowWrapper};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use wayland_client::{Connection, Proxy, protocol::wl_surface};

#[derive(Debug)]
struct OrderedWaylandOwner<S, C> {
    // Rust drops struct fields in declaration order: surface before connection.
    surface: S,
    connection: C,
}

/// Owner retained by Bevy's raw-handle wrapper for the full WSI lifetime.
#[derive(Debug)]
pub struct RetainedWaylandOwner {
    objects: OrderedWaylandOwner<wl_surface::WlSurface, Connection>,
    owner_thread: ThreadId,
}

impl RetainedWaylandOwner {
    fn new(connection: Connection, surface: wl_surface::WlSurface) -> Self {
        Self {
            objects: OrderedWaylandOwner {
                surface,
                connection,
            },
            owner_thread: thread::current().id(),
        }
    }

    fn assert_owner_thread(&self) {
        debug_assert_eq!(self.owner_thread, thread::current().id());
    }

    fn raw_window_handle(&self) -> Result<RawWindowHandle, RawHandleError> {
        self.assert_owner_thread();
        let pointer = NonNull::new(self.objects.surface.id().as_ptr() as *mut c_void)
            .ok_or(RawHandleError::NullSurfacePointer)?;
        Ok(RawWindowHandle::Wayland(WaylandWindowHandle::new(pointer)))
    }

    fn raw_display_handle(&self) -> Result<RawDisplayHandle, RawHandleError> {
        self.assert_owner_thread();
        let pointer = NonNull::new(self.objects.connection.backend().display_ptr() as *mut c_void)
            .ok_or(RawHandleError::NullDisplayPointer)?;
        Ok(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            pointer,
        )))
    }
}

impl Drop for RetainedWaylandOwner {
    fn drop(&mut self) {
        self.assert_owner_thread();
    }
}

impl HasWindowHandle for RetainedWaylandOwner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = self
            .raw_window_handle()
            .map_err(|_| HandleError::Unavailable)?;
        // SAFETY: `self` retains the live wl_surface and its connection, and
        // the runner-thread invariant above applies for the borrow duration.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for RetainedWaylandOwner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = self
            .raw_display_handle()
            .map_err(|_| HandleError::Unavailable)?;
        // SAFETY: `self` retains the live Connection and its wl_display, and
        // the runner-thread invariant above applies for the borrow duration.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawHandleError {
    NullDisplayPointer,
    NullSurfacePointer,
    BevyHandleUnavailable,
}

impl Display for RawHandleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NullDisplayPointer => formatter.write_str("Wayland display pointer was null"),
            Self::NullSurfacePointer => formatter.write_str("Wayland surface pointer was null"),
            Self::BevyHandleUnavailable => {
                formatter.write_str("Bevy could not borrow the retained Wayland handles")
            }
        }
    }
}

impl Error for RawHandleError {}

pub type RetainedWindow = WindowWrapper<RetainedWaylandOwner>;

pub fn retained_raw_handle(
    connection: Connection,
    surface: wl_surface::WlSurface,
) -> Result<(RetainedWindow, RawHandleWrapper), RawHandleError> {
    let owner = WindowWrapper::new(RetainedWaylandOwner::new(connection, surface));
    let handle =
        RawHandleWrapper::new(&owner).map_err(|_| RawHandleError::BevyHandleUnavailable)?;
    Ok((owner, handle))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::OrderedWaylandOwner;

    #[derive(Debug)]
    struct DropProbe {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.log.lock().unwrap().push(self.name);
        }
    }

    #[test]
    fn raw_handle_drops_before_retained_surface_and_connection() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let raw_handle = DropProbe {
            name: "raw-handle",
            log: log.clone(),
        };
        let owner = OrderedWaylandOwner {
            surface: DropProbe {
                name: "surface",
                log: log.clone(),
            },
            connection: DropProbe {
                name: "connection",
                log: log.clone(),
            },
        };
        drop(raw_handle);
        drop(owner);
        assert_eq!(
            *log.lock().unwrap(),
            ["raw-handle", "surface", "connection"]
        );
    }
}
