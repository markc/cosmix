# cosmixos First Light — Alpine COSMIC Desktop in Incus CT

> **First light: 2026-03-17, 6:44pm AEST (Tuesday)**
>
> A full COSMIC desktop environment running inside an Alpine Linux Incus container with direct DRM rendering, GPU acceleration, and full keyboard/mouse input. No prior public record of COSMIC (or any Smithay-based compositor) running in LXC/Incus with direct DRM.

## Hardware

| Component | Details |
|-----------|---------|
| **Machine** | CachyOS desktop workstation |
| **CPU** | (host CPU, shared with CT) |
| **GPU** | Intel Arc (xe driver, DRM device `/dev/dri/card0` + `renderD128`) |
| **Display** | Physical HDMI monitor (CT renders directly via DRM/KMS) |
| **Keyboard** | AT Translated Set 2 (wired) + Lofree Flow84 (wireless BT/2.4GHz) |
| **Mouse** | USB optical mouse + Lofree Flow84 trackpad |
| **Audio** | Audient iD4 USB interface (discovered by udev, not yet tested) |
| **OS** | CachyOS (Linux 6.19.7-1-cachyos-bore-lto) |
| **Container runtime** | Incus (LXC) |

## Software Stack

| Layer | Component | Version |
|-------|-----------|---------|
| **Container OS** | Alpine Linux | Edge (3.24+) |
| **Desktop** | COSMIC | 1.0.8 (Alpine `edge/community`) |
| **Compositor** | cosmic-comp | DRM/KMS backend (Smithay) |
| **Seat manager** | seatd | 0.9.3 (direct daemon, NOT seatd-launch) |
| **Graphics** | Mesa + xe driver | Mesa from Alpine Edge |
| **Init** | OpenRC | Alpine default |
| **Shell** | bash | (busybox ash for system scripts) |

## How It Came to Be — The Full Story

### The vision (2026-03-10)

The cosmixos concept started as a vision document (`_doc/2026-03-10-cosmix-os-vision.md`): a near-100% Rust desktop OS built on Alpine Linux with COSMIC as the desktop. The key insight was that the Rust ecosystem had reached a tipping point where every user-facing layer had a production-quality Rust alternative, and what was missing was an integration layer (cosmix-port) to make them a coherent system.

Alpine was chosen for musl (true static binaries), apk (fast package management), OpenRC (no systemd), and the fact that COSMIC was already packaged in Alpine Edge.

### The container approach

Rather than bare-metal Alpine, the plan was to run COSMIC inside an Incus container on the existing CachyOS workstation. Benefits:

- **Snapshotable** — entire desktop state captured with `incus snapshot`
- **Exportable** — `incus export` creates a portable tarball
- **Restorable** — destroyed desktop rebuilt in seconds from image
- **Isolatable** — CT boundary prevents desktop experiments from breaking the host
- **Replicable** — same image deployable across the mesh fleet

The eventual target is a Proxmox CT with PBS backup/restore for the daily driver desktop.

### The wrong first approach: cage / nested Wayland (Session 4, early)

The first working prototype used `cage` as a kiosk compositor on the host, with cosmic-comp inside the CT running as a nested Wayland client (Winit backend):

```bash
exec cage -- incus exec cosmixos -- su - cosmic -c '
    export WAYLAND_DISPLAY=wayland-0
    export XDG_RUNTIME_DIR=/mnt/wayland
    exec dbus-run-session -- cosmic-session
'
```

This worked for COSMIC-in-a-window but was architecturally wrong: it required cage on the host, a shared Wayland socket mount, an extra compositor layer, and didn't give COSMIC direct GPU access. It was a development aid, not the target.

### The right approach: direct DRM via seatd (Session 4, later)

The CT already had everything for direct rendering:
- GPU passthrough via Incus `gpu` device
- Input devices via `/dev/input` bind mount
- TTYs via `unix-char` devices
- seatd for device brokering

When cosmic-comp sees no `WAYLAND_DISPLAY`, it uses the DRM/KMS backend — rendering directly to the physical display. The host compositor must be stopped first (DRM master is exclusive).

**Session 4 result:** Desktop rendered on the physical display. Full COSMIC session visible — panel, wallpaper, launcher. But keyboard and mouse were completely unresponsive.

### The six-session debugging journey (Sessions 4-9)

What followed was a systematic excavation through four distinct layers of container/compositor interaction, each blocking input in a different way:

#### Blocker 1: seatd VT-bound seat (Sessions 4-6)

**Symptom:** seatd logs showed `Created VT-bound seat seat0` followed by `Could not open VT for client`. cosmic-comp exited with code 1.

**Root cause:** seatd's default mode creates a VT-bound seat, which requires VT ioctls (`VT_OPENQRY`, `VT_ACTIVATE`, `VT_SETMODE`). These fail in containers because the container doesn't own the VT subsystem — the host kernel does.

