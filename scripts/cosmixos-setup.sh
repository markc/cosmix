#!/bin/sh
# CosmixOS first-boot setup — auto-configures users, PostgreSQL, hostname, WireGuard, and mesh
#
# Two modes:
#   cosmixos-setup              → fully automatic (shortname, auto-assign IP via hub)
#   cosmixos-setup --manual     → interactive (prompts for name and IP)
#   cosmixos-setup --name foo   → automatic with custom name
#
# NS 3.0 UID convention:
#   0     root     system
#   1000  sysadm   admin, sudo, SSH, desktop login
#   1001  cosmix   service user, runs all cosmix daemons
#   1002+ u1002    per-vhost users
#
# Requires: wg, curl (for hub registration), ssh (fallback)
set -eu

WG_DIR="/etc/wireguard"
WG_CONF="$WG_DIR/wg0.conf"

# Mesh hub — the gw router that manages WireGuard peers
# Registration happens via SSH to the hub running mesh-hub.sh
HUB_HOST="gw"
HUB_SCRIPT="/etc/wireguard/mesh-hub.sh"
HUB_PUBKEY="e0z57kEnP17lmQhCSvuCxjR4sKx4mKyiEH0qNT9CjW8="
HUB_ENDPOINT="192.168.1.1:51822"

# --- Shortname: deterministic hostname from MAC address ---
shortname() {
    local mac
    mac=$(cat /sys/class/net/e*/address 2>/dev/null | head -1 | tr -d :)
    if [ -z "$mac" ]; then
        # Fallback: use hostname or random
        mac=$(hostname | md5sum | head -c12)
    fi
    echo "n$(echo "$mac" | head -c12 | tail -c5)"
}

# --- Parse args ---
MODE="auto"
CUSTOM_NAME=""
CUSTOM_IP=""
FORCE=false

while [ $# -gt 0 ]; do
    case "$1" in
        --manual)  MODE="manual" ;;
        --force)   FORCE=true ;;
        --name)    shift; CUSTOM_NAME="$1" ;;
        --ip)      shift; CUSTOM_IP="$1" ;;
        --help|-h) echo "Usage: $0 [--manual] [--force] [--name NAME] [--ip IP]"
                   echo ""
                   echo "Default: auto-provision hostname (MAC-derived) and mesh IP (via hub)"
                   echo ""
                   echo "Options:"
                   echo "  --manual    Interactive mode (prompts for name and IP)"
                   echo "  --force     Reconfigure even if already set up"
                   echo "  --name NAME Override auto-generated hostname"
                   echo "  --ip IP     Override hub-assigned mesh IP"
                   exit 0 ;;
        *)         echo "Unknown option: $1 (try --help)"; exit 1 ;;
    esac
    shift
done

echo "=== CosmixOS Setup ==="
echo ""

# Check if already configured
if [ -f "$WG_CONF" ] && [ "$FORCE" = false ]; then
    CURRENT_IP=$(grep "^Address" "$WG_CONF" | sed 's/Address = //' | sed 's|/.*||')
    CURRENT_NAME=$(hostname)
    echo "Already configured:"
    echo "  Hostname: $CURRENT_NAME"
    echo "  Mesh IP:  $CURRENT_IP"
    echo ""
    echo "Run with --force to reconfigure."
    exit 0
fi

# =====================================================================
# NS 3.0 User Provisioning (requires root)
# =====================================================================

if [ "$(id -u)" -ne 0 ]; then
    echo "--- Skipping user provisioning and PG bootstrap (not root) ---"
    echo "  Run as root for full setup, or continue for WireGuard-only config."
    echo ""
else

echo "--- User provisioning (NS 3.0) ---"

# Helper: create user if not exists, with specific UID
ensure_user() {
    local username="$1" uid="$2" shell="$3" home="$4" comment="$5"
    local gid="$uid"

    if id "$username" >/dev/null 2>&1; then
        echo "  $username already exists ($(id $username))"
        return 0
    fi

    # Check if UID is already taken by another user
    if getent passwd "$uid" >/dev/null 2>&1; then
        local existing
        existing=$(getent passwd "$uid" | cut -d: -f1)
        echo "  WARNING: UID $uid already used by '$existing' — skipping $username"
        echo "  Fix manually: userdel $existing, then re-run"
        return 1
    fi

    # Create group first
    if ! getent group "$username" >/dev/null 2>&1; then
        addgroup -g "$gid" "$username"
    fi

    # Create user
    adduser -D -u "$uid" -G "$username" -h "$home" -s "$shell" -g "$comment" "$username"
    echo "  created $username ($uid:$gid) home=$home"
}

