# Cosmix OS: A Near-100% Rust Desktop and Server Operating System

> Every binary the user directly interacts with is Rust — their shell,
> editor, terminal, file manager, compositor, mail client, system tools.
> The C lives underneath in the kernel, libc, and a handful of system
> daemons that the user never sees.

## The Insight

The Rust ecosystem has reached a tipping point. For the first time in Linux
history, every layer of the user-facing stack has a production-quality Rust
alternative. Not toy projects — tools that are often **better** than their
C/C++ originals. What's missing is someone assembling them into a coherent
operating system with a unifying integration layer.

That layer is cosmix-port. Without it, you have a collection of Rust binaries
that happen to coexist. With it, you have an **integrated system** where every
component is discoverable, scriptable, and orchestratable across machines.
The difference between "Rust tools on Linux" and "a Rust OS."

## Base: Alpine Linux

| Property | Value | Why |
|----------|-------|-----|
| **Libc** | musl | True static binaries, small footprint, clean design |
| **Package manager** | apk | Fast, dependency-solving, shared with postmarketOS/OpenWrt ecosystem |
| **Init** | OpenRC (initially) | No systemd to fight. cosmix-* services fill gaps, not replace incumbents |
| **Kernel** | Linux + CachyOS patches | BORE scheduler, MGLRU, preemption tuning — desktop responsiveness |
| **Desktop** | COSMIC | Already packaged in Alpine `edge/community`. 22+ packages. |
| **Build system** | abuild + mkimage | Custom ISO profiles, cross-compilation, mix custom + upstream packages |
| **Architectures** | x86_64, aarch64, armv7, riscv64 | Desktop, laptop, phone (postmarketOS), SBC, server |

Alpine was chosen over Arch/CachyOS because:
1. **No systemd** — cosmix-seat, cosmix-journal, cosmix-resolved are filling gaps, not fighting an incumbent
2. **musl** — static Rust binaries with zero runtime dependencies
3. **COSMIC already packaged** — the hard work of musl porting is done
4. **Minimal base** — ~5MB base image, add only what you need
5. **Build system** — `mkimage.sh --profile cosmix` produces a bootable ISO
6. **Multi-arch** — same system on desktop, server, ARM laptop, phone

CachyOS's kernel patches are applied as a custom `linux-cachyos` Alpine kernel
package, giving us the scheduler performance on Alpine's minimal base.

## The Stack

### Layer 1: Kernel (C — and that's fine)

```
Linux 6.x + CachyOS patches
├── BORE scheduler (better interactive latency)
├── MGLRU (better memory management)
├── Preemptive config tuned for desktop
├── Rust enabled (CONFIG_RUST=y)
└── New GPU drivers arriving in Rust (DRM subsystem mandating Rust)
```

The kernel is 34 million lines of C with ~25K lines of Rust. This ratio will
shift over years as new drivers and subsystems are written in Rust (GPU drivers
first), but the kernel being mostly C is fine. It works. It's maintained by
thousands of developers. We build on it, not replace it.

### Layer 2: Invisible C Layer (libc, device management, audio)

These are system plumbing the user never directly interacts with:

| Component | Language | Why it stays |
|-----------|----------|-------------|
| **musl libc** | C | Libc is C by definition. 1MB, audited, stable ABI. |
| **udevd** (eudev) | C | Thousands of vendor-contributed device rules. No Rust replacement needed. |
| **PipeWire** | C | Real-time audio graph. Complex, works perfectly, no Rust attempt exists. |
| **BlueZ** | C | Bluetooth stack, kernel-adjacent. Works. |
| **Mesa** | C/C++ | GPU drivers — transitioning to Rust for new DRM drivers. |
| **dbus-daemon** | C | Compatibility bus for PipeWire, BlueZ, NetworkManager. Invisible to user. |
| **polkit** | C | Authorization framework. Called by other services, not by users. |

**Rule:** If the user never types its name or sees its UI, it can stay C.

### Layer 3: System Services (Rust, cosmix-port integrated)

