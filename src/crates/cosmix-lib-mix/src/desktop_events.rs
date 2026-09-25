//! Desktop status sources: network links/addresses and audio volume.
//!
//! Both feed the evaluator-owned native-event queue in `native_events.rs`,
//! with the same lifecycle as `fs_watch`: an opaque handle per registration,
//! bounded coalescing with sticky overflow, cancellation on unwatch, and
//! retirement with the evaluator generation. Neither source polls.
//!
//! * **Network** is an rtnetlink subscription (`RTMGRP_LINK`,
//!   `RTMGRP_IPV4_IFADDR`, `RTMGRP_IPV6_IFADDR`) read by one thread per handle,
//!   blocked in poll(2) on the socket and a cancellation FD with no timeout.
//!   The socket is raw libc: the messages used are three fixed headers plus
//!   attributes, and the workspace's `netlink-packet-*` crates are split over
//!   two versions, so linking one into the core interpreter would be heavier
//!   than the parser below. `net_state()` answers the same questions from a
//!   netlink dump, so a behaviour can re-read truth after any batch.
//! * **Audio** is a managed `pactl subscribe` child (PipeWire's pulse server).
//!   No native PipeWire client crate is in the workspace, and libpipewire
//!   would bring a C library and its main loop into every Mix build. The child
//!   is an event stream, not a poll: it writes one line per server change and
//!   is otherwise blocked. Lines become `audio.changed` records; the volume
//!   itself is read by `audio_state()` (one `wpctl get-volume` call), which a
//!   behaviour runs once per delivered batch, so a burst costs one read.
use crate::{
    error::MixResult,
    native_events::refusal,
    value::Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Net and audio handles together, per evaluator.
pub(crate) const MAX_SOURCES: usize = 16;
/// Distinct coalesced records one handle holds before it reports overflow.
pub(crate) const MAX_SOURCE_PENDING: usize = 1024;

// ── rtnetlink wire format (linux/netlink.h, linux/rtnetlink.h) ──────────
const NLMSG_HDRLEN: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_OVERRUN: u16 = 4;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
#[cfg(target_os = "linux")]
const RTM_GETLINK: u16 = 18;
#[cfg(target_os = "linux")]
const RTM_GETADDR: u16 = 22;
#[cfg(target_os = "linux")]
const NLM_F_REQUEST: u16 = 0x1;
#[cfg(target_os = "linux")]
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_DUMP_INTR: u16 = 0x10;
const IFLA_IFNAME: u16 = 3;
const IFLA_OPERSTATE: u16 = 16;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
pub(crate) const RTMGRP_LINK: u32 = 0x1;
pub(crate) const RTMGRP_IPV4_IFADDR: u32 = 0x10;
pub(crate) const RTMGRP_IPV6_IFADDR: u32 = 0x100;

fn align(n: usize) -> usize {
    (n + 3) & !3
}
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// One link or address notification, decoded but not yet named.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Record {
    Link {
        index: i32,
        ifname: Option<String>,
        flags: u32,
        operstate: Option<u8>,
        removed: bool,
    },
    Addr {
        index: i32,
        family: u8,
        prefix: u8,
        address: Option<IpAddr>,
        label: Option<String>,
        removed: bool,
    },
}

/// One datagram's worth of messages. `overflow` means something could not be
/// trusted (kernel overrun, truncation, malformed framing): re-read state.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub records: Vec<Record>,
    pub overflow: bool,
    pub done: bool,
    pub interrupted: bool,
    pub error: Option<i32>,
}

fn attributes(b: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    let mut o = 0;
    while o + 4 <= b.len() {
        let len = u16_at(b, o) as usize;
        // Strip NLA_F_NESTED / NLA_F_NET_BYTEORDER.
        let kind = u16_at(b, o + 2) & 0x3fff;
        if len < 4 || o + len > b.len() {
            return None;
        }
        out.push((kind, &b[o + 4..o + len]));
        o += align(len);
    }
    Some(out)
}

fn c_string(v: &[u8]) -> Option<String> {
    let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
    std::str::from_utf8(&v[..end]).ok().map(str::to_owned)
}

