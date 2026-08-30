//! Manual live gate for the trayd 0.3 Mix run surface.
//!
//! Run only on a private session bus with an isolated HOME. The
//! transient services still use the real per-user systemd manager.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::MatchRule;

const BUS_NAME: &str = "dev.cosmix.trayd";
const OBJECT_PATH: &str = "/dev/cosmix/trayd";
const INTERFACE_NAME: &str = "dev.cosmix.trayd";

type WireMixScript = (String, String, String, bool, u64, u64);
type WireMixRun = (
    String,
    String,
    String,
    String,
    u64,
    u64,
    bool,
    i32,
    String,
    String,
    u64,
    u64,
);
type WireMixOutput = (u64, String, String);

#[derive(Debug, Deserialize, zbus::zvariant::Type)]
struct MixSnapshot {
    revision: u64,
    state: String,
    error: String,
    scripts: Vec<WireMixScript>,
    runs: Vec<WireMixRun>,
    active_runs: u32,
}

fn signal_iterator(connection: &Connection, member: &str) -> Result<MessageIterator, String> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BUS_NAME)
        .map_err(|error| error.to_string())?
        .path(OBJECT_PATH)
        .map_err(|error| error.to_string())?
        .interface(INTERFACE_NAME)
        .map_err(|error| error.to_string())?
        .member(member)
        .map_err(|error| error.to_string())?
        .build();
    MessageIterator::for_match_rule(rule, connection, Some(128))
        .map_err(|error| format!("cannot subscribe to {member}: {error}"))
}

fn get_snapshot(proxy: &Proxy<'_>) -> Result<MixSnapshot, String> {
    let snapshot: MixSnapshot = proxy
        .call("GetMixSnapshot", &())
        .map_err(|error| format!("GetMixSnapshot failed: {error}"))?;
    let _ = (
        snapshot.revision,
        &snapshot.state,
        &snapshot.error,
        &snapshot.scripts,
    );
    Ok(snapshot)
}

fn run_record(snapshot: &MixSnapshot, run_id: &str) -> Result<WireMixRun, String> {
    snapshot
        .runs
        .iter()
        .find(|run| run.0 == run_id)
        .cloned()
        .ok_or_else(|| format!("snapshot has no run {run_id}"))
}

fn script_record(snapshot: &MixSnapshot, script_id: &str) -> Result<WireMixScript, String> {
    snapshot
        .scripts
        .iter()
        .find(|script| script.0 == script_id)
        .cloned()
        .ok_or_else(|| format!("snapshot has no script {script_id}"))
}

fn wait_terminal(
    proxy: &Proxy<'_>,
    signals: &mut MessageIterator,
    run_id: &str,
) -> Result<WireMixRun, String> {
    loop {
        let message = signals
            .next()
            .ok_or_else(|| "MixRunChanged signal stream ended".to_owned())?
            .map_err(|error| format!("MixRunChanged failed: {error}"))?;
        let (_, changed_id) = message
            .body()
            .deserialize::<(u64, String)>()
            .map_err(|error| format!("cannot decode MixRunChanged: {error}"))?;
        if changed_id != run_id {
            continue;
        }
        let snapshot = get_snapshot(proxy)?;
        let run = run_record(&snapshot, run_id)?;
        if matches!(
            run.3.as_str(),
            "succeeded" | "failed" | "stopped" | "launch_failed"
        ) {
            return Ok(run);
        }
    }
}

fn wait_for_output(
    signals: &mut MessageIterator,
    run_id: &str,
    needle: &str,
) -> Result<(), String> {
    loop {
        let message = signals
            .next()
            .ok_or_else(|| "MixRunOutput signal stream ended".to_owned())?
            .map_err(|error| format!("MixRunOutput failed: {error}"))?;
        let (_, changed_id, chunks, _, _) = message
            .body()
            .deserialize::<(u64, String, Vec<WireMixOutput>, u64, u64)>()
            .map_err(|error| format!("cannot decode MixRunOutput: {error}"))?;
        if changed_id == run_id && chunks.iter().any(|chunk| chunk.2.contains(needle)) {
            return Ok(());
        }
    }
}

fn write_script(home: &Path, id: &str, source: &str) -> Result<(), String> {
    let path = home.join(".local/mix").join(id);
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    let content =
        format!("#!/opt/cosmix/bin/mix\n-- description: Phase 5 isolated live gate\n\n{source}");
    file.write_all(content.as_bytes())
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("setting {} mode: {error}", path.display()))
}

fn create_script(
    proxy: &Proxy<'_>,
    home: &Path,
    name: &str,
    source: &str,
) -> Result<String, String> {
    let id: String = proxy
        .call("CreateMixScript", &(name, "Phase 5 isolated live gate"))
        .map_err(|error| format!("CreateMixScript failed: {error}"))?;
    println!("LIVE_MIX_CREATE id={id} name={name}");
    write_script(home, &id, source)?;
    println!("LIVE_MIX_EDIT id={id} bytes={}", source.len());
    Ok(id)
}

fn cleanup(proxy: &Proxy<'_>, ids: &[String]) {
    for id in ids {
        let _: zbus::Result<()> = proxy.call("TrashMixScript", &(id.as_str(),));
        let _: zbus::Result<()> = proxy.call("PurgeMixScript", &(id.as_str(),));
    }
}

