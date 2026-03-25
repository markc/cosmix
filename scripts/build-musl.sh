#!/bin/bash
# Cross-compile musl (static) binaries from a glibc host
# Usage: ./scripts/build-musl.sh [--install] [--strip]
#
# For native Alpine builds, use build-alpine.sh instead.
# This script cross-compiles headless binaries only — GUI apps need native Alpine.
set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
DEST="$HOME/.local/bin"
MUSL_DIR="target/$TARGET/release"
GLIBC_DIR="target/release"
RUSTFLAGS="${RUSTFLAGS:--C target-cpu=x86-64-v3}"
export RUSTFLAGS

INSTALL=false
STRIP=false

for arg in "$@"; do
    case "$arg" in
        --install) INSTALL=true ;;
        --strip)   STRIP=true ;;
    esac
done

echo "=== Building musl (static) binaries ==="
echo "  RUSTFLAGS: $RUSTFLAGS"

# Binaries that have no system deps — build directly
echo "  cosmix-embed, cosmix-jmap..."
cargo build --release --target "$TARGET" -p cosmix-embed -p cosmix-jmap

# Binaries that need Lua — use lua54 feature instead of luajit
echo "  cosmix-web (lua54)..."
cargo build --release --target "$TARGET" -p cosmix-web \
    --no-default-features --features lua54,cosmix

echo "  cosmix-portd (lua54)..."
cargo build --release --target "$TARGET" -p cosmix-portd \
    --no-default-features --features lua54

echo ""
echo "=== Building glibc (desktop) binaries ==="

# cosmix daemon — needs LuaJIT, Wayland, iced, zbus (desktop-only)
echo "  cosmix (daemon)..."
cargo build --release --bin cosmix

if $STRIP; then
    echo ""
    echo "=== Stripping binaries ==="
    for bin in cosmix-web cosmix-portd cosmix-embed cosmix-jmap; do
        [ -f "$MUSL_DIR/$bin" ] && strip "$MUSL_DIR/$bin" && echo "  stripped $bin (musl)"
    done
    [ -f "$GLIBC_DIR/cosmix" ] && strip "$GLIBC_DIR/cosmix" && echo "  stripped cosmix (glibc)"
fi

echo ""
echo "=== Build summary ==="
echo "Static (musl):"
for bin in cosmix-web cosmix-portd cosmix-embed cosmix-jmap; do
    if [ -f "$MUSL_DIR/$bin" ]; then
        sz=$(du -h "$MUSL_DIR/$bin" | cut -f1)
        echo "  $bin  ${sz}  $(file -b "$MUSL_DIR/$bin" | cut -d, -f1-2)"
    fi
done

echo ""
echo "Dynamic (glibc, desktop-only):"
for bin in cosmix; do
    if [ -f "$GLIBC_DIR/$bin" ]; then
        sz=$(du -h "$GLIBC_DIR/$bin" | cut -f1)
        echo "  $bin  ${sz}  $(file -b "$GLIBC_DIR/$bin" | cut -d, -f1-2)"
    fi
done

if $INSTALL; then
    echo ""
    echo "=== Installing to $DEST ==="
    mkdir -p "$DEST"

    # Install musl binaries
    for bin in cosmix-web cosmix-portd cosmix-embed cosmix-jmap; do
        [ -f "$MUSL_DIR/$bin" ] && cp "$MUSL_DIR/$bin" "$DEST/" && echo "  installed $bin"
    done

    # Install glibc binaries
    for bin in cosmix; do
        [ -f "$GLIBC_DIR/$bin" ] && cp "$GLIBC_DIR/$bin" "$DEST/" && echo "  installed $bin"
    done

    echo "Done."
fi
