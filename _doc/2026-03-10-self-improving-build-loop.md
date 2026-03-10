# The Self-Improving Loop: Cosmix OS Build Server

> A Strix Halo build machine running Alpine + Incus that autonomously builds,
> tests, and publishes Cosmix OS nightly — with full AI memory continuity so
> every build is smarter than the last.

## The Machine

**AMD Strix Halo** (Ryzen AI Max+ 395 or similar):

| Spec | Value | Purpose |
|------|-------|---------|
| CPU | 16C/32T Zen 5 | Parallel Rust compilation, cross-compilation |
| RAM | 128GB LPDDR5X unified | Large builds + LLM inference share the same pool |
| GPU | RDNA 3.5, 40 CUs, 32MB Infinity Cache | ROCm LLM inference (70B+ models), GPU compute |
| Storage | 2-4TB NVMe | Package repo, ISO images, build cache, model weights |
| Network | 2.5GbE minimum | Package serving, mesh connectivity |

**Key advantage:** Unified memory means 128GB is available to both CPU builds
and GPU inference without a VRAM wall. The machine can compile during the day
and run 70B models at night, or both simultaneously.

## Host OS: Alpine + Incus

The build server itself runs Cosmix OS (Alpine base), dogfooding what it builds.
No Docker anywhere — Incus LXC containers for isolation.

```
Strix Halo Host (Alpine + Incus)
│
├── cosmix-daemon         — port registry, mesh, Lua scripting
├── cosmix-journal        — structured logging for all containers + host
├── cosmix-resolved       — DNS for mesh + local resolution
├── Incus daemon          — container/VM management
│
├── LXC: build            — nightly build environment (disposable)
├── LXC: openclaw         — AI agent (persistent, with memories)
├── LXC: ollama           — local LLM inference (ROCm GPU passthrough)
├── LXC: mattermost       — team communication hub
├── LXC: repo             — Alpine package repo + ISO hosting
│
└── VM: test              — boots last night's ISO for automated testing
```

### Why Incus, Not Docker

| Requirement | Docker | Incus LXC |
|------------|--------|-----------|
| Full system containers | No (process isolation) | Yes (full init, systemd/OpenRC) |
| GPU passthrough | Requires nvidia-docker | Native cgroup device passthrough |
| Persistent storage | Volume mounts, fragile | ZFS datasets, snapshots, live migration |
| No daemon dependency | Requires dockerd | Incus daemon, but containers survive restart |
| Proxmox-familiar workflow | Different paradigm | Same `incus exec` / `lxc exec` patterns |
| Alpine native | Alpine base images exist but Docker itself is Go bloat | Packaged in Alpine `community` repo |

## The Daily Cycle

### Daytime: Development (Human + AI)

```
09:00  Developer starts on cachyos (COSMIC desktop)
       ├── Claude Code for focused coding sessions
       ├── OpenClaw monitors repo, suggests improvements
       ├── Incremental builds on cachyos (fast feedback)
       └── Commits pushed to cosmix repo (GitHub or Forgejo)

       OpenClaw (on Halo) picks up changes:
       ├── Runs cargo check + cargo test on each push
       ├── Queries cosmix-port services for integration context
       ├── Updates vector embeddings for changed files
       └── Posts results to Mattermost #builds channel
```

### Evening: Build Trigger (Automated)

