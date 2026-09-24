//! `cosmix-inputd` — the input verb broker daemon (AmigaOS `input.device`
//! successor).
//!
//! Registers the `inputd` Bus service and serves keymap verbs over the
//! [`cosmix_input_core::Resolver`] and key/pointer injection through a persistent
//! uinput device — no physical keyboard needed for either surface. An
//! optional `--observe <device>` reader validates resolution against real
//! hardware safely (no grab, no re-emit, no verbs fired). The interception path
//! (`--grab`, EVIOCGRAB + uinput) re-emits unbound keys and fires bound verbs.

mod keymap_file;
mod reader;
mod service;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use cosmix_input_core::{Resolver, default_keymap};
use cosmix_input_schema::{InputKeymap, KEYMAP_SCHEMA_VERSION};

use service::Shared;

/// The Bus service name inputd registers as.
const SERVICE: &str = "inputd";

#[derive(Parser, Debug)]
#[command(
    name = "cosmix-inputd",
    about = "Input verb broker: keys become Bus verbs. Serves the inputd service; \
             --observe validates resolution on real hardware (safe); --grab is the \
             supervised interception path. Key and pointer verbs inject through uinput."
)]
struct Args {
    /// Read this keyboard event node in OBSERVE-ONLY mode (no grab, no re-emit,
    /// no verbs fired). Needs root; reads keystrokes — run it deliberately.
    #[arg(long, value_name = "DEVICE")]
    observe: Option<String>,

    /// EVIOCGRAB + uinput interception on this node (the supervised path): grab
    /// the device, swallow bound strokes and fire their verbs, re-emit the rest.
    /// Dead-keyboard-risk but recoverable — killing the process releases the grab.
    #[arg(long, value_name = "DEVICE")]
    grab: Option<String>,

    /// Like --grab, but locate the device by its Name in /proc/bus/input/devices
    /// (e.g. "keyd virtual keyboard") instead of a fixed path — the eventN can
    /// shift across keyd/boot restarts, so a systemd unit should use this.
    #[arg(long, value_name = "NAME", conflicts_with = "grab")]
    grab_name: Option<String>,

    /// Safety valve for --grab/--grab-name: auto-release the grab and exit after
    /// this many seconds. Omit to grab indefinitely (production). Use a small
    /// value for the first supervised bring-up so a wedged grab self-heals.
    #[arg(long, value_name = "SECONDS")]
    grab_timeout: Option<u64>,

    /// Keymap file to load and write rebinds through. Defaults to
    /// $COSMIX_INPUTD_KEYMAP or <config>/cosmix/inputd/keymap.json.
    #[arg(long, value_name = "PATH")]
    keymap: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Persistence: load the keymap file if present so rebinds are remembered;
    // otherwise seed it from the default so the file exists for next time.
    // An unusable document is moved aside (never deleted) before the defaults
    // are seeded; see keymap_file::open. Dropped rows and the recovery were
    // already logged there.
    let mut keymap_path: Option<PathBuf> = args
        .keymap
        .map(PathBuf::from)
        .or_else(keymap_file::default_path);
    let keymap = match keymap_path.as_deref() {
        Some(path) => {
            let opened = keymap_file::open(path, &keymap_file::local_stamp());
            if let Some(backup) = opened.recovered_from {
                service::set_recovered_from(backup);
            }
            if !opened.persist {
                // The unusable file could not be moved aside: never write over it.
                keymap_path = None;
            }
            InputKeymap {
                version: KEYMAP_SCHEMA_VERSION,
                semantic: cosmix_input_schema::Keymap::default(),
                physical: opened.rows,
            }
        }
        None => default_keymap(),
    };
    let resolver: Shared = Arc::new(Mutex::new(Resolver::new(keymap)));

    // Grab mode fires resolved verbs; the reader (blocking thread) sends them to
    // the tokio side (which owns the Bus client) over this channel. Keeping the
    // original sender alive means the receiver never closes even without a reader.
    let (fire_tx, fire_rx) = tokio::sync::mpsc::unbounded_channel::<reader::FiredVerb>();

