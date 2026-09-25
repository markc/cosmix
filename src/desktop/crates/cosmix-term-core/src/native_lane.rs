//! Frontend-neutral ownership of the native-session control lane.
use crate::{config, control::Control, native_session::Supervisor, tabs};
use std::sync::{Arc, Mutex};

/// Keep the recipient alive for the entire window lifetime. Create before the
/// event loop (the first launch has a bounded readiness wait), install after
/// creating the cleanup worker, and drop AFTER closing tabs and joining the
/// public Bus thread but BEFORE joining the cleanup worker.
pub struct NativeLane {
    supervisor: Option<Supervisor>,
    control: Option<Arc<Control>>,
}

impl NativeLane {
    pub fn start() -> Self {
        Self::from_startup(Supervisor::start())
    }

    /// Also used by frontend tests with an isolated native-ingress broker.
    pub fn from_startup(startup: Result<Supervisor, String>) -> Self {
        Self {
            supervisor: startup
                .map_err(|error| {
                    eprintln!("term native-session disabled; panes are graphics-only: {error}");
                })
                .ok(),
            control: None,
        }
    }

    pub fn open_tabs(&mut self, settings: config::Settings) -> Result<tabs::TabSet, String> {
        tabs::TabSet::with_supervisor(settings, self.supervisor.as_mut())
    }

    pub fn install_control(&mut self, tabs: Arc<Mutex<tabs::TabSet>>, cleanup: tabs::Cleanup) {
        self.control = self
            .supervisor
            .as_ref()
            .map(|supervisor| supervisor.handle.install_control(tabs, cleanup));
    }
}

impl Drop for NativeLane {
    fn drop(&mut self) {
        // Stop dispatch before releasing its last recipient. The actor may
        // hold another Control Arc while serving a bounded in-flight request.
        drop(self.supervisor.take());
        // Control owns a Cleanup sender; retaining it across reaper.join()
        // would keep that worker's receive loop alive forever.
        drop(self.control.take());
    }
}
