//! Read-only evidence from the grab reader's successful passthrough writes.
//! No key codes or text, no injection publisher, and no claim about upstream
//! hardware origin or downstream compositor delivery.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::service::Shared;

const CAPACITY: usize = 256;
pub const SNAPSHOT: &str = "input.passthrough.snapshot";
pub const WAIT: &str = "input.passthrough.wait";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub path: PathBuf,
    pub sysfs: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub rdev: u64,
}

impl Device {
    /// The caller supplies the owning open descriptor, never a device name.
    pub fn from_file(file: &File, path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        let metadata = file.metadata()?;
        let named = path.metadata()?;
        if !metadata.file_type().is_char_device() || metadata.dev() != named.dev()
            || metadata.ino() != named.ino() || metadata.rdev() != named.rdev()
        {
            return Err(io::Error::other("evdev descriptor/path identity mismatch"));
        }
        let name = path.file_name().and_then(|name| name.to_str())
            .filter(|name| name.strip_prefix("event").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
            .ok_or_else(|| io::Error::other("expected evdev event node"))?;
        let sysfs = Path::new("/sys/class/input").join(name).join("device").canonicalize()?;
        Ok(Self { path, sysfs, dev: metadata.dev(), ino: metadata.ino(), rdev: metadata.rdev() })
    }

    fn current(&self) -> bool {
        self.path.metadata().is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino && m.rdev() == self.rdev)
            && self.path.file_name().is_some_and(|name| Path::new("/sys/class/input").join(name).join("device").canonicalize().is_ok_and(|path| path == self.sysfs))
    }
}

use std::os::unix::fs::FileTypeExt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    boot_id: String,
    pid: u32,
    start_ticks: u64,
    executable: PathBuf,
    executable_blake3: String,
}

fn hash_bounded(path: &Path, bound: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > bound { return Err(io::Error::other("witness hash input too large")); }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 16384];
    let mut size = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        size += count as u64;
        if size > bound { return Err(io::Error::other("witness hash input grew beyond bound")); }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    path: Option<PathBuf>,
    file_blake3: Option<String>,
    resolver_blake3: String,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fence {
    owner: Owner,
    config: Config,
    lifetime: u64,
    ingress: Device,
    output: Device,
}

#[derive(Clone, Debug, Serialize)]
struct Record {
    sequence: u64,
    ingress_sequence: u64,
    read_monotonic_ns: u64,
    written_monotonic_ns: u64,
    kind: &'static str,
    generation: u64,
}

#[derive(Default)]
struct State {
    lifetime: u64,
    devices: Option<(Device, Device)>,
    sequence: u64,
    records: VecDeque<Record>,
}

pub struct Witness {
    owner: Owner,
    config_path: Option<PathBuf>,
    state: Mutex<State>,
    changed: Notify,
}

impl Witness {
    pub fn new(config_path: Option<PathBuf>) -> io::Result<Arc<Self>> {
        let stat = std::fs::read_to_string("/proc/self/stat")?;
        let tail = stat.rsplit_once(") ").ok_or_else(|| io::Error::other("invalid process stat"))?.1;
        let start_ticks = tail.split_whitespace().nth(19).ok_or_else(|| io::Error::other("missing process start"))?
            .parse().map_err(io::Error::other)?;
        let executable = std::fs::read_link("/proc/self/exe")?;
        let executable_blake3 = hash_bounded(Path::new("/proc/self/exe"), 256 * 1024 * 1024)?;
        let owner = Owner { boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_string(), pid: std::process::id(), start_ticks, executable, executable_blake3 };
        Ok(Arc::new(Self { owner, config_path, state: Mutex::new(State::default()), changed: Notify::new() }))
    }

