#!/usr/bin/env bash
#
# patch.sh — Apply/check/reset cosmic-comp overrides from overrides.toml
#
# Usage:
#   ./patch.sh apply   — Apply all overrides to cosmic-comp source
#   ./patch.sh status  — Show which overrides are applied vs pending
#   ./patch.sh reset   — Reset cosmic-comp source to upstream (git checkout)
#   ./patch.sh build   — Apply + build release
#   ./patch.sh install — Apply + build + install to /usr/bin/cosmic-comp

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OVERRIDES="$SCRIPT_DIR/overrides.toml"
COSMIC_COMP="$HOME/.gh/cosmix/cosmic/cosmic-comp"

if [[ ! -d "$COSMIC_COMP" ]]; then
    echo "error: cosmic-comp not found at $COSMIC_COMP"
    exit 1
fi

if [[ ! -f "$OVERRIDES" ]]; then
    echo "error: overrides.toml not found at $OVERRIDES"
    exit 1
fi

# Map TOML section to source file path
section_to_file() {
    local section="$1"
    # shell.mod → src/shell/mod.rs
    # shell.layout.floating.mod → src/shell/layout/floating/mod.rs
    echo "src/${section//./\/}.rs"
}

# Parse overrides.toml (simple parser — no nested tables, no inline tables)
# Outputs: section|constant|value
parse_overrides() {
    local section=""
    while IFS= read -r line; do
        # Skip comments and blank lines
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ "$line" =~ ^[[:space:]]*$ ]] && continue

        # Section header
        if [[ "$line" =~ ^\[([a-z._]+)\] ]]; then
            section="${BASH_REMATCH[1]}"
            continue
        fi

        # Key = value (integer) or Key = "value:f64" (float)
        if [[ -n "$section" && "$line" =~ ^([A-Z_]+)[[:space:]]*=[[:space:]]*\"([0-9.]+):f64\" ]]; then
            echo "${section}|${BASH_REMATCH[1]}|${BASH_REMATCH[2]}|f64"
        elif [[ -n "$section" && "$line" =~ ^([A-Z_]+)[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
            echo "${section}|${BASH_REMATCH[1]}|${BASH_REMATCH[2]}|millis"
        fi
    done < "$OVERRIDES"
}

# Apply a single override
apply_one() {
    local file="$1" constant="$2" value="$3" kind="$4"
    local filepath="$COSMIC_COMP/$file"

    if [[ ! -f "$filepath" ]]; then
        echo "  SKIP  $file — file not found"
        return 1
    fi

    if [[ "$kind" == "f64" ]]; then
        # Match: const NAME: f64 = 150.0;
        local current_value
        current_value=$(grep -oP "const\s+${constant}:\s+f64\s*=\s*\K[0-9.]+" "$filepath" 2>/dev/null || echo "")

        if [[ -z "$current_value" ]]; then
            echo "  SKIP  $file:$constant — f64 pattern not found"
            return 1
        fi

        if [[ "$current_value" == "$value" ]]; then
            echo "  OK    $file:$constant = $value (already applied)"
            return 0
        fi

        sed -i -E "s/const ${constant}: f64 = [0-9.]+/const ${constant}: f64 = ${value}/" "$filepath"
        echo "  PATCH $file:$constant — $current_value → $value"
    else
        # Match: [pub] const NAME: Duration = Duration::from_millis(N);
        local pattern="(pub\s+)?const\s+${constant}:\s+Duration\s*=\s*Duration::from_millis\([0-9]+\)"
        local current
        current=$(grep -oP "$pattern" "$filepath" 2>/dev/null || true)

        if [[ -z "$current" ]]; then
            echo "  SKIP  $file:$constant — Duration pattern not found"
            return 1
        fi

        if echo "$current" | grep -q "from_millis(${value})"; then
            echo "  OK    $file:$constant = ${value}ms (already applied)"
            return 0
        fi

        local old_value
        old_value=$(echo "$current" | grep -oP 'from_millis\(\K[0-9]+')

        sed -i -E "s/(pub\s+)?const\s+${constant}:\s+Duration\s*=\s*Duration::from_millis\([0-9]+\)/\1const ${constant}: Duration = Duration::from_millis(${value})/" "$filepath"
        sed -i -E "s/^const ${constant}/const ${constant}/" "$filepath"

        echo "  PATCH $file:$constant — ${old_value}ms → ${value}ms"
    fi
}

# Check status of a single override
status_one() {
    local file="$1" constant="$2" value="$3" kind="$4"
    local filepath="$COSMIC_COMP/$file"

    if [[ ! -f "$filepath" ]]; then
        echo "  MISS  $file — file not found"
        return
    fi

    local current_value unit
    if [[ "$kind" == "f64" ]]; then
        current_value=$(grep -oP "const\s+${constant}:\s+f64\s*=\s*\K[0-9.]+" "$filepath" 2>/dev/null || echo "?")
        unit=""
    else
        current_value=$(grep -oP "(pub\s+)?const\s+${constant}:\s+Duration\s*=\s*Duration::from_millis\(\K[0-9]+" "$filepath" 2>/dev/null || echo "?")
        unit="ms"
    fi

    if [[ "$current_value" == "$value" ]]; then
        echo "  OK    $file:$constant = ${value}${unit}"
    else
        echo "  DIFF  $file:$constant = ${current_value}${unit} (want ${value}${unit})"
    fi
}

cmd_apply() {
    echo "Applying overrides to $COSMIC_COMP"
    echo ""
    local count=0 applied=0
    while IFS='|' read -r section constant value kind; do
        local file
        file=$(section_to_file "$section")
        apply_one "$file" "$constant" "$value" "$kind" && ((applied++)) || true
        ((count++))
    done < <(parse_overrides)
    echo ""
    echo "Done: $applied/$count overrides applied"
}

cmd_status() {
    echo "Override status for $COSMIC_COMP"
    echo ""
    while IFS='|' read -r section constant value kind; do
        local file
        file=$(section_to_file "$section")
        status_one "$file" "$constant" "$value" "$kind"
    done < <(parse_overrides)
}

cmd_reset() {
    echo "Resetting cosmic-comp source to upstream..."
    cd "$COSMIC_COMP"
    git checkout -- src/
    echo "Done — all local patches removed"
}

cmd_build() {
    cmd_apply
    echo ""
    echo "Building cosmic-comp (release)..."
    cd "$COSMIC_COMP"
    make
    echo ""
    echo "Build complete. Run './patch.sh install' to install."
}

cmd_install() {
    cmd_apply
    echo ""
    echo "Building cosmic-comp (release)..."
    cd "$COSMIC_COMP"
    make
    echo ""
    echo "Installing to /usr/bin/cosmic-comp..."
    sudo install -Dm0755 target/release/cosmic-comp /usr/bin/cosmic-comp
    echo "Done. Restart cosmic-comp or log out/in to activate."
}

case "${1:-help}" in
    apply)   cmd_apply ;;
    status)  cmd_status ;;
    reset)   cmd_reset ;;
    build)   cmd_build ;;
    install) cmd_install ;;
    *)
        echo "Usage: $0 {apply|status|reset|build|install}"
        echo ""
        echo "  apply   — Apply overrides to cosmic-comp source"
        echo "  status  — Show which overrides are applied vs pending"
        echo "  reset   — Reset source to upstream (git checkout)"
        echo "  build   — Apply + build release"
        echo "  install — Apply + build + sudo install"
        ;;
esac