fn run() -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME must identify the isolated live store".to_owned())?;
    let connection =
        Connection::session().map_err(|error| format!("cannot connect to private bus: {error}"))?;
    let proxy = Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| format!("cannot create trayd proxy: {error}"))?;

    let mut success_id =
        create_script(&proxy, &home, "Live-success", "print(\"phase5-success\")\n")?;
    let failure_id = create_script(
        &proxy,
        &home,
        "Live-failure",
        "raise(\"PHASE5_FAILURE\", \"phase5-failure\")\n",
    )?;
    let stop_id = create_script(
        &proxy,
        &home,
        "Live-stop",
        "print(\"phase5-stop-started\")\nsleep(30)\n",
    )?;
    let result = (|| {
        proxy
            .call::<_, _, ()>(
                "UpdateMixScript",
                &(
                    success_id.as_str(),
                    "Renamed-success",
                    "Updated by the Phase 5 live gate",
                ),
            )
            .map_err(|error| format!("UpdateMixScript failed: {error}"))?;
        success_id = "Renamed-success".into();
        let renamed = script_record(&get_snapshot(&proxy)?, &success_id)?;
        if renamed.1 != "Renamed-success"
            || renamed.2 != "Updated by the Phase 5 live gate"
            || renamed.3
        {
            return Err(format!("unexpected renamed script: {renamed:?}"));
        }
        println!("LIVE_MIX_RENAME id={} name={}", renamed.0, renamed.1);

        let mut success_signals = signal_iterator(&connection, "MixRunChanged")?;
        let success_run: String = proxy
            .call("RunMixScript", &(success_id.as_str(),))
            .map_err(|error| format!("success RunMixScript failed: {error}"))?;
        let success = wait_terminal(&proxy, &mut success_signals, &success_run)?;
        if success.3 != "succeeded"
            || !success.8.contains("phase5-success")
            || !success.9.is_empty()
        {
            return Err(format!("unexpected success record: {success:?}"));
        }
        println!(
            "LIVE_MIX_SUCCESS state={} exit={} stdout={}B stderr={}B",
            success.3,
            success.7,
            success.8.len(),
            success.9.len()
        );

        let mut failure_signals = signal_iterator(&connection, "MixRunChanged")?;
        let failure_run: String = proxy
            .call("RunMixScript", &(failure_id.as_str(),))
            .map_err(|error| format!("failure RunMixScript failed: {error}"))?;
        let failure = wait_terminal(&proxy, &mut failure_signals, &failure_run)?;
        if failure.3 != "failed"
            || failure.7 == 0
            || !failure.9.contains("phase5-failure")
            || !failure.8.is_empty()
        {
            return Err(format!("unexpected failure record: {failure:?}"));
        }
        println!(
            "LIVE_MIX_FAILURE state={} exit={} stdout={}B stderr={}B",
            failure.3,
            failure.7,
            failure.8.len(),
            failure.9.len()
        );

        let mut stop_changes = signal_iterator(&connection, "MixRunChanged")?;
        let mut stop_output = signal_iterator(&connection, "MixRunOutput")?;
        let stop_run: String = proxy
            .call("RunMixScript", &(stop_id.as_str(),))
            .map_err(|error| format!("stop RunMixScript failed: {error}"))?;
        wait_for_output(&mut stop_output, &stop_run, "phase5-stop-started")?;
        proxy
            .call::<_, _, ()>("StopMixRun", &(stop_run.as_str(),))
            .map_err(|error| format!("StopMixRun failed: {error}"))?;
        let stopped = wait_terminal(&proxy, &mut stop_changes, &stop_run)?;
        if stopped.3 != "stopped" || !stopped.8.contains("phase5-stop-started") {
            return Err(format!("unexpected stopped record: {stopped:?}"));
        }
        let final_snapshot = get_snapshot(&proxy)?;
        if final_snapshot.active_runs != 0 {
            return Err(format!(
                "{} active runs remain after stop",
                final_snapshot.active_runs
            ));
        }
        println!(
            "LIVE_MIX_STOP state={} stdout={}B active_runs={}",
            stopped.3,
            stopped.8.len(),
            final_snapshot.active_runs
        );

        proxy
            .call::<_, _, ()>("TrashMixScript", &(success_id.as_str(),))
            .map_err(|error| format!("TrashMixScript failed: {error}"))?;
        let trashed = script_record(&get_snapshot(&proxy)?, &success_id)?;
        if !trashed.3 {
            return Err("TrashMixScript did not mark the script trashed".to_owned());
        }
        println!("LIVE_MIX_TRASH id={} trashed={}", trashed.0, trashed.3);

        proxy
            .call::<_, _, ()>("RestoreMixScript", &(success_id.as_str(),))
            .map_err(|error| format!("RestoreMixScript failed: {error}"))?;
        let restored = script_record(&get_snapshot(&proxy)?, &success_id)?;
        if restored.3 {
            return Err("RestoreMixScript left the script trashed".to_owned());
        }
        println!("LIVE_MIX_RESTORE id={} trashed={}", restored.0, restored.3);

        proxy
            .call::<_, _, ()>("TrashMixScript", &(success_id.as_str(),))
            .map_err(|error| format!("second TrashMixScript failed: {error}"))?;
        proxy
            .call::<_, _, ()>("PurgeMixScript", &(success_id.as_str(),))
            .map_err(|error| format!("PurgeMixScript failed: {error}"))?;
        if get_snapshot(&proxy)?
            .scripts
            .iter()
            .any(|script| script.0 == success_id)
        {
            return Err("PurgeMixScript left the script in the catalogue".to_owned());
        }
        println!("LIVE_MIX_PURGE id={success_id} present=false");
        Ok(())
    })();
    cleanup(&proxy, &[success_id, failure_id, stop_id]);
    result
}

fn main() {
    if let Err(error) = run() {
        eprintln!("LIVE_MIX_ERROR {error}");
        std::process::exit(1);
    }
    println!("LIVE_MIX_GATE_PASS");
}
