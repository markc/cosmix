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

/// Why a keymap document did not load.
#[derive(Debug)]
pub enum LoadError {
    /// No file at the path: a first run, nothing of the user's to preserve.
    Absent,
    /// The bytes were read but the DOCUMENT is unusable (invalid UTF-8, not
    /// JSON, not an object, missing/non-array `physical`, missing/non-u32
    /// `version`). Carries the reason and the identity of the bytes that were
    /// read. This is the only class startup moves aside.
    Invalid(String, FileId),
    /// Opening or reading failed for any other reason (EACCES, EIO, EISDIR,
    /// ...). The file may well be valid, so it is never moved aside.
    Io(String),
}

impl LoadError {
    /// The human reason, for logs and replies.
    pub fn reason(&self) -> String {
        match self {
            LoadError::Absent => "absent".to_string(),
            LoadError::Invalid(reason, _) => reason.clone(),
            LoadError::Io(reason) => reason.clone(),
        }
    }
}

/// `(st_dev, st_ino)` of an inode: which bytes a load actually read.
pub type FileId = (u64, u64);

fn file_id(meta: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Load persisted physical rows. Per-row problems drop one row (see
/// [`Loaded::dropped`]); only a document-level problem is an error.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoadError::Absent);
        }
        Err(error) => return Err(LoadError::Io(format!("open failed: {error}"))),
    };
    // Identity of the bytes being read, taken from the open fd, so a later
    // move-aside can prove it moved THESE bytes and not a replacement.
    let id = match file.metadata() {
        Ok(meta) => file_id(&meta),
        Err(error) => return Err(LoadError::Io(format!("fstat failed: {error}"))),
    };
    let mut text = String::new();
    match file.read_to_string(&mut text) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(LoadError::Invalid(format!("not UTF-8: {error}"), id));
        }
        Err(error) => return Err(LoadError::Io(format!("read failed: {error}"))),
    }
    parse(&text, path).map_err(|reason| LoadError::Invalid(reason, id))
}

/// What startup ended up with: the rows to serve, whether rebinds may be
/// persisted to the path, and — when the user's file was unusable — where it
/// was moved to.
pub struct Opened {
    pub rows: Vec<PhysicalBinding>,
    /// The backup the unusable document was renamed to, if that happened.
    pub recovered_from: Option<PathBuf>,
    /// `Some("<path>: <reason>")` when the path must NOT be written this run:
    /// an unusable file that could not be moved aside, a read error on a file
    /// that may be valid, or a file that reappeared while seeding. The
    /// defaults are then served in memory only. `input.reload` of a usable
    /// file clears it.
    pub persist_disabled: Option<String>,
}

/// The points [`open_with`] exposes to tests, so each race window can be
/// driven deterministically. Production passes a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The load failed as `Invalid`; the move-aside has not started.
    AfterFailedLoad,
    /// About to rename the file onto this candidate backup name.
    BeforeRename,
    /// About to create the defaults at the (now vacant) path.
    BeforeSeed,
}

/// Startup load: the file's rows if it loads; otherwise the defaults. An
/// unusable document is first renamed to `<name>.bad-<stamp>` beside it (never
/// deleted, never over an existing name) so seeding cannot overwrite the
/// user's keymap. `stamp` is `YYYYmmdd-HHMMSS` ([`local_stamp`]); a parameter
/// so tests are deterministic.
pub fn open(path: &Path, stamp: &str) -> Opened {
    open_with(path, stamp, &mut |_, _| {})
}

/// [`open`] with a stage hook (tests inject races through it).
pub fn open_with(path: &Path, stamp: &str, hook: &mut dyn FnMut(Stage, &Path)) -> Opened {
    open_attempt(path, stamp, hook, true)
}

fn defaults() -> Vec<PhysicalBinding> {
    cosmix_input_core::default_keymap().physical
}

fn disabled(path: &Path, reason: String, recovered_from: Option<PathBuf>) -> Opened {
    let reason = format!("{}: {reason}", path.display());
    eprintln!("cosmix-inputd: keymap persistence disabled: {reason}; serving defaults in memory");
    Opened {
        rows: defaults(),
        recovered_from,
        persist_disabled: Some(reason),
    }
}

