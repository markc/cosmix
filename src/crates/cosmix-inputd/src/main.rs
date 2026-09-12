//! `cosmix-inputd` — the input verb broker daemon (AmigaOS `input.device`
//! successor). See `~/.ctl/_plan/2026-09-12-cosmix-inputd-plan.md`.
//!
//! This is the **non-grab scaffolding**: it registers the `inputd` Bus service
//! and serves the `input.*` verbs (query/bind/unbind/mode/reload) over the
//! [`cosmix_input_core::Resolver`] — no keyboard needed for that surface. An
//! optional `--observe <device>` reader validates resolution against real
//! hardware safely (no grab, no re-emit, no verbs fired). The interception path
//! (`--grab`, EVIOCGRAB + uinput) is the dead-keyboard-risk step reserved for a
//! supervised bring-up and is not wired.

mod reader;
mod service;

use std::sync::{Arc, Mutex};

use clap::Parser;
use cosmix_input_core::{Resolver, default_keymap};

use service::Shared;

/// The Bus service name inputd registers as.
const SERVICE: &str = "inputd";

#[derive(Parser, Debug)]
#[command(
    name = "cosmix-inputd",
    about = "Input verb broker: keys become Bus verbs. Serves the inputd service; \
             --observe validates resolution on real hardware (safe); --grab is the \
             supervised interception step (not yet wired)."
)]
struct Args {
    /// Read this keyboard event node in OBSERVE-ONLY mode (no grab, no re-emit,
    /// no verbs fired). Needs root; reads keystrokes — run it deliberately.
    #[arg(long, value_name = "DEVICE")]
    observe: Option<String>,

    /// Reserved: EVIOCGRAB + uinput interception. Refuses for now (supervised P2).
    #[arg(long, value_name = "DEVICE")]
    grab: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let resolver: Shared = Arc::new(Mutex::new(Resolver::new(default_keymap())));

    // The evdev reader (if requested) runs on its own blocking thread; the Bus
    // service runs on the tokio runtime below. The two share the resolver.
    if let Some(device) = args.grab.clone() {
        let shared = Arc::clone(&resolver);
        std::thread::spawn(move || {
            if let Err(error) = reader::run_grab(&device, shared) {
                eprintln!("cosmix-inputd: grab reader: {error}");
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(resolver))
}

/// Reconnect loop: (re)register `inputd` with noded and serve `input.*` until the
/// connection drops, then reconnect.
async fn serve(resolver: Shared) -> anyhow::Result<()> {
    let bi = cosmix_buildinfo::build_info!();
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
                eprintln!("cosmix-inputd: registered as '{SERVICE}'; serving input.*");
                serve_bus(&client, &resolver).await;
                eprintln!("cosmix-inputd: broker disconnected; reconnecting");
            }
            Err(error) => {
                eprintln!("cosmix-inputd: connect failed ({error}); retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Drain and answer inbound `input.*` commands until the connection closes.
async fn serve_bus(client: &cosmix_client::NodedClient, resolver: &Shared) {
    let Some(mut rx) = client.incoming_async().await else {
        return;
    };
    while let Some(cmd) = rx.recv().await {
        let (rc, body) = service::dispatch(resolver, &cmd);
        let _ = client
            .respond_parts(&cmd.from, &cmd.command, cmd.id.as_deref(), rc, &body)
            .await;
    }
}