# sysadm: admin user with sudo
ensure_user "sysadm" 1000 "/bin/bash" "/home/sysadm" "System Admin"
if id sysadm >/dev/null 2>&1; then
    # Add to sudo/wheel group
    addgroup sysadm wheel 2>/dev/null || true
    # Ensure wheel has sudo access
    if [ -d /etc/sudoers.d ]; then
        echo '%wheel ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/wheel
        chmod 440 /etc/sudoers.d/wheel
    fi
    # SSH authorized_keys from root (if root has keys, sysadm should too)
    if [ -f /root/.ssh/authorized_keys ] && [ ! -f /home/sysadm/.ssh/authorized_keys ]; then
        mkdir -p /home/sysadm/.ssh
        cp /root/.ssh/authorized_keys /home/sysadm/.ssh/
        chown -R sysadm:sysadm /home/sysadm/.ssh
        chmod 700 /home/sysadm/.ssh
        chmod 600 /home/sysadm/.ssh/authorized_keys
        echo "  copied SSH keys to sysadm"
    fi
fi

# cosmix: service user with sudo for dev/admin access
ensure_user "cosmix" 1001 "/bin/bash" "/home/cosmix" "Cosmix Service"
if id cosmix >/dev/null 2>&1; then
    # sudo via wheel group
    addgroup cosmix wheel 2>/dev/null || true
    # utmp for writing login records (wtmp/btmp/lastlog)
    addgroup cosmix utmp 2>/dev/null || true
    # Create runtime directories
    install -d -o cosmix -g cosmix -m 755 /run/cosmix
    install -d -o cosmix -g cosmix -m 755 /var/log/cosmix
    # Config directory
    install -d -o cosmix -g cosmix -m 700 /home/cosmix/.config/cosmix
    # SSH authorized_keys from root (same as sysadm)
    if [ -f /root/.ssh/authorized_keys ] && [ ! -f /home/cosmix/.ssh/authorized_keys ]; then
        mkdir -p /home/cosmix/.ssh
        cp /root/.ssh/authorized_keys /home/cosmix/.ssh/
        chown -R cosmix:cosmix /home/cosmix/.ssh
        chmod 700 /home/cosmix/.ssh
        chmod 600 /home/cosmix/.ssh/authorized_keys
        echo "  copied SSH keys to cosmix"
    fi
fi

echo ""

# =====================================================================
# PostgreSQL Bootstrap
# =====================================================================

echo "--- PostgreSQL bootstrap ---"

# Check if PostgreSQL is installed and running
if ! command -v psql >/dev/null 2>&1; then
    echo "  PostgreSQL not installed — installing..."
    apk add postgresql postgresql-client 2>/dev/null || true
fi

# Initialize PG data dir if needed (Alpine-specific)
if [ ! -d /var/lib/postgresql/data ] || [ -z "$(ls -A /var/lib/postgresql/data 2>/dev/null)" ]; then
    echo "  Initializing PostgreSQL..."
    # Alpine's postgresql package expects this
    mkdir -p /var/lib/postgresql/data
    chown postgres:postgres /var/lib/postgresql/data
    su - postgres -c "initdb -D /var/lib/postgresql/data --auth-local=peer --auth-host=md5" 2>/dev/null || true
fi

# Ensure PG is running
rc-update add postgresql default 2>/dev/null || true
if ! rc-service postgresql status >/dev/null 2>&1; then
    rc-service postgresql start
    sleep 1
fi

# Create cosmix PG role (login, createdb — owns the cosmix database)
if su - postgres -c "psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname='cosmix'\"" | grep -q 1; then
    echo "  PG role 'cosmix' exists"
else
    su - postgres -c "createuser -l cosmix"
    su - postgres -c "psql -c \"ALTER ROLE cosmix WITH PASSWORD 'cosmix'\""
    echo "  created PG role: cosmix"
fi

# Create cosmix database
if su - postgres -c "psql -tAc \"SELECT 1 FROM pg_database WHERE datname='cosmix'\"" | grep -q 1; then
    echo "  database 'cosmix' exists"
else
    su - postgres -c "createdb -O cosmix cosmix"
    echo "  created database: cosmix (owner: cosmix)"
fi

# Enable pgvector extension (if available)
su - postgres -c "psql -d cosmix -c 'CREATE EXTENSION IF NOT EXISTS vector'" 2>/dev/null && \
    echo "  pgvector extension enabled" || \
    echo "  pgvector not available (install postgresql-pgvector for memory/embeddings)"

# Apply schemas as cosmix role
echo "  Applying schemas..."

