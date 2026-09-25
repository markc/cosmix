//! `ced --headless` (ced E1 plan §7.3): the Controller and the Bus thread with
//! no window — every `ced.*` verb except `ced.layout` (UNAVAILABLE), so the
//! Mix e2e can drive the real model.
//!
//! The loop is event-driven: it blocks on the bus thread's delivery channel
//! and wakes only for a delivery (a reply, topic, timer, deadline, connection
//! edge or `ced.*` command). Clipboard reads answer "empty" (there is no
//! window clipboard); notices go to stderr; the session is written when the
//! Controller asks (debounced there) and on `app.quit`.

use iced::futures::StreamExt;

use crate::bus::{self, Delivery};
use crate::config::Config;
use crate::controller::{Controller, Effect};
use crate::dirs::{AppDirs, COMPONENT};
use crate::session;

/// Run until `app.quit` or SIGTERM. `service` is the Bus name.
pub fn run(service: &str, config: Config) -> anyhow::Result<()> {
    let (bus, mut deliveries) = bus::spawn(service).map_err(|e| anyhow::anyhow!("ced --headless: {e}"))?;
    let dirs = AppDirs::resolve(COMPONENT);
    let session_path = dirs.as_ref().map(AppDirs::session_file);
    let mut ctl = Controller::new(config, rand::random(), true);
    ctl.set_paths(
        dirs.as_ref().map(|d| d.config_file().to_string_lossy().into_owned()),
        session_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    );
    if let Some(p) = &session_path {
        ctl.set_session(session::load(p));
    }
    let mut work = ctl.start();
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    runtime.block_on(async {
        loop {
            // Perform everything pending; the controller may answer effects
            // (a clipboard read) with more.
            while !work.is_empty() {
                let mut next = Vec::new();
                for effect in std::mem::take(&mut work) {
                    match &effect {
                        Effect::SaveSession => save(&ctl, session_path.as_deref()),
                        Effect::Quit => {
                            save(&ctl, session_path.as_deref());
                            bus.perform(&effect);
                            return;
                        }
                        Effect::Notice { tab, notice } => eprintln!("ced: tab {tab:?}: {notice:?}"),
                        Effect::ClipboardRead { intent, .. } => next.extend(ctl.on_paste(intent.clone(), None)),
                        Effect::ClipboardWrite { .. } => {}
                        _ => bus.perform(&effect),
                    }
                }
                work = next;
            }
            let Some(delivery) = deliveries.next().await else {
                save(&ctl, session_path.as_deref());
                return;
            };
            work = match delivery {
                Delivery::Incoming(i) => ctl.on_incoming(i),
                Delivery::Command(c) => ctl.on_bus_command(c),
            };
        }
    });
    Ok(())
}

fn save(ctl: &Controller, path: Option<&std::path::Path>) {
    if let Some(p) = path
        && let Err(e) = session::save(p, &ctl.session())
    {
        eprintln!("ced: session not saved to {}: {e}", p.display());
    }
}