    // Resolve --grab-name to a concrete device path (stable across keyd/boot
    // restarts); --grab takes a path directly. clap enforces they're exclusive.
    let grab_device: Option<String> = match (&args.grab, &args.grab_name) {
        (Some(path), _) => Some(path.clone()),
        (None, Some(name)) => match resolve_device_by_name(name) {
            Ok(path) => {
                eprintln!("cosmix-inputd: --grab-name {name:?} resolved to {path}");
                Some(path)
            }
            Err(error) => {
                eprintln!("cosmix-inputd: {error}");
                std::process::exit(1);
            }
        },
        (None, None) => None,
    };

    // The evdev reader (if requested) runs on its own blocking thread; the Bus
    // service runs on the tokio runtime below. The two share the resolver.
    if let Some(device) = grab_device {
        let shared = Arc::clone(&resolver);
        let tx = fire_tx.clone();
        let timeout = args.grab_timeout.map(Duration::from_secs);
        std::thread::spawn(move || {
            // run_grab only returns on a device/uinput error (otherwise it loops
            // until the watchdog exits the process). The grab is already released
            // by fd-close here; exit so the daemon does not linger grab-less, and
            // systemd (Restart=always) can re-establish it.
            if let Err(error) = reader::run_grab(&device, shared, tx, timeout) {
                eprintln!("cosmix-inputd: grab reader stopped: {error}");
                std::process::exit(1);
            }
        });
    } else if let Some(device) = args.observe.clone() {
        let shared = Arc::clone(&resolver);
        std::thread::spawn(move || {
            if let Err(error) = reader::run_observe(&device, shared) {
                eprintln!("cosmix-inputd: observe reader stopped: {error}");
            }
        });
    }
    // Hold the sender open for the process lifetime so `fire_rx.recv()` parks
    // rather than returning None when no grab reader is running.
    let _fire_tx = fire_tx;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(resolver, keymap_path, fire_rx))
}