```
22:00  OpenClaw triggers nightly build pipeline:

       Phase 1: Cargo workspace (30-40 min)
       ├── cargo build --release --target x86_64-unknown-linux-musl
       ├── cargo build --release --target aarch64-unknown-linux-musl
       ├── cargo test --release (full test suite)
       └── Binaries staged to /srv/cosmix/staging/

       Phase 2: Alpine packages (60-90 min)
       ├── abuild cosmix-daemon, cosmix-web, cosmix-seat, etc.
       ├── abuild cosmix-journal, cosmix-resolved, cosmixctl
       ├── abuild linux-cachyos (kernel with BORE/MGLRU patches)
       ├── abuild uutils-coreutils, ripgrep, fd, bat, eza, etc.
       ├── abuild cosmic-comp, cosmic-files, cosmic-edit, etc.
       │   (only if upstream changed — cached otherwise)
       └── All packages signed and indexed

       Phase 3: ISO build (15-20 min)
       ├── mkimage.sh --profile cosmix --arch x86_64
       ├── mkimage.sh --profile cosmix --arch aarch64
       └── ISOs staged for publishing

       Phase 4: Automated testing (30-60 min)
       ├── Boot ISO in Incus VM (incus launch cosmix-test)
       ├── Verify boot completes
       ├── Run smoke tests via cosmix-port:
       │   ├── cosmixctl journal status
       │   ├── cosmixctl dns resolve example.com
       │   ├── cosmixctl seat info
       │   └── cosmix shell -e 'print(cosmix.port("journal"):call("info"))'
       ├── Verify COSMIC session starts (headless or VNC)
       └── Destroy test VM

       Phase 5: Publish (5 min)
       ├── Sign packages + ISO
       ├── Sync to cosmix.nexus package repo
       ├── Update APKINDEX
       ├── Update download page with changelog
       └── Post build report to Mattermost #releases

       Phase 6: Memory update
       ├── Embed build logs into vector store
       ├── Record any new failure patterns
       ├── Update CLAUDE.md if architecture changed
       └── Commit memory files to repo
```

### Next Morning: Dogfood

```
09:00  Developer checks Mattermost #releases
       ├── Build report: success/failure, duration, package sizes
       ├── Test results: smoke test pass/fail
       ├── Changelog: what changed since last build
       └── ISO download link

       On cachyos (daily driver):
       ├── apk upgrade (pulls new cosmix packages from cosmix.nexus)
       ├── New cosmix-port improvements are live immediately
       ├── cosmix-journal now captures X that it missed yesterday
       ├── cosmix-resolved handles Y edge case from yesterday's fix
       └── File new issues, fix, commit → cycle repeats

       On test hardware / VM:
       ├── Boot fresh ISO to verify clean install experience
       ├── Walk through first-run setup
       ├── Verify OpenClaw onboarding works for new contributors
       └── Note UX issues → fix → commit → next nightly picks it up
```

## OpenClaw Configuration for Cosmix

OpenClaw runs in an Incus LXC container on the Halo machine. No Docker.
Talks to local Ollama (also in Incus, with ROCm GPU passthrough) for inference,
with cloud API fallback for complex reasoning tasks.

### Deployment

```
incus launch images:alpine/edge openclaw
incus exec openclaw -- apk add nodejs npm git cargo
incus exec openclaw -- npm install -g openclaw  # or from source
```

### Modified Skills

| Default OpenClaw | Cosmix Modification |
|-----------------|-------------------|
| Docker Compose deployment | Native Alpine package in Incus LXC |
| Chat via Signal/Telegram | **Email + Mattermost** |
| Generic coding-agent | Rust/cargo/abuild-aware, knows cosmix-port |
| Generic file memory | Markdown files synced to cosmix repo |
| Generic vector memory | Embeddings of codebase + build logs + Alpine aports |
| Cloud LLM only | **Local Ollama (ROCm on Strix Halo)** + cloud fallback |
| Heartbeat daemon | Integrated with cosmix-daemon cron/timer |

### Communication Channels