fn ip(family: u8, v: &[u8]) -> Option<IpAddr> {
    match (family, v.len()) {
        (AF_INET, 4) => Some(IpAddr::V4(Ipv4Addr::new(v[0], v[1], v[2], v[3]))),
        (AF_INET6, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(v);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

fn link(body: &[u8], removed: bool) -> Option<Record> {
    // struct ifinfomsg { u8 family, u8 pad, u16 type, i32 index, u32 flags, u32 change }
    if body.len() < 16 {
        return None;
    }
    let index = u32_at(body, 4) as i32;
    let flags = u32_at(body, 8);
    let mut ifname = None;
    let mut operstate = None;
    for (kind, v) in attributes(&body[16..])? {
        match kind {
            IFLA_IFNAME => ifname = c_string(v),
            IFLA_OPERSTATE if !v.is_empty() => operstate = Some(v[0]),
            _ => {}
        }
    }
    Some(Record::Link {
        index,
        ifname,
        flags,
        operstate,
        removed,
    })
}

fn addr(body: &[u8], removed: bool) -> Option<Record> {
    // struct ifaddrmsg { u8 family, u8 prefixlen, u8 flags, u8 scope, u32 index }
    if body.len() < 8 {
        return None;
    }
    let family = body[0];
    let prefix = body[1];
    let index = u32_at(body, 4) as i32;
    let mut address = None;
    let mut local = None;
    let mut label = None;
    for (kind, v) in attributes(&body[8..])? {
        match kind {
            IFA_ADDRESS => address = ip(family, v),
            IFA_LOCAL => local = ip(family, v),
            IFA_LABEL => label = c_string(v),
            _ => {}
        }
    }
    // On a point-to-point IPv4 link IFA_ADDRESS is the peer; IFA_LOCAL is ours.
    Some(Record::Addr {
        index,
        family,
        prefix,
        address: local.or(address),
        label,
        removed,
    })
}

/// Decode one netlink datagram. Pure: fixture bytes in, records out.
pub(crate) fn parse(buf: &[u8]) -> Parsed {
    let mut out = Parsed::default();
    let mut o = 0;
    while o + NLMSG_HDRLEN <= buf.len() {
        let len = u32_at(buf, o) as usize;
        let kind = u16_at(buf, o + 4);
        let flags = u16_at(buf, o + 6);
        if len < NLMSG_HDRLEN || o + len > buf.len() {
            out.overflow = true;
            return out;
        }
        if flags & NLM_F_DUMP_INTR != 0 {
            out.interrupted = true;
        }
        let body = &buf[o + NLMSG_HDRLEN..o + len];
        match kind {
            NLMSG_DONE => {
                out.done = true;
                return out;
            }
            NLMSG_ERROR => {
                if body.len() < 4 {
                    out.overflow = true;
                } else {
                    let code = u32_at(body, 0) as i32;
                    if code != 0 {
                        // i32::MIN has no errno: untrusted framing, not a panic.
                        match code.checked_neg() {
                            Some(errno) => out.error = Some(errno),
                            None => out.overflow = true,
                        }
                    }
                }
            }
            NLMSG_OVERRUN => out.overflow = true,
            RTM_NEWLINK | RTM_DELLINK => match link(body, kind == RTM_DELLINK) {
                Some(r) => out.records.push(r),
                None => out.overflow = true,
            },
            RTM_NEWADDR | RTM_DELADDR => match addr(body, kind == RTM_DELADDR) {
                Some(r) => out.records.push(r),
                None => out.overflow = true,
            },
            _ => {}
        }
        o += align(len);
    }
    if o < buf.len() {
        out.overflow = true;
    }
    out
}

fn operstate_name(state: u8) -> &'static str {
    match state {
        1 => "notpresent",
        2 => "down",
        3 => "lowerlayerdown",
        4 => "testing",
        5 => "dormant",
        6 => "up",
        _ => "unknown",
    }
}

/// Operationally up. Loopback and tunnel drivers (lo, WireGuard) report
/// operstate "unknown"; for those the carrier flags decide.
fn link_up(flags: u32, operstate: Option<u8>) -> bool {
    match operstate {
        Some(6) => true,
        None | Some(0) => flags & IFF_UP != 0 && flags & IFF_RUNNING != 0,
        _ => false,
    }
}

/// Name a record and render it as a coalescing key plus the `$event.args`
/// change map. `names` caches index -> ifname from link records; `lookup`
/// resolves an index the cache has not seen (if_indextoname in production).
pub(crate) fn change(
    record: &Record,
    names: &mut BTreeMap<i32, String>,
    lookup: &dyn Fn(i32) -> Option<String>,
) -> (String, serde_json::Value) {
    match record {
        Record::Link {
            index,
            ifname,
            flags,
            operstate,
            removed,
        } => {
            let name = ifname
                .clone()
                .or_else(|| names.get(index).cloned())
                .or_else(|| lookup(*index))
                .unwrap_or_default();
            if *removed {
                names.remove(index);
            } else if !name.is_empty() {
                names.insert(*index, name.clone());
            }
            let mut v = serde_json::json!({
                "kind": "link",
                "ifname": name,
                "index": index,
                "up": !removed && link_up(*flags, *operstate),
                "loopback": flags & IFF_LOOPBACK != 0,
                "removed": removed,
            });
            if let Some(state) = operstate {
                v["operstate"] = operstate_name(*state).into();
            }
            (format!("link:{index}"), v)
        }
        Record::Addr {
            index,
            family,
            prefix,
            address,
            label,
            removed,
        } => {
            let name = names
                .get(index)
                .cloned()
                .or_else(|| label.clone())
                .or_else(|| lookup(*index))
                .unwrap_or_default();
            let address = address.map(|a| a.to_string()).unwrap_or_default();
            let family = match *family {
                AF_INET => "inet",
                AF_INET6 => "inet6",
                _ => "other",
            };
            let v = serde_json::json!({
                "kind": "addr",
                "ifname": name,
                "index": index,
                "up": !removed,
                "family": family,
                "address": address,
                "prefix": prefix,
                "removed": removed,
            });
            (format!("addr:{index}:{address}/{prefix}"), v)
        }
    }
}

/// Suppresses repeats that change nothing a status model can see: wireless
/// drivers re-announce links on every scan and IPv6 re-announces addresses
/// on every lifetime refresh. Cleared on overflow, so a lost message can
/// never hide a later real change.
#[derive(Default)]
pub(crate) struct Dedupe {
    links: BTreeMap<i64, serde_json::Value>,
    addrs: BTreeSet<String>,
}

impl Dedupe {
    pub fn admit(&mut self, key: &str, v: &serde_json::Value) -> bool {
        let removed = v["removed"] == true;
        match v["kind"].as_str() {
            Some("link") => {
                let index = v["index"].as_i64().unwrap_or(-1);
                if removed {
                    self.links.remove(&index);
                    return true;
                }
                if self.links.get(&index) == Some(v) {
                    return false;
                }
                self.links.insert(index, v.clone());
                true
            }
            Some("addr") => {
                if removed {
                    self.addrs.remove(key);
                    true
                } else {
                    self.addrs.insert(key.to_owned())
                }
            }
            _ => true,
        }
    }

    pub fn clear(&mut self) {
        self.links.clear();
        self.addrs.clear();
    }
}

/// `net_watch` options: `{events: ["link", "addr"]}` (default both).
pub(crate) fn net_groups(value: Option<&Value>) -> MixResult<u32> {
    let all = RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR;
    let Some(value) = value else { return Ok(all) };
    let Value::Map(map) = value else {
        return Err(refusal("NET_WATCH_OPTIONS", "options must be a map"));
    };
    let mut groups = all;
    for (key, value) in map.iter() {
        match (key.as_str(), value) {
            ("events", Value::List(values)) => {
                groups = 0;
                for v in values.iter() {
                    match v {
                        Value::String(s) if s == "link" => groups |= RTMGRP_LINK,
                        Value::String(s) if s == "addr" => {
                            groups |= RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR
                        }
                        _ => {
                            return Err(refusal(
                                "NET_WATCH_OPTIONS",
                                "events must be a list of \"link\" and/or \"addr\"",
                            ));
                        }
                    }
                }
                if groups == 0 {
                    return Err(refusal("NET_WATCH_OPTIONS", "events must not be empty"));
                }
            }
            _ => {
                return Err(refusal(
                    "NET_WATCH_OPTIONS",
                    format!("invalid watch option: {key}"),
                ));
            }
        }
    }
    Ok(groups)
}

// ── audio ───────────────────────────────────────────────────────────────

const FACILITIES: [&str; 9] = [
    "sink",
    "source",
    "sink-input",
    "source-output",
    "module",
    "client",
    "sample-cache",
    "server",
    "card",
];

pub(crate) struct AudioOptions {
    pub facilities: Vec<String>,
    pub runtime_dir: Option<String>,
}

impl AudioOptions {
    /// `audio_watch` takes `{facilities, runtime_dir}`; `audio_state` only
    /// `{runtime_dir}`. Default facilities leave out `client` and the stream
    /// facilities: every pulse client connecting (including pactl itself)
    /// would otherwise produce events that change no volume.
    pub fn parse(value: Option<&Value>, watch: bool) -> MixResult<Self> {
        let mut opts = Self {
            facilities: ["sink", "source", "server", "card"]
                .map(str::to_owned)
                .to_vec(),
            runtime_dir: None,
        };
        let Some(value) = value else { return Ok(opts) };
        let Value::Map(map) = value else {
            return Err(refusal("AUDIO_OPTIONS", "options must be a map"));
        };
        for (key, value) in map.iter() {
            match (key.as_str(), value) {
                ("runtime_dir", Value::String(s)) if !s.is_empty() => {
                    opts.runtime_dir = Some(s.clone())
                }
                ("runtime_dir", Value::Nil) => opts.runtime_dir = None,
                ("facilities", Value::List(values)) if watch => {
                    opts.facilities.clear();
                    for v in values.iter() {
                        let Value::String(s) = v else {
                            return Err(refusal(
                                "AUDIO_OPTIONS",
                                "facilities must be a list of strings",
                            ));
                        };
                        if !FACILITIES.contains(&s.as_str()) {
                            return Err(refusal(
                                "AUDIO_OPTIONS",
                                format!("unknown facility: {s}"),
                            ));
                        }
                        if !opts.facilities.contains(s) {
                            opts.facilities.push(s.clone());
                        }
                    }
                    if opts.facilities.is_empty() {
                        return Err(refusal("AUDIO_OPTIONS", "facilities must not be empty"));
                    }
                }
                _ => {
                    return Err(refusal(
                        "AUDIO_OPTIONS",
                        format!("invalid audio option: {key}"),
                    ));
                }
            }
        }
        Ok(opts)
    }
}

/// `Event 'change' on sink #56` -> ("sink", "change", Some(56)).
/// `Event 'change' on server` has no index.
pub(crate) fn parse_pactl(line: &str) -> Option<(String, String, Option<u64>)> {
    let rest = line.trim().strip_prefix("Event '")?;
    let (kind, rest) = rest.split_once("' on ")?;
    let (facility, index) = match rest.split_once(" #") {
        Some((facility, index)) => (facility, Some(index.trim().parse().ok()?)),
        None => (rest, None),
    };
    Some((facility.to_owned(), kind.to_owned(), index))
}

pub(crate) fn audio_change(
    line: &str,
    facilities: &[String],
) -> Option<(String, serde_json::Value)> {
    let (facility, kind, index) = parse_pactl(line)?;
    if !facilities.contains(&facility) {
        return None;
    }
    let key = match index {
        Some(i) => format!("{facility}#{i}"),
        None => facility.clone(),
    };
    Some((
        key,
        serde_json::json!({"facility": facility, "kind": kind, "index": index}),
    ))
}

/// Splits a byte stream into complete lines. A line longer than the cap is
/// discarded and reported as overflow rather than growing without bound.
#[derive(Default)]
pub(crate) struct LineBuffer {
    pending: Vec<u8>,
    discarding: bool,
}

const MAX_LINE: usize = 64 * 1024;

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        let mut overflow = false;
        for &b in bytes {
            if b == b'\n' {
                if self.discarding {
                    self.discarding = false;
                } else {
                    lines.push(String::from_utf8_lossy(&self.pending).into_owned());
                }
                self.pending.clear();
            } else if !self.discarding {
                if self.pending.len() >= MAX_LINE {
                    self.pending.clear();
                    self.discarding = true;
                    overflow = true;
                } else {
                    self.pending.push(b);
                }
            }
        }
        (lines, overflow)
    }
}