/// Reconnect loop: (re)register `inputd` with noded and serve `input.*` until the
/// connection drops, then reconnect.
async fn serve(
    resolver: Shared,
    keymap_path: Option<PathBuf>,
    mut fire_rx: tokio::sync::mpsc::UnboundedReceiver<reader::FiredVerb>,
) -> anyhow::Result<()> {
    let bi = cosmix_buildinfo::build_info!();
    // Retained across broker reconnects, with or without an evdev reader.
    let mut injector = reader::PointerInjector::default();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(
            SERVICE,
            provenance.clone(),
        )
        .await
        {
            Ok(client) => {
                let client = client.with_verbs(service::verb_manifest());
                eprintln!("cosmix-inputd: registered as '{SERVICE}'; serving input.*");
                serve_bus(
                    &client,
                    &resolver,
                    keymap_path.as_deref(),
                    &mut injector,
                    &mut fire_rx,
                )
                .await;
                eprintln!("cosmix-inputd: broker disconnected; reconnecting");
            }
            Err(error) => {
                eprintln!("cosmix-inputd: connect failed ({error}); retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Drain and answer inbound `input.*` commands, and fire verbs the grab reader
/// resolves, until the connection closes.
async fn serve_bus(
    client: &cosmix_client::NodedClient,
    resolver: &Shared,
    keymap_path: Option<&std::path::Path>,
    injector: &mut reader::PointerInjector,
    fire_rx: &mut tokio::sync::mpsc::UnboundedReceiver<reader::FiredVerb>,
) {
    let Some(mut rx) = client.incoming_async().await else {
        return;
    };
    loop {
        tokio::select! {
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else { break };
                let (rc, body) = service::dispatch(resolver, keymap_path, injector, &cmd);
                let _ = client
                    .respond_parts(&cmd.from, &cmd.command, cmd.id.as_deref(), rc, &body)
                    .await;
            }
            maybe_fired = fire_rx.recv() => {
                // The sender is held open for the process lifetime, so this parks
                // rather than yielding None; break defensively if it ever closes.
                let Some(fired) = maybe_fired else { break };
                fire_verb(client, &fired).await;
            }
        }
    }
}

/// The `(service, command)` a fired verb is sent as. A row with an explicit
/// `service` targets it with the verb UNCHANGED; without one the service is the
/// verb's first dot-segment (`desktop.workspace.next` -> `desktop`) and the
/// command is still the whole verb. `None` when no target can be derived.
fn fire_target(fired: &reader::FiredVerb) -> Option<(&str, &str)> {
    let service = match fired.service.as_deref() {
        Some(explicit) => explicit,
        None => fired.verb.split('.').next().unwrap_or(""),
    };
    if service.is_empty() {
        return None;
    }
    Some((service, fired.verb.as_str()))
}

/// Deliver a resolved verb fire-and-forget to [`fire_target`]; a placeholder
/// verb with no handler (e.g. `user.f09`) simply no-routes, which is fine. A
/// binding with `args` sends them as the body — the ARexx model: a message is
/// a verb plus arguments.
async fn fire_verb(client: &cosmix_client::NodedClient, fired: &reader::FiredVerb) {
    let Some((service, command)) = fire_target(fired) else {
        return;
    };
    // Belt-and-braces: whatever path admitted the row (bind, file load, a
    // future replace_physical caller), a non-map must never reach the wire.
    let body = match fired.args.clone() {
        Some(args) if args.is_object() => args,
        Some(_) => {
            eprintln!("cosmix-inputd: fire {}: non-object args dropped", fired.verb);
            serde_json::json!({})
        }
        None => serde_json::json!({}),
    };
    if let Err(error) = client.send(service, command, body).await {
        eprintln!("cosmix-inputd: fire {command} -> {service}: {error}");
    }
}

/// Find a keyboard event node by its device Name in /proc/bus/input/devices, so a
/// unit can target e.g. "keyd virtual keyboard" without hardcoding an eventN
/// (which shifts when keyd restarts or across boots). Returns the first matching
/// `/dev/input/eventN`. Blocks are separated by blank lines; each carries an
/// `N: Name="..."` and an `H: Handlers=... eventN ...`.
fn resolve_device_by_name(name: &str) -> anyhow::Result<String> {
    let text = std::fs::read_to_string("/proc/bus/input/devices")?;
    for block in text.split("\n\n") {
        let name_matches = block.lines().any(|line| {
            line.strip_prefix("N: Name=")
                .map(|rest| rest.trim().trim_matches('"') == name)
                .unwrap_or(false)
        });
        if !name_matches {
            continue;
        }
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("H: Handlers=")
                && let Some(node) = rest.split_whitespace().find(|tok| {
                    tok.strip_prefix("event")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
            {
                return Ok(format!("/dev/input/{node}"));
            }
        }
    }
    anyhow::bail!("no input device named {name:?} in /proc/bus/input/devices");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(verb: &str, service: Option<&str>) -> reader::FiredVerb {
        reader::FiredVerb {
            verb: verb.to_string(),
            args: None,
            service: service.map(str::to_string),
        }
    }

    #[test]
    fn explicit_service_is_the_target_and_the_verb_is_unchanged() {
        let f = fired("desktop.clipboard.menu", Some("desktop-vt1"));
        assert_eq!(fire_target(&f), Some(("desktop-vt1", "desktop.clipboard.menu")));
    }

    #[test]
    fn absent_service_routes_by_first_segment() {
        let f = fired("desktop.workspace.next", None);
        assert_eq!(fire_target(&f), Some(("desktop", "desktop.workspace.next")));
        // The pre-fix live row still routes exactly as before (to the first
        // segment, whole string as command) — the rule itself is unchanged.
        let f = fired("desktop-vt1.desktop.clipboard.menu", None);
        assert_eq!(
            fire_target(&f),
            Some(("desktop-vt1", "desktop-vt1.desktop.clipboard.menu"))
        );
    }

    #[test]
    fn no_derivable_target_fires_nothing() {
        assert_eq!(fire_target(&fired(".x", None)), None);
        assert_eq!(fire_target(&fired("desktop.x", Some(""))), None);
    }

    #[test]
    fn default_clipboard_rows_fire_at_the_citizen() {
        // End to end through the shipped keymap: resolve the stroke, build the
        // FiredVerb the grab reader would, and derive the wire target.
        let r = cosmix_input_core::Resolver::new(default_keymap());
        for (code, verb) in [(108, "desktop.clipboard.menu"), (103, "desktop.clipboard.rotate")] {
            let out = r.resolve(
                code,
                cosmix_input_schema::SideModifiers::RIGHT_CTRL,
                cosmix_input_core::Edge::Press,
            );
            let f = reader::FiredVerb {
                verb: out.verb.expect("bound").as_str().to_string(),
                args: out.args,
                service: out.service,
            };
            assert_eq!(fire_target(&f), Some(("desktop-vt1", verb)));
        }
    }
}
