# Cosmix-Systemd: The Sweet Spot Strategy

> Replace the parts of systemd that benefit most from Rust + cosmix-port.
> Keep the parts that are already solved problems. Don't fight battles that
> don't need fighting.

## Principle

systemd is ~70 binaries and ~1.8M lines of C. A full rewrite is 15-23
developer-years. But systemd is not monolithic in practice — it's a collection
of loosely-coupled daemons sharing a D-Bus bus and a common build system.

The sweet spot: **replace the components where Rust + cosmix-port genuinely
improves the experience**, keep the rest, and let D-Bus survive as a
compatibility layer for third-party services (PipeWire, BlueZ, NetworkManager).

## Target Systems

| System | Role | Scope |
|--------|------|-------|
| cachyos | COSMIC desktop workstation | Full stack (seat, journal, DNS, init services) |
| gcwg, mko, mmc | Mesh server nodes | Journal, DNS, service orchestration via cosmix-port |

Not targeting: arbitrary distros, GNOME/KDE, multi-seat, enterprise.

## What to Build

### 1. cosmix-seat — logind Replacement (Priority: High)

**Why replace:** logind is the biggest D-Bus dependency between the compositor
and systemd. It handles DRM/input device brokering, session lifecycle, and VT
switching. For COSMIC on a single-seat desktop, 90% of logind is dead weight
(multi-seat, multi-session, inhibit locks, power management delegation).

**What it does:**
- Opens DRM and input device file descriptors on behalf of cosmic-comp
- Passes FDs via `SCM_RIGHTS` over Unix socket (standard pattern, seatd does this)
- Tracks the active session (one session, one seat)
- Handles VT switching (if applicable — COSMIC rarely uses VTs)
- Exposes cosmix-port commands for session management

**cosmix-port commands:**
```
HELP        — list commands
INFO        — session state, user, seat
ACTIVATE    — (no-op, always active)
SWITCH-VT   n — switch to VT n
LOCK        — lock session
UNLOCK      — unlock session
LIST-DEVICES — list brokered DRM/input devices
```

**What it replaces:** logind (systemd-logind / elogind)