fn open_attempt(
    path: &Path,
    stamp: &str,
    hook: &mut dyn FnMut(Stage, &Path),
    may_retry: bool,
) -> Opened {
    match load(path) {
        Ok(loaded) => {
            eprintln!(
                "cosmix-inputd: loaded keymap ({} rows, {} dropped) from {}",
                loaded.rows.len(),
                loaded.dropped.len(),
                path.display()
            );
            Opened {
                rows: loaded.rows,
                recovered_from: None,
                persist_disabled: None,
            }
        }
        Err(LoadError::Absent) => seed(path, None, hook),
        Err(LoadError::Io(reason)) => disabled(path, format!("unreadable ({reason})"), None),
        Err(LoadError::Invalid(reason, id)) => {
            hook(Stage::AfterFailedLoad, path);
            // A symlinked keymap: load read the TARGET, but a rename would move
            // the LINK, so the identity check could never match. Recovering
            // through a link is the operator's call: touch nothing.
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return disabled(
                    path,
                    format!(
                        "unusable ({reason}); keymap path is a symlink; recover the target manually"
                    ),
                    None,
                );
            }
            let backup = match backup_unusable(path, stamp, hook) {
                Ok(backup) => backup,
                Err(error) => {
                    return disabled(
                        path,
                        format!("unusable ({reason}) and could not be moved aside ({error})"),
                        None,
                    );
                }
            };
            // Prove the rename moved the bytes that failed to parse. If the
            // file was replaced in the window (perhaps by a now-valid one),
            // put it back and load that instead.
            let moved = std::fs::symlink_metadata(&backup).map(|m| file_id(&m)).ok();
            if moved != Some(id) {
                if let Err(error) = rename_noreplace(&backup, path) {
                    return disabled(
                        path,
                        format!(
                            "replaced while being recovered; the replacement is at {} and could not be restored ({error})",
                            backup.display()
                        ),
                        None,
                    );
                }
                eprintln!(
                    "cosmix-inputd: keymap {} was replaced while being recovered; restored it and reloading",
                    path.display()
                );
                if may_retry {
                    return open_attempt(path, stamp, hook, false);
                }
                return disabled(path, "replaced repeatedly while being recovered".to_string(), None);
            }
            eprintln!(
                "cosmix-inputd: keymap {} unusable ({reason}); moved to {} and seeding defaults",
                path.display(),
                backup.display()
            );
            seed(path, Some(backup), hook)
        }
    }
}

/// Create the defaults at a vacant path. Exclusive: a file that appeared since
/// the path was found vacant is never overwritten (persistence is disabled
/// instead, and the next `input.reload` loads that file).
fn seed(path: &Path, recovered_from: Option<PathBuf>, hook: &mut dyn FnMut(Stage, &Path)) -> Opened {
    let rows = defaults();
    hook(Stage::BeforeSeed, path);
    match save_new(path, &rows) {
        Ok(()) => eprintln!("cosmix-inputd: seeded default keymap at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return disabled(
                path,
                "file reappeared while seeding the defaults; left untouched (input.reload loads it)"
                    .to_string(),
                recovered_from,
            );
        }
        // Unwritable directory etc.: nothing of the user's is at risk, so
        // persistence stays on and each later save reports its own error.
        Err(error) => eprintln!("cosmix-inputd: could not seed keymap {}: {error}", path.display()),
    }
    Opened {
        rows,
        recovered_from,
        persist_disabled: None,
    }
}

/// Rename an unusable keymap to `<file name>.bad-<stamp>` in the same
/// directory. Exclusive: each candidate is taken with `RENAME_NOREPLACE`, so a
/// name that exists — or appears between choosing it and renaming — is never
/// clobbered; the next `-1`, `-2`, … suffix is tried instead.
fn backup_unusable(
    path: &Path,
    stamp: &str,
    hook: &mut dyn FnMut(Stage, &Path),
) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("keymap path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let base = format!("{name}.bad-{stamp}");
    for n in 0..1000u32 {
        let candidate = if n == 0 {
            path.with_file_name(&base)
        } else {
            path.with_file_name(format!("{base}-{n}"))
        };
        hook(Stage::BeforeRename, &candidate);
        match rename_noreplace(path, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("no free backup name"))
}

/// `rename(from, to)` that fails with `AlreadyExists` instead of replacing an
/// existing `to`: `renameat2(RENAME_NOREPLACE)`, or — on a filesystem that
/// does not support the flag — `link` + `unlink`, which is equally exclusive.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let cstr = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other)
    };
    let (c_from, c_to) = (cstr(from)?, cstr(to)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if !matches!(error.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOSYS)) {
        return Err(error);
    }
    link_move(from, to, &mut |_| {}, &mut |p| std::fs::remove_file(p))
}