# Users table (cosmix-web bootstrap)
su - postgres -c "psql -d cosmix" <<'SCHEMA'
CREATE TABLE IF NOT EXISTS users (
    id          SERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    email       TEXT UNIQUE NOT NULL,
    password    TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'user',
    created_at  TIMESTAMPTZ DEFAULT NOW(),
    updated_at  TIMESTAMPTZ DEFAULT NOW()
);
SCHEMA
echo "    users table"

# Mesh tables
su - postgres -c "psql -d cosmix" <<'SCHEMA'
CREATE TABLE IF NOT EXISTS mesh_nodes (
    name        TEXT PRIMARY KEY,
    public_key  TEXT NOT NULL,
    mesh_ip     INET NOT NULL UNIQUE,
    endpoint    TEXT,
    role        TEXT NOT NULL DEFAULT 'server',
    version     TEXT,
    services    JSONB DEFAULT '[]',
    last_seen   TIMESTAMPTZ,
    created_at  TIMESTAMPTZ DEFAULT now(),
    updated_at  TIMESTAMPTZ DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mesh_services (
    node        TEXT NOT NULL REFERENCES mesh_nodes(name) ON DELETE CASCADE,
    service     TEXT NOT NULL,
    host        TEXT NOT NULL,
    port        INTEGER NOT NULL,
    priority    SMALLINT DEFAULT 10,
    weight      SMALLINT DEFAULT 0,
    metadata    JSONB DEFAULT '{}',
    last_seen   TIMESTAMPTZ DEFAULT now(),
    PRIMARY KEY (node, service)
);

CREATE TABLE IF NOT EXISTS mesh_ipam (
    mesh_ip     INET PRIMARY KEY,
    node        TEXT REFERENCES mesh_nodes(name) ON DELETE SET NULL,
    status      TEXT NOT NULL DEFAULT 'allocated',
    allocated_at TIMESTAMPTZ DEFAULT now(),
    note        TEXT
);

DO $$ BEGIN ALTER TABLE mesh_nodes ADD COLUMN web_url TEXT; EXCEPTION WHEN duplicate_column THEN NULL; END $$;
DO $$ BEGIN ALTER TABLE mesh_nodes ADD COLUMN admin_password_enc TEXT; EXCEPTION WHEN duplicate_column THEN NULL; END $$;

CREATE INDEX IF NOT EXISTS mesh_nodes_last_seen_idx ON mesh_nodes(last_seen);
CREATE INDEX IF NOT EXISTS mesh_services_service_idx ON mesh_services(service);
SCHEMA
echo "    mesh tables"

# Memory tables (only if pgvector is available)
if su - postgres -c "psql -d cosmix -tAc \"SELECT 1 FROM pg_extension WHERE extname='vector'\"" 2>/dev/null | grep -q 1; then
    su - postgres -c "psql -d cosmix" <<'SCHEMA'
CREATE TABLE IF NOT EXISTS memory_chunks (
    id          BIGSERIAL PRIMARY KEY,
    session_id  TEXT,
    source      TEXT,
    content     TEXT NOT NULL,
    summary     TEXT,
    embedding   vector(768),
    metadata    JSONB DEFAULT '{}',
    created_at  TIMESTAMPTZ DEFAULT NOW(),
    accessed_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS task_log (
    id          BIGSERIAL PRIMARY KEY,
    task        TEXT NOT NULL,
    outcome     TEXT,
    agent       TEXT,
    tokens_used INTEGER DEFAULT 0,
    duration_ms INTEGER,
    metadata    JSONB DEFAULT '{}',
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS file_index (
    id          BIGSERIAL PRIMARY KEY,
    filepath    TEXT UNIQUE NOT NULL,
    content_hash TEXT,
    summary     TEXT,
    embedding   vector(768),
    last_indexed TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS memory_chunks_embedding_idx
    ON memory_chunks USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);

CREATE INDEX IF NOT EXISTS file_index_embedding_idx
    ON file_index USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);
SCHEMA
    echo "    memory tables (pgvector)"
fi

# Grant all privileges on cosmix DB objects to cosmix role
su - postgres -c "psql -d cosmix -c 'GRANT ALL ON ALL TABLES IN SCHEMA public TO cosmix'"
su - postgres -c "psql -d cosmix -c 'GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO cosmix'"
su - postgres -c "psql -d cosmix -c 'ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO cosmix'"
su - postgres -c "psql -d cosmix -c 'ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON SEQUENCES TO cosmix'"
echo "  grants applied"

# Configure pg_hba.conf for cosmix local+host auth
PG_HBA="/var/lib/postgresql/data/pg_hba.conf"
if [ -f "$PG_HBA" ]; then
    if ! grep -q "cosmix" "$PG_HBA"; then
        # Add before the first non-comment line
        sed -i '/^local/i \
# cosmix service user — peer for local, md5 for TCP\
local   cosmix      cosmix                          peer\
host    cosmix      cosmix      127.0.0.1/32        md5\
host    cosmix      cosmix      ::1/128             md5' "$PG_HBA"
        # Reload PG to pick up hba changes
        rc-service postgresql reload 2>/dev/null || true
        echo "  pg_hba.conf updated"
    fi
fi

echo ""

fi  # end root-only section

# =====================================================================
# Hostname + WireGuard (original setup)
# =====================================================================

# --- Determine node name ---
if [ "$MODE" = "manual" ]; then
    printf "Node name (e.g. mynode): "
    read -r NODE_NAME
    [ -z "$NODE_NAME" ] && echo "Error: node name required" && exit 1
elif [ -n "$CUSTOM_NAME" ]; then
    NODE_NAME="$CUSTOM_NAME"
else
    NODE_NAME=$(shortname)
    echo "Auto-assigned hostname: $NODE_NAME"
fi

# --- Generate WireGuard keys ---
mkdir -p "$WG_DIR"
chmod 700 "$WG_DIR"

PRIVATE_KEY=$(wg genkey)
PUBLIC_KEY=$(echo "$PRIVATE_KEY" | wg pubkey)

# --- Get mesh IP ---
if [ -n "$CUSTOM_IP" ]; then
    MESH_IP="$CUSTOM_IP"
    echo "Using specified IP: $MESH_IP"
elif [ "$MODE" = "manual" ]; then
    printf "Mesh IP address (e.g. 172.16.2.20): "
    read -r MESH_IP
    [ -z "$MESH_IP" ] && echo "Error: mesh IP required" && exit 1
else
    # Auto-register with the hub
    echo "Registering with mesh hub ($HUB_HOST)..."
    MESH_IP=$(ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
        "$HUB_HOST" "register $PUBLIC_KEY $NODE_NAME" 2>/dev/null) || true

    if [ -z "$MESH_IP" ] || echo "$MESH_IP" | grep -q "ERROR"; then
        echo "WARNING: Could not reach mesh hub."
        echo "Falling back to manual IP assignment."
        printf "Mesh IP address (e.g. 172.16.2.20): "
        read -r MESH_IP
        [ -z "$MESH_IP" ] && echo "Error: mesh IP required" && exit 1
    else
        echo "Hub assigned IP: $MESH_IP"
    fi
fi

# --- Write WireGuard config ---
cat > "$WG_CONF" <<EOF
[Interface]
# ${NODE_NAME} on cosmix mesh (172.16.2.0/24)
Address = ${MESH_IP}/32
PrivateKey = ${PRIVATE_KEY}

[Peer]
# gw router wg1 (mesh hub)
PublicKey = ${HUB_PUBKEY}
Endpoint = ${HUB_ENDPOINT}
AllowedIPs = 172.16.2.0/24
PersistentKeepalive = 25
EOF

chmod 600 "$WG_CONF"

# --- Set hostname ---
echo "$NODE_NAME" > /etc/hostname
hostname "$NODE_NAME"

# --- Fix shell environment (Alpine-specific) ---
grep -q "export SHELL=/bin/bash" /etc/profile 2>/dev/null || \
    echo "export SHELL=/bin/bash" >> /etc/profile
sed -i 's|root:x:0:0:root:/root:/bin/sh|root:x:0:0:root:/root:/bin/bash|' /etc/passwd 2>/dev/null || true
touch /root/.hushlogin

# --- Enable services ---
rc-update add wg-quick.wg0 default 2>/dev/null || true

# Bring up WireGuard
wg-quick up wg0 2>/dev/null && echo "WireGuard interface up" || echo "WARNING: wg-quick up failed"

# Start cosmix services if present
for svc in cosmix-webd cosmix-portd cosmix-indexd cosmix-maild; do
    if [ -f "/etc/init.d/$svc" ]; then
        rc-service "$svc" start 2>/dev/null || true
    fi
done

echo ""
echo "=== Setup complete ==="
echo "  Hostname:   $NODE_NAME"
echo "  Mesh IP:    $MESH_IP"
echo "  Public key: $PUBLIC_KEY"
echo "  Hub:        $HUB_ENDPOINT"
echo "  Users:      sysadm (1000), cosmix (1001)"
echo "  Database:   cosmix (PostgreSQL)"
echo ""
echo "Verify: ping 172.16.2.4  (gcwg)"
echo "        ping 172.16.2.210 (mko)"
