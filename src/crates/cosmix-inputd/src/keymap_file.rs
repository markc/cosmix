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

/// The load-side document: rows stay raw JSON until each is admitted on its
/// own, so one malformed row costs one row — not the whole file (which would
/// make `main` reseed the defaults over the user's keymap).
#[derive(Deserialize)]
struct RawKeymap {
    #[allow(dead_code)]
    version: u32,
    physical: Vec<serde_json::Value>,
}

/// A loaded keymap: the admitted rows plus a record of every row dropped at
/// admission (`{code, modifiers, action, service, reason}`, fields copied
/// verbatim from the file, `null` when absent), so `input.reload` can report
/// exactly what it refused.
pub struct Loaded {
    pub rows: Vec<PhysicalBinding>,
    pub dropped: Vec<serde_json::Value>,
}

/// The exact action prefix the pre-`service` defaults on the first live host
/// wrote for the clipboard rows. Those rows never worked (the citizen is
/// registered `desktop-vt1` and answers `desktop.clipboard.*`); on load they
/// are rewritten to an explicit target. Deliberately narrow — a generic
/// "split `<svc>.<verb>`" would re-route rows that are valid today.
const LEGACY_CLIPBOARD_PREFIX: &str = "desktop-vt1.desktop.clipboard.";
const LEGACY_CLIPBOARD_SERVICE: &str = "desktop-vt1";

/// Load persisted physical rows, or `None` if the file is absent or unreadable.
pub fn load(path: &Path) -> Option<Loaded> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text, path)
}