/// The fallback move for filesystems without `RENAME_NOREPLACE`: `link(from,
/// to)` (exclusive — EEXIST if `to` exists), then unlink `from` ONLY if it is
/// still the inode just linked. `link`+`unlink` is not one atomic step, so:
/// - `from` replaced between the two (say, by a now-valid keymap): the new
///   link is removed, `from` is left alone, and the move fails "replaced
///   during backup" — the caller then disables persistence and seeds nothing;
/// - the unlink fails: the new link is removed and the error returned, so the
///   bytes are never left under two names;
/// - `from` vanished: the bytes now live only at `to`, which is kept (Ok).
///
/// A replacement landing in the instant between the final identity check and
/// the unlink cannot be excluded without `RENAME_NOREPLACE`; this narrows the
/// window to that one step. `before_unlink` and `unlink` are the test seams.
fn link_move(
    from: &Path,
    to: &Path,
    before_unlink: &mut dyn FnMut(&Path),
    unlink: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::hard_link(from, to)?;
    let undo = |error: std::io::Error| {
        // `from` still names the bytes, so dropping the new name loses nothing.
        let _ = std::fs::remove_file(to);
        Err(error)
    };
    let linked = match std::fs::symlink_metadata(to) {
        Ok(meta) => file_id(&meta),
        Err(error) => return undo(error),
    };
    before_unlink(from);
    match std::fs::symlink_metadata(from) {
        Ok(meta) if file_id(&meta) == linked => {}
        Ok(_) => return undo(std::io::Error::other("replaced during backup")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return undo(error),
    }
    match unlink(from) {
        Ok(()) => Ok(()),
        Err(error) => undo(error),
    }
}

/// The current local time as `YYYYmmdd-HHMMSS`, for backup names.
pub fn local_stamp() -> String {
    // SAFETY: time(NULL) and localtime_r into a zeroed, caller-owned tm are
    // both thread-safe; a failed conversion leaves the zeroed struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        libc::localtime_r(&now, &mut tm);
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
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

/// Parse and admit a keymap document (the pure half of [`load`]). `Err` is a
/// document-level reason; per-row problems land in [`Loaded::dropped`].
fn parse(text: &str, path: &Path) -> Result<Loaded, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("not JSON: {error}"))?;
    // serde's derived struct visitor also accepts a JSON ARRAY as a positional
    // field list (`[1, []]` would load), so require the object shape first.
    if !value.is_object() {
        return Err("not a JSON object".to_string());
    }
    let parsed: RawKeymap =
        serde_json::from_value(value).map_err(|error| format!("bad document: {error}"))?;
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
    Ok(Loaded {
        rows: physical,
        dropped,
    })
}

/// Persist physical rows durably: write a temp file next to the target, fsync,
/// then atomically rename over it. Creates parent directories as needed.
pub fn save(path: &Path, physical: &[PhysicalBinding]) -> std::io::Result<()> {
    let tmp = write_tmp(path, physical)?;
    std::fs::rename(&tmp, path)
}

