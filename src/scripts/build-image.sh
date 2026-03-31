#!/bin/bash
# Build CosmixOS container images from the cosmixos CT
#
# Usage:
#   ./scripts/build-image.sh [mesh|desktop|both] [--snapshot NAME] [--ct NAME]
#
# The rootfs tarball is the universal artifact — platform adapters (Incus,
# Proxmox, BinaryLane) import it in their native format.
#
# Prereqs:
#   - Source CT exists with at least one snapshot
#   - Binaries already built (run build-alpine.sh inside CT first)
set -euo pipefail

VARIANT=""
DIST_DIR="dist"
SNAPSHOT=""
SOURCE_CT="cosmixos"
DATE=$(date +%Y%m%d)

# Parse args
while [ $# -gt 0 ]; do
    case "$1" in
        mesh|desktop|both) VARIANT="$1" ;;
        --snapshot) shift; SNAPSHOT="$1" ;;
        --ct)       shift; SOURCE_CT="$1" ;;
        --help|-h)
            echo "Usage: $0 [mesh|desktop|both] [--snapshot NAME] [--ct NAME]"
            echo ""
            echo "Options:"
            echo "  mesh|desktop|both  Variant to build (default: both)"
            echo "  --snapshot NAME    Base snapshot (default: latest in CT)"
            echo "  --ct NAME          Source CT name (default: cosmixos)"
            exit 0 ;;
        *) echo "Unknown option: $1 (try --help)"; exit 1 ;;
    esac
    shift
done

VARIANT="${VARIANT:-both}"

# Auto-detect snapshot if not specified: use the latest one
if [ -z "$SNAPSHOT" ]; then
    SNAPSHOT=$(incus snapshot list "$SOURCE_CT" --format csv -c n | tail -1)
    if [ -z "$SNAPSHOT" ]; then
        echo "ERROR: No snapshots found for CT '$SOURCE_CT'"
        echo "Create one first: incus snapshot create $SOURCE_CT base-cosmix-1.0"
        exit 1
    fi
    echo "Using latest snapshot: $SNAPSHOT"
fi

mkdir -p "$DIST_DIR"

# Binaries to include per variant
MESH_BINS=(cosmix-webdcosmix-portd cosmix-indexd cosmix-maild)
DESKTOP_BINS=(cosmix cosmix-webdcosmix-portd cosmix-indexd cosmix-maild
              cosmix-calc cosmix-view cosmix-mail)
# cosmix-toot excluded: keytar→libsecret→gmodule→dlopen incompatible with static linking