/// The identity of a raw row for a drop record / log line.
fn dropped_record(raw: &serde_json::Value, reason: &str) -> serde_json::Value {
    let field = |name: &str| raw.get(name).cloned().unwrap_or(serde_json::Value::Null);
    let stroke = raw.get("stroke");
    let sub = |name: &str| {
        stroke
            .and_then(|s| s.get(name))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    serde_json::json!({
        "code": sub("code"),
        "modifiers": sub("modifiers"),
        "action": field("action"),
        "service": field("service"),
        "reason": reason,
    })
}

/// Rewrite a legacy clipboard row (no `service`, action under
/// [`LEGACY_CLIPBOARD_PREFIX`]) to target the citizen explicitly. Returns true
/// when the row was rewritten.
fn migrate_legacy_clipboard_row(row: &mut PhysicalBinding) -> bool {
    if row.service.is_some() {
        return false;
    }
    let Some(verb) = row.action.as_str().strip_prefix(LEGACY_CLIPBOARD_PREFIX) else {
        return false;
    };
    let Ok(action) = cosmix_input_schema::ActionId::intern(&format!("desktop.clipboard.{verb}"))
    else {
        return false;
    };
    row.action = action;
    row.service = Some(LEGACY_CLIPBOARD_SERVICE.to_string());
    true
}

/// Parse and admit a keymap document (the pure half of [`load`]).
fn parse(text: &str, path: &Path) -> Option<Loaded> {
    let parsed: RawKeymap = serde_json::from_str(text)
        .map_err(|error| eprintln!("cosmix-inputd: keymap {} unreadable: {error}", path.display()))
        .ok()?;
    let mut dropped = Vec::new();
    let mut drop_row = |raw: &serde_json::Value, reason: String| {
        let record = dropped_record(raw, &reason);
        eprintln!(
            "cosmix-inputd: keymap {}: row dropped: {record}",
            path.display()
        );
        dropped.push(record);
    };
    let mut physical = Vec::with_capacity(parsed.physical.len());
    for raw in &parsed.physical {
        match serde_json::from_value::<PhysicalBinding>(raw.clone()) {
            Ok(row) => physical.push((raw, row)),
            Err(error) => drop_row(raw, format!("malformed row: {error}")),
        }
    }
    // Legacy clipboard rows get their explicit target before validation.
    for (_, row) in &mut physical {
        let old = row.action.as_str().to_string();
        if migrate_legacy_clipboard_row(row) {
            eprintln!(
                "cosmix-inputd: keymap {}: migrated row code {} modifiers {}: action {old:?} -> service {:?} action {:?}",
                path.display(),
                row.stroke.code,
                serde_json::to_string(&row.stroke.modifiers).unwrap_or_default(),
                LEGACY_CLIPBOARD_SERVICE,
                row.action.as_str()
            );
        }
    }
    // An explicit `service` target must be registered-name-shaped. A bad one
    // drops the WHOLE row: stripping just the field would silently re-route
    // the verb to its first segment, a different service than the author named.
    let mut admitted = Vec::with_capacity(physical.len());
    for (raw, row) in physical {
        match row.service.as_deref() {
            Some(name) if !cosmix_input_core::service_is_valid(name) => {
                drop_row(raw, format!("invalid service {name:?}"));
            }
            _ => admitted.push(row),
        }
    }
    let mut physical = admitted;
    // This path bypasses `bind_physical`'s admission checks, so enforce the
    // args invariants here too: a fired body is a map, and it is bounded. A
    // hand-edited violation is dropped to None (the row still binds) rather
    // than shipped to handlers.
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
    Some(Loaded {
        rows: physical,
        dropped,
    })
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

    fn load_str(text: &str) -> Loaded {
        parse(text, Path::new("t")).expect("document parses")
    }

    #[test]
    fn loader_accepts_the_service_field_and_its_absence() {
        let loaded = load_str(&doc(&format!("{MENU_WITH},{NEXT_WITHOUT}")));
        let rows = loaded.rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].service.as_deref(), Some("desktop-vt1"));
        assert_eq!(rows[0].action.as_str(), "desktop.clipboard.menu");
        assert_eq!(rows[1].service, None, "pre-field files load unchanged");
        assert!(loaded.dropped.is_empty());
    }

    #[test]
    fn loader_drops_a_row_with_a_malformed_service() {
        for bad in ["", "Desktop", "desk.vt1", "desktop-vt1.alpha.bus"] {
            let row = MENU_WITH.replace("desktop-vt1", bad);
            let loaded = load_str(&doc(&format!("{row},{NEXT_WITHOUT}")));
            assert_eq!(loaded.rows.len(), 1, "{bad:?} row must be dropped");
            assert_eq!(loaded.rows[0].action.as_str(), "desktop.workspace.next");
            // The drop record names the row's full identity and the reason.
            assert_eq!(loaded.dropped.len(), 1);
            let record = &loaded.dropped[0];
            assert_eq!(record["code"], 108);
            assert_eq!(record["modifiers"]["right_ctrl"], true);
            assert_eq!(record["action"], "desktop.clipboard.menu");
            assert_eq!(record["service"], bad);
            assert!(
                record["reason"].as_str().unwrap().contains("invalid service"),
                "{record}"
            );
        }
    }

    #[test]
    fn one_wrong_type_row_costs_one_row_not_the_file() {
        // Before per-row admission a non-string `service` failed the WHOLE
        // document (None), and main then reseeded the defaults over the file.
        for bad_value in ["42", "null", "[\"desktop-vt1\"]", "{}"] {
            let row = MENU_WITH.replace("\"desktop-vt1\"", bad_value);
            let loaded = parse(&doc(&format!("{row},{NEXT_WITHOUT}")), Path::new("t"))
                .unwrap_or_else(|| panic!("service {bad_value} failed the whole file"));
            if bad_value == "null" {
                // `null` is an absent Option — a valid untargeted row.
                assert_eq!(loaded.rows.len(), 2);
                continue;
            }
            assert_eq!(loaded.rows.len(), 1, "service {bad_value}: good row kept");
            assert_eq!(loaded.rows[0].action.as_str(), "desktop.workspace.next");
            assert_eq!(loaded.dropped.len(), 1);
            assert_eq!(loaded.dropped[0]["code"], 108);
            assert!(
                loaded.dropped[0]["reason"]
                    .as_str()
                    .unwrap()
                    .starts_with("malformed row"),
                "{}",
                loaded.dropped[0]
            );
        }
    }

    #[test]
    fn legacy_clipboard_rows_migrate_to_an_explicit_target() {
        // Exactly the two rows the pre-`service` host file carries.
        let legacy = r#"
            {"stroke":{"code":108,"modifiers":{"right_ctrl":true}},
             "action":"desktop-vt1.desktop.clipboard.menu","scope":"global","repeat":"ignore"},
            {"stroke":{"code":103,"modifiers":{"right_ctrl":true}},
             "action":"desktop-vt1.desktop.clipboard.rotate","scope":"global","repeat":"ignore"}"#;
        let loaded = load_str(&doc(legacy));
        assert!(loaded.dropped.is_empty());
        let got: Vec<_> = loaded
            .rows
            .iter()
            .map(|r| (r.stroke.code, r.service.as_deref(), r.action.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (108, Some("desktop-vt1"), "desktop.clipboard.menu"),
                (103, Some("desktop-vt1"), "desktop.clipboard.rotate"),
            ]
        );
    }

    #[test]
    fn unrelated_dotted_actions_are_not_migrated() {
        // A hyphenated first segment that is NOT the seeded clipboard prefix is
        // a valid row today and must be left exactly as written.
        for action in [
            "foo-bar.baz.qux",
            "desktop-vt1.desktop.workspace.next",
            "desktop-vt5.desktop.clipboard.menu",
        ] {
            let row = format!(
                r#"{{"stroke":{{"code":63}},"action":"{action}"}}"#
            );
            let loaded = load_str(&doc(&row));
            assert_eq!(loaded.rows.len(), 1);
            assert_eq!(loaded.rows[0].action.as_str(), action);
            assert_eq!(loaded.rows[0].service, None, "{action} gained a target");
        }
        // A legacy-prefixed row that already names a service is left alone.
        let row = r#"{"stroke":{"code":63},"action":"desktop-vt1.desktop.clipboard.menu","service":"other-svc"}"#;
        let loaded = load_str(&doc(row));
        assert_eq!(loaded.rows[0].action.as_str(), "desktop-vt1.desktop.clipboard.menu");
        assert_eq!(loaded.rows[0].service.as_deref(), Some("other-svc"));
    }

    #[test]
    fn save_then_load_round_trips_the_service() {
        let dir = std::env::temp_dir().join(format!("inputd-keymap-test-{}", std::process::id()));
        let path = dir.join("keymap.json");
        let rows = cosmix_input_core::default_keymap().physical;
        save(&path, &rows).unwrap();
        let back = load(&path).unwrap().rows;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(back, rows);
        assert!(back.iter().any(|r| r.service.as_deref() == Some("desktop-vt1")));
    }
}
