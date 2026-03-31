# Proxmox USB SSD Desktop — Alpine COSMIC in a CT

> Install Proxmox on a WD My Passport SSD (500GB, USB-C), run Alpine Edge + COSMIC
> desktop as a privileged LXC container with GPU passthrough. PBS backup/restore for
> the entire desktop environment.

## Why

- **PBS safety net** — backup/restore/clone entire desktop in minutes
- **ZFS storage** — snapshots, send/receive, data integrity
- **Dogfood** — run cosmix on real Alpine, discover pain points
- **Incus replacement** — Proxmox LXC is native, no extra container runtime
- **Portable** — boot any x86_64 machine from the USB SSD

## Hardware

- **Host:** Intel Meteor Lake minipc (Intel Arc GPU, xe driver)
- **USB SSD:** WD My Passport SSD 500GB (USB-C, ~1000MB/s)
- **GPU:** Intel Arc (integrated), device `/dev/dri/card0` + `renderD128`
- **Existing system:** CachyOS on internal NVMe (untouched, always available as fallback)

## Architecture

```
Physical display ← DRM/KMS ← cosmic-comp (Alpine CT) ← GPU passthrough
                                    ↑
                              Proxmox host (headless, Debian Trixie)
                              Web UI at https://<ip>:8006
                              ZFS pool on USB SSD
```

The CT IS the desktop. Proxmox host is headless — managed via web UI from
phone/laptop or SSH. The physical monitor shows the Alpine COSMIC session.

## Risk Assessment

- **Low risk:** CachyOS on NVMe is untouched. Reboot into NVMe = back to normal.
- **Medium risk:** seatd/DRM inside privileged CT might need experimentation.
- **Fallback:** If CT desktop fails, install Alpine directly on USB SSD (btrfs),
  skip Proxmox entirely. The COSMIC desktop + cosmix stack works either way.

---

## Phase 1: Install Proxmox on USB SSD

### 1.1 Download Proxmox VE ISO

From another machine or the current CachyOS desktop:

```bash
cd ~/Downloads
wget https://enterprise.proxmox.com/iso/proxmox-ve_8.4-1.iso
# Or whatever the latest 8.x release is
```

### 1.2 Write to a USB stick (NOT the SSD — a throwaway stick for the installer)

```bash
# Identify the throwaway USB stick (NOT the SSD!)
lsblk
sudo dd if=proxmox-ve_8.4-1.iso of=/dev/sdX bs=4M status=progress
```

### 1.3 Boot from the installer USB stick

- BIOS: set boot order to USB
- Install Proxmox VE
- **Target disk: select the WD My Passport SSD** (should show as /dev/sdX, ~500GB)
- **Filesystem: ZFS (RAID0)** — single disk, no redundancy needed for a desktop
- **Hostname:** `cosmixos.local` or `pve-desk.local`
- **IP:** static on your LAN (e.g. 192.168.1.50) or DHCP
- **Root password:** set something memorable
- Complete install, reboot, remove installer USB stick

### 1.4 Boot from USB SSD

- BIOS: set boot order to USB SSD
- Proxmox should boot to its console login
- Web UI: `https://<ip>:8006` from phone/laptop

### 1.5 Post-install (SSH or console)

```bash
# Remove enterprise repo (unless you have a subscription)
sed -i 's/^deb/#deb/' /etc/apt/sources.list.d/pve-enterprise.list

# Add no-subscription repo
echo 'deb http://download.proxmox.com/debian/pve bookworm pve-no-subscription' > \
  /etc/apt/sources.list.d/pve-no-subscription.list

apt update && apt dist-upgrade -y
```

---

## Phase 2: Create Alpine Desktop CT

### 2.1 Download Alpine CT template

Via Proxmox web UI: Datacenter → Storage → local → CT Templates → Templates → alpine-3.21 (or latest)

Or via CLI:

```bash
pveam update
pveam download local alpine-3.21-default_20241217_amd64.tar.xz
```

Note: this is Alpine stable. We'll switch to Edge inside the CT.

### 2.2 Create the CT

```bash
# Create privileged CT (required for DRM access)
pct create 100 local:vztmpl/alpine-3.21-default_20241217_amd64.tar.xz \
  --hostname cosmixos \
  --memory 16384 \
  --cores 8 \
  --rootfs local-zfs:80 \
  --net0 name=eth0,bridge=vmbr0,ip=dhcp \
  --unprivileged 0 \
  --features nesting=1 \
  --ostype alpine \
  --password
```

### 2.3 Configure GPU + Display Passthrough

Edit `/etc/pve/lxc/100.conf` — add these lines:

```conf
# GPU passthrough (Intel Arc / xe driver)
lxc.cgroup2.devices.allow: c 226:* rwm
lxc.mount.entry: /dev/dri dev/dri none bind,optional,create=dir

# TTY/VT access (for DRM backend — CT drives the physical display)
lxc.cgroup2.devices.allow: c 4:* rwm
lxc.mount.entry: /dev/tty0 dev/tty0 none bind,optional,create=file
lxc.mount.entry: /dev/tty1 dev/tty1 none bind,optional,create=file
lxc.mount.entry: /dev/tty7 dev/tty7 none bind,optional,create=file

# Input devices (keyboard + mouse)
lxc.cgroup2.devices.allow: c 13:* rwm
lxc.mount.entry: /dev/input dev/input none bind,optional,create=dir
lxc.mount.entry: /dev/uinput dev/uinput none bind,optional,create=file

# Framebuffer (may be needed)
lxc.cgroup2.devices.allow: c 29:* rwm
```

### 2.4 Start the CT

```bash
pct start 100
pct enter 100
```

---

## Phase 3: Configure Alpine Inside CT

### 3.1 Switch to Alpine Edge

```bash
# Replace stable repos with Edge
cat > /etc/apk/repositories << 'EOF'
https://dl-cdn.alpinelinux.org/alpine/edge/main
https://dl-cdn.alpinelinux.org/alpine/edge/community
https://dl-cdn.alpinelinux.org/alpine/edge/testing
EOF

apk update
apk upgrade --available
```

### 3.2 Install COSMIC Desktop

```bash
# Prerequisites
apk add eudev dbus mesa-dri-gallium mesa-va-gallium \
  xkeyboard-config font-noto font-dejavu bash sudo

# COSMIC packages
apk add cosmic-comp cosmic-session cosmic-panel cosmic-applets \
  cosmic-app-library cosmic-bg cosmic-settings cosmic-settings-daemon \
  cosmic-launcher cosmic-notifications cosmic-osd cosmic-files \
  cosmic-term cosmic-icons cosmic-edit cosmic-screenshot

# Icon themes
apk add hicolor-icon-theme adwaita-icon-theme

# Seat management (critical for DRM)
apk add seatd

# Display manager
apk add greetd cosmic-greeter

# udev
setup-devd udev
```

### 3.3 Create NS 3.0 Users

```bash
# sysadm (1000) — admin, sudo, desktop login
addgroup -g 1000 sysadm
adduser -D -u 1000 -G sysadm -h /home/sysadm -s /bin/bash -g "System Admin" sysadm
addgroup sysadm wheel
addgroup sysadm video
addgroup sysadm render
addgroup sysadm input
addgroup sysadm seat
echo 'sysadm:SET_A_PASSWORD' | chpasswd
echo '%wheel ALL=(ALL) ALL' > /etc/sudoers.d/wheel
chmod 440 /etc/sudoers.d/wheel

# cosmix (1001) — service user
addgroup -g 1001 cosmix
adduser -D -u 1001 -G cosmix -h /home/cosmix -s /bin/bash -g "Cosmix Service" cosmix
```

### 3.4 Configure Seat + Greeter

```bash
# Enable services
rc-update add dbus default
rc-update add seatd default
rc-update add greetd default

# greetd config — use cosmic-greeter
cat > /etc/greetd/config.toml << 'EOF'
[terminal]
vt = 1

[default_session]
command = "cosmic-greeter"
user = "cosmic-greeter"
EOF

# Ensure cosmic-greeter user exists and is in video/render groups
addgroup cosmic-greeter video 2>/dev/null || true
addgroup cosmic-greeter render 2>/dev/null || true
addgroup cosmic-greeter seat 2>/dev/null || true
```

### 3.5 Install Daily Driver Apps

```bash
# Browser + Email
apk add firefox thunderbird

# KDE Connect
apk add kdeconnect

# PDF viewer
apk add okular
# or lighter: apk add zathura zathura-pdf-mupdf

# Dev tools
apk add git nano openssh rsync curl

# Node.js (for Claude Code)
apk add nodejs npm

# Claude Code
npm install -g @anthropic-ai/claude-code
```

### 3.6 Install Cosmix Stack

```bash
# PostgreSQL
apk add postgresql postgresql-client
rc-update add postgresql default
rc-service postgresql start

# WireGuard
apk add wireguard-tools

# Rust toolchain (for building cosmix)
apk add build-base pkgconf openssl-dev openssl-libs-static \
  linux-headers cmake perl lua5.4-dev

su - cosmix -c '
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source ~/.cargo/env
  rustup target add x86_64-unknown-linux-musl
'

# Or: copy pre-built binaries from the existing cosmixos CT
# scp cosmixos:/usr/local/bin/cosmix-* /usr/local/bin/
```