    fn config(&self, resolver: &Shared) -> Result<Config, &'static str> {
        let live = resolver.lock().map_err(|_| "resolver_poisoned")?;
        let resolver_blake3 = blake3::hash(&serde_json::to_vec(&(live.mode(), live.physical_rows())).map_err(|_| "config_encoding")?).to_hex().to_string();
        let file_blake3 = self.config_path.as_ref().map(|path| hash_bounded(path, 1024 * 1024)).transpose().map_err(|_| "config_unavailable")?;
        Ok(Config { path: self.config_path.clone(), file_blake3, resolver_blake3, generation: live.generation() })
    }

    fn fence(&self, config: Config, state: &State) -> Result<Fence, &'static str> {
        let (ingress, output) = state.devices.as_ref().ok_or("reader_inactive")?;
        if !ingress.current() || !output.current() { return Err("device_replaced"); }
        Ok(Fence { owner: self.owner.clone(), config, lifetime: state.lifetime, ingress: ingress.clone(), output: output.clone() })
    }

    pub fn snapshot(&self, resolver: &Shared) -> Result<serde_json::Value, &'static str> {
        let config = self.config(resolver)?;
        let state = self.state.lock().map_err(|_| "witness_poisoned")?;
        let fence = self.fence(config, &state)?;
        Ok(serde_json::json!({"schema":"input-passthrough.v1","fence":fence,"cursor":state.sequence,"capacity":CAPACITY,"upstream_hardware_origin_verified":false,"downstream_delivery_verified":false}))
    }

    pub fn reader(self: &Arc<Self>, ingress: Device, output: Device) -> ReaderGuard {
        let mut state = self.state.lock().expect("witness poisoned");
        state.lifetime += 1;
        state.devices = Some((ingress, output));
        state.sequence = 0;
        state.records.clear();
        let lifetime = state.lifetime;
        drop(state);
        self.changed.notify_waiters();
        ReaderGuard { witness: Arc::clone(self), lifetime }
    }

    pub fn notify_config(&self) { self.changed.notify_waiters(); }

    pub async fn wait(&self, resolver: &Shared, request: WaitRequest) -> Result<serde_json::Value, &'static str> {
        if request.timeout_ms == 0 || request.timeout_ms > 30000 { return Err("invalid_deadline"); }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.timeout_ms);
        loop {
            // Enable before inspecting state: no successful write can be lost
            // between inspection and parking, including concurrent waiters.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let config = self.config(resolver)?;
            {
                let state = self.state.lock().map_err(|_| "witness_poisoned")?;
                if self.fence(config, &state)? != request.fence { return Err("stale_fence"); }
                if request.after > state.sequence { return Err("future_cursor"); }
                if state.records.front().is_some_and(|record| request.after.saturating_add(1) < record.sequence) { return Err("cursor_gap"); }
                let records: Vec<_> = state.records.iter().filter(|record| record.sequence > request.after).cloned().collect();
                if !records.is_empty() {
                    return Ok(serde_json::json!({"schema":"input-passthrough.v1","fence":request.fence,"after":request.after,"cursor":state.sequence,"records":records,"upstream_hardware_origin_verified":false,"downstream_delivery_verified":false}));
                }
            }
            tokio::time::timeout_at(deadline, notified).await.map_err(|_| "deadline_elapsed")?;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    fence: Fence,
    after: u64,
    timeout_ms: u64,
}

/// Held only by the grab reader. Dropping it invalidates every prior fence.
pub struct ReaderGuard {
    witness: Arc<Witness>,
    lifetime: u64,
}

impl ReaderGuard {
    pub fn written(&self, ingress_sequence: u64, read_monotonic_ns: u64, kind: u16, generation: u64) {
        // Successful KEY and SYN writes only; no codes, values, or text.
        let kind = match kind { 0 => "sync", 1 => "keyboard", _ => return };
        let mut state = self.witness.state.lock().expect("witness poisoned");
        if state.lifetime != self.lifetime || state.devices.is_none() { return; }
        state.sequence += 1;
        let sequence = state.sequence;
        state.records.push_back(Record { sequence, ingress_sequence, read_monotonic_ns, written_monotonic_ns: monotonic_ns(), kind, generation });
        if state.records.len() > CAPACITY { state.records.pop_front(); }
        drop(state);
        self.witness.changed.notify_waiters();
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        let mut state = self.witness.state.lock().expect("witness poisoned");
        if state.lifetime == self.lifetime { state.devices = None; state.records.clear(); }
        drop(state);
        self.witness.changed.notify_waiters();
    }
}

pub fn monotonic_ns() -> u64 {
    let mut value = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // CLOCK_MONOTONIC is supported by the Linux evdev platform.
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) }, 0);
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

pub fn reply(result: Result<serde_json::Value, &'static str>) -> (u8, String) {
    match result {
        Ok(value) => (0, value.to_string()),
        Err(code) => (10, serde_json::json!({"error_code":code,"upstream_hardware_origin_verified":false,"downstream_delivery_verified":false}).to_string()),
    }
}
