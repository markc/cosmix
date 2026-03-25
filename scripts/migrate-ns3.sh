#!/bin/bash
# Migrate an existing cosmix node to NS 3.0 user convention
#
# What this does:
#   1. Creates sysadm (1000:1000) if missing
#   2. Creates or renames cosmic → cosmix (1001:1001)
#   3. Renames PG database markweb → cosmix (if markweb exists)
#   4. Creates/renames PG role to cosmix
#   5. Fixes file ownership and service files
#
# Usage:
#   migrate-ns3.sh --check      Dry run — show what would change
#   migrate-ns3.sh --apply      Apply changes (must run as root)
#
# IMPORTANT:
#   - Stop all cosmix services before running with --apply
#   - Snapshot/backup the node first
#   - Run on the target node, not remotely
#
# NS 3.0 UID convention:
#   0     root     system
#   1000  sysadm   admin, sudo, SSH, desktop login
#   1001  cosmix   service user, runs all cosmix daemons
set -euo pipefail

MODE="${1:---check}"

if [ "$MODE" != "--check" ] && [ "$MODE" != "--apply" ]; then
    echo "Usage: $0 [--check|--apply]"
    exit 1
fi

DRY_RUN=true
[ "$MODE" = "--apply" ] && DRY_RUN=false

if [ "$DRY_RUN" = false ] && [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: --apply must run as root"
    exit 1
fi

# Detect init system
INIT_SYSTEM="unknown"
if command -v systemctl >/dev/null 2>&1 && systemctl --version >/dev/null 2>&1; then
    INIT_SYSTEM="systemd"
elif command -v rc-service >/dev/null 2>&1; then
    INIT_SYSTEM="openrc"
fi

echo "=== NS 3.0 Migration ==="
echo "Mode: $([ "$DRY_RUN" = true ] && echo 'CHECK (dry run)' || echo 'APPLY')"
echo "Init: $INIT_SYSTEM"
echo "Host: $(hostname)"
echo ""

CHANGES=0

# Helper: log a planned/applied change
change() {
    CHANGES=$((CHANGES + 1))
    if [ "$DRY_RUN" = true ]; then
        echo "  WOULD: $*"
    else
        echo "  APPLY: $*"
    fi
}

# =====================================================================
# 1. User: sysadm (1000:1000)
# =====================================================================
echo "--- sysadm (1000:1000) ---"

if id sysadm >/dev/null 2>&1; then
    SYSADM_UID=$(id -u sysadm)
    if [ "$SYSADM_UID" -eq 1000 ]; then
        echo "  OK: sysadm exists with UID 1000"
    else
        echo "  WARNING: sysadm exists but has UID $SYSADM_UID (expected 1000)"
        echo "  Manual fix needed — cannot safely change UID of existing user"
    fi
else
    # Check what currently owns UID 1000
    CURRENT_1000=$(getent passwd 1000 2>/dev/null | cut -d: -f1 || true)
    if [ -n "$CURRENT_1000" ]; then
        echo "  UID 1000 currently used by: $CURRENT_1000"
        echo "  This user will NOT be touched — create sysadm with a different UID,"
        echo "  or rename $CURRENT_1000 → sysadm manually first."
        echo "  (On cachyos, cosmic=1000 stays as the desktop user — skip sysadm there)"
    else
        change "create sysadm (1000:1000)"
        if [ "$DRY_RUN" = false ]; then
            if command -v addgroup >/dev/null 2>&1; then
                # Alpine/BusyBox
                addgroup -g 1000 sysadm 2>/dev/null || true
                adduser -D -u 1000 -G sysadm -h /home/sysadm -s /bin/bash -g "System Admin" sysadm
            else
                # Debian/Arch — useradd
                groupadd -g 1000 sysadm 2>/dev/null || true
                useradd -u 1000 -g sysadm -m -d /home/sysadm -s /bin/bash -c "System Admin" sysadm
            fi
            # Add to sudo group
            if getent group wheel >/dev/null 2>&1; then
                usermod -aG wheel sysadm 2>/dev/null || addgroup sysadm wheel 2>/dev/null || true
            elif getent group sudo >/dev/null 2>&1; then
                usermod -aG sudo sysadm 2>/dev/null || true
            fi
            # Copy SSH keys from root
            if [ -f /root/.ssh/authorized_keys ]; then
                mkdir -p /home/sysadm/.ssh
                cp /root/.ssh/authorized_keys /home/sysadm/.ssh/
                chown -R sysadm:sysadm /home/sysadm/.ssh
                chmod 700 /home/sysadm/.ssh
                chmod 600 /home/sysadm/.ssh/authorized_keys
            fi
        fi
    fi
fi
echo ""

# =====================================================================
# 2. User: cosmix (1001:1001)
# =====================================================================
echo "--- cosmix (1001:1001) ---"

if id cosmix >/dev/null 2>&1; then
    COSMIX_UID=$(id -u cosmix)
    if [ "$COSMIX_UID" -eq 1001 ]; then
        echo "  OK: cosmix exists with UID 1001"
    else
        echo "  WARNING: cosmix exists but has UID $COSMIX_UID (expected 1001)"
        echo "  Manual fix needed"
    fi
elif id cosmic >/dev/null 2>&1; then
    COSMIC_UID=$(id -u cosmic)
    if [ "$COSMIC_UID" -eq 1001 ]; then
        change "rename user cosmic → cosmix (UID 1001 stays)"
        if [ "$DRY_RUN" = false ]; then
            # Rename the user
            if command -v usermod >/dev/null 2>&1; then
                usermod -l cosmix -d /home/cosmix -m cosmic
                groupmod -n cosmix cosmic 2>/dev/null || true
            else
                # Alpine: no usermod, edit passwd/group directly
                sed -i "s/^cosmic:/cosmix:/" /etc/passwd
                sed -i "s|/home/cosmic|/home/cosmix|" /etc/passwd
                sed -i "s/^cosmic:/cosmix:/" /etc/group
                if [ -d /home/cosmic ] && [ ! -d /home/cosmix ]; then
                    mv /home/cosmic /home/cosmix
                fi
            fi
        fi
    else
        echo "  cosmic exists at UID $COSMIC_UID (not 1001)"
        echo "  Cannot safely rename — UID mismatch"
        echo "  Options:"
        echo "    a) If cosmic=1000 (desktop user on cachyos): create cosmix separately at 1001"
        echo "    b) Otherwise: manually userdel cosmic, then re-run"

        # On cachyos (cosmic=1000 is the desktop user), create cosmix as a new user
        if [ "$COSMIC_UID" -eq 1000 ]; then
            CURRENT_1001=$(getent passwd 1001 2>/dev/null | cut -d: -f1 || true)
            if [ -z "$CURRENT_1001" ]; then
                change "create cosmix (1001:1001) alongside cosmic (1000, desktop user)"
                if [ "$DRY_RUN" = false ]; then
                    if command -v addgroup >/dev/null 2>&1; then
                        addgroup -g 1001 cosmix 2>/dev/null || true
                        adduser -D -u 1001 -G cosmix -h /home/cosmix -s /bin/bash -g "Cosmix Service" cosmix
                    else
                        groupadd -g 1001 cosmix 2>/dev/null || true
                        useradd -u 1001 -g cosmix -m -d /home/cosmix -s /bin/bash -c "Cosmix Service" cosmix
                    fi
                fi
            else
                echo "  UID 1001 taken by '$CURRENT_1001' — manual fix needed"
            fi
        fi
    fi
else
    # No cosmic or cosmix — create fresh
    CURRENT_1001=$(getent passwd 1001 2>/dev/null | cut -d: -f1 || true)
    if [ -n "$CURRENT_1001" ]; then
        echo "  UID 1001 currently used by: $CURRENT_1001"
        echo "  Manual fix needed"
    else
        change "create cosmix (1001:1001)"
        if [ "$DRY_RUN" = false ]; then
            if command -v addgroup >/dev/null 2>&1; then
                addgroup -g 1001 cosmix 2>/dev/null || true
                adduser -D -u 1001 -G cosmix -h /home/cosmix -s /bin/bash -g "Cosmix Service" cosmix
            else
                groupadd -g 1001 cosmix 2>/dev/null || true
                useradd -u 1001 -g cosmix -m -d /home/cosmix -s /bin/bash -c "Cosmix Service" cosmix
            fi
        fi
    fi
fi

# Ensure cosmix group memberships and runtime directories
if [ "$DRY_RUN" = false ] && id cosmix >/dev/null 2>&1; then
    addgroup cosmix wheel 2>/dev/null || true
    addgroup cosmix utmp 2>/dev/null || true
    install -d -o cosmix -g cosmix -m 755 /run/cosmix 2>/dev/null || true
    install -d -o cosmix -g cosmix -m 755 /var/log/cosmix 2>/dev/null || true
    install -d -o cosmix -g cosmix -m 700 /home/cosmix/.config/cosmix 2>/dev/null || true
fi
echo ""

# =====================================================================
# 3. PostgreSQL: database + role
# =====================================================================
echo "--- PostgreSQL ---"

if ! command -v psql >/dev/null 2>&1; then
    echo "  PostgreSQL not installed — skipping"
else
    PG_RUNNING=false
    if [ "$INIT_SYSTEM" = "systemd" ]; then
        systemctl is-active postgresql >/dev/null 2>&1 && PG_RUNNING=true
    elif [ "$INIT_SYSTEM" = "openrc" ]; then
        rc-service postgresql status >/dev/null 2>&1 && PG_RUNNING=true
    fi

    if [ "$PG_RUNNING" = false ]; then
        echo "  PostgreSQL not running — skipping"
        echo "  Start it and re-run: $0 $MODE"
    else
        # Check PG role
        if su - postgres -c "psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname='cosmix'\"" 2>/dev/null | grep -q 1; then
            echo "  OK: PG role 'cosmix' exists"
        elif su - postgres -c "psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname='cosmic'\"" 2>/dev/null | grep -q 1; then
            change "rename PG role cosmic → cosmix"
            if [ "$DRY_RUN" = false ]; then
                su - postgres -c "psql -c \"ALTER ROLE cosmic RENAME TO cosmix\""
                su - postgres -c "psql -c \"ALTER ROLE cosmix WITH PASSWORD 'cosmix'\""
            fi
        else
            change "create PG role cosmix"
            if [ "$DRY_RUN" = false ]; then
                su - postgres -c "createuser -l cosmix"
                su - postgres -c "psql -c \"ALTER ROLE cosmix WITH PASSWORD 'cosmix'\""
            fi
        fi

        # Check database
        if su - postgres -c "psql -tAc \"SELECT 1 FROM pg_database WHERE datname='cosmix'\"" 2>/dev/null | grep -q 1; then
            echo "  OK: database 'cosmix' exists"
        elif su - postgres -c "psql -tAc \"SELECT 1 FROM pg_database WHERE datname='markweb'\"" 2>/dev/null | grep -q 1; then
            change "rename database markweb → cosmix"
            if [ "$DRY_RUN" = false ]; then
                # Must disconnect all clients first
                su - postgres -c "psql -c \"SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='markweb' AND pid <> pg_backend_pid()\""
                su - postgres -c "psql -c \"ALTER DATABASE markweb RENAME TO cosmix\""
                su - postgres -c "psql -c \"ALTER DATABASE cosmix OWNER TO cosmix\""
            fi
        else
            change "create database cosmix"
            if [ "$DRY_RUN" = false ]; then
                su - postgres -c "createdb -O cosmix cosmix"
            fi
        fi

        # Grants (idempotent)
        if [ "$DRY_RUN" = false ]; then
            su - postgres -c "psql -d cosmix -c 'GRANT ALL ON ALL TABLES IN SCHEMA public TO cosmix'" 2>/dev/null || true
            su - postgres -c "psql -d cosmix -c 'GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO cosmix'" 2>/dev/null || true
            su - postgres -c "psql -d cosmix -c 'ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO cosmix'" 2>/dev/null || true
            su - postgres -c "psql -d cosmix -c 'ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON SEQUENCES TO cosmix'" 2>/dev/null || true
        fi

        # pg_hba.conf — ensure cosmix has local+TCP access
        for PG_HBA in /var/lib/postgresql/data/pg_hba.conf /etc/postgresql/*/main/pg_hba.conf; do
            if [ -f "$PG_HBA" ] && ! grep -q "cosmix" "$PG_HBA" 2>/dev/null; then
                change "add cosmix to $PG_HBA"
                if [ "$DRY_RUN" = false ]; then
                    sed -i '/^local/i \
# cosmix service user — peer for local, md5 for TCP\
local   cosmix      cosmix                          peer\
host    cosmix      cosmix      127.0.0.1/32        md5\
host    cosmix      cosmix      ::1/128             md5' "$PG_HBA"
                    # Reload PG
                    if [ "$INIT_SYSTEM" = "systemd" ]; then
                        systemctl reload postgresql 2>/dev/null || true
                    else
                        rc-service postgresql reload 2>/dev/null || true
                    fi
                fi
            fi
        done
    fi
fi
echo ""

# =====================================================================
# 4. Service files
# =====================================================================
echo "--- Service files ---"

# OpenRC init scripts
for svc_file in /etc/init.d/cosmix /etc/init.d/cosmix-web /etc/init.d/cosmix-portd; do
    if [ -f "$svc_file" ]; then
        if grep -q 'command_user="cosmic"' "$svc_file" 2>/dev/null; then
            change "fix $svc_file: cosmic → cosmix"
            if [ "$DRY_RUN" = false ]; then
                sed -i 's/command_user="cosmic"/command_user="cosmix"/' "$svc_file"
                sed -i 's/--owner cosmic:cosmic/--owner cosmix:cosmix/' "$svc_file"
            fi
        elif grep -q 'command_user="cosmix"' "$svc_file" 2>/dev/null; then
            echo "  OK: $svc_file already uses cosmix"
        fi
    fi
done

# systemd units (check for hardcoded User=cosmic)
if [ "$INIT_SYSTEM" = "systemd" ]; then
    for unit_file in /etc/systemd/system/cosmix*.service; do
        if [ -f "$unit_file" ]; then
            if grep -q 'User=cosmic$' "$unit_file" 2>/dev/null; then
                change "fix $unit_file: User=cosmic → User=cosmix"
                if [ "$DRY_RUN" = false ]; then
                    sed -i 's/^User=cosmic$/User=cosmix/' "$unit_file"
                    sed -i 's/^Group=cosmic$/Group=cosmix/' "$unit_file"
                    systemctl daemon-reload
                fi
            elif grep -q 'User=cosmix' "$unit_file" 2>/dev/null; then
                echo "  OK: $unit_file already uses cosmix"
            fi
        fi
    done
fi
echo ""

# =====================================================================
# 5. Config files
# =====================================================================
echo "--- Config files ---"

# web.toml: update database_url if it references markweb or cosmic
for cfg in /home/cosmix/.config/cosmix/web.toml /home/cosmic/.config/cosmix/web.toml /etc/cosmix/web.toml; do
    if [ -f "$cfg" ]; then
        if grep -q 'markweb\|cosmic:cosmic\|cosmic@localhost' "$cfg" 2>/dev/null; then
            change "update $cfg: database_url → cosmix"
            if [ "$DRY_RUN" = false ]; then
                sed -i 's|postgres://cosmic:cosmic@localhost/markweb|postgres://cosmix:cosmix@localhost/cosmix|g' "$cfg"
                sed -i 's|postgres://cosmic:cosmic@localhost/cosmic|postgres://cosmix:cosmix@localhost/cosmix|g' "$cfg"
                sed -i 's|postgres://cosmic@localhost/markweb|postgres://cosmix:cosmix@localhost/cosmix|g' "$cfg"
            fi
        else
            echo "  OK: $cfg looks clean"
        fi
    fi
done

# daemon.toml: update memory database_url if present
for cfg in /home/cosmix/.config/cosmix/daemon.toml /home/cosmic/.config/cosmix/daemon.toml /etc/cosmix/daemon.toml; do
    if [ -f "$cfg" ]; then
        if grep -q 'markweb\|cosmic.*localhost' "$cfg" 2>/dev/null; then
            change "update $cfg: database references → cosmix"
            if [ "$DRY_RUN" = false ]; then
                sed -i 's|markweb|cosmix|g' "$cfg"
                sed -i 's|cosmic:cosmic|cosmix:cosmix|g' "$cfg"
            fi
        fi
    fi
done
echo ""

# =====================================================================
# 6. File ownership (if cosmix user exists and old cosmic dirs remain)
# =====================================================================
echo "--- File ownership ---"

if [ "$DRY_RUN" = false ] && id cosmix >/dev/null 2>&1; then
    # Fix ownership on key directories
    for dir in /run/cosmix /var/log/cosmix /home/cosmix; do
        if [ -d "$dir" ]; then
            chown -R cosmix:cosmix "$dir" 2>/dev/null || true
        fi
    done
    echo "  ownership fixed on /run/cosmix, /var/log/cosmix, /home/cosmix"
fi
echo ""

# =====================================================================
# Summary
# =====================================================================
echo "=== Summary ==="
if [ "$DRY_RUN" = true ]; then
    if [ "$CHANGES" -eq 0 ]; then
        echo "  Node is already NS 3.0 compliant. No changes needed."
    else
        echo "  $CHANGES change(s) needed. Run with --apply to execute."
        echo ""
        echo "  Before applying:"
        echo "    1. Snapshot/backup the node"
        echo "    2. Stop cosmix services:"
        if [ "$INIT_SYSTEM" = "openrc" ]; then
            echo "       rc-service cosmix-web stop; rc-service cosmix stop"
        else
            echo "       systemctl stop cosmix-web cosmix"
        fi
        echo "    3. Run: $0 --apply"
    fi
else
    echo "  $CHANGES change(s) applied."
    echo ""
    echo "  Next steps:"
    echo "    1. Update web.toml database_url if not auto-fixed"
    echo "    2. Restart services:"
    if [ "$INIT_SYSTEM" = "openrc" ]; then
        echo "       rc-service cosmix-web start; rc-service cosmix start"
    else
        echo "       systemctl start cosmix-web cosmix"
    fi
    echo "    3. Verify: psql -U cosmix -d cosmix -c '\\dt'"
fi
