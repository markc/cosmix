#!/bin/sh
# mesh-hub.sh — Mesh registration hub (runs on gw router or any node with SSH to gw)
#
# Manages the WireGuard peer registry for the cosmix mesh.
# Assigns IPs from 172.16.2.0/24, tracks nodes in a JSON registry.
#
# Usage:
#   mesh-hub.sh register <pubkey> <hostname>   → assigns IP, adds peer, prints IP
#   mesh-hub.sh list                           → show all registered nodes
#   mesh-hub.sh remove <hostname>              → remove a node
#   mesh-hub.sh next-ip                        → show next available IP
#
# The registry lives at /etc/wireguard/mesh-registry.json on the gw router.
# Format: {"nodes": [{"name":"n273f0","ip":"172.16.2.20","pubkey":"...","registered":"2026-03-15"}]}

set -eu

MESH_SUBNET="172.16.2"
MESH_CIDR="/32"
WG_IFACE="wg1"
WG_CONF="/etc/wireguard/${WG_IFACE}.conf"
REGISTRY="/etc/wireguard/mesh-registry.json"

# Reserved IPs (permanent nodes — never auto-assign these)
# 1=gw, 4=gcwg, 5=cachyos, 9=mmc, 210=mko
RESERVED="1 4 5 9 210"

# IP range for auto-assignment (avoid reserved, start high enough to not collide)
IP_RANGE_START=20
IP_RANGE_END=200

init_registry() {
    if [ ! -f "$REGISTRY" ] || [ ! -s "$REGISTRY" ]; then
        echo '{"nodes":[]}' > "$REGISTRY"
    fi
}

# Find next available IP in the auto-assign range
next_ip() {
    init_registry
    local ip=$IP_RANGE_START
    while [ $ip -le $IP_RANGE_END ]; do
        # Check if IP is reserved
        local skip=false
        for r in $RESERVED; do
            if [ "$ip" = "$r" ]; then
                skip=true
                break
            fi
        done
        if $skip; then
            ip=$((ip + 1))
            continue
        fi

        # Check if IP is in registry
        if ! grep -q "\"${MESH_SUBNET}.${ip}\"" "$REGISTRY" 2>/dev/null; then
            # Check if IP is in active WireGuard peers
            if ! wg show "$WG_IFACE" allowed-ips 2>/dev/null | grep -q "${MESH_SUBNET}.${ip}/"; then
                echo "${MESH_SUBNET}.${ip}"
                return 0
            fi
        fi
        ip=$((ip + 1))
    done
    echo "ERROR: no free IPs in range ${IP_RANGE_START}-${IP_RANGE_END}" >&2
    return 1
}

register() {
    local pubkey="$1"
    local hostname="$2"
    init_registry

    # Check if already registered (by hostname or pubkey)
    if grep -q "\"$hostname\"" "$REGISTRY" 2>/dev/null; then
        # Return existing IP
        local existing_ip
        existing_ip=$(sed -n "s/.*\"name\":\"${hostname}\",\"ip\":\"\([^\"]*\)\".*/\1/p" "$REGISTRY")
        if [ -n "$existing_ip" ]; then
            echo "$existing_ip"
            return 0
        fi
    fi

    # Assign next IP
    local ip
    ip=$(next_ip) || return 1

    # Add WireGuard peer
    wg set "$WG_IFACE" peer "$pubkey" allowed-ips "${ip}${MESH_CIDR}"

    # Persist to conf file
    cat >> "$WG_CONF" <<EOF

[Peer]
# ${hostname} (auto-registered)
PublicKey = ${pubkey}
AllowedIPs = ${ip}${MESH_CIDR}
EOF

    # Add to registry
    local date
    date=$(date -u +%Y-%m-%d)
    # Simple JSON append (no jq dependency)
    local tmp="${REGISTRY}.tmp"
    if [ "$(cat "$REGISTRY")" = '{"nodes":[]}' ]; then
        echo "{\"nodes\":[{\"name\":\"${hostname}\",\"ip\":\"${ip}\",\"pubkey\":\"${pubkey}\",\"registered\":\"${date}\"}]}" > "$tmp"
    else
        sed "s|\]}|,{\"name\":\"${hostname}\",\"ip\":\"${ip}\",\"pubkey\":\"${pubkey}\",\"registered\":\"${date}\"}]}|" "$REGISTRY" > "$tmp"
    fi
    mv "$tmp" "$REGISTRY"

    echo "$ip"
}