| Channel | Purpose | How |
|---------|---------|-----|
| **Mattermost** (#builds) | Build status, test results, CI notifications | OpenClaw posts via Mattermost API |
| **Mattermost** (#dev) | Development discussion, code review, AI suggestions | OpenClaw participates as a bot |
| **Mattermost** (#releases) | Nightly ISO + package announcements | Automated post with changelog |
| **Email** | External contributor notifications, digest summaries | OpenClaw sends via Stalwart SMTP |
| **cosmix.nexus** | Public website, package repo, ISO downloads, docs | Static site + Alpine repo served by cosmix-web |
| **cosmix-port** | Internal machine-to-machine queries | OpenClaw is a cosmix port, queryable from Lua |

### OpenClaw as a cosmix-port

OpenClaw registers as a port on the mesh, making it scriptable:

```lua
-- Query build status from any mesh node
local claw = cosmix.port("openclaw@halo")

-- Last build result
local build = claw:call("build-status")
print(build.date .. ": " .. build.result)
print("Packages: " .. build.package_count)
print("ISO size: " .. build.iso_size)
print("Duration: " .. build.duration)

-- Semantic search across codebase and build history
local hits = claw:call("search", {
    query = "AMP framing buffer overflow fix"
})
for _, hit in ipairs(hits) do
    print(hit.file .. ":" .. hit.line .. " — " .. hit.snippet)
end

-- Trigger a build
claw:send("build", { profile = "cosmix", arch = "x86_64" })

-- Ask it to investigate a failure
claw:send("investigate", {
    issue = "cosmix-journal drops entries under backpressure",
    context = "started after commit abc123"
})
```

### Mattermost Integration

Mattermost is a Go binary with a React frontend, backed by PostgreSQL — the
same PostgreSQL that Stalwart and cosmix-web use. It runs in its own Incus
container.

```
LXC: mattermost
├── Mattermost server (Go binary)
├── Connects to shared PostgreSQL (on host or dedicated LXC)
├── OpenClaw bot integration via incoming/outgoing webhooks
├── Channels: #builds, #dev, #releases, #general
└── Accessible at mattermost.cosmix.nexus
```

Future consideration: Mattermost is Go. If the Cosmix vision demands replacing
it, a cosmix-chat service (Axum + HTMX + cosmix-port) could serve the same
role with native integration. But Mattermost works today and has a mature
client ecosystem — pragmatism over purity.

## Memory Architecture

The build server maintains layered memory that persists across sessions,
builds, and contributors:

### Layer 1: Project Memory (Git)

```
cosmix/
├── CLAUDE.md                      — architecture, conventions, decisions
├── _doc/*.md                      — design documents
├── _journal/*.md                  — operational logs
└── crates/*/                      — the code itself (its own documentation)
```

Every contributor gets this on `git clone`. Every AI session loads CLAUDE.md.

### Layer 2: AI Session Memory (Claude Code)

```
~/.claude/projects/cosmix/memory/
├── MEMORY.md                      — key patterns, architecture decisions
├── debugging.md                   — hard-won debugging lessons
└── patterns.md                    — recurring code patterns
```

Persists across Claude Code sessions on the developer's machine.

### Layer 3: OpenClaw Persistent Memory (File)

```
/var/lib/openclaw/memory/
├── project-context.md             — current project state, active branches
├── build-patterns.md              — common build failures and fixes
├── contributor-preferences.md     — per-contributor workflow notes
├── architecture-decisions.md      — why things are the way they are
└── daily-logs/
    ├── 2026-03-10.md
    ├── 2026-03-11.md
    └── ...
```

Markdown files on disk. Human-readable, git-trackable, grep-searchable.

### Layer 4: OpenClaw Vector Memory (Semantic)

```
Vector DB (pgvector on PostgreSQL):
├── Codebase embeddings            — every .rs file, chunked by function
├── Build log embeddings           — nightly build outputs, error messages
├── Documentation embeddings       — _doc/*.md, CLAUDE.md, README
├── Commit message embeddings      — git log, searchable by intent
├── Issue/fix pattern embeddings   — "X broke because Y, fixed by Z"
└── Mattermost conversation embeds — development discussions
```

Semantic search across everything. "How did we fix the AMP framing bug?"
returns the commit, the build log, the discussion, and the code change.

### Layer 5: cosmix-port Runtime Memory

```
Every running service self-describes:
├── HELP                           — what commands are available
├── INFO                           — current state, version, capabilities
└── Runtime state                  — queryable live via cosmix-port
```

Not stored — generated live. Always current, never stale.

### Memory Flow for New Contributors

```
New contributor:
1. Downloads Cosmix OS ISO from cosmix.nexus        → gets Layer 1 (git)
2. Installs, opens terminal                         → Layer 2 bootstraps
3. OpenClaw greets them on Mattermost               → has Layer 3+4 context
4. They ask "what should I work on?"                 → OpenClaw searches
   vector memory for open issues, recent failures,
   and suggests a starter task with full context
5. They start coding                                → cosmix-port (Layer 5)
   provides live system state for debugging
6. They commit                                      → Layer 4 updates
   (embeddings regenerated overnight)
```

Zero ramp-up time. The AI knows the project's entire history.

## cosmix.nexus Website

The public face of Cosmix OS, served by cosmix-web (Axum + HTMX):

```
cosmix.nexus/
├── /                              — landing page, vision, screenshots
├── /download                      — ISO downloads (x86_64, aarch64)
├── /packages                      — browsable Alpine package index
├── /docs                          — generated from _doc/*.md
├── /changelog                     — nightly build changelogs
├── /mattermost                    — link to Mattermost instance
└── /api                           — package repo (APKINDEX, .apk files)
```

Package repository served directly:

```
# Any Alpine user can add this
echo "https://cosmix.nexus/packages/v1/x86_64" >> /etc/apk/repositories
wget -qO /etc/apk/keys/cosmix.rsa.pub https://cosmix.nexus/keys/cosmix.rsa.pub
apk update && apk add cosmix-daemon cosmix-web cosmixctl
```

### ISO Hosting

```
cosmix.nexus/download/
├── cosmix-x86_64-2026-03-10.iso       — nightly (datestamped)
├── cosmix-x86_64-latest.iso           — symlink to newest
├── cosmix-aarch64-2026-03-10.iso
├── cosmix-aarch64-latest.iso
├── SHA256SUMS                         — checksums
└── SHA256SUMS.sig                     — GPG signed
```

## Build Pipeline Detail

### The Build Container

A disposable Incus container created fresh for each nightly build:

```bash
# Create clean build environment
incus launch images:alpine/edge build-$(date +%Y%m%d)
incus exec build-$(date +%Y%m%d) -- sh -c '
    apk add alpine-sdk cargo rust lua5.4-dev abuild git
    # ... build steps ...
'
# Extract artifacts
incus file pull build-$(date +%Y%m%d)/srv/packages/ /srv/cosmix/staging/
# Destroy — clean slate for tomorrow
incus delete build-$(date +%Y%m%d)
```

Every build starts clean. No "works on the build machine" drift.

### Package Signing

```bash
# One-time setup: generate signing key
abuild-keygen -a -n
# Key stored at /etc/apk/keys/cosmix-*.rsa.pub
# Private key in /root/.abuild/

# abuild automatically signs packages during build
# APKINDEX.tar.gz is signed with the same key
```

### Build Cache

Cargo's target directory and abuild's package cache are on persistent
ZFS datasets, surviving container destruction:

```
/srv/cosmix/
├── cargo-cache/       ← mounted into build container, survives rebuild
├── abuild-cache/      ← compiled dependencies, survives rebuild
├── staging/           ← current build output
├── published/         ← synced to cosmix.nexus
└── iso/               ← generated ISOs
```

ZFS snapshots before each build provide instant rollback if a nightly breaks.

## The Self-Improving Loop

```
            ┌──────────────────────────────┐
            │                              │
            ▼                              │
   ┌─────────────────┐                    │
   │  Developer codes │  (daytime)         │
   │  on cachyos      │                    │
   └────────┬─────────┘                    │
            │ git push                     │
            ▼                              │
   ┌─────────────────┐                    │
   │  OpenClaw picks  │                    │
   │  up changes,     │                    │
   │  runs tests      │                    │
   └────────┬─────────┘                    │
            │ passes? yes                  │
            ▼                              │
   ┌─────────────────┐                    │
   │  Nightly build   │  (overnight)       │
   │  full stack      │                    │
   │  x86_64+aarch64  │                    │
   └────────┬─────────┘                    │
            │                              │
            ▼                              │
   ┌─────────────────┐                    │
   │  Test ISO in VM  │                    │
   │  smoke tests via │                    │
   │  cosmix-port     │                    │
   └────────┬─────────┘                    │
            │ passes?                      │
            ▼                              │
   ┌─────────────────┐                    │
   │  Publish to      │                    │
   │  cosmix.nexus    │                    │
   │  packages + ISO  │                    │
   └────────┬─────────┘                    │
            │                              │
            ▼                              │
   ┌─────────────────┐                    │
   │  Update vector   │                    │
   │  memory, build   │                    │
   │  logs, patterns  │                    │
   └────────┬─────────┘                    │
            │                              │
            ▼                              │
   ┌─────────────────┐                    │
   │  Morning:        │                    │
   │  dogfood the     │──── bugs found ────┘
   │  new build       │
   │  on cachyos      │
   └──────────────────┘
```

### Day 1
- cosmix-port has basic commands per service
- OpenClaw builds and tests, ISO boots but is rough
- Vector memory has initial codebase embeddings

### Day 30
- 30 days of build logs in vector memory
- OpenClaw recognizes common failure patterns ("this error means X, fixed by Y")
- cosmix-port services are more robust from daily dogfooding
- ISO is daily-drivable on the developer's machine

### Day 90
- OpenClaw autonomously fixes common build regressions
- cosmix-port covers 80% of system administration tasks
- ISO is publicly usable — early adopters from Mattermost community
- New contributors boot ISO, OpenClaw onboards them with full context
- Package repo has 90+ days of nightly builds

### Day 180
- Cosmix OS is a real thing people install and use
- Every nightly build is better than the last
- The AI remembers every failure, every fix, every design decision
- Contributors join with zero ramp-up — the system teaches them
- cosmix.nexus has downloads, docs, and an active Mattermost

### Day 365
- The self-improving loop has compounded for a year
- 365 nightly builds, each incrementally better
- Vector memory contains the project's entire institutional knowledge
- The build machine dogfoods its own output — Cosmix OS building Cosmix OS
- OpenClaw is the most knowledgeable "team member" — it never forgets

## Infrastructure Cost

| Component | Cost | Notes |
|-----------|------|-------|
| Strix Halo machine | ~$2-3K (one-time) | Mini PC or laptop |
| Domain (cosmix.nexus) | ~$30/year | |
| Electricity | ~$5-10/month | Idle: ~30W. Building: ~120W. |
| Cloud LLM API (fallback) | ~$20-50/month | For complex reasoning when local Ollama isn't enough |
| **Total ongoing** | **~$35-65/month** | |

No cloud servers. No CI/CD service. No Docker registry. No hosting fees
beyond the domain. The machine pays for itself by eliminating every SaaS
dependency in the development pipeline.

## What This Replaces

| Traditional | Cosmix Build Loop |
|------------|-------------------|
| GitHub Actions / GitLab CI | OpenClaw + abuild in Incus container |
| Docker Hub / GHCR | cosmix.nexus Alpine repo (self-hosted) |
| Ansible / Puppet | Lua scripts via cosmix-port across mesh |
| Slack / Discord | Mattermost (self-hosted, in Incus) |
| Notion / Confluence | _doc/*.md in git + vector search |
| New Relic / Datadog | cosmix-journal + cosmix-port queries |
| Vercel / Netlify | cosmix-web on cosmix.nexus |
| Cloud LLM (exclusive) | Local Ollama (ROCm) + cloud fallback |

**Zero external dependencies for the core development loop.**
The machine, the code, the builds, the packages, the website, the AI —
all self-hosted, all under your control, all connected via cosmix-port.

---

*Document created: 2026-03-10*
*Status: Vision document — pending Strix Halo hardware acquisition*
*Depends on: cosmix-os-vision.md, cosmix-systemd-strategy.md, Alpine COSMIC packages*
