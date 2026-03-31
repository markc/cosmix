#!/bin/bash
# Build all cosmix binaries natively on Alpine Edge (fully static musl)
# Usage: ./scripts/build-alpine.sh [headless|desktop|all] [--install] [--strip]
#
# Requires:
#   - rustup (NOT apk's rust) with x86_64-unknown-linux-musl target
#   - .cargo/config.toml with target-specific rustflags
#   - Static library packages: libxkbcommon-static, font-dejavu,
#     openssl-libs-static, libgcrypt-static, libgpg-error-static,
#     glib-static, libsecret-static, gettext-static, lua5.4-dev
#
# Each crate is built with -p to avoid workspace feature unification
# (mlua's luajit and lua54 features are mutually exclusive).
set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
DEST="$HOME/.local/bin"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/alpine}"
REL="$CARGO_TARGET_DIR/$TARGET/release"

HEADLESS_BINS=(cosmix cosmix-webdcosmix-portd cosmix-indexd cosmix-maild)
DESKTOP_BINS=(cosmix-calc cosmix-view cosmix-mail)
ALL_BINS=("${HEADLESS_BINS[@]}" "${DESKTOP_BINS[@]}")

TIER="${1:-all}"
INSTALL=false
STRIP=false

for arg in "$@"; do
    case "$arg" in
        --install) INSTALL=true ;;
        --strip)   STRIP=true ;;
        headless)  TIER=headless ;;
        desktop)   TIER=desktop ;;
        all)       TIER=all ;;
    esac
done

echo "=== Building on Alpine (static musl via --target $TARGET) ==="

if [[ "$TIER" == "headless" || "$TIER" == "all" ]]; then
    echo ""
    echo "--- Tier 1+2: Headless binaries ---"

    echo "  cosmix-indexd, cosmix-maild..."
    cargo build --release --target "$TARGET" -p cosmix-indexd -p cosmix-maild

    echo "  cosmix-webd(lua54)..."
    cargo build --release --target "$TARGET" -p cosmix-webd\
        --no-default-features --features lua54,cosmix

    echo "  cosmix-portd (lua54)..."
    cargo build --release --target "$TARGET" -p cosmix-portd \
        --no-default-features --features lua54

    echo ""
    echo "--- Tier 2: Headless daemon ---"
    echo "  cosmix (lua54, no desktop)..."
    cargo build --release --target "$TARGET" -p cosmix-daemon \
        --no-default-features --features lua54
fi

if [[ "$TIER" == "desktop" || "$TIER" == "all" ]]; then
    echo ""
    echo "--- Tier 3: GUI apps ---"
    echo "  cosmix-calc..."
    cargo build --release --target "$TARGET" -p cosmix-calc

    echo "  cosmix-view..."
    cargo build --release --target "$TARGET" -p cosmix-view

    echo "  cosmix-mail (lua54)..."
    cargo build --release --target "$TARGET" -p cosmix-mail \
        --no-default-features --features lua54

    # cosmix-toot: skipped — keytar/libsecret depends on glib/gmodule
    # which requires dlopen (incompatible with static linking).
    # Needs file-based credentials to replace keytar.
    # echo "  cosmix-toot..."
    # cargo build --release --target "$TARGET" -p cosmix-toot
fi

if $STRIP; then
    echo ""
    echo "=== Stripping binaries ==="
    for bin in "${ALL_BINS[@]}"; do
        [ -f "$REL/$bin" ] && strip "$REL/$bin" && echo "  stripped $bin"
    done
fi

echo ""
echo "=== Build summary ==="
for bin in "${ALL_BINS[@]}"; do
    if [ -f "$REL/$bin" ]; then
        sz=$(du -h "$REL/$bin" | cut -f1)
        link=$(ldd "$REL/$bin" 2>&1 | head -1)
        echo "  $bin  ${sz}  $link"
    else
        echo "  $bin  MISSING"
    fi
done

if $INSTALL; then
    echo ""
    echo "=== Installing to $DEST ==="
    mkdir -p "$DEST"
    for bin in "${ALL_BINS[@]}"; do
        [ -f "$REL/$bin" ] && cp "$REL/$bin" "$DEST/" && echo "  installed $bin"
    done
    echo "Done."
fi