/// `wpctl get-volume` output -> `audio_state()` map. A missing default sink
/// is an ordinary state (`ok:false` with a reason), not an error.
pub(crate) fn parse_wpctl(success: bool, stdout: &str, stderr: &str) -> serde_json::Value {
    if success
        && let Some(rest) = stdout.trim().strip_prefix("Volume:")
        && let Some(volume) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        && volume.is_finite()
    {
        return serde_json::json!({
            "ok": true,
            "volume": volume,
            "level": (volume * 100.0).round(),
            "muted": rest.contains("[MUTED]"),
        });
    }
    let reason = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("no default audio sink");
    serde_json::json!({"ok": false, "volume": 0.0, "level": 0.0, "muted": false, "reason": reason})
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::native_events::Queue;
    use std::{
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
            unix::{net::UnixStream, process::CommandExt},
        },
        process::{Child, Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    fn route_socket(groups: u32) -> std::io::Result<OwnedFd> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        sa.nl_groups = groups;
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }

    fn set_option<T>(fd: RawFd, option: libc::c_int, value: &T) {
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                value as *const T as *const libc::c_void,
                std::mem::size_of::<T>() as libc::socklen_t,
            );
        }
    }

    enum Received {
        Data(usize),
        Again,
        Overrun,
        Truncated,
        Failed(std::io::Error),
    }

    fn receive(fd: RawFd, buf: &mut [u8], flags: libc::c_int) -> Received {
        loop {
            let n = unsafe {
                libc::recv(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    flags | libc::MSG_TRUNC,
                )
            };
            if n >= 0 {
                let n = n as usize;
                return if n > buf.len() {
                    Received::Truncated
                } else {
                    Received::Data(n)
                };
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Received::Again,
                Some(libc::ENOBUFS) => return Received::Overrun,
                _ => return Received::Failed(e),
            }
        }
    }

    fn index_name(index: i32) -> Option<String> {
        let mut buf = [0 as libc::c_char; libc::IF_NAMESIZE];
        let p = unsafe { libc::if_indextoname(index as libc::c_uint, buf.as_mut_ptr()) };
        if p.is_null() {
            return None;
        }
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_str()
            .ok()
            .map(str::to_owned)
    }

    fn wireless(ifname: &str) -> bool {
        if ifname.is_empty() || ifname.contains('/') || ifname == "." || ifname == ".." {
            return false;
        }
        let dir = std::path::Path::new("/sys/class/net").join(ifname);
        dir.join("wireless").exists() || dir.join("phy80211").exists()
    }

    fn named(record: &Record, names: &mut BTreeMap<i32, String>) -> (String, serde_json::Value) {
        let (key, mut v) = change(record, names, &index_name);
        if v["kind"] == "link" {
            let is_wireless = v["removed"] != true && wireless(v["ifname"].as_str().unwrap_or(""));
            v["wireless"] = is_wireless.into();
        }
        (key, v)
    }

    pub(crate) struct NetSource {
        cancel: UnixStream,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl NetSource {
        pub fn new(queue: Arc<Queue>, handle: String, groups: u32) -> MixResult<Self> {
            let fd = route_socket(groups).map_err(|e| {
                refusal("NET_WATCH_IO", format!("rtnetlink subscription failed: {e}"))
            })?;
            // Headroom for a burst (a VPN coming up announces many
            // addresses) before the kernel reports ENOBUFS.
            let size: libc::c_int = 1 << 20;
            set_option(fd.as_raw_fd(), libc::SO_RCVBUF, &size);
            let (cancel, wake) =
                UnixStream::pair().map_err(|e| refusal("NET_WATCH_IO", e.to_string()))?;
            let worker = thread::Builder::new()
                .name("mix-netlink".into())
                .spawn(move || net_worker(fd, wake, queue, handle))
                .map_err(|e| refusal("NET_WATCH_IO", e.to_string()))?;
            Ok(Self {
                cancel,
                worker: Some(worker),
            })
        }
    }

    impl Drop for NetSource {
        fn drop(&mut self) {
            let _ = self.cancel.shutdown(std::net::Shutdown::Write);
            if let Some(w) = self.worker.take() {
                let _ = w.join();
            }
        }
    }

    /// poll(2) with an infinite timeout on the socket and the cancellation
    /// FD: an idle network costs no wakeups. Each readiness drains every
    /// queued datagram and publishes ONE batch, so a burst is one delivery.
    fn net_worker(fd: OwnedFd, wake: UnixStream, queue: Arc<Queue>, handle: String) {
        let mut names = BTreeMap::new();
        let mut dedupe = Dedupe::default();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                queue.source_closed(
                    &handle,
                    serde_json::json!({"error_code": "NET_WATCH_IO", "message": e.to_string()}),
                );
                return;
            }
            // Cancellation wins simultaneous readiness.
            if fds[1].revents != 0 {
                return;
            }
            if fds[0].revents & libc::POLLNVAL != 0 {
                queue.source_closed(
                    &handle,
                    serde_json::json!({"error_code": "NET_WATCH_IO", "message": "netlink socket closed"}),
                );
                return;
            }
            let mut changes = Vec::new();
            let mut overflow = false;
            loop {
                match receive(fd.as_raw_fd(), &mut buf, libc::MSG_DONTWAIT) {
                    Received::Data(n) => {
                        let parsed = parse(&buf[..n]);
                        overflow |= parsed.overflow;
                        for record in &parsed.records {
                            changes.push(named(record, &mut names));
                        }
                    }
                    Received::Again => break,
                    // ENOBUFS: the kernel dropped notifications. Keep
                    // draining; the batch says overflow so state is re-read.
                    Received::Overrun | Received::Truncated => overflow = true,
                    // A readable socket whose every recv fails would wake the
                    // evaluator with overflow forever. End the source instead;
                    // the behaviour decides whether to subscribe again.
                    Received::Failed(e) => {
                        queue.source_closed(
                            &handle,
                            serde_json::json!({"error_code": "NET_WATCH_IO", "message": format!("netlink receive failed: {e}")}),
                        );
                        return;
                    }
                }
            }
            if overflow {
                dedupe.clear();
            }
            changes.retain(|(key, v)| overflow || dedupe.admit(key, v));
            if overflow {
                // Re-seed from what was delivered, so the next repeat is
                // suppressed again.
                for (key, v) in &changes {
                    dedupe.admit(key, v);
                }
            }
            if !changes.is_empty() || overflow {
                queue.source(&handle, changes, overflow);
            }
        }
    }

    fn dump(
        fd: &OwnedFd,
        kind: u16,
        seq: u32,
        buf: &mut [u8],
        deadline: std::time::Instant,
    ) -> MixResult<(Vec<Record>, bool)> {
        // nlmsghdr + ifinfomsg (16 bytes) or ifaddrmsg (8 bytes), family AF_UNSPEC.
        let body = if kind == RTM_GETLINK { 16 } else { 8 };
        let len = NLMSG_HDRLEN + body;
        let mut req = [0u8; NLMSG_HDRLEN + 16];
        req[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
        req[4..6].copy_from_slice(&kind.to_ne_bytes());
        req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        req[8..12].copy_from_slice(&seq.to_ne_bytes());
        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let sent = unsafe {
            libc::sendto(
                fd.as_raw_fd(),
                req.as_ptr() as *const libc::c_void,
                len,
                0,
                &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(refusal(
                "NET_STATE_IO",
                std::io::Error::last_os_error().to_string(),
            ));
        }
        let mut records = Vec::new();
        let mut interrupted = false;
        loop {
            // Every recv gets only what is left of the one overall deadline.
            let Some((secs, micros)) =
                receive_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                return Err(refusal("NET_STATE_IO", "netlink dump timed out"));
            };
            let timeout = libc::timeval {
                tv_sec: secs as libc::time_t,
                tv_usec: micros as libc::suseconds_t,
            };
            set_option(fd.as_raw_fd(), libc::SO_RCVTIMEO, &timeout);
            match receive(fd.as_raw_fd(), buf, 0) {
                Received::Data(n) => {
                    let parsed = parse(&buf[..n]);
                    if let Some(errno) = parsed.error {
                        return Err(refusal(
                            "NET_STATE_IO",
                            std::io::Error::from_raw_os_error(errno).to_string(),
                        ));
                    }
                    if parsed.overflow {
                        return Err(refusal("NET_STATE_IO", "malformed netlink dump"));
                    }
                    interrupted |= parsed.interrupted;
                    records.extend(parsed.records);
                    if parsed.done {
                        return Ok((records, interrupted));
                    }
                }
                Received::Again => {
                    return Err(refusal("NET_STATE_IO", "netlink dump timed out"));
                }
                Received::Overrun | Received::Truncated => {
                    return Err(refusal("NET_STATE_IO", "netlink dump overran its buffer"));
                }
                Received::Failed(e) => return Err(refusal("NET_STATE_IO", e.to_string())),
            }
        }
    }

    /// `net_state()` answers within this, all retries included.
    const NET_STATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

    /// Links and addresses from a dump. A dump the kernel marks interrupted
    /// (NLM_F_DUMP_INTR: the tables changed underneath it) is retried.
    pub(crate) fn net_state() -> MixResult<serde_json::Value> {
        let mut buf = vec![0u8; 64 * 1024];
        // A dump reply is immediate; the deadline only bounds a wedged kernel.
        // It covers every attempt together, so the evaluator waits at most
        // this long however the dumps fail.
        let deadline = std::time::Instant::now() + NET_STATE_DEADLINE;
        for _ in 0..3 {
            let fd = route_socket(0).map_err(|e| refusal("NET_STATE_IO", e.to_string()))?;
            let (links, a) = dump(&fd, RTM_GETLINK, 1, &mut buf, deadline)?;
            let (addrs, b) = dump(&fd, RTM_GETADDR, 2, &mut buf, deadline)?;
            if a || b {
                continue;
            }
            return Ok(snapshot(&links, &addrs, &index_name, &wireless));
        }
        Err(refusal(
            "NET_STATE_INCONSISTENT",
            "network tables kept changing during three consecutive dumps",
        ))
    }

    pub(crate) struct AudioSource {
        #[cfg_attr(not(test), allow(dead_code))]
        pub pid: i32,
        child: Arc<Mutex<Option<Child>>>,
        cancelled: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    fn command(program: &str, runtime_dir: &Option<String>) -> Command {
        let mut command = Command::new(program);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // pactl/wpctl messages are translated; the parsers read English.
            .env("LC_ALL", "C")
            .process_group(0);
        if let Some(dir) = runtime_dir {
            command.env("XDG_RUNTIME_DIR", dir);
        }
        command
    }

    /// `PR_SET_PDEATHSIG(SIGKILL)` on the child, as `spawn(argv,
    /// {die_with_parent:true})` arms it: if the evaluator thread that owns the
    /// source dies without dropping it (a SIGKILLed or panicking behaviour),
    /// the kernel kills the child instead of leaving an orphan holding a pulse
    /// connection. The `getppid` check closes the fork→prctl race.
    ///
    /// The kernel signals when the creating THREAD ends, so this is armed only
    /// on the owned-children host thread (the mix binary's evaluator, which
    /// lives as long as the process); elsewhere the drop path alone owns it.
    fn arm_parent_death(command: &mut Command) {
        if !crate::builtins::owned_spawns::enabled_here() {
            return;
        }
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: prctl, getppid and _exit are raw syscalls, safe in the
        // post-fork pre-exec window: no locks, no allocation.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(0);
                }
                Ok(())
            });
        }
    }

    fn spawn_error(program: &str, e: std::io::Error) -> crate::MixError {
        if e.kind() == std::io::ErrorKind::NotFound {
            refusal(
                "AUDIO_UNAVAILABLE",
                format!("{program} not found on PATH (PipeWire's pulse tools provide pactl; WirePlumber provides wpctl)"),
            )
        } else {
            refusal("AUDIO_WATCH_IO", format!("{program}: {e}"))
        }
    }

    impl AudioSource {
        pub fn new(queue: Arc<Queue>, handle: String, opts: AudioOptions) -> MixResult<Self> {
            let mut command = command("pactl", &opts.runtime_dir);
            command.arg("subscribe");
            Self::start(queue, handle, opts.facilities, command)
        }

        /// Owns `command`'s child as the event stream (tests substitute a
        /// fixture for pactl here, without touching PATH).
        pub(crate) fn start(
            queue: Arc<Queue>,
            handle: String,
            facilities: Vec<String>,
            mut command: Command,
        ) -> MixResult<Self> {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0);
            arm_parent_death(&mut command);
            let mut child = command.spawn().map_err(|e| spawn_error("pactl", e))?;
            let pid = child.id() as i32;
            // Only this source reaps it; process_alive must not steal the status.
            crate::builtins::register_managed_pid(pid);
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                crate::builtins::unregister_managed_pid(pid);
                return Err(refusal("AUDIO_WATCH_IO", "pactl stdout was not captured"));
            };
            let child = Arc::new(Mutex::new(Some(child)));
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_child = child.clone();
            let worker_cancelled = cancelled.clone();
            let worker = thread::Builder::new()
                .name("mix-audio-events".into())
                .spawn(move || {
                    audio_worker(stdout, worker_child, worker_cancelled, queue, handle, facilities)
                });
            match worker {
                Ok(worker) => Ok(Self {
                    pid,
                    child,
                    cancelled,
                    worker: Some(worker),
                }),
                Err(e) => {
                    if let Some(mut child) = child.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    crate::builtins::unregister_managed_pid(pid);
                    Err(refusal("AUDIO_WATCH_IO", e.to_string()))
                }
            }
        }
    }

    fn kill_group(child: &mut Child) {
        // Unreaped, so the PID (and the group it leads) cannot be reused yet.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
    }

    /// Blocked in read(2) on the child's stdout; each read publishes one
    /// batch. EOF means the child died (server restart, no server) unless
    /// the source was cancelled: that is reported once as `closed`.
    fn audio_worker(
        mut stdout: std::process::ChildStdout,
        child: Arc<Mutex<Option<Child>>>,
        cancelled: Arc<AtomicBool>,
        queue: Arc<Queue>,
        handle: String,
        facilities: Vec<String>,
    ) {
        let mut lines = LineBuffer::default();
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let (complete, overflow) = lines.push(&buf[..n]);
                    let changes: Vec<_> = complete
                        .iter()
                        .filter_map(|line| audio_change(line, &facilities))
                        .collect();
                    if !changes.is_empty() || overflow {
                        queue.source(&handle, changes, overflow);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let taken = child.lock().unwrap().take();
        let Some(mut child) = taken else { return };
        let pid = child.id() as i32;
        // A closed stdout with a live process is still a dead event stream.
        kill_group(&mut child);
        let status = child.wait();
        crate::builtins::unregister_managed_pid(pid);
        let (exit_code, message) = match status {
            Ok(status) => (status.code(), format!("pactl subscribe exited ({status})")),
            Err(e) => (None, format!("pactl subscribe could not be reaped: {e}")),
        };
        queue.source_closed(
            &handle,
            serde_json::json!({
                "error_code": "AUDIO_SOURCE_EXITED",
                "message": message,
                "exit_code": exit_code,
            }),
        );
    }

    impl Drop for AudioSource {
        fn drop(&mut self) {
            self.cancelled.store(true, Ordering::Release);
            let mut pid = None;
            if let Some(child) = self.child.lock().unwrap().as_mut() {
                pid = Some(child.id() as i32);
                kill_group(child);
            }
            // The kill closes the pipe, so the reader sees EOF and exits.
            if let Some(w) = self.worker.take() {
                let _ = w.join();
            }
            if let Some(mut child) = self.child.lock().unwrap().take() {
                let _ = child.wait();
            }
            if let Some(pid) = pid {
                crate::builtins::unregister_managed_pid(pid);
            }
        }
    }

    /// One `wpctl get-volume @DEFAULT_AUDIO_SINK@`, bounded by a 2 s
    /// deadline (a wedged PipeWire must not hang the evaluator).
    pub(crate) fn audio_state(opts: &AudioOptions) -> MixResult<serde_json::Value> {
        let mut command = command("wpctl", &opts.runtime_dir);
        command
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .stderr(Stdio::piped());
        arm_parent_death(&mut command);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(parse_wpctl(false, "", "wpctl not found on PATH"));
            }
            Err(e) => return Err(refusal("AUDIO_STATE_IO", format!("wpctl: {e}"))),
        };
        bounded_output(child, std::time::Duration::from_secs(2), true)
    }

    fn timed_out(limit: std::time::Duration) -> serde_json::Value {
        parse_wpctl(
            false,
            "",
            &format!("wpctl timed out after {} s", limit.as_secs_f64()),
        )
    }

    /// Collect `child`'s output within `limit` on every path, killing its
    /// process group when the limit passes. A pidfd waits in poll(2); without
    /// one (pre-5.3 kernel, EMFILE, a seccomp filter — or `use_pidfd` false in
    /// tests) a waiter thread collects the output and the caller waits on a
    /// channel with the same limit. Never an unbounded `wait`.
    pub(crate) fn bounded_output(
        mut child: Child,
        limit: std::time::Duration,
        use_pidfd: bool,
    ) -> MixResult<serde_json::Value> {
        let deadline = std::time::Instant::now() + limit;
        let pid = child.id() as i32;
        let fd = if use_pidfd {
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
        } else {
            -1
        };
        if fd < 0 {
            return waiter_output(child, deadline, limit);
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut pfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let rc = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                kill_group(&mut child);
                let _ = child.wait();
                return Ok(parse_wpctl(false, "", &format!("wpctl wait failed: {e}")));
            }
            if rc == 0 {
                kill_group(&mut child);
                let _ = child.wait();
                return Ok(timed_out(limit));
            }
            break;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| refusal("AUDIO_STATE_IO", format!("wpctl: {e}")))?;
        Ok(parse_wpctl(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ))
    }

    fn waiter_output(
        child: Child,
        deadline: std::time::Instant,
        limit: std::time::Duration,
    ) -> MixResult<serde_json::Value> {
        let pid = child.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = thread::Builder::new()
            .name("mix-wpctl-wait".into())
            .spawn(move || {
                let _ = tx.send(child.wait_with_output());
            });
        if let Err(e) = waiter {
            // The closure (and the Child in it) is gone; the pid is still
            // unreaped, so its group cannot have been reused.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return Err(refusal("AUDIO_STATE_IO", format!("wpctl waiter: {e}")));
        }
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Ok(output)) => Ok(parse_wpctl(
                output.status.success(),
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )),
            Ok(Err(e)) => Err(refusal("AUDIO_STATE_IO", format!("wpctl: {e}"))),
            Err(_) => {
                // Still running at the deadline means still unreaped, so the
                // group id is the child's (only a child exiting in this very
                // instant could be reaped first). The waiter then finishes on
                // its own; nothing joins it.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                Ok(timed_out(limit))
            }
        }
    }
}