/// Like [`save`], but only onto a VACANT path: fails with `AlreadyExists`
/// (and leaves the existing file untouched) if anything is there.
fn save_new(path: &Path, physical: &[PhysicalBinding]) -> std::io::Result<()> {
    let tmp = write_tmp(path, physical)?;
    rename_noreplace(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Write the document durably to `<path>.json.tmp` (creating parent
/// directories) and return the temp path.
fn write_tmp(path: &Path, physical: &[PhysicalBinding]) -> std::io::Result<PathBuf> {
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
    Ok(tmp)
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
                .unwrap_or_else(|e| panic!("service {bad_value} failed the whole file: {e}"));
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

    /// A fresh, empty directory unique to one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inputd-open-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Like [`scratch`] but under `/tmp` (world-traversable), for tests whose
    /// check runs as uid 65534 — the harness temp dir may be root-only.
    fn scratch_tmp(tag: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/inputd-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_unusable_document_is_moved_aside_before_the_defaults_are_seeded() {
        let defaults = cosmix_input_core::default_keymap().physical;
        let version_string = doc("").replace("\"version\":1", "\"version\":\"1\"");
        let version_negative = doc("").replace("\"version\":1", "\"version\":-1");
        for (tag, text) in [
            ("not-json", "this is { not json"),
            // An array is the case serde's struct visitor would otherwise
            // accept positionally.
            ("non-object", r#"[1,[]]"#),
            ("no-physical", r#"{"version":1}"#),
            ("physical-not-array", r#"{"version":1,"physical":{}}"#),
            ("version-string", version_string.as_str()),
            ("version-negative", version_negative.as_str()),
            ("no-version", r#"{"physical":[]}"#),
        ] {
            let dir = scratch(tag);
            let path = dir.join("keymap.json");
            std::fs::write(&path, text).unwrap();
            let opened = open(&path, "20260924-101112");
            let backup = dir.join("keymap.json.bad-20260924-101112");
            assert_eq!(opened.recovered_from.as_deref(), Some(backup.as_path()), "{tag}");
            assert!(opened.persist_disabled.is_none(), "{tag}");
            assert_eq!(opened.rows, defaults, "{tag}: defaults served");
            // The user's bytes survive verbatim; the live path holds the defaults.
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), text, "{tag}");
            assert_eq!(load(&path).unwrap().rows, defaults, "{tag}: defaults seeded");
            assert_eq!(
                dir_names(&dir),
                vec!["keymap.json", "keymap.json.bad-20260924-101112"],
                "{tag}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_second_recovery_in_the_same_second_keeps_the_first_backup() {
        let dir = scratch("twice");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "first").unwrap();
        let _ = open(&path, "20260924-101112");
        std::fs::write(&path, "second").unwrap();
        let opened = open(&path, "20260924-101112");
        let second = dir.join("keymap.json.bad-20260924-101112-1");
        assert_eq!(opened.recovered_from.as_deref(), Some(second.as_path()));
        assert_eq!(
            std::fs::read_to_string(dir.join("keymap.json.bad-20260924-101112")).unwrap(),
            "first"
        );
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_good_or_absent_file_makes_no_backup() {
        let dir = scratch("good");
        let path = dir.join("keymap.json");
        std::fs::write(&path, doc(NEXT_WITHOUT)).unwrap();
        let opened = open(&path, "20260924-101112");
        assert!(opened.recovered_from.is_none());
        assert_eq!(opened.rows.len(), 1);
        assert_eq!(opened.rows[0].action.as_str(), "desktop.workspace.next");
        assert_eq!(dir_names(&dir), vec!["keymap.json"], "file untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc(NEXT_WITHOUT));
        // Absent: a first run seeds without any backup.
        std::fs::remove_file(&path).unwrap();
        let opened = open(&path, "20260924-101112");
        assert!(opened.recovered_from.is_none());
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const STAMP: &str = "20260924-101112";

    /// Replace `path` with `text` as a NEW inode (write + rename), the way an
    /// editor or another writer would.
    fn replace_with_new_inode(path: &Path, text: &str) {
        let tmp = path.with_extension("race");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Names the child test a re-executed test binary should act as.
    const PERM_CHILD_ENV: &str = "INPUTD_PERM_CHILD";
    /// The scratch directory handed to that child.
    const PERM_DIR_ENV: &str = "INPUTD_PERM_DIR";

    /// Run the child test `name` in a FRESH process — this test binary
    /// re-executed with `--exact` — rather than forking the multithreaded
    /// harness. The child drops to uid/gid 65534 when started as root (root
    /// bypasses the mode bits these tests depend on; the cbc workers run as
    /// root) and performs the check. The parent requires a zero exit AND the
    /// harness's "1 passed" line, so a mistyped name (0 tests run, exit 0)
    /// cannot pass vacuously. Bounded by a 20 s deadline: past it the child
    /// is killed, reaped, and the test fails.
    fn run_perm_child(name: &str, dir: &Path) {
        use std::io::Read;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("keymap_file::tests::{name}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PERM_CHILD_ENV, name)
            .env(PERM_DIR_ENV, dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("re-exec the test binary");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait for child") {
                break status;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let reaped = child.wait();
                panic!("{name}: child timed out (reaped: {reaped:?})");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let (mut out, mut err) = (String::new(), String::new());
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "{name}: child failed ({status})\n{out}\n{err}");
        assert!(out.contains("1 passed"), "{name}: child test did not run\n{out}\n{err}");
    }

    /// In a child started by [`run_perm_child`] for `name`: drop to 65534 if
    /// root and return the scratch dir. Anywhere else (the normal test run):
    /// `None`, and the child test is a no-op pass.
    fn perm_child(name: &str) -> Option<PathBuf> {
        if std::env::var(PERM_CHILD_ENV).ok().as_deref() != Some(name) {
            return None;
        }
        let dir = PathBuf::from(std::env::var_os(PERM_DIR_ENV).expect("scratch dir"));
        if unsafe { libc::geteuid() } == 0 {
            // SAFETY: plain credential syscalls with valid arguments; glibc
            // applies set*id to every thread of this fresh process.
            let dropped = unsafe {
                libc::setgroups(0, std::ptr::null()) == 0
                    && libc::setresgid(65534, 65534, 65534) == 0
                    && libc::setresuid(65534, 65534, 65534) == 0
            };
            assert!(dropped, "privilege drop failed: {}", std::io::Error::last_os_error());
            assert_eq!(unsafe { libc::geteuid() }, 65534);
        }
        Some(dir)
    }

    #[test]
    fn non_utf8_bytes_are_an_unusable_document() {
        let dir = scratch("non-utf8");
        let path = dir.join("keymap.json");
        std::fs::write(&path, [0xff, 0xfe, 0x7b]).unwrap();
        let opened = open(&path, STAMP);
        let backup = dir.join(format!("keymap.json.bad-{STAMP}"));
        assert_eq!(opened.recovered_from.as_deref(), Some(backup.as_path()));
        assert_eq!(std::fs::read(&backup).unwrap(), vec![0xff, 0xfe, 0x7b]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_error_never_moves_the_file_and_disables_persistence() {
        // EISDIR: an I/O error, not a document error (runs the same as root).
        let dir = scratch("eisdir");
        let path = dir.join("keymap.json");
        std::fs::create_dir(&path).unwrap();
        let opened = open(&path, STAMP);
        assert!(opened.recovered_from.is_none());
        let reason = opened.persist_disabled.expect("persistence disabled");
        assert!(reason.contains("unreadable"), "{reason}");
        assert_eq!(opened.rows, cosmix_input_core::default_keymap().physical);
        assert!(path.is_dir(), "the directory was left in place");
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_valid_file_is_not_moved_aside() {
        // A VALID keymap the daemon cannot read (mode 0000, EACCES) must stay
        // exactly where it is. The directory is world-writable so a wrong
        // move-aside WOULD succeed — the test cannot pass vacuously.
        let dir = scratch_tmp("eacces");
        chmod(&dir, 0o777);
        let path = dir.join("keymap.json");
        std::fs::write(&path, doc(NEXT_WITHOUT)).unwrap();
        chmod(&path, 0o000);
        run_perm_child("perm_child_eacces_file", &dir);
        chmod(&path, 0o644);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc(NEXT_WITHOUT));
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn perm_child_eacces_file() {
        let Some(dir) = perm_child("perm_child_eacces_file") else { return };
        let opened = open(&dir.join("keymap.json"), STAMP);
        assert!(opened.recovered_from.is_none(), "EACCES file was moved aside");
        let reason = opened.persist_disabled.expect("persistence disabled");
        assert!(reason.contains("open failed"), "{reason}");
        // The child could list the directory, so it could also have moved.
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
    }

    #[test]
    fn a_failed_move_aside_never_writes_the_path() {
        // The real rename-failure branch: a readable but unusable file in a
        // read-only (0555) directory. renameat2 fails with EACCES; nothing may
        // be written and persistence must be disabled.
        let dir = scratch_tmp("eacces-dir");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "not json").unwrap();
        chmod(&path, 0o644);
        chmod(&dir, 0o555);
        run_perm_child("perm_child_readonly_dir", &dir);
        chmod(&dir, 0o755);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn perm_child_readonly_dir() {
        let Some(dir) = perm_child("perm_child_readonly_dir") else { return };
        let opened = open(&dir.join("keymap.json"), STAMP);
        assert!(opened.recovered_from.is_none());
        let reason = opened.persist_disabled.expect("persistence disabled");
        assert!(reason.contains("could not be moved aside"), "{reason}");
        assert_eq!(opened.rows, cosmix_input_core::default_keymap().physical);
        assert_eq!(dir_names(&dir), vec!["keymap.json"]);
    }

    #[test]
    fn a_symlinked_bad_keymap_is_left_alone() {
        let dir = scratch("symlink");
        let target = dir.join("real.json");
        std::fs::write(&target, "not json").unwrap();
        let path = dir.join("keymap.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let opened = open(&path, STAMP);
        assert!(opened.recovered_from.is_none());
        let reason = opened.persist_disabled.expect("persistence disabled");
        assert!(reason.contains("symlink"), "{reason}");
        assert_eq!(std::fs::read_link(&path).unwrap(), target, "link untouched");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "not json");
        assert_eq!(dir_names(&dir), vec!["keymap.json", "real.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_move_moves_when_nothing_interferes() {
        let dir = scratch("link-ok");
        let (from, to) = (dir.join("keymap.json"), dir.join("backup"));
        std::fs::write(&from, "bad").unwrap();
        link_move(&from, &to, &mut |_| {}, &mut |p| std::fs::remove_file(p)).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "bad");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_move_never_unlinks_a_replacement() {
        // Another writer replaces the source between link and unlink: the new
        // link is dropped, the replacement survives, the move fails.
        let dir = scratch("link-swap");
        let (from, to) = (dir.join("keymap.json"), dir.join("backup"));
        std::fs::write(&from, "bad").unwrap();
        let error = link_move(
            &from,
            &to,
            &mut |p| replace_with_new_inode(p, "valid"),
            &mut |p| std::fs::remove_file(p),
        )
        .unwrap_err();
        assert!(error.to_string().contains("replaced during backup"), "{error}");
        assert!(!to.exists(), "the link to the old bytes was removed");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "valid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_move_removes_the_link_when_the_unlink_fails() {
        let dir = scratch("link-unlink");
        let (from, to) = (dir.join("keymap.json"), dir.join("backup"));
        std::fs::write(&from, "bad").unwrap();
        let error = link_move(&from, &to, &mut |_| {}, &mut |_| {
            Err(std::io::Error::other("injected unlink failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected"), "{error}");
        assert!(!to.exists(), "never left under two names");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "bad");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_move_never_clobbers_an_existing_target() {
        let dir = scratch("link-exists");
        let (from, to) = (dir.join("keymap.json"), dir.join("backup"));
        std::fs::write(&from, "bad").unwrap();
        std::fs::write(&to, "earlier backup").unwrap();
        let error =
            link_move(&from, &to, &mut |_| {}, &mut |p| std::fs::remove_file(p)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "earlier backup");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "bad");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_name_taken_mid_rename_is_never_clobbered() {
        let dir = scratch("clobber");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "original").unwrap();
        let first = dir.join(format!("keymap.json.bad-{STAMP}"));
        let mut intruded = false;
        let opened = open_with(&path, STAMP, &mut |stage, candidate| {
            // Another writer takes the chosen name after it was chosen.
            if stage == Stage::BeforeRename && !intruded {
                intruded = true;
                std::fs::write(candidate, "intruder").unwrap();
            }
        });
        let second = dir.join(format!("keymap.json.bad-{STAMP}-1"));
        assert_eq!(opened.recovered_from.as_deref(), Some(second.as_path()));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "intruder");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_replaced_before_the_move_aside_is_restored_and_loaded() {
        let dir = scratch("replaced");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "not json").unwrap();
        let mut replaced = false;
        let opened = open_with(&path, STAMP, &mut |stage, p| {
            // The operator fixes the file between the failed parse and the rename.
            if stage == Stage::AfterFailedLoad && !replaced {
                replaced = true;
                replace_with_new_inode(p, &doc(NEXT_WITHOUT));
            }
        });
        assert!(opened.recovered_from.is_none());
        assert!(opened.persist_disabled.is_none());
        assert_eq!(opened.rows.len(), 1);
        assert_eq!(opened.rows[0].action.as_str(), "desktop.workspace.next");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc(NEXT_WITHOUT));
        assert_eq!(dir_names(&dir), vec!["keymap.json"], "no backup of the good file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_reappears_before_seeding_is_never_overwritten() {
        let dir = scratch("reappear");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "not json").unwrap();
        let opened = open_with(&path, STAMP, &mut |stage, p| {
            if stage == Stage::BeforeSeed {
                std::fs::write(p, "reappeared").unwrap();
            }
        });
        let backup = dir.join(format!("keymap.json.bad-{STAMP}"));
        assert_eq!(opened.recovered_from.as_deref(), Some(backup.as_path()));
        let reason = opened.persist_disabled.expect("persistence disabled");
        assert!(reason.contains("reappeared"), "{reason}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "reappeared");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not json");
        assert_eq!(
            dir_names(&dir),
            vec!["keymap.json".to_string(), format!("keymap.json.bad-{STAMP}")],
            "no temp file left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_stamp_has_the_backup_shape() {
        let stamp = local_stamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-');
        assert!(stamp.bytes().enumerate().all(|(i, b)| i == 8 || b.is_ascii_digit()));
    }
}
