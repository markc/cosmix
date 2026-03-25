#!/bin/bash
# Deploy cosmix binaries from Alpine build to mesh nodes.
#
# Usage: ./scripts/deploy.sh [nodes...]
#   No args = deploy to all mesh nodes (gcwg mko mmc)
#   ./scripts/deploy.sh gcwg mko   — deploy to specific nodes
#
# Must run on cosmixos (or wherever build-alpine.sh runs).
# Cleans changed crates first to work around ZFS bind mount fingerprint caching.
set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/alpine}"
REL="$CARGO_TARGET_DIR/$TARGET/release"
DEST="/usr/local/bin"

NODES=("${@:-gcwg mko mmc}")
if [ $# -eq 0 ]; then
    NODES=(gcwg mko mmc)
fi

BINS=(cosmix cosmix-web)

echo "=== Clean changed crates (ZFS bind mount workaround) ==="
cargo clean -p cosmix-daemon -p cosmix-web 2>/dev/null || true

echo ""
echo "=== Building headless binaries ==="
./scripts/build-alpine.sh headless --strip

echo ""
echo "=== Build results ==="
for bin in "${BINS[@]}"; do
    if [ -f "$REL/$bin" ]; then
        sz=$(du -h "$REL/$bin" | cut -f1)
        echo "  $bin  $sz"
    else
        echo "  $bin  MISSING — aborting"
        exit 1
    fi
done

echo ""
for node in "${NODES[@]}"; do
    echo "=== Deploying to $node ==="
    scp -q "${BINS[@]/#/$REL/}" "$node:/tmp/" || { echo "  scp failed — skipping $node"; continue; }
    ssh "$node" "
        systemctl stop cosmix cosmix-web 2>/dev/null
        cp ${BINS[*]/#//tmp/} $DEST/
        chmod 755 ${BINS[*]/#/$DEST/}
        systemctl start cosmix cosmix-web 2>/dev/null
        echo '  started services'
    " 2>&1
    echo "  done"
done

echo ""
echo "=== Verify ==="
for node in "${NODES[@]}"; do
    status=$(ssh "$node" "systemctl is-active cosmix-web 2>&1" 2>&1)
    version=$(ssh "$node" "cosmix-web --help 2>&1 | head -1" 2>&1)
    echo "  $node: cosmix-web=$status  ($version)"
done
