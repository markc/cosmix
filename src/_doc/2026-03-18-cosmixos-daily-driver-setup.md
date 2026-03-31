# CosmixOS Daily Driver — Complete Reproduction Guide

> **Date:** 2026-03-18
>
> Complete, tested instructions for reproducing a COSMIC desktop running inside an Alpine Linux Incus container on a CachyOS host, with shared home directory, glibc Claude Code, and SSH access. This is the daily-driver configuration for dogfooding cosmixos as a primary development environment.
>
> **Prerequisite reading:** `_doc/2026-03-17-cosmixos-first-light.md` for the debugging journey and technical discoveries that led to this configuration.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│  CachyOS Host (glibc, systemd, Incus)                   │
│                                                         │
│  /home/cosmix/          ←── bind-mounted into CT        │
│  /opt/claude-code/      ←── bind-mounted into CT        │
│  /usr/lib/              ←── bind-mounted as /host-lib   │
│  /usr/lib64/            ←── bind-mounted into CT        │
│  /run/udev/             ←── bind-mounted via /opt/...   │
│  /dev/dri/*             ←── GPU passthrough              │
│  /dev/input/*           ←── input device passthrough     │
│  /dev/tty0-7            ←── TTY passthrough              │
│                                                         │
│  ┌───────────────────────────────────────────────────┐  │
│  │  cosmixos CT (Alpine Edge, musl, OpenRC)          │  │
│  │                                                   │  │
│  │  COSMIC 1.0.8 desktop (cosmic-comp, DRM/KMS)     │  │
│  │  cosmix user (UID 1001, wheel+utmp)               │  │
│  │  Claude Code via glibc ld-linux shim              │  │
│  │  sshd, dbus, seatd                               │  │
│  │  /home/cosmix/ ←── same files as host             │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

**Key insight:** The home directory bind-mount means the host and CT share config, SSH keys, shell customizations, COSMIC settings, browser profiles, and Claude memories. One identity, two operating systems.

## Host Requirements

| Component | Requirement |
|-----------|-------------|
| **OS** | CachyOS (or any Arch-based with recent kernel) |
| **Kernel** | 6.19+ (tested: 6.19.7-1-cachyos-bore-lto) |
| **GPU** | Intel Arc (xe driver) — AMD should also work |
| **Container runtime** | Incus (LXC) |
| **Claude Code** | Installed at `/opt/claude-code/` (Anthropic's SEA binary) |
| **User** | `cosmix` with UID 1001 on the host |

## Step 1: Create the Alpine CT

```bash
incus launch images:alpine/edge cosmixos
```

## Step 2: Container Configuration

### Privileged mode and UID mapping

```bash
incus config set cosmixos security.privileged true
incus config set cosmixos raw.idmap "both 1001 1001"
```

### GPU passthrough

```bash
incus config device add cosmixos gpu gpu gid=1001
```

### TTY devices (tty0-tty7)

```bash
for i in $(seq 0 7); do
  incus config device add cosmixos tty$i unix-char \
    path=/dev/tty$i uid=0 gid=5 mode=0660 major=4 minor=$i
done
```

### Input devices and cgroup permissions

```bash
incus config set cosmixos raw.lxc "lxc.mount.entry = /dev/input dev/input none bind,create=dir 0 0
lxc.cgroup2.devices.allow = c 13:* rwm"
```

### Host udev database (required for libinput device discovery)

```bash
incus config device add cosmixos host-udev disk \
  source=/run/udev path=/opt/host-udev readonly=true
```

### Home directory bind-mount

```bash
incus config device add cosmixos home-cosmix disk \
  source=/home/cosmix path=/home/cosmix shift=true
```

### Glibc compatibility (for Claude Code)

```bash
# Claude Code binary
incus config device add cosmixos claude-code disk \
  source=/opt/claude-code path=/opt/claude-code

# Host glibc dynamic linker
incus config device add cosmixos host-lib64 disk \
  source=/usr/lib64 path=/usr/lib64 readonly=true

# Host glibc runtime libraries
incus config device add cosmixos host-usrlib disk \
  source=/usr/lib path=/host-lib readonly=true
```

### Apply configuration

```bash
incus restart cosmixos
```

## Step 3: Alpine Packages

```bash
incus exec cosmixos -- sh -c '
# Ensure edge repos with community and testing
cat > /etc/apk/repositories << REPOS
https://dl-cdn.alpinelinux.org/alpine/edge/main
https://dl-cdn.alpinelinux.org/alpine/edge/community
https://dl-cdn.alpinelinux.org/alpine/edge/testing
REPOS
apk update && apk upgrade

# Core system
apk add bash nano rsync sudo openssh eudev dbus seatd \
  mesa-dri-gallium mesa-va-gallium mesa-utils libinput \
  xkeyboard-config xwayland font-noto font-dejavu \
  hicolor-icon-theme adwaita-icon-theme gcompat

# COSMIC desktop
apk add cosmic-comp cosmic-session cosmic-panel cosmic-applets \
  cosmic-app-library cosmic-bg cosmic-settings cosmic-settings-daemon \
  cosmic-launcher cosmic-notifications cosmic-osd cosmic-files \
  cosmic-term cosmic-icons cosmic-edit cosmic-screenshot \
  cosmic-workspaces cosmic-idle cosmic-randr cosmic-greeter \
  xdg-desktop-portal-cosmic

# Enable services
setup-devd udev
rc-update add dbus default
rc-update add sshd default
rc-update add seatd default
'
```

## Step 4: User Setup (NS 3.0 Convention)

```bash
incus exec cosmixos -- sh -c '
# sysadm: admin user (UID 1000)
addgroup -g 1000 sysadm
adduser -D -u 1000 -G sysadm -s /bin/bash -g "System Admin" sysadm
addgroup sysadm wheel

# cosmix: service + desktop user (UID 1001)
addgroup -g 1001 cosmix
adduser -D -u 1001 -G cosmix -h /home/cosmix -s /bin/bash -g "Cosmix Service" cosmix

# cosmix group memberships
addgroup cosmix wheel    # sudo access
addgroup cosmix utmp     # login record writing (wtmp/btmp)
addgroup cosmix tty      # terminal devices
addgroup cosmix input    # input devices (/dev/input/*)
addgroup cosmix video    # video/DRM devices
addgroup cosmix render   # GPU render nodes
addgroup cosmix seat     # seatd access

# Passwordless sudo for wheel group
mkdir -p /etc/sudoers.d
echo "%wheel ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/wheel
chmod 440 /etc/sudoers.d/wheel

# Runtime directories
install -d -o cosmix -g cosmix -m 755 /run/cosmix
install -d -o cosmix -g cosmix -m 755 /var/log/cosmix
install -d -o cosmix -g cosmix -m 700 /home/cosmix/.config/cosmix

# Clear default motd
true > /etc/motd
'
```

### Expected group membership

```
cosmix : cosmix tty wheel input video utmp render seat
```

## Step 5: Glibc Loader for Claude Code

The Claude Code binary is a glibc-linked Node.js SEA (Single Executable Application). Alpine uses musl. The solution: create `/lib64/ld-linux-x86-64.so.2` (the standard glibc loader path) pointing to the host's loader via the bind-mount, then use `LD_LIBRARY_PATH` to find the host's glibc at runtime.

```bash
incus exec cosmixos -- sh -c '
# Create the glibc loader symlink at the path the binary expects
mkdir -p /lib64
ln -sf /usr/lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2

# Create the claude wrapper
cat > /usr/local/bin/claude << '\''WRAPPER'\''
#!/bin/sh
export DISABLE_AUTOUPDATER=1
export LD_LIBRARY_PATH=/host-lib
exec /opt/claude-code/bin/claude "$@"
WRAPPER
chmod +x /usr/local/bin/claude

# Symlink into /usr/bin so it is always in PATH
ln -sf /usr/local/bin/claude /usr/bin/claude
'
```

### How it works

```
claude (wrapper)
  └─ sets LD_LIBRARY_PATH=/host-lib (host glibc libs)
     └─ exec /opt/claude-code/bin/claude (glibc ELF binary)
        └─ /lib64/ld-linux-x86-64.so.2 (symlink to host loader)
           └─ loads libc.so.6, libm.so.6, etc from /host-lib
```

**Critical detail:** The binary must be executed directly (`exec /opt/claude-code/bin/claude`), NOT via the ld-linux loader (`/usr/lib64/ld-linux-x86-64.so.2 --library-path ... /opt/claude-code/bin/claude`). The ld-linux invocation changes `argv[0]`, which causes the embedded Bun/Node runtime to identify as `bun` instead of `claude`, breaking the SEA entrypoint entirely.

### Verification

```bash
incus exec cosmixos -- su -l cosmix -c 'claude --version'
# Expected: 1.3.11 (or current version)
```

## Step 6: SSH Access

The home directory bind-mount means `~/.ssh/` is shared. If the host's `~/.ssh/id_ed25519.pub` is in `~/.ssh/authorized_keys`, SSH to the CT works immediately:

```bash
# On the host, if not already done:
cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys

# Test:
ssh cosmix@<CT_IP>
```

**Note on SSH multiplexing:** If `~/.ssh/config` has `ControlMaster auto` and `IdentitiesOnly yes`, a bare IP connection (`ssh cosmix@10.11.0.117`) will only work if a mux socket already exists from a prior connection via a configured host alias. The mux socket bypasses key negotiation entirely.

## Step 7: Shell Environment

The shared `~/.myrc` auto-detects the OS for prompt differentiation:

```bash
if grep -q '^ID=alpine' /etc/os-release 2>/dev/null; then
    export LABEL=alpine
    export COLOR=36      # cyan prompt
else
    export LABEL=cachyos
    export COLOR=32      # green prompt
fi
```

The `~/.rc/_shrc` toolkit detects `OSTYP=alpine` automatically and provides appropriate aliases:

| Alias | Expands to (Alpine) |
|-------|-------------------|
| `u` | `sudo apk update && sudo apk upgrade` |
| `i` | `sudo apk add` |
| `r` | `sudo apk del` |
| `s` | `sudo apk search -v` |

## Desktop Launch Methods

### Method A: Greeter login (shared session)

Log in via the CachyOS greeter as the `cosmix` user. Since the home directory is bind-mounted, the COSMIC desktop loads the same configuration in both environments. The desktop runs on the **host's** cosmic-comp, but all user-space config comes from the shared home.

**Limitation:** This is the host desktop, not the CT desktop. Apps run on the host.

### Method B: Direct DRM takeover (CT desktop)

From a host text TTY (Ctrl+Alt+F2):

```bash
bash ~/.gh/cosmix/scripts/start-cosmixos
```

This stops the host compositor, hands DRM master to the CT's cosmic-comp, and launches a full COSMIC session inside Alpine. See `scripts/start-cosmixos` for the complete launch sequence.

**Limitation:** Host and CT compositor cannot run simultaneously (DRM master is exclusive). Logout returns to the host greeter.

## What's Shared vs Isolated

| Resource | Shared? | Mechanism |
|----------|---------|-----------|
| Home directory (`~`) | Yes | Incus disk device, bind-mount |
| COSMIC config (`~/.config/cosmic/`) | Yes | Part of home |
| Browser profiles (`~/.mozilla/`, `~/.thunderbird/`) | Yes | Part of home |
| Claude config + memories (`~/.claude/`) | Yes | Part of home |
| SSH keys + config (`~/.ssh/`) | Yes | Part of home |
| Shell config (`~/.rc/`, `~/.myrc`) | Yes | Part of home |
| System packages | No | Alpine apk vs CachyOS pacman |
| Init system | No | OpenRC vs systemd |
| C library | No | musl vs glibc |
| Kernel | Yes | Host kernel (always) |
| GPU | Yes | Passthrough, exclusive DRM master |

**Warning:** Do not run the same application (Firefox, Thunderbird) simultaneously on host and CT — they share profile directories and lock files. One desktop session at a time.

## Final Incus Device Summary

```
$ incus config device list cosmixos

claude-code   disk       /opt/claude-code ← /opt/claude-code
gpu           gpu        gid=1001
home-cosmix   disk       /home/cosmix ← /home/cosmix (shift=true)
host-lib64    disk       /usr/lib64 ← /usr/lib64 (readonly)
host-udev     disk       /opt/host-udev ← /run/udev (readonly)
host-usrlib   disk       /host-lib ← /usr/lib (readonly)
tty0-tty7     unix-char  /dev/tty0-7 (uid=0, gid=5, mode=0660)
```

Raw LXC config:
```
lxc.mount.entry = /dev/input dev/input none bind,create=dir 0 0
lxc.cgroup2.devices.allow = c 13:* rwm
```

## Applying to New Containers

All of this is codified in `scripts/cosmixos-setup.sh` (first-boot provisioning) and `scripts/build-image.sh` (image creation). The glibc/Claude setup should be added to `build-image.sh` for inclusion in the base image.

## Troubleshooting

### `apk add` fails with "N errors; M packages"

Run `apk fix` first — the package database may have stale entries from interrupted operations or version skew on Alpine Edge.

### Claude shows "Bun" help instead of Claude help

The binary is being invoked via the ld-linux loader directly (e.g., `/usr/lib64/ld-linux-x86-64.so.2 /opt/claude-code/bin/claude`). This changes `argv[0]` and breaks the SEA identity. Fix: use `LD_LIBRARY_PATH` and exec the binary directly.

### SSH key auth fails to CT

With `IdentitiesOnly yes` in `~/.ssh/config`, SSH won't offer keys unless an `IdentityFile` matches. Use a configured host alias (`ssh cosmix@cos`) or ensure a mux socket exists. Alternatively, add an `IdentityFile` line to a host entry for the CT IP.

### Desktop crash on Ctrl+C in cosmic-term

Likely a COSMIC compositor bug (alpha software), not a signal propagation issue. Ctrl+C in a terminal should only SIGINT the foreground process. Check `journalctl --user -b -1` for cosmic-comp crash traces after reboot.

### No input devices (keyboard/mouse unresponsive)

Verify the udev bind-mount is active inside the CT:
```bash
ls /run/udev/data/ | wc -l    # should show 500+ entries
```
If empty, the `mount --bind /opt/host-udev /run/udev` step was missed. The `start-cosmixos` script handles this automatically.