These run in the background but are queryable and scriptable via cosmix-port:

| Service | Replaces | Status | cosmix-port? |
|---------|----------|--------|-------------|
| **cosmix-daemon** | meshd | Complete | Yes — port registry, Lua scripting, mesh |
| **cosmix-journal** | journald | Planned | Yes — structured logs + plain text |
| **cosmix-seat** | logind/elogind | Planned | Yes — DRM/input brokering for COSMIC |
| **cosmix-resolved** | resolved/unbound | Planned | Yes — wraps hickory-dns |
| **cosmix-web** | Laravel/markweb | Complete | Yes — Axum + HTMX |
| **ntpd-rs** | timesyncd/chrony | Exists (ISRG) | Future cosmix-port wrapper |
| **sudo-rs** | sudo | Exists (ISRG) | N/A — privilege tool, not a service |
| **Stalwart** | Postfix+Dovecot | Exists | Future cosmix-port wrapper |
| **rustls** | OpenSSL | Exists | N/A — library |

ISRG/Prossimo (the Let's Encrypt people) are funding sudo-rs and ntpd-rs
specifically to bring memory safety to critical system tools. We adopt their
work rather than duplicating it.

### Layer 4: Core Userspace Tools (Rust replaces GNU)

Every command the user types in a terminal is Rust:

| GNU/Classic | Rust Replacement | Alpine pkg | Notes |
|-------------|-----------------|------------|-------|
| **coreutils** (ls, cp, mv, cat, chmod, etc.) | **uutils-coreutils** | Yes | Ubuntu 26.04 LTS shipping this. 100+ utilities. |
| **grep** | **ripgrep** (rg) | Yes | Faster than grep, better UX |
| **find** | **fd** | Yes | Intuitive syntax, respects .gitignore |
| **sed** | **sd** | Yes | Simpler regex syntax |
| **cat/less** | **bat** | Yes | Syntax highlighting, git integration |
| **ls** | **eza** | Yes | Colors, icons, git status |
| **du** | **dust** | Yes | Visual disk usage |
| **top/htop** | **bottom** (btm) | Yes | Modern TUI, per-process GPU/network |
| **diff** | **delta** | Yes | Syntax-aware, side-by-side |
| **cd** (smart jump) | **zoxide** | Yes | Frecency-based directory jumping |
| **wc** (code stats) | **tokei** | Yes | Language-aware line counting |
| **ps** | **procs** | Yes | Colored, sortable, tree view |
| **curl** (interactive) | **xh** | Partial | HTTPie-like, built on reqwest |
| **hexdump/xxd** | **hexyl** | Yes | Colored hex viewer |

### Layer 5: Shell & Terminal (Rust)

| Classic | Rust Replacement | Alpine pkg | Notes |
|---------|-----------------|------------|-------|
| **bash/zsh** | **nushell** | Yes | Structured data shell. bash stays for legacy scripts. |
| **tmux/screen** | **zellij** | Yes | Session management, floating panes |
| **xterm/gnome-terminal** | **alacritty** | Yes | GPU-accelerated, minimal |
| | **cosmic-term** | Yes | COSMIC-native, iced-based |
| **prompt** | **starship** | Yes | Cross-shell, informative, fast |

### Layer 6: Desktop Applications (100% Rust — COSMIC)

| Application | Package | Notes |
|-------------|---------|-------|
| **Compositor** | cosmic-comp | Wayland compositor, smithay-based |
| **File manager** | cosmic-files | Dual-pane, tabs |
| **Text editor** | cosmic-edit | Syntax highlighting, LSP |
| **Terminal** | cosmic-term | iced-based |
| **Settings** | cosmic-settings | System configuration |
| **App launcher** | cosmic-launcher | Application search |
| **App library** | cosmic-app-library | Grid launcher |
| **Screenshot** | cosmic-screenshot | Screen capture |
| **Media player** | cosmic-player | Audio/video |
| **Calculator** | cosmix-calc | With cosmix-port |
| **Image viewer** | cosmix-view | With cosmix-port |
| **Mail client** | cosmix-mail | JMAP via Stalwart, with cosmix-port |
| **Web dashboard** | cosmix-web | Axum + HTMX, accessible from any device |

### Layer 7: Developer Tools (Rust)

| Classic | Rust Replacement | Notes |
|---------|-----------------|-------|
| **vim/neovim** | **helix** | Modal editor, LSP built-in, tree-sitter |
| **git (C)** | **gitoxide** (gix) | Pure Rust git. Library and CLI. |
| **make** | **just** | Command runner, simpler than make |
| **cargo** | — | Rust's own build system |
| **hyperfine** | — | Benchmarking tool |
| **tokei** | — | Code statistics |

## The cosmix-port Difference

Without cosmix-port, this is just a list of Rust binaries on Alpine. With it,
it's an integrated operating system:

```lua
-- Every Rust service is a port. Every port is scriptable.
-- Every port is reachable across the mesh.

-- Desktop automation
local files = cosmix.port("cosmic-files")
files:call("open", { path = "/home/user/Documents" })

-- System queries from Lua
local journal = cosmix.port("journal")
local errors = journal:call("query", { priority = "err", since = "-1h" })

-- Cross-node orchestration
local dns = cosmix.port("resolved@mko")
dns:send("flush")

-- Mail from a script
local mail = cosmix.port("cosmix-mail")
local inbox = mail:call("status")
cosmix.notify("You have " .. inbox.unread .. " unread emails")

-- Chain it all together
local nodes = {"cachyos", "mko", "gcwg"}
for _, node in ipairs(nodes) do
    local j = cosmix.port("journal@" .. node)
    local errs = j:call("query", { priority = "err", since = "-8h" })
    if #errs > 0 then
        mail:call("send", {
            to = "admin@kanary.org",
            subject = node .. ": " .. #errs .. " errors",
            body = cosmix.json(errs)
        })
    end
end
```

This is infrastructure-as-Lua-script. No Ansible, no SSH loops, no YAML.
Every service is a port, every port speaks AMP, every node is reachable.

## Deployment Variants

### Desktop (Primary)

The full COSMIC desktop experience on a laptop or workstation.

```
Alpine base + linux-cachyos kernel
├── COSMIC desktop (all components)
├── cosmix-daemon + cosmix-port apps
├── cosmix-seat (session management)
├── cosmix-journal (logging)
├── cosmix-resolved (DNS)
├── Rust userspace (uutils, ripgrep, fd, bat, eza, nushell, etc.)
├── PipeWire, BlueZ, NetworkManager (C — invisible plumbing)
└── Developer tools (helix, zellij, alacritty, cargo)
```

### Server Node

Headless mesh node running services.

```
Alpine base + linux-lts kernel
├── cosmix-daemon (mesh + ports + Lua)
├── cosmix-web (Axum + HTMX dashboard)
├── cosmix-journal (logging)
├── cosmix-resolved (DNS)
├── Stalwart (mail — JMAP/IMAP/SMTP)
├── PostgreSQL (C — the one exception, and it's earned it)
├── Rust userspace (uutils, ripgrep, etc.)
└── sudo-rs, ntpd-rs
```

### Embedded / ARM

Minimal image for Raspberry Pi, ARM SBC, or postmarketOS phone.

```
Alpine base + linux-lts kernel
├── COSMIC desktop (aarch64 — already in Alpine)
├── cosmix-daemon (static binary, ~10MB)
├── Minimal Rust userspace
└── Static binaries — copy and run, zero deps
```

### Custom ISO Build

```bash
# Clone Alpine aports
git clone https://gitlab.alpinelinux.org/alpine/aports.git

# Add cosmix overlay (custom packages)
# - linux-cachyos: kernel with BORE/MGLRU patches
# - cosmix-daemon, cosmix-web, cosmix-seat, cosmix-journal, cosmix-resolved
# - cosmixctl
# - Default config: nushell, helix, starship, Rust userspace tools

# Build the ISO
sh aports/scripts/mkimage.sh \
    --arch x86_64 \
    --profile cosmix \
    --outdir /tmp/cosmix-iso

# Result: cosmix-x86_64.iso — bootable, installable
```

## What Percentage Is Rust?

Measured by "binaries the user directly interacts with":

| Layer | Rust % | Notes |
|-------|--------|-------|
| Desktop apps | 100% | COSMIC + cosmix apps |
| Terminal tools | 95% | uutils + Rust CLI tools. bash stays for scripts. |
| System services (user-visible) | 80% | cosmix-*, sudo-rs, ntpd-rs. NetworkManager stays C. |
| System services (invisible) | 20% | PipeWire, BlueZ, udevd, polkit — all C, all invisible |
| Kernel | ~1% | Growing as DRM mandates Rust for new GPU drivers |

**By user experience: ~95% Rust.** Everything you see, type, click, and
interact with is Rust. The C is plumbing — important, reliable, invisible.

**By line count: ~30-40% Rust.** The kernel is 34M lines of C. But line count
is the wrong metric. The user doesn't experience the kernel.

## Naming

**Cosmix OS** — Alpine base, COSMIC desktop, cosmix integration layer.

Or simply: **Cosmix**. The OS is the project. The project is the OS.

The name already captures it: COSMIC + remix. A remixed Linux where Rust
replaces everything the user touches, Lua scripts orchestrate it all, and
the mesh connects every node.

## Timeline

| Phase | Focus | Builds on |
|-------|-------|-----------|
| **Now** | CachyOS daily driver, build cosmix apps and services | Current setup |
| **Phase A** | Package cosmix-* as Alpine APKBUILDs, test on Alpine VM | Alpine's existing COSMIC packages |
| **Phase B** | Build `linux-cachyos` Alpine kernel package | CachyOS patch set |
| **Phase C** | Create `mkimg.cosmix.sh` profile, first bootable ISO | Alpine mkimage |
| **Phase D** | Replace default tools: uutils, nushell, Rust CLI, helix | Alpine community packages |
| **Phase E** | cosmix-journal + cosmix-resolved in daily use | cosmix-systemd strategy doc |
| **Phase F** | cosmix-seat replacing elogind for COSMIC sessions | Requires COSMIC testing |
| **Phase G** | Public release — "Cosmix OS" installable ISO | All above |

Each phase is independently useful. Phase A can happen tomorrow — it's just
writing APKBUILDs for crates that already compile.

## What This Is Not

- **Not a new distro for its own sake.** It's Alpine + COSMIC + cosmix packages.
  The Alpine community maintains the base, COSMIC team maintains the desktop,
  we maintain the integration layer.

- **Not ideologically pure.** C stays where C works (kernel, libc, audio, Bluetooth).
  The goal is user experience, not language purity.

- **Not a fork.** Standard Alpine, standard COSMIC, custom cosmix overlay.
  `apk upgrade` still works. Alpine security patches still apply.

- **Not just for desktops.** The same cosmix packages run on servers (without
  COSMIC), on ARM (same APKBUILDs), on phones (postmarketOS is Alpine).

## The Vision

A developer sits down at their Cosmix OS desktop. Every application they
open — file manager, editor, terminal, mail client, calculator — is Rust,
built with libcosmic, integrated via cosmix-port. They write a Lua script
that queries their mail on one server, checks logs on another, deploys code
to a third, and posts a summary to their dashboard. Every service is a port.
Every port is scriptable. Every node is reachable.

The C is there — in the kernel, in musl, in PipeWire pushing audio to their
speakers. But they never see it, never think about it, never debug a
use-after-free or chase a buffer overflow in their daily tools.

**It's AmigaOS ARexx meets Alpine Linux meets the Rust ecosystem.
And it's buildable today.**

---

*Document created: 2026-03-10*
*Status: Vision document — builds on cosmix-systemd-strategy.md and pure-rust-stack-vision.md*
*Depends on: Alpine COSMIC packages (exist), cosmix-port (complete), cosmix-daemon (complete)*
