#!/bin/bash
# Spawn a new cosmixos mesh node as a local Incus CT
#
# Usage:
#   mesh-spawn.sh                    Auto-name, auto-provision
#   mesh-spawn.sh --name mynode      Custom name
#   mesh-spawn.sh --image IMAGE      Use specific image alias (default: latest cosmixos-mesh-*)
#
# What happens:
#   1. Creates an Incus CT from the cosmixos image
#   2. Starts it — local.d/cosmixos-setup.start runs on first boot
#   3. cosmixos-setup auto-provisions: users, PG, WG key, hub registration, mesh join
#
# The node is fully autonomous after boot — no further intervention needed.
set -euo pipefail

NAME=""
IMAGE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --name)  shift; NAME="$1" ;;
        --image) shift; IMAGE="$1" ;;
        --help|-h)
            echo "Usage: $0 [--name NAME] [--image IMAGE]"
            echo ""
            echo "Spawn a new cosmixos mesh node as a local Incus CT."
            echo "Default: auto-generated name, latest cosmixos-mesh image."
            exit 0 ;;
        *) echo "Unknown option: $1 (try --help)"; exit 1 ;;
    esac
    shift
done

# Find the latest cosmixos mesh image if not specified
if [ -z "$IMAGE" ]; then
    IMAGE=$(incus image list --format csv -c l | grep "^cosmixos-mesh" | sort -r | head -1)
    if [ -z "$IMAGE" ]; then
        echo "ERROR: No cosmixos-mesh-* image found."
        echo "Build one first: scripts/build-image.sh mesh"
        exit 1
    fi
fi

# Generate a CT name if not specified
if [ -z "$NAME" ]; then
    # Use a short random suffix
    SUFFIX=$(head -c4 /dev/urandom | xxd -p | head -c5)
    NAME="cosmixos-n${SUFFIX}"
fi

echo "=== Spawning mesh node ==="
echo "  CT name: $NAME"
echo "  Image:   $IMAGE"
echo ""

# Create and start
incus launch "$IMAGE" "$NAME"

echo ""
echo "  CT '$NAME' is starting."
echo "  First-boot provisioning runs automatically via local.d."
echo ""
echo "  Monitor: incus exec $NAME -- tail -f /var/log/cosmixos-setup.log"
echo "  Shell:   incus exec $NAME -- bash"
echo "  Status:  incus exec $NAME -- wg show wg0"
echo ""
echo "  To destroy: scripts/mesh-destroy.sh $NAME"