build_variant() {
    local name="$1"
    shift
    local bins=("$@")
    local ct="cosmixos-${name}-build"
    local image_name="cosmixos-${name}-${DATE}"

    echo "=== Building ${name} image ==="

    # Create CT from snapshot (inherits br0 from default profile)
    echo "  Creating CT from snapshot ${SNAPSHOT}..."
    incus copy "${SOURCE_CT}/${SNAPSHOT}" "$ct"
    incus start "$ct"

    # Wait for CT to be ready
    sleep 2

    # Install binaries (already stripped in source CT)
    local bin_dir="/home/cosmix/.gh/cosmix/target/alpine/x86_64-unknown-linux-musl/release"
    echo "  Installing binaries from ${bin_dir}..."
    for bin in "${bins[@]}"; do
        local src="${bin_dir}/${bin}"
        if incus exec "${SOURCE_CT}" -- test -f "$src" 2>/dev/null; then
            # Pull from source CT, push to build CT
            incus file pull "${SOURCE_CT}${src}" "/tmp/${bin}"
            incus file push "/tmp/${bin}" "${ct}/usr/local/bin/${bin}"
            incus exec "$ct" -- chmod +x "/usr/local/bin/${bin}"
            rm -f "/tmp/${bin}"
            echo "    installed ${bin}"
        else
            echo "    WARNING: ${bin} not found"
        fi
    done

    # Install OpenRC service files (from host repo)
    local repo_dir
    repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
    echo "  Installing service files..."
    for svc in cosmix cosmix-webdcosmix-portd; do
        local svc_src="${repo_dir}/scripts/etc/init.d/${svc}"
        if [ -f "$svc_src" ]; then
            incus file push "$svc_src" "${ct}/etc/init.d/${svc}"
            incus exec "$ct" -- chmod +x "/etc/init.d/${svc}"
            echo "    service: ${svc}"
        fi
    done

    # Install first-boot setup script (from host repo)
    local setup_src="${repo_dir}/scripts/cosmixos-setup.sh"
    if [ -f "$setup_src" ]; then
        incus file push "$setup_src" "${ct}/usr/local/bin/cosmixos-setup"
        incus exec "$ct" -- chmod +x /usr/local/bin/cosmixos-setup
        echo "    setup: cosmixos-setup"
    fi

    # Install local.d auto-start (runs cosmixos-setup on first boot)
    local locald_src="${repo_dir}/scripts/etc/local.d/cosmixos-setup.start"
    if [ -f "$locald_src" ]; then
        incus exec "$ct" -- mkdir -p /etc/local.d
        incus file push "$locald_src" "${ct}/etc/local.d/cosmixos-setup.start"
        incus exec "$ct" -- chmod +x /etc/local.d/cosmixos-setup.start
        incus exec "$ct" -- rc-update add local default 2>/dev/null || true
        echo "    auto-start: local.d/cosmixos-setup.start"
    fi

    # Variant-specific cleanup
    if [[ "$name" == "mesh" ]]; then
        echo "  Removing desktop packages for mesh variant..."
        incus exec "$ct" -- sh -c '
            apk del cosmic-comp cosmic-applets cosmic-bg cosmic-files \
                cosmic-greeter cosmic-launcher cosmic-notifications \
                cosmic-osd cosmic-panel cosmic-randr cosmic-screenshot \
                cosmic-session cosmic-settings cosmic-store cosmic-term \
                cosmic-workspaces 2>/dev/null || true
            apk del mesa-dri-gallium mesa-va-gallium 2>/dev/null || true
        '
    fi

    if [[ "$name" == "desktop" ]]; then
        echo "  Installing build deps for self-rebuild..."
        incus exec "$ct" -- apk add git build-base pkgconf \
            openssl-dev openssl-libs-static linux-headers cmake perl \
            mesa-dev wayland-dev libxkbcommon-dev libxkbcommon-static \
            fontconfig-dev libinput-dev eudev-dev \
            lua5.4-dev font-dejavu

        echo "  Installing rustup + musl target..."
        incus exec "$ct" -- su - cosmix -c '
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            source ~/.cargo/env
            rustup target add x86_64-unknown-linux-musl
        '

        echo "  Cloning source..."
        incus exec "$ct" -- su - cosmix -c '
            mkdir -p ~/.gh
            cd ~/.gh
            git clone https://github.com/markc/cosmix.git 2>/dev/null || true
        '
    fi

    # Essentials: shell, SSH, WireGuard, tools
    echo "  Installing essentials..."
    incus exec "$ct" -- apk add bash openssh openssh-client nano rsync wireguard-tools

    # Fix root shell and environment (Alpine-specific)
    echo "  Fixing shell environment..."
    incus exec "$ct" -- sh -c '
        # Root shell → bash
        sed -i "s|root:x:0:0:root:/root:/bin/sh|root:x:0:0:root:/root:/bin/bash|" /etc/passwd
        # $SHELL export (Alpine busybox login does not set it from /etc/passwd)
        grep -q "export SHELL=/bin/bash" /etc/profile || echo "export SHELL=/bin/bash" >> /etc/profile
        # Hush Alpine MOTD
        touch /root/.hushlogin
        # Enable sshd at boot
        rc-update add sshd default
    '

    # Install mesh registration SSH key (for auto-provisioning via gw hub)
    echo "  Installing mesh registration key..."
    incus exec "$ct" -- mkdir -p /root/.ssh
    incus exec "$ct" -- chmod 700 /root/.ssh

    local repo_dir_abs
    repo_dir_abs="$(cd "$(dirname "$0")/.." && pwd)"

    # Push the mesh-register key pair (restricted: can only run mesh-hub.sh on gw)
    local key_dir="${HOME}/.ssh/keys"
    if [ -f "${key_dir}/mesh-register" ]; then
        incus file push "${key_dir}/mesh-register" "${ct}/root/.ssh/id_ed25519"
        incus file push "${key_dir}/mesh-register.pub" "${ct}/root/.ssh/id_ed25519.pub"
        incus exec "$ct" -- chmod 600 /root/.ssh/id_ed25519
        # SSH config for gw hub (no mux — each registration is a one-shot)
        incus exec "$ct" -- sh -c 'cat > /root/.ssh/config <<SSHEOF
Host gw
    Hostname 192.168.1.1
    User root
    IdentityFile ~/.ssh/id_ed25519
    ControlMaster no
    ControlPath none
    StrictHostKeyChecking accept-new
SSHEOF
chmod 600 /root/.ssh/config'
        echo "    mesh key: mesh-register → /root/.ssh/id_ed25519"
    else
        echo "    WARNING: ${key_dir}/mesh-register not found — auto-provisioning will need manual key push"
    fi

    # Clean up
    echo "  Cleaning caches and build artifacts..."
    incus exec "$ct" -- sh -c '
        rm -rf /var/cache/apk/*
        rm -rf /tmp/*
        rm -rf /home/cosmix/.gh/cosmix/target 2>/dev/null || true
        rm -rf /home/cosmix/.cargo/registry/cache 2>/dev/null || true
    '

    # Stop and publish
    incus stop "$ct"

    echo "  Publishing image..."
    incus publish "$ct" --alias "$image_name" \
        description="CosmixOS ${name} ${DATE}"

    echo "  Exporting..."
    incus image export "$image_name" "${DIST_DIR}/${image_name}"

    # Clean up build CT
    incus delete "$ct"

    echo "  Image: ${DIST_DIR}/${image_name}.tar.gz"

    # Extract rootfs for Proxmox compatibility
    echo "  Extracting rootfs.tar.xz for Proxmox..."
    cd "$DIST_DIR"
    if tar tf "${image_name}.tar.gz" rootfs.tar.xz >/dev/null 2>&1; then
        tar xf "${image_name}.tar.gz" rootfs.tar.xz
        mv rootfs.tar.xz "${image_name}-rootfs.tar.xz"
        echo "  Proxmox rootfs: ${DIST_DIR}/${image_name}-rootfs.tar.xz"
    else
        echo "  Note: unified tarball format — use directly with Proxmox"
    fi
    cd ..

    echo "  Done: ${name}"
    echo ""
}

case "$VARIANT" in
    mesh)
        build_variant "mesh" "${MESH_BINS[@]}"
        ;;
    desktop)
        build_variant "desktop" "${DESKTOP_BINS[@]}"
        ;;
    both)
        build_variant "mesh" "${MESH_BINS[@]}"
        build_variant "desktop" "${DESKTOP_BINS[@]}"
        ;;
    *)
        echo "Usage: $0 [mesh|desktop|both]"
        exit 1
        ;;
esac

# Generate checksums
echo "=== Generating checksums ==="
cd "$DIST_DIR"
sha256sum *.tar.* > SHA256SUMS 2>/dev/null || true
cat SHA256SUMS
cd ..

echo ""
echo "All images built in ${DIST_DIR}/"
