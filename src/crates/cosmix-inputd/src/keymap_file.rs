//! Keymap persistence — so a rebind is remembered across restarts.
//!
//! The plan's target format is strict-data `.mix`, but cosmix_config has no
//! generic `.mix` serializer yet (cosmix-actions hand-rolls one for the semantic
//! keymap). Until that lands this uses JSON with the plan's interim durability
//! mechanism — write to a temp file, fsync, atomic rename — so a crash mid-write
//! never truncates the live keymap. The physical rows are the same
//! [`PhysicalBinding`] the Bus wire and `.mix` will use, so the format migration
//! is a serializer swap, not a data change.

use std::io::Write;
use std::path::{Path, PathBuf};

use cosmix_input_schema::{KEYMAP_SCHEMA_VERSION, PhysicalBinding};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct PersistedKeymap {
    version: u32,
    physical: Vec<PhysicalBinding>,
}

/// Resolve the keymap path: `$COSMIX_INPUTD_KEYMAP`, else (running as root)
/// `/var/lib/cosmix/inputd/keymap.json` — the same path the systemd unit's
/// StateDirectory grants, so a manual bring-up run and the service agree on
/// where state lives — else `$XDG_CONFIG_HOME`/`$HOME/.config` for a non-root
/// run. The daemon normally runs as root (evdev + uinput), and a root
/// daemon's `$HOME` is both wrong for state and unwritable under the unit's
/// ProtectHome (the silent-persistence-failure bug of 2026-09-13).
pub fn default_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("COSMIX_INPUTD_KEYMAP") {
        return Some(PathBuf::from(explicit));
    }
    if unsafe { libc::geteuid() } == 0 {
        return Some(PathBuf::from("/var/lib/cosmix/inputd/keymap.json"));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("cosmix").join("inputd").join("keymap.json"))
}

/// Load persisted physical rows, or `None` if the file is absent or unreadable.
pub fn load(path: &Path) -> Option<Vec<PhysicalBinding>> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text, path)
}

/// Parse and admit a keymap document (the pure half of [`load`]).
fn parse(text: &str, path: &Path) -> Option<Vec<PhysicalBinding>> {
    let parsed: PersistedKeymap = serde_json::from_str(text)
        .map_err(|error| eprintln!("cosmix-inputd: keymap {} unreadable: {error}", path.display()))
        .ok()?;
    // This path bypasses `bind_physical`'s admission checks, so enforce the
    // args invariants here too: a fired body is a map, and it is bounded. A
    // hand-edited violation is dropped to None (the row still binds) rather
    // than shipped to handlers.
    let mut physical = parsed.physical;
    for row in &mut physical {
        let bad = row.args.as_ref().is_some_and(|args| {
            !args.is_object()
                || serde_json::to_string(args).map(|s| s.len()).unwrap_or(usize::MAX)
                    > cosmix_input_core::MAX_ARGS_BYTES
        });
        if bad {
            eprintln!(
                "cosmix-inputd: keymap {}: row {:?} has non-object or oversized args; ignoring them",
                path.display(),
                row.action.as_str()
            );
            row.args = None;
        }
    }
    // An explicit `service` target must be registered-name-shaped. A bad one
    // drops the WHOLE row: stripping just the field would silently re-route
    // the verb to its first segment, a different service than the author named.
    physical.retain(|row| match row.service.as_deref() {
        Some(name) if !cosmix_input_core::service_is_valid(name) => {
            eprintln!(
                "cosmix-inputd: keymap {}: row {:?} has invalid service {name:?}; row dropped",
                path.display(),
                row.action.as_str()
            );
            false
        }
        _ => true,
    });
    Some(physical)
}

/// Persist physical rows durably: write a temp file next to the target, fsync,
/// then atomically rename over it. Creates parent directories as needed.
pub fn save(path: &Path, physical: &[PhysicalBinding]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = PersistedKeymap {
        version: KEYMAP_SCHEMA_VERSION,
        physical: physical.to_vec(),
    };
    let json = serde_json::to_string_pretty(&doc)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(rows: &str) -> String {
        format!(r#"{{"version":1,"physical":[{rows}]}}"#)
    }

    const MENU_WITH: &str = r#"{"stroke":{"code":108,"modifiers":{"right_ctrl":true}},
        "action":"desktop.clipboard.menu","service":"desktop-vt1"}"#;
    const NEXT_WITHOUT: &str =
        r#"{"stroke":{"code":106,"modifiers":{"right_ctrl":true}},"action":"desktop.workspace.next"}"#;

    #[test]
    fn loader_accepts_the_service_field_and_its_absence() {
        let rows = parse(&doc(&format!("{MENU_WITH},{NEXT_WITHOUT}")), Path::new("t")).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].service.as_deref(), Some("desktop-vt1"));
        assert_eq!(rows[0].action.as_str(), "desktop.clipboard.menu");
        assert_eq!(rows[1].service, None, "pre-field files load unchanged");
    }

    #[test]
    fn loader_drops_a_row_with_a_malformed_service() {
        for bad in ["", "Desktop", "desk.vt1", "desktop-vt1.alpha.bus"] {
            let row = MENU_WITH.replace("desktop-vt1", bad);
            let rows = parse(&doc(&format!("{row},{NEXT_WITHOUT}")), Path::new("t")).unwrap();
            assert_eq!(rows.len(), 1, "{bad:?} row must be dropped");
            assert_eq!(rows[0].action.as_str(), "desktop.workspace.next");
        }
    }

    #[test]
    fn save_then_load_round_trips_the_service() {
        let dir = std::env::temp_dir().join(format!("inputd-keymap-test-{}", std::process::id()));
        let path = dir.join("keymap.json");
        let rows = cosmix_input_core::default_keymap().physical;
        save(&path, &rows).unwrap();
        let back = load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(back, rows);
        assert!(back.iter().any(|r| r.service.as_deref() == Some("desktop-vt1")));
    }
}