### 3.7 First Boot Test

```bash
# Start services
rc-service dbus start
rc-service seatd start

# If greetd doesn't auto-start on the physical display:
rc-service greetd start
```

If seatd/greetd works, you should see the COSMIC greeter on the physical display.
Log in as `sysadm`.

---

## Phase 4: Restore Your Environment

Once the desktop is up and Claude Code is installed:

```bash
# From the CachyOS system (boot into NVMe temporarily), or via SSH:
rsync -av /home/cosmic/.gh/ sysadm@<proxmox-ct-ip>:/home/sysadm/.gh/
rsync -av /home/cosmic/.ns/ sysadm@<proxmox-ct-ip>:/home/sysadm/.ns/
rsync -av /home/cosmic/.claude/ sysadm@<proxmox-ct-ip>:/home/sysadm/.claude/

# SSH keys
rsync -av /home/cosmic/.ssh/ sysadm@<proxmox-ct-ip>:/home/sysadm/.ssh/

# Git config
scp /home/cosmic/.gitconfig sysadm@<proxmox-ct-ip>:~/.gitconfig
```

Then on the new desktop:

```bash
cd ~/.gh/cosmix && claude
# Continue from the NS 3.0 TODOs
```

---

## Phase 5: PBS Backup (the payoff)

Once everything works, set up PBS backup:

```bash
# On Proxmox host, add PBS storage
pvesm add pbs pbs-local \
  --server <pbs-ip> \
  --datastore <store-name> \
  --username backup@pbs \
  --password <password>

# Backup the desktop CT
vzdump 100 --storage pbs-local --mode snapshot --compress zstd
```

Now you can restore/clone your entire desktop — apps, config, data, everything — in
minutes. Break something? `pct restore 100 <backup-file>`.

---

## Troubleshooting

### cosmic-comp fails to acquire DRM device

The CT might not see the DRM device properly. Check:

```bash
ls -la /dev/dri/           # Should show card0 + renderD128
cat /sys/class/drm/card0/device/driver  # Should show xe or i915
```

If missing, ensure the `lxc.cgroup2.devices.allow` and `lxc.mount.entry` lines
are correct in `/etc/pve/lxc/100.conf`.

### seatd can't manage the seat

seatd needs to see the DRM device and TTY. If it fails:

```bash
# Try running seatd in debug mode
seatd -l debug

# Alternative: use logind instead of seatd
apk add elogind
rc-update add elogind default
rc-update del seatd default
```

### No keyboard/mouse input

Ensure `/dev/input` is mounted in the CT config and the user is in the `input` group.

### Falls back to software rendering

Check Mesa driver is loading:

```bash
WAYLAND_DISPLAY= DISPLAY= glxinfo -B 2>/dev/null | grep "OpenGL renderer"
# Or inside a Wayland session:
EGL_LOG_LEVEL=debug cosmic-comp 2>&1 | head -20
```

### If DRM approach fails entirely

**Fallback A:** Install Alpine directly on the USB SSD (no Proxmox, ext4 or btrfs).
Same COSMIC setup, just not in a container. Loses PBS backup but guaranteed to work.

**Fallback B:** Run the CT in nested Wayland mode instead. Install a minimal Wayland
compositor on the Proxmox host (e.g. `cage` or `labwc`) that does nothing but display
the CT's cosmic-comp as a fullscreen Wayland client. Indirect but avoids DRM issues.

---

## Reference: Incus → Proxmox Config Translation

| Incus | Proxmox (`/etc/pve/lxc/100.conf`) |
|-------|-----------------------------------|
| `type: gpu` | `lxc.cgroup2.devices.allow: c 226:* rwm` + mount entry |
| `raw.idmap: both 1001 1001` | Not needed (privileged CT, UIDs match directly) |
| `shift: true` | Not needed (privileged CT) |
| `type: disk, source: /path` | `mp0: /path,mp=/mount/point` |
| `security.privileged: true` | `unprivileged: 0` |

---

## Timeline Estimate

- Phase 1 (Proxmox install): ~30 min
- Phase 2 (CT creation + config): ~15 min
- Phase 3 (Alpine + COSMIC + apps): ~30 min
- Phase 4 (restore environment): ~15 min
- Troubleshooting seatd/DRM: 0–60 min (the unknown)

Total: 1.5–2.5 hours, depending on seatd cooperation.
