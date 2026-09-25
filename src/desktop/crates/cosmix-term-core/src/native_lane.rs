//! Frontend-neutral ownership of the native-session control lane.
use crate::{config, control::Control, native_session::Supervisor, tabs};
use std::sync::{Arc, Mutex};

/// Keep the recipient alive for the entire window lifetime. Create before the
/// event loop, release control's cleanup sender after closing tabs and joining
/// the public Bus thread, then drop AFTER joining the cleanup worker.
pub struct NativeLane {
    supervisor: Option<Supervisor>,
    control: Option<Arc<Control>>,
}

impl NativeLane {
    /// All native startup (including account/config lookup) runs off the UI.
    pub fn start_background(
        tabs: Arc<Mutex<tabs::TabSet>>,
        settings: config::Settings,
        cleanup: tabs::Cleanup,
    ) -> std::io::Result<std::thread::JoinHandle<Self>> {
        Self::start_background_with(tabs, settings, cleanup, Supervisor::start)
    }

    pub fn start_background_with(
        tabs: Arc<Mutex<tabs::TabSet>>,
        settings: config::Settings,
        cleanup: tabs::Cleanup,
        start: impl FnOnce() -> Result<Supervisor, String> + Send + 'static,
    ) -> std::io::Result<std::thread::JoinHandle<Self>> {
        std::thread::Builder::new()
            .name("term-native-start".into())
            .spawn(move || {
                let mut lane = Self::from_startup(start());
                if let Some(supervisor) = lane.supervisor.as_mut() {
                    supervisor.wait_startup();
                }
                let native = lane.supervisor.as_ref().map(|s| s.handle.clone());
                let launch = native.clone();
                let titles = tabs.lock().unwrap().titles_changed();
                let ready =
                    tabs::TabSet::with_initial_notifier(settings, native, titles, move || {
                        crate::terminal::Terminal::start_session(settings, launch.as_ref(), 1)
                    });
                match ready {
                    Ok(ready) => tabs.lock().unwrap().finish_startup(ready),
                    Err(error) => {
                        eprintln!("term first pane: {error}");
                        tabs.lock().unwrap().shutdown();
                    }
                }
                lane.install_control(tabs, cleanup);
                lane
            })
    }

    /// Called after TabSet::shutdown, which serialises with native layout
    /// mutations. No later dispatch can enqueue a removed pane.
    pub fn release_cleanup(&self) {
        if let Some(control) = &self.control {
            control.release_cleanup();
        }
    }

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
        // Normal exit releases Control's Cleanup sender before reaper.join().
        // Dropping Control here also covers partial setup failures.
        drop(self.control.take());
    }
}
