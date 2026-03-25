#!/bin/bash
# Cleanly remove a cosmixos mesh node (Incus CT)
#
# Usage:
#   mesh-destroy.sh <ct-name>           Remove CT and clean up mesh
#   mesh-destroy.sh <ct-name> --force   Skip confirmation
#
# What happens:
#   1. Reads the node's hostname and WG pubkey from inside the CT
#   2. Stops the CT
#   3. Removes WG peer from gw hub (mesh-hub.sh remove)
#   4. Removes node from PG mesh tables (on any node with PG access)
#   5. Deletes the Incus CT
set -euo pipefail

if [ $# -lt 1 ] || [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    echo "Usage: $0 <ct-name> [--force]"
    echo ""
    echo "Cleanly remove a cosmixos mesh node:"
    echo "  - Withdraws from mesh (WG peer, PG tables)"
    echo "  - Deletes the Incus CT"
    exit 0
fi

CT_NAME="$1"
FORCE=false
[ "${2:-}" = "--force" ] && FORCE=true

# Verify CT exists
if ! incus info "$CT_NAME" >/dev/null 2>&1; then
    echo "ERROR: CT '$CT_NAME' not found"
    incus list --format csv -c n | grep cosmixos || true
    exit 1
fi

# Get node info from inside the CT (if running)
NODE_NAME=""
MESH_IP=""
CT_STATUS=$(incus info "$CT_NAME" | grep "^Status:" | awk '{print $2}')

if [ "$CT_STATUS" = "RUNNING" ]; then
    NODE_NAME=$(incus exec "$CT_NAME" -- hostname 2>/dev/null || true)
    MESH_IP=$(incus exec "$CT_NAME" -- sh -c "grep '^Address' /etc/wireguard/wg0.conf 2>/dev/null | sed 's/Address = //' | sed 's|/.*||'" 2>/dev/null || true)
fi

echo "=== Destroying mesh node ==="
echo "  CT:       $CT_NAME"
echo "  Hostname: ${NODE_NAME:-unknown}"
echo "  Mesh IP:  ${MESH_IP:-unknown}"
echo ""

if [ "$FORCE" = false ]; then
    printf "Proceed? [y/N] "
    read -r CONFIRM
    if [ "$CONFIRM" != "y" ] && [ "$CONFIRM" != "Y" ]; then
        echo "Aborted."
        exit 0
    fi
fi

# 1. Graceful shutdown: tell cosmix daemon to broadcast node_withdraw
if [ "$CT_STATUS" = "RUNNING" ]; then
    echo "  Sending node_withdraw..."
    # If cosmix daemon is running, SIGTERM triggers node_withdraw broadcast
    incus exec "$CT_NAME" -- sh -c 'pkill -TERM cosmix 2>/dev/null; sleep 1' || true
fi

# 2. Remove WG peer from gw hub
if [ -n "$NODE_NAME" ]; then
    echo "  Removing from mesh hub (gw)..."
    ssh -o ConnectTimeout=5 gw "remove $NODE_NAME" 2>/dev/null && \
        echo "    hub: removed $NODE_NAME" || \
        echo "    hub: could not reach gw (clean up manually: ssh gw remove $NODE_NAME)"
fi

# 3. Remove from PG mesh tables (local PostgreSQL)
if [ -n "$NODE_NAME" ] && command -v psql >/dev/null 2>&1; then
    echo "  Removing from mesh tables..."
    psql -U cosmix -d cosmix -c "DELETE FROM mesh_services WHERE node = '$NODE_NAME'" 2>/dev/null || true
    psql -U cosmix -d cosmix -c "DELETE FROM mesh_ipam WHERE node = '$NODE_NAME'" 2>/dev/null || true
    psql -U cosmix -d cosmix -c "DELETE FROM mesh_nodes WHERE name = '$NODE_NAME'" 2>/dev/null || true
    echo "    PG: cleaned mesh_nodes, mesh_services, mesh_ipam"
fi

# 4. Stop and delete CT
echo "  Stopping CT..."
incus stop "$CT_NAME" --force 2>/dev/null || true

echo "  Deleting CT..."
incus delete "$CT_NAME"

echo ""
echo "  Node '$CT_NAME' (${NODE_NAME:-unknown}) destroyed."
echo "  Mesh IP ${MESH_IP:-unknown} is now free for reuse."