**Scope:** ~2-3K lines of Rust. Comparable to [seatd](https://git.sr.ht/~kennylevinsen/seatd)
(~2K lines of C), which proves this can be minimal.

**Existing art:** seatd already does device brokering without logind. cosmix-seat
would be seatd + cosmix-port + session lifecycle. Could even wrap seatd's
`libseat` initially and add native support later.

**COSMIC integration:** cosmic-comp already supports libseat as a backend.
cosmix-seat would implement the libseat protocol, making it a transparent
drop-in for the compositor.

**Effort:** 0.5-1 dev-year

### 2. cosmix-journal — journald Replacement (Priority: High)

**Why replace:** journald's binary-only log format is the #1 complaint about
systemd. Every sysadmin wants `grep` to work on logs. Additionally, querying
logs across mesh nodes is a natural cosmix-port use case.

**What it does:**
- Reads from `/dev/kmsg` (kernel ring buffer) and `stdout/stderr` of managed services
- Accepts structured log entries via Unix socket (`/run/systemd/journal/socket` — keep the path for compatibility)
- Writes **both** structured index (SQLite or custom) **and** plain text files
- Rotates based on size and time
- Serves queries via cosmix-port

**cosmix-port commands:**
```
HELP                — list commands
INFO                — log statistics, disk usage, retention policy
QUERY               priority=err since=-1h unit=nginx — structured query
TAIL                n=50 unit=nginx — last N entries
FOLLOW              unit=nginx — stream new entries (subscription)
EXPORT              since=2026-03-01 format=json — bulk export
VACUUM              max-size=100M — force cleanup
STATS               — entry counts by unit, priority, time
```

**Dual output format:**
```
/var/log/cosmix/
├── structured/          ← binary index (fast queries)
│   ├── index.db         ← SQLite: timestamp, unit, priority, message hash
│   └── entries/         ← compressed message blobs
├── text/                ← plain text (grep-friendly)
│   ├── syslog           ← traditional combined log
│   ├── nginx.log        ← per-unit log files
│   ├── cosmix-web.log
│   └── kernel.log
└── journal.conf         ← retention, rotation, output config
```

**Key design decisions:**
- Plain text is the **primary** format, not an afterthought
- Structured index enables fast queries without sacrificing readability
- SQLite for the index (proven, embeddable, queryable with standard tools)
- Per-unit text files so `tail -f /var/log/cosmix/text/nginx.log` just works
- Compatible socket path so existing services that log to journald work unchanged

**Cross-mesh queries:**
```lua
-- Query logs across all mesh nodes
for _, node in ipairs({"cachyos", "mko", "gcwg"}) do
    local journal = cosmix.port("journal@" .. node)
    local errors = journal:call("query", {
        priority = "err",
        since = "-1h"
    })
    if #errors > 0 then
        print(node .. ": " .. #errors .. " errors")
        for _, entry in ipairs(errors) do
            print("  " .. entry.unit .. ": " .. entry.message)
        end
    end
end
```

**What it replaces:** systemd-journald + journalctl

**Effort:** 0.5-1 dev-year

### 3. cosmix-resolved — resolved Replacement (Priority: Medium)

**Why replace:** Already planned as part of the cosmix vision (see
`2026-03-08-pure-rust-stack-vision.md`). hickory-dns does the heavy lifting.
resolved's DNSSEC handling and split-DNS behavior are widely criticized.

**What it does:**
- Local DNS resolver/cache on 127.0.0.53
- Forwards to upstream resolvers (plain DNS, DoT, DoH)
- Wraps [hickory-dns](https://github.com/hickory-dns/hickory-dns) resolver library
- Exposes cosmix-port for DNS management
- Optional: authoritative serving for local mesh domains (`.amp` TLD)

**cosmix-port commands:**
```
HELP                — list commands
INFO                — resolver config, cache stats, upstream servers
RESOLVE             name=example.com type=A — resolve a name
FLUSH               — flush DNS cache
CACHE-STATS         — hit/miss rates, cache size
SET-UPSTREAM        servers=1.1.1.1,8.8.8.8 — change upstream resolvers
BLOCK               name=ads.example.com — add to blocklist
UNBLOCK             name=ads.example.com — remove from blocklist
BLOCKLIST-STATS     — blocked query counts
```

**Mesh DNS:**
```lua
-- Resolve mesh node names automatically
-- cosmix-resolved knows about the WireGuard mesh
local dns = cosmix.port("resolved")
dns:call("resolve", { name = "mko.amp" })
-- Returns: { address = "172.16.2.210", source = "mesh" }
```

**What it replaces:** systemd-resolved + resolvectl

**Effort:** 0.3-0.5 dev-year (hickory-dns does the hard work)

### 4. cosmix-timesync — timesyncd Replacement (Priority: Low)

**Why replace:** Trivial to build, good starter project, and completes the
"no systemd daemons" picture for server nodes. NTP is a well-specified protocol.

**What it does:**
- Simple NTP client (SNTP)
- Sets system clock via `clock_settime` / `adjtimex`
- Periodic sync with configurable interval
- cosmix-port for status queries

**cosmix-port commands:**
```
HELP        — list commands
INFO        — current server, last sync time, offset
SYNC        — force immediate sync
STATUS      — detailed timing information
SET-SERVER  server=pool.ntp.org — change NTP server
```

**What it replaces:** systemd-timesyncd

**Effort:** 0.1-0.2 dev-year

### 5. cosmixctl — systemctl/journalctl Replacement (Priority: Medium)

**Why replace:** Not replacing systemctl per se — `cosmixctl` is the unified
CLI for all cosmix-port services. It talks cosmix-port natively, not D-Bus.

**What it does:**
- Single CLI binary for all cosmix system services
- Subcommands map to cosmix-port commands
- Can target local or remote ports (`cosmixctl --node mko journal query ...`)

**Usage:**
```bash
# Journal queries (replaces journalctl)
cosmixctl journal tail -n 50
cosmixctl journal query --priority=err --since=-1h --unit=nginx
cosmixctl journal follow --unit=cosmix-web

# DNS management (replaces resolvectl)
cosmixctl dns resolve example.com
cosmixctl dns flush
cosmixctl dns cache-stats

# Session management (replaces loginctl)
cosmixctl seat info
cosmixctl seat lock

# Time sync (replaces timedatectl)
cosmixctl time status
cosmixctl time sync

# Cross-node operations
cosmixctl --node mko journal query --priority=err --since=-1h
cosmixctl --node gcwg dns flush
```

**Effort:** 0.3-0.5 dev-year

## What to Keep

These are solved problems. Replacing them gains nothing (or costs too much):

| Component | Why keep it | Notes |
|-----------|-------------|-------|
| **systemd PID 1** | Never crashes, 15 years of edge cases, battle-tested | Replacing PID 1 is 3-5 dev-years of the hardest systems programming. The risk/reward ratio is terrible. |
| **udevd** | Device management is a solved problem | eudev proves it runs independently of systemd. Thousands of vendor-contributed rules. No cosmix-port value. |
| **dbus-daemon** | PipeWire, BlueZ, NetworkManager, polkit depend on it | D-Bus becomes a compatibility layer for third-party services, not the system backbone. |
| **NetworkManager** | Network config is endlessly complex | CachyOS uses NM. Replacing it means handling every Wi-Fi driver quirk, VPN plugin, captive portal, etc. |
| **PipeWire** | Audio/video routing is its own universe | No reason to touch this. It works. |
| **tmpfiles/sysctl/modules-load** | Trivial and correct | Config-parser-and-apply tools. Not worth a separate project. Could cosmix-port wrap them later if needed. |

## What Dies

| Component | Replaced by | Reason |
|-----------|------------|--------|
| systemd-journald | cosmix-journal | Binary-only logs → dual text+structured. cosmix-port queries. Cross-mesh log aggregation. |
| systemd-resolved | cosmix-resolved | hickory-dns core. Mesh DNS integration. Simpler than resolved's DNSSEC/split-DNS complexity. |
| systemd-logind | cosmix-seat | COSMIC-only, single-seat. 2K lines vs 30K lines. cosmix-port scriptable. |
| systemd-timesyncd | cosmix-timesync | Trivial, completes the picture. |
| journalctl | cosmixctl journal | Unified CLI, cross-mesh queries. |
| resolvectl | cosmixctl dns | Same CLI, same port system. |
| loginctl | cosmixctl seat | Same CLI, same port system. |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        COSMIC Desktop                           │
│                                                                 │
│  cosmic-comp ←──libseat──→ cosmix-seat (DRM/input brokering)   │
│       │                         │                               │
│  cosmic-*  apps                 │   cosmix-port (AMP/Unix)      │
│       │                         │         │                     │
│       └────── cosmix-port ──────┘         │                     │
│                    │                      │                     │
│              cosmix-daemon                │                     │
│         (port registry + Lua + mesh)      │                     │
│                    │                      │                     │
├────────────────────┼──────────────────────┼─────────────────────┤
│                    │                      │                     │
│  ┌─────────────────┼──────────────────────┼──────────────────┐  │
│  │                 │                      │                  │  │
│  │  cosmix-journal │  cosmix-resolved     │  cosmix-timesync │  │
│  │  (logging)      │  (DNS/hickory)       │  (NTP)           │  │
│  │  text + index   │  cache + mesh DNS    │                  │  │
│  │       │         │       │              │                  │  │
│  │  cosmix-port    │  cosmix-port         │  cosmix-port     │  │
│  │                 │                      │                  │  │
│  └─────────────────┼──────────────────────┼──────────────────┘  │
│                    │                      │                     │
│  ┌─────────────────┴──────────────────────┴──────────────────┐  │
│  │          Kept as-is (not replaced)                        │  │
│  │                                                           │  │
│  │  systemd PID 1    udevd    dbus-daemon    NetworkManager  │  │
│  │  (service mgmt)   (devs)   (compat bus)   (network)      │  │
│  │                                                           │  │
│  │  PipeWire    polkit    BlueZ    UPower                    │  │
│  │  (audio)     (auth)   (BT)     (power)                   │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  cosmixctl  ← unified CLI for all cosmix-port services          │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Server Node Variant

Server nodes (gcwg, mko, mmc) run a subset — no seat, no desktop:

```
┌──────────────────────────────────────────┐
│            Server Node (e.g. mko)        │
│                                          │
│  cosmix-daemon                           │
│  (port registry + mesh + Lua)            │
│       │                                  │
│  cosmix-web      (Axum + HTMX)           │
│  cosmix-journal  (logging)               │
│  cosmix-resolved (DNS)                   │
│  cosmix-timesync (NTP)                   │
│       │                                  │
│  All expose cosmix-port                  │
│  All queryable from any mesh node        │
│                                          │
│  ┌────────────────────────────────────┐  │
│  │  Kept: systemd PID 1, udevd       │  │
│  │  Kept: dbus-daemon (if needed)     │  │
│  │  Kept: Stalwart, PostgreSQL        │  │
│  └────────────────────────────────────┘  │
└──────────────────────────────────────────┘
```

## The Killer Feature: Cross-Mesh System Administration

The real payoff isn't replacing systemd on one machine — it's having cosmix-port
on every system service across every mesh node. One Lua script manages your
entire infrastructure:

```lua
-- Morning health check across all nodes
local nodes = {"cachyos", "gcwg", "mko", "mmc"}

for _, node in ipairs(nodes) do
    print("=== " .. node .. " ===")

    -- Check for errors in the last 8 hours
    local journal = cosmix.port("journal@" .. node)
    local errors = journal:call("query", {
        priority = "err",
        since = "-8h"
    })
    print("  Errors: " .. #errors)

    -- Check DNS cache health
    local dns = cosmix.port("resolved@" .. node)
    local stats = dns:call("cache-stats")
    print("  DNS cache hit rate: " .. stats.hit_rate .. "%")

    -- Check time sync
    local time = cosmix.port("timesync@" .. node)
    local info = time:call("status")
    print("  Clock offset: " .. info.offset_ms .. "ms")
end

-- Flush DNS everywhere after a zone change
for _, node in ipairs(nodes) do
    cosmix.port("resolved@" .. node):send("flush")
end
print("DNS flushed on all nodes")
```

This is infrastructure-as-Lua-script. No Ansible, no Puppet, no SSH loops.
Every service is a port, every port is scriptable, every node is reachable.

## Effort Estimate

| Component | Dev-years | Priority |
|-----------|-----------|----------|
| cosmix-seat | 0.5-1 | High — unblocks D-Bus removal from COSMIC session |
| cosmix-journal | 0.5-1 | High — most user-visible improvement |
| cosmix-resolved | 0.3-0.5 | Medium — hickory-dns handles the hard part |
| cosmix-timesync | 0.1-0.2 | Low — trivial, completionist |
| cosmixctl | 0.3-0.5 | Medium — unified CLI |
| **Total** | **1.7-3.2** | |

With AI-assisted development: **~1-2 years solo**.

Compare to full systemd replacement: 15-23 dev-years. This is **~10-15% of
that effort** for **~80% of the practical benefit**.

## Implementation Order

```
Phase 1: cosmix-journal     ← most complained about, cleanest boundary
Phase 2: cosmix-resolved    ← hickory-dns makes this fast, high daily-use value
Phase 3: cosmix-seat        ← requires COSMIC integration testing
Phase 4: cosmix-timesync    ← trivial, do whenever
Phase 5: cosmixctl          ← build incrementally as each service lands
```

Start with journal because:
1. It has the cleanest boundary (reads from socket, writes to files)
2. It doesn't affect boot (journald can be replaced while systemd runs as PID 1)
3. The improvement is immediately visible (plain text logs!)
4. Cross-mesh log queries are a compelling demo of the cosmix-port value
5. It doesn't require COSMIC patches (unlike cosmix-seat)

## Future: PID 1 (If Ever)

If the component services prove the pattern, PID 1 replacement becomes
conceivable. But only under these conditions:

1. All four service replacements are stable and in daily use
2. A distro partner exists (CachyOS? COSMIC OS? Chimera?)
3. The unit file parser handles 95%+ of Arch packages without modification
4. Someone has the appetite for 3-5 more dev-years of work

Until then, systemd PID 1 stays. It's boring and reliable, which is exactly
what PID 1 should be.

---

*Document created: 2026-03-10*
*Status: Strategic plan — implementation not yet started*
*Depends on: cosmix-port (complete), cosmix-daemon (complete), mesh (complete)*