remove_node() {
    local hostname="$1"
    init_registry

    # Get pubkey from registry
    local pubkey
    pubkey=$(sed -n "s/.*\"name\":\"${hostname}\".*\"pubkey\":\"\([^\"]*\)\".*/\1/p" "$REGISTRY")

    if [ -z "$pubkey" ]; then
        echo "ERROR: node '$hostname' not found in registry" >&2
        return 1
    fi

    # Remove WireGuard peer
    wg set "$WG_IFACE" peer "$pubkey" remove

    # Remove from conf (remove the 3-line block)
    sed -i "/# ${hostname}/,+2d" "$WG_CONF"

    # Remove from registry
    # Simple approach: rebuild without the node
    local tmp="${REGISTRY}.tmp"
    grep -v "\"${hostname}\"" "$REGISTRY" | sed 's/,\]/]/' | sed 's/\[,/[/' > "$tmp"
    mv "$tmp" "$REGISTRY"

    echo "Removed ${hostname}"
}

list_nodes() {
    init_registry
    echo "=== Mesh Registry ==="
    echo ""
    printf "%-15s %-18s %-50s %s\n" "NAME" "IP" "PUBKEY" "REGISTERED"
    printf "%s\n" "$(printf '%.0s-' $(seq 1 95))"

    # Parse JSON manually (no jq)
    # Also show permanent nodes from wg show
    wg show "$WG_IFACE" allowed-ips 2>/dev/null | while read -r pubkey ips; do
        local ip
        ip=$(echo "$ips" | sed 's|/32||')
        local name="(unknown)"

        # Check registry
        local reg_name
        reg_name=$(sed -n "s/.*\"name\":\"\([^\"]*\)\",\"ip\":\"${ip}\".*/\1/p" "$REGISTRY" 2>/dev/null)
        if [ -n "$reg_name" ]; then
            name="$reg_name"
        else
            # Check well-known permanent nodes
            case "$ip" in
                ${MESH_SUBNET}.4)   name="gcwg" ;;
                ${MESH_SUBNET}.5)   name="cachyos" ;;
                ${MESH_SUBNET}.9)   name="mmc" ;;
                ${MESH_SUBNET}.210) name="mko" ;;
                ${MESH_SUBNET}.12)  name="mesh2" ;;
                ${MESH_SUBNET}.13)  name="mesh3" ;;
            esac
        fi

        local date=""
        local reg_date
        reg_date=$(sed -n "s/.*\"ip\":\"${ip}\".*\"registered\":\"\([^\"]*\)\".*/\1/p" "$REGISTRY" 2>/dev/null)
        [ -n "$reg_date" ] && date="$reg_date"

        local short_pub
        short_pub=$(echo "$pubkey" | cut -c1-20)...
        printf "%-15s %-18s %-50s %s\n" "$name" "$ip" "${short_pub}" "$date"
    done
}

# --- Main ---
case "${1:-help}" in
    register)
        [ $# -lt 3 ] && echo "Usage: mesh-hub.sh register <pubkey> <hostname>" >&2 && exit 1
        register "$2" "$3"
        ;;
    remove)
        [ $# -lt 2 ] && echo "Usage: mesh-hub.sh remove <hostname>" >&2 && exit 1
        remove_node "$2"
        ;;
    list)
        list_nodes
        ;;
    next-ip)
        next_ip
        ;;
    *)
        echo "mesh-hub.sh — Cosmix mesh WireGuard registry"
        echo ""
        echo "Usage:"
        echo "  mesh-hub.sh register <pubkey> <hostname>  — register node, assign IP"
        echo "  mesh-hub.sh list                          — show all nodes"
        echo "  mesh-hub.sh remove <hostname>             — remove a node"
        echo "  mesh-hub.sh next-ip                       — show next free IP"
        ;;
esac