**Fix:** `SEATD_VTBOUND=0` environment variable (seatd's built-in mechanism, checked in `server.c:42-45`).

#### Blocker 2: seatd-launch strips all environment variables (Session 8)

**Symptom:** Despite setting `SEATD_VTBOUND=0`, seatd continued creating VT-bound seats. The fix from Blocker 1 appeared to have no effect.

**Root cause:** `seatd-launch` (the recommended way to use seatd) passes a **NULL environment array** to the seatd child process via `execve()`:

```c
// seatd-launch/seatd-launch.c
char *env[1] = {NULL};
execve(SEATD_INSTALLPATH, command, env);
```

Every environment variable is stripped before seatd starts. `SEATD_VTBOUND=0` never reached the seatd process.

**Fix:** Abandoned `seatd-launch` entirely. Run seatd directly as a daemon:

```bash
setsid env SEATD_VTBOUND=0 seatd -u cosmic -g seat -l debug &
```

Then connect cosmic-session via `LIBSEAT_BACKEND=seatd`.

#### Blocker 3: udev tagging failure in containers (Session 7)

**Symptom:** Desktop rendered (DRM master acquired) but `libinput list-devices` returned zero devices. Input device nodes existed at `/dev/input/event*` with correct permissions, but libinput couldn't find them.

**Root cause:** libinput discovers devices via udev database tags (`ID_INPUT_KEYBOARD`, `ID_INPUT_MOUSE`, `LIBINPUT_DEVICE_GROUP`). In an Incus CT:
- `/dev` is tmpfs (not devtmpfs), so `rc-service udev start` fails
- `udevadm trigger` sends synthetic uevents but udevd silently drops most of them (only 4-5 of 20+ devices processed)
- Without tags, libinput's `udev_enumerate_scan_devices()` returns nothing

**Research:** Deep search across Proxmox forums, Linux Containers forums, sr.ht/seatd source, wlroots issues, and libinput source. No prior documentation of COSMIC in LXC/Incus. The standard solution across all container runtimes: bind-mount the host's `/run/udev` (read-only). The host's udevd has already tagged every device.

**Implementation challenge:** Incus mounts tmpfs on `/run`, blocking `lxc.mount.entry` targeting `/run/udev`.

**Fix:** Two-step mount:
1. Incus disk device: `source=/run/udev path=/opt/host-udev readonly=true`
2. In start script: `mount --bind /opt/host-udev /run/udev`

Result: 504 udev database entries visible inside CT, all input devices properly tagged.

#### Blocker 4: cgroup v2 device permissions (Session 9)

**Symptom:** `libinput list-devices` now found all devices with correct tags, but every evdev device failed with "Operation not permitted" — even as root.

**Root cause:** The `/dev/input` bind mount made device nodes visible in the filesystem, but the **cgroup v2 BPF device controller** blocked access at the kernel level. Character device major 13 (input subsystem) was never whitelisted.

**Fix:**
```bash
incus config set cosmixos raw.lxc "lxc.mount.entry = /dev/input dev/input none bind,create=dir 0 0
lxc.cgroup2.devices.allow = c 13:* rwm"
```

#### Also fixed: busybox ash doesn't have `disown` (Session 9)

The start script used `setsid ... & disown` but Alpine's `sh` is busybox ash, which lacks `disown` (a bash builtin). With `set -e`, the script exited on the `disown` failure before seatd started. Fix: removed `disown` (redundant after `setsid`).

### Summary of all blockers

| # | Blocker | Layer | Fix | Session |
|---|---------|-------|-----|---------|
| 1 | seatd VT ioctls fail in CT | Seat management | `SEATD_VTBOUND=0` | 4-6 |
| 2 | seatd-launch strips env vars | Alpine packaging | Direct seatd daemon | 8 |
| 3 | udev database empty in CT | Device discovery | Bind-mount host `/run/udev` | 7 |
| 4 | cgroup blocks evdev access | Kernel security | `lxc.cgroup2.devices.allow = c 13:* rwm` | 9 |

## Exact Reproduction Steps

### 1. Incus CT Configuration

```bash
# Create Alpine Edge CT
incus launch images:alpine/edge cosmixos

# Privileged with UID mapping
incus config set cosmixos security.privileged true
incus config set cosmixos raw.idmap "both 1001 1001"

# GPU passthrough
incus config device add cosmixos gpu gpu gid=1001

# TTY devices (tty0-tty7)
for i in $(seq 0 7); do
  incus config device add cosmixos tty$i unix-char \
    path=/dev/tty$i uid=0 gid=5 mode=0660 major=4 minor=$i
done

# Input devices + cgroup permission
incus config set cosmixos raw.lxc "lxc.mount.entry = /dev/input dev/input none bind,create=dir 0 0
lxc.cgroup2.devices.allow = c 13:* rwm"

# Host udev database (for libinput device discovery)
incus config device add cosmixos host-udev disk \
  source=/run/udev path=/opt/host-udev readonly=true

# Restart to apply
incus restart cosmixos
```

### 2. Alpine Packages

```bash
incus exec cosmixos -- sh -c '
  # Enable edge repos
  sed -i "s|#\(.*community\)|\1|" /etc/apk/repositories
  sed -i "s|v[0-9.]*|edge|g" /etc/apk/repositories
  apk update

  # Core
  apk add eudev dbus mesa-dri-gallium mesa-va-gallium \
    xkeyboard-config font-noto font-dejavu bash seatd libinput

  # COSMIC desktop (22+ packages)
  apk add cosmic-comp cosmic-session cosmic-panel cosmic-applets \
    cosmic-app-library cosmic-bg cosmic-settings cosmic-settings-daemon \
    cosmic-launcher cosmic-notifications cosmic-osd cosmic-files \
    cosmic-term cosmic-icons cosmic-edit cosmic-screenshot

  # Icon themes
  apk add hicolor-icon-theme adwaita-icon-theme

  # Services
  setup-devd udev
  rc-update add dbus default
  # NOTE: seatd and cosmic-greeter NOT in default runlevel (managed by start script)
'
```

### 3. User Setup

```bash
incus exec cosmixos -- sh -c '
  # Groups matching host GIDs
  addgroup -g 989 render 2>/dev/null || true
  addgroup -g 985 video 2>/dev/null || true

  # cosmic user (UID 1001)
  adduser -D -u 1001 -s /bin/bash cosmic

  # All required groups
  for g in render video input seat tty hostinput; do
    addgroup cosmic $g 2>/dev/null || true
  done
'
```

### 4. Launch

From a host text TTY (Ctrl+Alt+F2):

```bash
bash ~/.gh/cosmix/scripts/start-cosmixos
```

The script:
1. Stops host compositor (frees DRM master)
2. Polls until GPU device is free
3. Starts dbus, cleans stale seatd
4. Bind-mounts host udev: `/opt/host-udev` → `/run/udev`
5. Creates `XDG_RUNTIME_DIR`
6. Starts seatd daemon with `SEATD_VTBOUND=0`
7. Launches `cosmic-session` as user `cosmic` with `LIBSEAT_BACKEND=seatd`
8. On exit: kills seatd, restarts host compositor

## What This Enables

### Immediate

- **Daily driver testing** — use COSMIC-in-Alpine for real work, find rough edges
- **Snapshot-based safety** — `incus snapshot cosmixos good-state` before experiments
- **Build self-hosting** — compile cosmix inside the same CT that runs the desktop

### Near-term

- **Desktop image variant** — `build-image.sh desktop` for fleet deployment
- **Proxmox migration** — same setup on Proxmox CT with PBS backup/restore
- **Second machine** — USB SSD with Proxmox, Alpine CT, daily driver desktop

### Long-term

- **cosmixos as product** — portable, snapshotable, mesh-connected COSMIC desktop
- **Self-hosting complete** — cosmix builds itself, on itself, in itself
- **Fleet desktops** — deploy desktop CTs across mesh nodes with `mesh-spawn.sh`

## Key Technical Discoveries

1. **seatd-launch strips environment** — Alpine's `seatd-launch` passes `NULL` env to `execve()`. Direct daemon invocation is the only way to pass `SEATD_VTBOUND=0`.

2. **udev in containers needs host bind-mount** — Container udevd can't reliably process synthetic uevents. The host's `/run/udev` database is the authoritative source.

3. **cgroup v2 BPF device controller is invisible** — Device nodes are visible in the filesystem but access is blocked at the kernel level. `lxc.cgroup2.devices.allow` is required for any device class beyond the defaults.

4. **VT ioctls fail in containers** — seatd must run in non-VT-bound mode. This is supported but not the default, and not documented for container use.

5. **DRM master is exclusive** — Only one process system-wide can hold DRM master on a GPU. Host and CT compositor cannot coexist.

6. **busybox ash != bash** — `disown` is a bash builtin not available in ash. Scripts run via `incus exec -- sh -c` use ash.

7. **Nobody had done this before** — No public documentation of COSMIC/Smithay in LXC/Incus with direct DRM. Every blocker was discovered empirically.

## References

- [Proxmox: Forward udev properties to container](https://forum.proxmox.com/threads/forward-all-udev-properties-to-container.170247/)
- [CachyOS LXC gist (sway/hyprland in privileged CT)](https://gist.github.com/yvesh/463594f0d3e9174a8032f236a59f8a50)
- [seatd source: SEATD_VTBOUND check](https://git.sr.ht/~kennylevinsen/seatd) — `server.c:42-45`
- [seatd-launch source: NULL env](https://git.sr.ht/~kennylevinsen/seatd) — `seatd-launch/seatd-launch.c`
- [libinput source: required udev properties](https://github.com/jadahl/libinput) — `evdev.c:80-94`
- [wlroots #2257: udev is mandatory for libinput](https://github.com/swaywm/wlroots/issues/2257)
- [Toolbox #992: /run/udev/tags bind-mount fix](https://github.com/containers/toolbox/issues/992)
