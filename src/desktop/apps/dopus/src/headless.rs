//! Headless dopus: the same core and the same Bus surface, no window. The
//! P1 shape — a real process neighbours can `dopus.state` and drive
//! `dopus.action` against — so the verb layer is proven before the twin-pane
//! window lands in P2. The windowed app and this loop share
//! [`verbs::serve_command`]; only the transports differ.
//!
//! Law wiring: a drainer thread owns `on_event`/`tick` exclusively (law 2:
//! every event through `on_event` exactly once, on one thread; law 1: tick at
//! the drain cadence) and answers every dialog immediately (law 3:
//! `No`/dismissal — fail-closed, nothing wedges, no file operation ever
//! starts). `dopus.theme.set` is refused: a theme with nothing to paint is
//! a lie, and the core never renders.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iced::futures::StreamExt;

use cosmix_dopus_core::{ConfigFile, ConfirmAnswer, CoreEvent, DOpusConfig, DopusCore};

use crate::bus::{self, Delivery};
use crate::dirs::AppDirs;
use crate::keys;
use crate::verbs::{self, ServerMeta, Served};

/// The drain cadence: bounds how long a worker reply waits (law 2) and is
/// the tick period (law 1).
const DRAIN_TICK: Duration = Duration::from_millis(200);

/// What the shared law-wiring does with the core's derived events: answer
/// every dialog (law 3), refuse `OpenFile` with a log line (law 4's P1
/// posture). Shared verbatim by the windowed app's handler.
pub fn answer_derived(core: &mut DopusCore, events: Vec<CoreEvent>, log: impl Fn(String)) {
    for event in events {
        match event {
            CoreEvent::ConfirmRequested { token, .. } => core.confirm(token, ConfirmAnswer::No),
            CoreEvent::PromptRequested { token, .. } => core.prompt_text(token, None),
            CoreEvent::OpenFile(path) => log(format!(
                "refusing OpenFile({}), no file-operations surface in P1",
                cosmix_dopus_core::sanitise_display_path(&path)
            )),
            CoreEvent::Status { .. }
            | CoreEvent::InfoChanged
            | CoreEvent::SelectionChanged { .. }
            | CoreEvent::ListingStarted { .. }
            | CoreEvent::ListingArrived { .. }
            | CoreEvent::CountArrived { .. }
            | CoreEvent::OperationArrived { .. }
            | CoreEvent::ConfigSettled(_)
            | CoreEvent::RefreshAll => {}
        }
    }
}

/// Lock helper that recovers from a poisoned guard (a panicked drainer must
/// not take the whole process down with it).
fn lock(core: &Mutex<DopusCore>) -> MutexGuard<'_, DopusCore> {
    core.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run headless until `dopus.quit`. The Bus is mandatory here — a headless
/// dopus with nobody to talk to is just a memory leak.
pub fn run(
    config: DOpusConfig,
    config_file: Option<ConfigFile>,
    dirs: Option<AppDirs>,
    service: &str,
    noded_url: &str,
) -> anyhow::Result<()> {
    let keymap_path: Option<PathBuf> = dirs.as_ref().map(|d| d.keymap_file());
    let keymap = keys::load(keymap_path.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let meta = ServerMeta {
        service: service.to_owned(),
        headless: true,
        config_path: dirs.as_ref().map(|d| d.config_dir().join("config.conf.mix").display().to_string()),
        // A headless process paints nothing; theme.set is refused below.
        theme_scheme: String::new(),
        theme_mode: String::new(),
        actions: verbs::action_table(&keymap),
    };

    let (core, receiver) = DopusCore::new(config, config_file);
    let core = Arc::new(Mutex::new(core));
    let (bus, mut deliveries) = bus::spawn(service, noded_url).map_err(|e| anyhow::anyhow!("{e}"))?;

    // The drainer: laws 1-4 on one thread.
    {
        let core = Arc::clone(&core);
        std::thread::Builder::new()
            .name("dopus-headless-core".to_owned())
            .spawn(move || loop {
                let derived = {
                    let mut core = lock(&core);
                    match receiver.recv_timeout(DRAIN_TICK) {
                        Ok(event) => core.on_event(event),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => core.tick(Instant::now()),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                answer_derived(&mut lock(&core), derived, |line| tracing::info!("{line}"));
            })
            .expect("spawning the headless core drainer");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("headless runtime: {e}"))?;
    tracing::info!("dopus headless as `{service}`");
    runtime.block_on(async {
        while let Some(delivery) = deliveries.next().await {
            let Delivery::Command(command) = delivery else { continue };
            if command.verb == "dopus.theme.set" {
                bus.respond(
                    command.id,
                    10,
                    serde_json::to_string(&verbs::Refusal {
                        error_code: verbs::code::UNAVAILABLE.to_owned(),
                        message: "theme selection needs the windowed app (P1 headless paints nothing)".to_owned(),
                        reason: Some("headless".to_owned()),
                    })
                    .unwrap_or_default(),
                );
                continue;
            }
            let mut quit = false;
            for served in verbs::serve_command(&command, &mut lock(&core), &meta, &cosmix_buildinfo::build_info!()) {
                match served {
                    Served::Reply { id, rc, body } => bus.respond(id, rc, body),
                    Served::ThemeSet { .. } => unreachable!("theme.set was refused above"),
                    Served::Quit { id } => {
                        bus.respond(
                            id,
                            0,
                            serde_json::to_string(&verbs::QuitReply { quitting: true }).unwrap_or_default(),
                        );
                        quit = true;
                    }
                }
            }
            if quit {
                break;
            }
        }
    });
    bus.quit();
    Ok(())
}