/// `SO_RCVTIMEO` as `(seconds, microseconds)` for what is left of a deadline,
/// or `None` once it has passed. A zero timeval means "block forever", so a
/// sub-microsecond remainder rounds up to one microsecond.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn receive_timeout(left: std::time::Duration) -> Option<(u64, u32)> {
    if left.is_zero() {
        return None;
    }
    let (secs, micros) = (left.as_secs(), left.subsec_micros());
    Some(if secs == 0 && micros == 0 {
        (0, 1)
    } else {
        (secs, micros)
    })
}

/// The `net_state()` value, from dumped records. Pure apart from the two
/// lookups, which tests replace.
pub(crate) fn snapshot(
    links: &[Record],
    addrs: &[Record],
    lookup: &dyn Fn(i32) -> Option<String>,
    wireless: &dyn Fn(&str) -> bool,
) -> serde_json::Value {
    let mut names = BTreeMap::new();
    let mut link_rows = BTreeMap::new();
    for record in links {
        let (_, mut v) = change(record, &mut names, lookup);
        let index = v["index"].as_i64().unwrap_or(-1);
        let is_wireless = wireless(v["ifname"].as_str().unwrap_or(""));
        if let Some(m) = v.as_object_mut() {
            m.remove("kind");
            m.remove("removed");
            m.insert("wireless".into(), is_wireless.into());
        }
        link_rows.insert(index, v);
    }
    let mut addr_rows = Vec::new();
    for record in addrs {
        let (_, mut v) = change(record, &mut names, lookup);
        if let Some(m) = v.as_object_mut() {
            m.remove("kind");
            m.remove("removed");
            m.remove("up");
        }
        addr_rows.push(v);
    }
    serde_json::json!({
        "links": link_rows.into_values().collect::<Vec<_>>(),
        "addresses": addr_rows,
    })
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{AudioSource, NetSource, audio_state, net_state};

#[cfg(not(target_os = "linux"))]
mod other {
    use super::*;
    use crate::native_events::Queue;
    use std::sync::Arc;

    pub(crate) struct NetSource;
    impl NetSource {
        pub fn new(_: Arc<Queue>, _: String, _: u32) -> MixResult<Self> {
            Err(refusal("NET_WATCH_UNSUPPORTED", "net_watch requires Linux rtnetlink"))
        }
    }
    pub(crate) struct AudioSource;
    impl AudioSource {
        pub fn new(_: Arc<Queue>, _: String, _: AudioOptions) -> MixResult<Self> {
            Err(refusal("AUDIO_WATCH_UNSUPPORTED", "audio_watch requires Linux"))
        }
    }
    pub(crate) fn net_state() -> MixResult<serde_json::Value> {
        Err(refusal("NET_STATE_UNSUPPORTED", "net_state requires Linux rtnetlink"))
    }
    pub(crate) fn audio_state(_: &AudioOptions) -> MixResult<serde_json::Value> {
        Err(refusal("AUDIO_STATE_UNSUPPORTED", "audio_state requires Linux"))
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) use other::{AudioSource, NetSource, audio_state, net_state};

/// One registration, owned by `NativeEvents`. Dropping it cancels and joins.
pub(crate) enum Source {
    #[allow(dead_code)]
    Net(NetSource),
    #[allow(dead_code)]
    Audio(AudioSource),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(len: usize, kind: u16, flags: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(len as u32).to_ne_bytes());
        b.extend_from_slice(&kind.to_ne_bytes());
        b.extend_from_slice(&flags.to_ne_bytes());
        b.extend_from_slice(&7u32.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        b
    }

    fn attr(kind: u16, value: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&((4 + value.len()) as u16).to_ne_bytes());
        b.extend_from_slice(&kind.to_ne_bytes());
        b.extend_from_slice(value);
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b
    }

    fn message(kind: u16, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut b = header(NLMSG_HDRLEN + body.len(), kind, flags);
        b.extend_from_slice(body);
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b
    }

    fn link_msg(kind: u16, index: i32, flags: u32, name: &str, operstate: u8) -> Vec<u8> {
        let mut body = vec![0u8, 0, 1, 0];
        body.extend_from_slice(&index.to_ne_bytes());
        body.extend_from_slice(&flags.to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.push(0);
        body.extend(attr(IFLA_IFNAME, &name_bytes));
        body.extend(attr(IFLA_OPERSTATE, &[operstate]));
        message(kind, 0, &body)
    }

    fn addr_msg(kind: u16, family: u8, prefix: u8, index: u32, attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![family, prefix, 0, 0];
        body.extend_from_slice(&index.to_ne_bytes());
        for (k, v) in attrs {
            body.extend(attr(*k, v));
        }
        message(kind, 0, &body)
    }

    fn no_lookup(_: i32) -> Option<String> {
        None
    }

    #[test]
    fn parses_link_and_address_messages_from_fixture_bytes() {
        let mut dgram = link_msg(RTM_NEWLINK, 3, IFF_UP | IFF_RUNNING, "eth0", 6);
        dgram.extend(addr_msg(
            RTM_NEWADDR,
            AF_INET,
            24,
            3,
            &[(IFA_ADDRESS, vec![192, 0, 2, 7]), (IFA_LABEL, b"eth0\0".to_vec())],
        ));
        let mut v6 = [0u8; 16];
        v6[0] = 0x20;
        v6[1] = 0x01;
        v6[2] = 0x0d;
        v6[3] = 0xb8;
        v6[15] = 1;
        dgram.extend(addr_msg(RTM_DELADDR, AF_INET6, 64, 3, &[(IFA_ADDRESS, v6.to_vec())]));
        let parsed = parse(&dgram);
        assert!(!parsed.overflow && !parsed.done && parsed.error.is_none());
        assert_eq!(parsed.records.len(), 3);

        let mut names = BTreeMap::new();
        let (key, link) = change(&parsed.records[0], &mut names, &no_lookup);
        assert_eq!(key, "link:3");
        assert_eq!(link["kind"], "link");
        assert_eq!(link["ifname"], "eth0");
        assert_eq!(link["up"], true);
        assert_eq!(link["operstate"], "up");
        assert_eq!(link["removed"], false);

        let (key, v4) = change(&parsed.records[1], &mut names, &no_lookup);
        assert_eq!(key, "addr:3:192.0.2.7/24");
        assert_eq!(v4["family"], "inet");
        assert_eq!(v4["address"], "192.0.2.7");
        assert_eq!(v4["prefix"], 24);
        assert_eq!(v4["up"], true);

        // IPv6 carries no label: the name comes from the link record's cache.
        let (_, v6) = change(&parsed.records[2], &mut names, &no_lookup);
        assert_eq!(v6["ifname"], "eth0");
        assert_eq!(v6["family"], "inet6");
        assert_eq!(v6["address"], "2001:db8::1");
        assert_eq!(v6["removed"], true);
        assert_eq!(v6["up"], false);
    }

    #[test]
    fn point_to_point_prefers_local_and_unknown_operstate_uses_carrier_flags() {
        let dgram = addr_msg(
            RTM_NEWADDR,
            AF_INET,
            32,
            9,
            &[(IFA_ADDRESS, vec![198, 51, 100, 1]), (IFA_LOCAL, vec![198, 51, 100, 2])],
        );
        let parsed = parse(&dgram);
        let (_, v) = change(&parsed.records[0], &mut BTreeMap::new(), &|_| Some("wg0".into()));
        assert_eq!(v["address"], "198.51.100.2");
        assert_eq!(v["ifname"], "wg0");

        let wg = parse(&link_msg(RTM_NEWLINK, 9, IFF_UP | IFF_RUNNING, "wg0", 0));
        let (_, v) = change(&wg.records[0], &mut BTreeMap::new(), &no_lookup);
        assert_eq!(v["up"], true);
        assert_eq!(v["operstate"], "unknown");
        let down = parse(&link_msg(RTM_NEWLINK, 4, IFF_UP, "eth1", 2));
        let (_, v) = change(&down.records[0], &mut BTreeMap::new(), &no_lookup);
        assert_eq!(v["up"], false);
        assert_eq!(v["operstate"], "down");
        let gone = parse(&link_msg(RTM_DELLINK, 4, IFF_UP | IFF_RUNNING, "eth1", 6));
        let (_, v) = change(&gone.records[0], &mut BTreeMap::new(), &no_lookup);
        assert_eq!(v["up"], false);
        assert_eq!(v["removed"], true);
    }

    #[test]
    fn malformed_overrun_error_done_and_interrupted_frames() {
        let good = link_msg(RTM_NEWLINK, 1, IFF_UP | IFF_LOOPBACK | IFF_RUNNING, "lo", 0);
        // Truncated datagram: the header claims more than was received.
        assert!(parse(&good[..good.len() - 4]).overflow);
        // An attribute running past its message.
        let mut bad = good.clone();
        let attr_len_at = NLMSG_HDRLEN + 16;
        bad[attr_len_at..attr_len_at + 2].copy_from_slice(&200u16.to_ne_bytes());
        let parsed = parse(&bad);
        assert!(parsed.overflow && parsed.records.is_empty());
        assert!(parse(&message(NLMSG_OVERRUN, 0, &[])).overflow);
        let parsed = parse(&message(NLMSG_ERROR, 0, &(-libc::EPERM).to_ne_bytes()));
        assert_eq!(parsed.error, Some(libc::EPERM));
        let ack = parse(&message(NLMSG_ERROR, 0, &0i32.to_ne_bytes()));
        assert!(ack.error.is_none());
        // No errno negates to i32::MIN's magnitude: untrusted, not a panic.
        let parsed = parse(&message(NLMSG_ERROR, 0, &i32::MIN.to_ne_bytes()));
        assert!(parsed.overflow && parsed.error.is_none());
        let mut dump = message(RTM_NEWLINK, NLM_F_DUMP_INTR, &good[NLMSG_HDRLEN..]);
        dump.extend(message(NLMSG_DONE, 0, &0i32.to_ne_bytes()));
        let parsed = parse(&dump);
        assert!(parsed.done && parsed.interrupted);
        assert_eq!(parsed.records.len(), 1);
    }

    #[test]
    fn dedupe_suppresses_repeats_but_never_a_real_change() {
        let mut d = Dedupe::default();
        let mut names = BTreeMap::new();
        let up = parse(&link_msg(RTM_NEWLINK, 3, IFF_UP | IFF_RUNNING, "wlan0", 6));
        let (k, v) = change(&up.records[0], &mut names, &no_lookup);
        assert!(d.admit(&k, &v));
        assert!(!d.admit(&k, &v), "a scan re-announcement is not a change");
        let dormant = parse(&link_msg(RTM_NEWLINK, 3, IFF_UP, "wlan0", 5));
        let (k2, v2) = change(&dormant.records[0], &mut names, &no_lookup);
        assert!(d.admit(&k2, &v2));
        let a = serde_json::json!({"kind": "addr", "removed": false});
        assert!(d.admit("addr:3:192.0.2.9/24", &a));
        assert!(!d.admit("addr:3:192.0.2.9/24", &a), "lifetime refresh");
        let gone = serde_json::json!({"kind": "addr", "removed": true});
        assert!(d.admit("addr:3:192.0.2.9/24", &gone));
        assert!(d.admit("addr:3:192.0.2.9/24", &a), "re-added after removal");
        d.clear();
        assert!(d.admit(&k2, &v2), "overflow forgets what was seen");
    }

    #[test]
    fn snapshot_names_addresses_from_links_and_strips_event_fields() {
        let links = parse(&link_msg(RTM_NEWLINK, 2, IFF_UP | IFF_RUNNING, "wlan0", 6)).records;
        let addrs = parse(&addr_msg(RTM_NEWADDR, AF_INET, 24, 2, &[(IFA_ADDRESS, vec![192, 0, 2, 5])])).records;
        let v = snapshot(&links, &addrs, &no_lookup, &|name| name == "wlan0");
        assert_eq!(v["links"][0]["ifname"], "wlan0");
        assert_eq!(v["links"][0]["wireless"], true);
        assert!(v["links"][0].get("kind").is_none());
        assert_eq!(v["addresses"][0]["ifname"], "wlan0");
        assert!(v["addresses"][0].get("up").is_none());
    }

    #[test]
    fn net_options_select_groups_and_refuse_nonsense() {
        assert_eq!(
            net_groups(None).unwrap(),
            RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR
        );
        let only_links = Value::map(
            [("events".to_string(), Value::list(vec![Value::String("link".into())]))]
                .into_iter()
                .collect(),
        );
        assert_eq!(net_groups(Some(&only_links)).unwrap(), RTMGRP_LINK);
        let bad = Value::map([("recursive".to_string(), Value::Bool(true))].into_iter().collect());
        assert!(matches!(net_groups(Some(&bad)),
            Err(crate::MixError::Structured(info)) if info.code == "NET_WATCH_OPTIONS"));
    }

    #[test]
    fn pactl_lines_filter_and_coalesce_keys() {
        assert_eq!(
            parse_pactl("Event 'change' on sink #56"),
            Some(("sink".into(), "change".into(), Some(56)))
        );
        assert_eq!(
            parse_pactl("Event 'change' on server"),
            Some(("server".into(), "change".into(), None))
        );
        assert_eq!(
            parse_pactl("Event 'remove' on sink-input #12\n"),
            Some(("sink-input".into(), "remove".into(), Some(12)))
        );
        assert_eq!(parse_pactl("Connection failure: Connection refused"), None);
        assert_eq!(parse_pactl("Event 'new' on sink #x"), None);
        let defaults = AudioOptions::parse(None, true).unwrap().facilities;
        assert!(audio_change("Event 'new' on client #2448", &defaults).is_none());
        let (key, v) = audio_change("Event 'change' on sink #56", &defaults).unwrap();
        assert_eq!(key, "sink#56");
        assert_eq!(v["index"], 56);
        let (key, v) = audio_change("Event 'change' on server", &defaults).unwrap();
        assert_eq!(key, "server");
        assert!(v["index"].is_null());
    }

    #[test]
    fn line_buffer_joins_split_reads_and_caps_runaway_lines() {
        let mut b = LineBuffer::default();
        let (lines, overflow) = b.push(b"Event 'change' on si");
        assert!(lines.is_empty() && !overflow);
        let (lines, _) = b.push(b"nk #1\nEvent 'change' on card #43\n");
        assert_eq!(lines, vec!["Event 'change' on sink #1", "Event 'change' on card #43"]);
        let (lines, overflow) = b.push(&vec![b'x'; MAX_LINE + 10]);
        assert!(lines.is_empty() && overflow);
        let (lines, _) = b.push(b"tail\nEvent 'change' on server\n");
        assert_eq!(lines, vec!["Event 'change' on server"], "the runaway line is dropped whole");
    }

    #[test]
    fn wpctl_output_and_missing_sink() {
        let v = parse_wpctl(true, "Volume: 0.40 [MUTED]\n", "");
        assert_eq!(v["ok"], true);
        assert_eq!(v["level"], 40.0);
        assert_eq!(v["muted"], true);
        let v = parse_wpctl(true, "Volume: 1.00\n", "");
        assert_eq!(v["muted"], false);
        assert_eq!(v["level"], 100.0);
        let v = parse_wpctl(false, "", "Translate ID error: '@DEFAULT_AUDIO_SINK@' is not a valid ID\n");
        assert_eq!(v["ok"], false);
        assert!(v["reason"].as_str().unwrap().contains("not a valid ID"));
        assert_eq!(parse_wpctl(true, "garbage", "")["ok"], false);
    }

    #[cfg(target_os = "linux")]
    fn fixture(script: &str) -> std::process::Command {
        // A stand-in for `pactl subscribe`: the source owns whatever command
        // it is handed, so no PATH manipulation is needed.
        let mut c = std::process::Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    #[cfg(target_os = "linux")]
    fn audio_queue() -> std::sync::Arc<crate::native_events::Queue> {
        let q = std::sync::Arc::new(crate::native_events::Queue::default());
        q.register_source_for_test("audio:1", "audio.changed");
        q
    }

    #[cfg(target_os = "linux")]
    fn reaped(pid: i32) -> bool {
        let mut status = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn audio_stream_lines_become_filtered_batches_and_unwatch_reaps_the_group() {
        let q = audio_queue();
        let source = linux::AudioSource::start(
            q.clone(),
            "audio:1".into(),
            AudioOptions::parse(None, true).unwrap().facilities,
            // Two lines in one write, a client event to filter, then block
            // in a grandchild that must die with the group.
            fixture(
                "printf \"Event 'new' on client #9\\nEvent 'change' on sink #5\\nEvent 'change' on sink #5\\n\"; sleep 3600",
            ),
        )
        .unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), q.next(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.command, "audio.changed");
        let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
        let changes = body["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1, "client filtered, repeated sink coalesced");
        assert_eq!(changes[0]["facility"], "sink");
        assert_eq!(changes[0]["index"], 5);
        assert!(body.get("closed").is_none());
        let pid = source.pid;
        drop(source);
        assert!(reaped(pid), "unwatch kills and reaps the pactl stand-in");
        // Cancellation is not an unexpected exit: no terminal batch appears.
        assert!(!q.source_ready_for_test("audio:1"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn audio_stream_exit_reports_closed_once() {
        let q = audio_queue();
        let source = linux::AudioSource::start(
            q.clone(),
            "audio:1".into(),
            vec!["server".into()],
            fixture("echo \"Event 'change' on server\"; exit 3"),
        )
        .unwrap();
        let mut closed = None;
        let mut saw_server = false;
        while closed.is_none() {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), q.next(None))
                .await
                .unwrap()
                .unwrap();
            let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
            saw_server |= body["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["facility"] == "server");
            closed = body.get("closed").cloned();
            if closed.is_some() {
                assert_eq!(body["overflow"], true);
            }
        }
        let closed = closed.unwrap();
        assert!(saw_server);
        assert_eq!(closed["error_code"], "AUDIO_SOURCE_EXITED");
        assert_eq!(closed["exit_code"], 3);
        assert!(reaped(source.pid), "the stream's own exit is reaped by its reader");
        drop(source);
        assert!(!q.source_ready_for_test("audio:1"));
    }

    #[test]
    fn receive_timeout_never_becomes_block_forever() {
        use std::time::Duration;
        assert_eq!(receive_timeout(Duration::ZERO), None);
        assert_eq!(receive_timeout(Duration::from_nanos(1)), Some((0, 1)));
        assert_eq!(receive_timeout(Duration::from_millis(1500)), Some((1, 500_000)));
        assert_eq!(receive_timeout(Duration::from_secs(2)), Some((2, 0)));
    }

    /// Review m5: the deadline holds with and without a pidfd, and a prompt
    /// child's output still comes back on both paths.
    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_output_enforces_its_limit_on_every_path() {
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};
        let spawn = |script: &str| {
            let mut c = fixture(script);
            c.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0);
            c.spawn().unwrap()
        };
        for use_pidfd in [true, false] {
            let started = Instant::now();
            let limit = Duration::from_millis(200);
            let v = linux::bounded_output(spawn("sleep 30"), limit, use_pidfd).unwrap();
            assert_eq!(v["ok"], false, "pidfd={use_pidfd}");
            assert!(
                v["reason"].as_str().unwrap().contains("timed out after 0.2 s"),
                "pidfd={use_pidfd}: {v}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "pidfd={use_pidfd}: the wait outlived its limit"
            );
            let v = linux::bounded_output(
                spawn("echo 'Volume: 0.25 [MUTED]'"),
                Duration::from_secs(10),
                use_pidfd,
            )
            .unwrap();
            assert_eq!(v["ok"], true, "pidfd={use_pidfd}: {v}");
            assert_eq!(v["level"], 25.0);
            assert_eq!(v["muted"], true);
        }
    }

    #[test]
    fn audio_options_refuse_unknown_keys_and_facilities() {
        let facilities = |names: &[&str]| {
            Value::map(
                [(
                    "facilities".to_string(),
                    Value::list(names.iter().map(|n| Value::String((*n).into())).collect()),
                )]
                .into_iter()
                .collect(),
            )
        };
        assert_eq!(
            AudioOptions::parse(Some(&facilities(&["sink", "sink"])), true).unwrap().facilities,
            vec!["sink".to_string()]
        );
        for bad in [facilities(&["speaker"]), facilities(&[])] {
            assert!(matches!(AudioOptions::parse(Some(&bad), true),
                Err(crate::MixError::Structured(info)) if info.code == "AUDIO_OPTIONS"));
        }
        // audio_state takes only runtime_dir.
        assert!(AudioOptions::parse(Some(&facilities(&["sink"])), false).is_err());
    }
}
