//! Does an icon name actually resolve for the desktop that will render it?
//!
//! A DBusMenu item carrying an unresolvable icon name renders with *no* icon —
//! strictly worse than the generic fallback it displaced. The host resolves the
//! name we publish against the *active* icon theme, so that is what we have to
//! resolve against too: an Adwaita-only name looks fine on a box that has
//! Adwaita installed and renders blank under Breeze.
//!
//! Theme inheritance makes this more than a directory scan — Breeze inherits
//! hicolor, so our own `dev.cosmix.tray` is found *through* Breeze — which is
//! why the lookup itself is delegated to `freedesktop-icons` rather than
//! hand-rolled against the spec.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A config file we are willing to read in full. `kdeglobals` is a few KiB in
/// practice; the cap exists so a pathological file cannot stall the
/// menu-refresh worker, which holds the refresh slot while it runs.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// The icon theme the host will resolve our published names against, resolved
/// once per menu refresh.
///
/// Deliberately NOT cached for process life: the tray outlives any number of
/// theme switches, and a frozen theme silently reintroduces exactly the bug
/// this module exists to prevent — names filtered against Breeze while the
/// session now renders with something else.
pub(crate) struct IconTheme {
    name: String,
}

impl IconTheme {
    /// Detection is deliberately cross-desktop: this skin is the portable
    /// fallback for everything that is not Plasma, so reading only kdeglobals
    /// would silently mis-resolve on GNOME or wlroots. `hicolor` is the
    /// spec-mandated last resort and every real theme inherits it.
    pub(crate) fn detect() -> Self {
        Self {
            name: detect_theme().unwrap_or_else(|| "hicolor".to_owned()),
        }
    }

    /// Whether `name` can be found as an icon in this theme.
    ///
    /// We publish DBusMenu `icon-name`, which is a *themed name* and nothing
    /// else. A value containing a separator is a path, and a path is not a
    /// themed name — the host looks it up in the theme, finds nothing, and
    /// renders blank. So a path-valued `Icon=` is rejected here and picks up
    /// the caller's generic fallback, which is the whole point of this filter.
    pub(crate) fn name_resolves(&self, name: &str) -> bool {
        if name.is_empty() || name.contains('/') {
            return false;
        }
        // No `.with_cache()`. That cache is process-global and stores misses as
        // well as hits, and this daemon is long-lived: one lookup before an
        // application's icon is installed would pin that name as unresolvable
        // for the rest of the session, so the item would keep falling back to
        // the generic icon across every refresh even though the artwork is now
        // on disk. Refreshes are user-paced and each does a handful of lookups,
        // so the cache buys nothing worth that.
        //
        // Dropping it does not make this fully live, and the residue is worth
        // naming: `freedesktop-icons` builds its registry of INSTALLED THEMES
        // in a process-global lazy value, so a theme installed after our first
        // lookup stays invisible for the rest of the session and names are then
        // filtered against hicolor instead. Files appearing inside an
        // already-known theme are picked up correctly, which is the case that
        // actually recurs (our own icons landing during a deploy). The
        // remaining one needs a tray restart.
        freedesktop_icons::lookup(name)
            .with_theme(&self.name)
            .find()
            .is_some()
    }

    /// Keep `icon` only if it resolves, so the caller's generic fallback
    /// applies to a name that would otherwise render as blank space.
    pub(crate) fn keep_if_resolvable(&self, icon: Option<String>) -> Option<String> {
        icon.filter(|name| self.name_resolves(name))
    }
}

/// Known limitation: on GNOME under Wayland the icon theme is authoritative in
/// GSettings, and `gtk-3.0/settings.ini` may not exist at all — so a host that
/// also carries a stale `kdeglobals` gets that stale theme here. Consulting
/// GSettings means a `gsettings`/GIO dependency for a desktop this skin only
/// serves as a portable fallback, and the failure it would prevent is one
/// cosmetic misfilter, so it is not worth the dependency. Reconsider if this
/// module ever becomes the primary path on GNOME.
fn detect_theme() -> Option<String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.to_ascii_lowercase().contains("kde") {
        return kde_theme().or_else(|| Some("breeze".to_owned()));
    }
    gtk_theme().or_else(kde_theme)
}

/// KDE's effective value is layered, and the layering is *not* simply
/// "user wins". Every file in the stack is applied from lowest precedence to
/// highest; a file may supply a value, declare the key immutable, or both. The
/// first immutability declaration freezes whatever the stack has resolved so
/// far — every higher file, the user's included, is then ignored.
///
/// That ordering is what makes a lock with no local value work, which is the
/// shape a locked-down deployment actually uses: an administrator drops
/// `[Icons][$i]` into a high-precedence system file to pin whatever the
/// *lower* system files chose. Measured against `kreadconfig6` on KF6 6.28 —
/// with a value in the low dir and a bare group lock in the high one, KConfig
/// returns the low dir's value, not the user's.
fn kde_theme() -> Option<String> {
    let stack = kde_globals_stack();
    let locale = kde_locale(&stack);
    resolve_layered(&stack, "Icons", "Theme", locale.as_deref())
}

/// The locale KConfig uses to pick between localised entries.
///
/// It is **not** `LC_ALL`/`LC_MESSAGES`/`LANG`. Measured against `kreadconfig6`
/// on KF6 6.28: with `Theme[en_AU]` present and `LC_ALL=en_AU.utf8` exported,
/// KConfig still returned the *untagged* value — and only started honouring the
/// tagged one once `[Locale] Language=en_AU` appeared in the same kdeglobals
/// stack. So the locale is config state resolved through the very same
/// precedence stack, and with no `Language` entry anywhere there is no locale at
/// all and only untagged entries are eligible. Reading the environment here was
/// simply reading the wrong source.
///
/// `Language` is a colon-separated preference list, but only its first entry
/// selects an entry — measured: `Language=en_AU:en_GB` with only `Theme[en_GB]`
/// present returned the untagged value, not the `en_GB` one.
fn kde_locale(stack: &[PathBuf]) -> Option<String> {
    let raw = resolve_layered(stack, "Locale", "Language", None)?;
    let first = raw.split(':').next().unwrap_or(&raw).trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// Fold a precedence stack, lowest first, into one effective value.
///
/// Split out from `kde_theme` so the layering can be tested against real files
/// without mutating process-global environment variables — the expectations in
/// those tests are transcribed from `kreadconfig6` runs, so they are a record of
/// what KConfig actually does rather than of what this function does.
fn resolve_layered(
    files: &[PathBuf],
    group: &str,
    key: &str,
    locale: Option<&str>,
) -> Option<String> {
    let mut value = None;
    for path in files {
        let Some(text) = read_config(path) else {
            continue;
        };
        let found = ini_value(&text, group, key, locale);
        if found.value.is_some() {
            value = found.value;
        }
        // Checked after the value is taken, so a file that both sets and locks
        // the key contributes its own value before shutting the door.
        if found.locked {
            break;
        }
    }
    value
}

/// The machine-wide KConfig floor, below every XDG file. Its name is assembled
/// at runtime inside KConfig, so `strings` on the shipped library does not show
/// it — which is exactly how an earlier round of this file talked itself out of
/// reading it. `strace` on `kreadconfig6` shows the `access("/etc/kde5rc", R_OK)`
/// plainly, and with a file bind-mounted there its value is returned.
const SYSTEM_RC: &str = "/etc/kde5rc";

/// Every file KConfig merges for a global key, lowest precedence first.
///
/// The whole `system.kdeglobals` family sits below the whole `kdeglobals`
/// family — they are **not** interleaved per directory, which is what this
/// function used to do. Measured by writing a distinct value into all six files
/// and deleting the winner one at a time; `kreadconfig6` walked them in exactly
/// this order:
///
/// ```text
/// /etc/kde5rc
/// $XDG_CONFIG_DIRS[n..0]/system.kdeglobals
/// $XDG_CONFIG_HOME/system.kdeglobals
/// $XDG_CONFIG_DIRS[n..0]/kdeglobals
/// $XDG_CONFIG_HOME/kdeglobals
/// ```
///
/// The per-directory interleaving got both halves wrong: a value in a
/// *lower-listed* directory's `kdeglobals` lost to a *higher-listed* one's
/// `system.kdeglobals`, and `$XDG_CONFIG_HOME/system.kdeglobals` — a real file
/// KDE writes — was never opened at all.
fn kde_globals_stack() -> Vec<PathBuf> {
    globals_stack(config_home().as_deref(), &config_dirs())
}

/// The ordering itself, split from the environment so it can be tested without
/// mutating process-global env vars from a parallel test thread.
fn globals_stack(home: Option<&Path>, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut stack = vec![PathBuf::from(SYSTEM_RC)];
    for name in ["system.kdeglobals", "kdeglobals"] {
        for dir in dirs {
            stack.push(dir.join(name));
        }
        if let Some(home) = home {
            stack.push(home.join(name));
        }
    }
    stack
}

fn gtk_theme() -> Option<String> {
    let base = config_home()?;
    for version in ["gtk-4.0", "gtk-3.0"] {
        let Some(text) = read_config(&base.join(version).join("settings.ini")) else {
            continue;
        };
        if let Some(value) = ini_value(&text, "Settings", "gtk-icon-theme-name", None).value {
            return Some(value);
        }
    }
    None
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
}

/// System config directories, lowest precedence first — the order KConfig
/// applies them in, which is the reverse of the `XDG_CONFIG_DIRS` listing.
fn config_dirs() -> Vec<PathBuf> {
    let raw = std::env::var("XDG_CONFIG_DIRS").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = raw
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect();
    if dirs.is_empty() {
        dirs.push(PathBuf::from("/etc/xdg"));
    }
    dirs.reverse();
    dirs
}

/// Read a config file, refusing anything that is not a regular file of bounded
/// size. The `is_file` check is what keeps a FIFO — which would block the
/// refresh worker on open — out of this path.
fn read_config(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let mut text = String::new();
    // Bounded again at read time: the metadata check above is advisory, since
    // the file can grow between the stat and the open.
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CONFIG_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// What one config file has to say about one key.
///
/// `value` and `locked` are independent on purpose. Collapsing them into
/// `Option<IniValue>` made a lock carrying no value unrepresentable, so a file
/// whose entire content was `[Icons][$i]` read as "nothing here" and the user
/// file went on to override the very key it had pinned.
struct IniLookup {
    value: Option<String>,
    locked: bool,
}

/// Read `key` from `[group]`. Written out rather than pulled from an INI crate
/// because the only hard parts — not reading a key out of the wrong group, and
/// honouring KDE's `[$i]` immutability marker — are a few lines, and getting
/// the first one wrong is how the initial attempt at this picked up a `Theme=`
/// belonging to `[Colors:Window]`.
fn ini_value(text: &str, group: &str, key: &str, locale: Option<&str>) -> IniLookup {
    let mut in_group = false;
    let mut locked = false;
    let mut value: Option<String> = None;
    // How well the winning entry's locale tag matched, so a localised entry can
    // outrank the plain one without a later plain one clawing it back.
    let mut best_rank = 0u8;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(heading) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // KDE marks immutability at three levels, and only the key level was
            // handled here. The other two are the ones a locked-down deployment
            // actually uses, and both used to read as "group not found": a
            // whole-file `[$i]` looked like a group named `$i`, and a
            // group-level `[Icons][$i]` looked like the nested group
            // `Icons`/`$i`. So an administrator who pinned the icon theme for
            // the whole machine got it silently ignored and the user file won —
            // the exact inversion this function exists to prevent.
            let mut segments = heading.split("][");
            let name = segments.next().unwrap_or_default().trim();
            let markers: Vec<&str> = segments.map(str::trim).collect();
            if name == "$i" && markers.is_empty() {
                // A bare `[$i]` heading is not a group at all; it locks the
                // whole file — including keys on either side of it, which is
                // why it sets the sticky flag rather than per-group state.
                locked = true;
                in_group = false;
                continue;
            }
            // `[Icons][Sub]` is still a nested group and still not ours — every
            // segment after the name has to be an immutability marker for this
            // to be the group itself.
            in_group = name == group && markers.iter().all(|marker| *marker == "$i");
            if in_group && markers.contains(&"$i") {
                locked = true;
            }
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((found_key, raw)) = line.split_once('=') else {
            continue;
        };
        let (bare, markers) = split_key_markers(found_key.trim());
        if bare != key {
            continue;
        }
        // An entry tagged for somebody else's locale is not this key at all, so
        // it contributes neither a value nor a lock. The lock used to be taken
        // FIRST, which meant a stray `Theme[$i][fr_FR]` in a system file froze
        // the whole stack for an en_AU desktop and pinned it to whatever the
        // lower files said. Measured against `kreadconfig6`: with no matching
        // `Language`, that entry is inert and the user's value still wins.
        let Some(rank) = locale_rank(&markers, locale) else {
            continue;
        };
        // A `$i` anywhere in the *applicable* key's markers locks it. Reading
        // the markers as a LIST is what fixed `Theme[$i][en_AU]`: the old single
        // `split_once('[')` took the whole tail as one opaque marker, matched it
        // against neither `$i` nor a bare locale, and discarded the entry — lock
        // and all. KConfig accepts the markers in either order.
        if markers.contains(&"$i") {
            locked = true;
        }
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        // Better locale match wins; among equals the later duplicate wins,
        // matching KConfig's last-one-wins read.
        if rank >= best_rank {
            best_rank = rank;
            value = Some(raw.to_owned());
        }
    }
    IniLookup { value, locked }
}

/// Split `Theme[$i][en_AU]` into `("Theme", ["$i", "en_AU"])`.
fn split_key_markers(key: &str) -> (&str, Vec<&str>) {
    let Some((bare, rest)) = key.split_once('[') else {
        return (key, Vec::new());
    };
    let markers = rest
        .trim_end()
        .trim_end_matches(']')
        .split("][")
        .map(str::trim)
        .filter(|marker| !marker.is_empty())
        .collect();
    (bare.trim_end(), markers)
}

/// How well this entry's locale tag fits the running locale, or `None` if it is
/// for some other locale and must be skipped entirely. Higher is better: an
/// exact `en_AU` beats the language-only `en`, which beats the untagged entry.
fn locale_rank(markers: &[&str], locale: Option<&str>) -> Option<u8> {
    let Some(tag) = markers.iter().find(|marker| **marker != "$i") else {
        return Some(1);
    };
    let locale = locale?;
    if *tag == locale {
        return Some(3);
    }
    let language = locale.split('_').next().unwrap_or(locale);
    if *tag == language {
        return Some(2);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(name: &str) -> IconTheme {
        IconTheme {
            name: name.to_owned(),
        }
    }

    fn value_of(text: &str) -> Option<String> {
        ini_value(text, "Icons", "Theme", None).value
    }

    fn locked_by(text: &str) -> bool {
        ini_value(text, "Icons", "Theme", None).locked
    }

    #[test]
    fn a_path_is_never_a_themed_name() {
        // We publish DBusMenu `icon-name`. Neither an absolute path nor a
        // relative one is a themed name, so both must reach the generic
        // fallback rather than be published and render blank — including a
        // path that really does exist on disk.
        let own_binary = std::env::current_exe().expect("test binary has a path");
        let theme = theme("hicolor");
        assert!(!theme.name_resolves(own_binary.to_str().expect("utf-8 path")));
        assert!(!theme.name_resolves("assets/logo.svg"));
        assert!(!theme.name_resolves("./Cargo.toml"));
    }

    #[test]
    fn an_empty_name_never_resolves() {
        assert!(!theme("hicolor").name_resolves(""));
    }

    #[test]
    fn a_name_no_theme_declares_does_not_resolve() {
        let theme = theme("hicolor");
        assert!(!theme.name_resolves("cosmix-tray-no-such-icon-exists-anywhere"));
        assert_eq!(
            theme.keep_if_resolvable(Some("cosmix-tray-no-such-icon".into())),
            None
        );
        assert_eq!(theme.keep_if_resolvable(None), None);
    }

    #[test]
    fn a_key_is_read_from_its_own_group_only() {
        let fixture = "\
[Colors:Window]
Theme=wrong-one

[Icons]
Theme=right-one
";
        assert_eq!(value_of(fixture), Some("right-one".to_owned()));
        assert!(ini_value(fixture, "Missing", "Theme", None).value.is_none());
        // A group that exists but lacks the key must not fall through.
        assert!(value_of("[Icons]\nOther=x\n").is_none());
        // A nested group is a different group.
        assert!(value_of("[Icons][Sub]\nTheme=nested\n").is_none());
    }

    #[test]
    fn a_blank_value_is_not_a_theme_name() {
        assert!(value_of("[Icons]\nTheme=\n").is_none());
        assert!(value_of("[Icons]\nTheme=   \n").is_none());
    }

    #[test]
    fn a_comment_is_not_a_key() {
        assert!(value_of("[Icons]\n#Theme=commented\n").is_none());
        assert!(value_of("[Icons]\n;Theme=commented\n").is_none());
    }

    #[test]
    fn kde_markers_are_understood() {
        // Immutable is still our key, and is flagged so the user file cannot
        // override it.
        let locked = ini_value("[Icons]\nTheme[$i]=locked\n", "Icons", "Theme", None);
        assert_eq!(locked.value.as_deref(), Some("locked"));
        assert!(locked.locked);
        let plain = ini_value("[Icons]\nTheme=breeze\n", "Icons", "Theme", None);
        assert_eq!(plain.value.as_deref(), Some("breeze"));
        assert!(!plain.locked);
    }

    #[test]
    fn group_and_file_immutability_are_honoured() {
        // `[Icons][$i]` locks every key in the group. This used to parse as the
        // nested group `Icons`/`$i`, so the value was not found at all and the
        // user file silently won.
        let group = ini_value("[Icons][$i]\nTheme=locked-group\n", "Icons", "Theme", None);
        assert_eq!(group.value.as_deref(), Some("locked-group"));
        assert!(group.locked);

        // A bare `[$i]` locks the whole file, including keys written before it.
        assert!(locked_by("[Icons]\nTheme=locked-file\n[$i]\n"));
        let after = ini_value("[$i]\n[Icons]\nTheme=locked-file\n", "Icons", "Theme", None);
        assert_eq!(after.value.as_deref(), Some("locked-file"));
        assert!(after.locked);

        // And the nested-group case still has to stay excluded, since it is
        // told apart from `[Icons][$i]` only by the marker text.
        assert!(value_of("[Icons][Sub][$i]\nTheme=nested\n").is_none());
        assert!(!locked_by("[Icons][Sub][$i]\nTheme=nested\n"));
    }

    #[test]
    fn a_lock_carrying_no_value_still_locks() {
        // The shape a locked-down deployment actually uses: a high-precedence
        // system file that pins the key without setting it, so whatever the
        // lower system files chose survives the user file. Verified against
        // `kreadconfig6` on KF6 6.28 — it returns the lower file's value, not
        // the user's. Collapsing value and lock into one Option made this
        // unrepresentable and the user file won.
        let group = ini_value("[Icons][$i]\n", "Icons", "Theme", None);
        assert!(group.value.is_none());
        assert!(group.locked);

        let file = ini_value("[$i]\n[Icons]\nOther=x\n", "Icons", "Theme", None);
        assert!(file.value.is_none());
        assert!(file.locked);
    }

    #[test]
    fn locale_and_immutability_markers_combine() {
        // KConfig accepts the markers in either order, and the old
        // `split_once('[')` took the whole tail as one opaque marker — so both
        // of these were discarded outright, taking the lock with them.
        //
        // The lock is only in force when the locale APPLIES, which is why the
        // locale is passed here: see `an_entry_for_another_locale_does_not_lock`
        // for the other half of the rule.
        for text in [
            "[Icons]\nTheme[$i][en_AU]=locked-and-localised\n",
            "[Icons]\nTheme[en_AU][$i]=locked-and-localised\n",
        ] {
            let found = ini_value(text, "Icons", "Theme", Some("en_AU"));
            assert!(found.locked, "{text}");
            assert_eq!(
                found.value.as_deref(),
                Some("locked-and-localised"),
                "{text}"
            );
            assert!(!locked_by(text), "inert without a matching locale: {text}");
        }
    }

    #[test]
    fn an_entry_for_another_locale_does_not_lock() {
        // The bug this replaced: the `$i` was taken BEFORE the locale was
        // checked, so a `Theme[$i][fr_FR]` sitting in a system file froze the
        // whole stack on an en_AU desktop and pinned it to the lower files'
        // value. kreadconfig6 with no matching Language returns the user's
        // value, so the foreign entry must be wholly inert — no value, no lock.
        let foreign = ini_value("[Icons]\nTheme[$i][fr_FR]=nope\n", "Icons", "Theme", None);
        assert!(foreign.value.is_none());
        assert!(!foreign.locked, "a foreign-locale entry must not lock");

        let still_foreign = ini_value(
            "[Icons]\nTheme[$i][fr_FR]=nope\n",
            "Icons",
            "Theme",
            Some("en_AU"),
        );
        assert!(!still_foreign.locked);

        // ...but the matching one both supplies and locks. kreadconfig6 with
        // `Language=en_AU` returns this value over a user file's.
        let matching = ini_value(
            "[Icons]\nTheme[$i][en_AU]=yes\n",
            "Icons",
            "Theme",
            Some("en_AU"),
        );
        assert_eq!(matching.value.as_deref(), Some("yes"));
        assert!(matching.locked);
    }

    #[test]
    fn the_globals_stack_groups_the_families_rather_than_interleaving_them() {
        // Transcribed from the measurement: a distinct value in all six files,
        // then delete the winner and re-read. kreadconfig6 walked them in this
        // order, so every `system.kdeglobals` ranks below every `kdeglobals`.
        let home = PathBuf::from("/home/u/.config");
        // `config_dirs` hands these over already reversed, lowest first.
        let dirs = vec![
            PathBuf::from("/etc/xdg-low"),
            PathBuf::from("/etc/xdg-high"),
        ];
        assert_eq!(
            globals_stack(Some(&home), &dirs),
            vec![
                PathBuf::from("/etc/kde5rc"),
                PathBuf::from("/etc/xdg-low/system.kdeglobals"),
                PathBuf::from("/etc/xdg-high/system.kdeglobals"),
                PathBuf::from("/home/u/.config/system.kdeglobals"),
                PathBuf::from("/etc/xdg-low/kdeglobals"),
                PathBuf::from("/etc/xdg-high/kdeglobals"),
                PathBuf::from("/home/u/.config/kdeglobals"),
            ]
        );
        // No HOME still leaves the machine-wide floor and the system dirs.
        assert_eq!(globals_stack(None, &[])[0], PathBuf::from("/etc/kde5rc"));
        assert_eq!(globals_stack(None, &[]).len(), 1);
    }

    #[test]
    fn the_locale_comes_from_the_config_not_the_environment() {
        let dir = std::env::temp_dir().join(format!("cosmix-tray-locale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("fixture");
            path
        };

        // Only the FIRST entry of the colon-separated list selects an entry —
        // measured: `Language=en_AU:en_GB` with only a `[en_GB]` value present
        // returned the untagged one.
        let list = write("list.kdeglobals", "[Locale]\nLanguage=en_AU:en_GB\n");
        assert_eq!(
            kde_locale(std::slice::from_ref(&list)).as_deref(),
            Some("en_AU")
        );

        // No `Language` anywhere means no locale at all, so only untagged
        // entries are eligible. This is the case that matters: KConfig does not
        // fall back to LC_ALL/LANG, which is what this code used to read.
        let none = write("none.kdeglobals", "[Icons]\nTheme=x\n");
        assert_eq!(kde_locale(&[none]), None);
        let blank = write("blank.kdeglobals", "[Locale]\nLanguage=\n");
        assert_eq!(kde_locale(&[blank]), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn locale_tagged_values_rank_above_the_plain_one() {
        // Exact territory beats language-only beats untagged; a foreign locale
        // is skipped rather than accepted.
        assert_eq!(locale_rank(&["en_AU"], Some("en_AU")), Some(3));
        assert_eq!(locale_rank(&["en"], Some("en_AU")), Some(2));
        assert_eq!(locale_rank(&[], Some("en_AU")), Some(1));
        assert_eq!(locale_rank(&["de_DE"], Some("en_AU")), None);
        // `$i` is not a locale tag, so a locked untagged entry still ranks as
        // untagged rather than being read as locale `$i`.
        assert_eq!(locale_rank(&["$i"], Some("en_AU")), Some(1));
        // With no locale set, only the untagged entry is eligible.
        assert_eq!(locale_rank(&["de_DE"], None), None);
        assert_eq!(locale_rank(&[], None), Some(1));
    }

    #[test]
    fn key_markers_split_into_a_list() {
        assert_eq!(split_key_markers("Theme"), ("Theme", vec![]));
        assert_eq!(split_key_markers("Theme[$i]"), ("Theme", vec!["$i"]));
        assert_eq!(
            split_key_markers("Theme[$i][en_AU]"),
            ("Theme", vec!["$i", "en_AU"])
        );
        assert_eq!(
            split_key_markers("Theme [en_AU] "),
            ("Theme", vec!["en_AU"])
        );
    }

    /// Every expectation here was transcribed from a `kreadconfig6` run on
    /// KF6 6.28 with `XDG_CONFIG_HOME` and `XDG_CONFIG_DIRS` pointed at a
    /// scratch tree — so a failure means this diverged from KConfig, not from
    /// somebody's reading of the docs.
    #[test]
    fn the_layering_matches_what_kconfig_actually_does() {
        let dir = std::env::temp_dir().join(format!("cosmix-tray-kconfig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("fixture");
            path
        };
        let resolve = |files: &[PathBuf]| resolve_layered(files, "Icons", "Theme", None);

        // Precedence within one directory: `system.kdeglobals` sits BELOW
        // `kdeglobals`, which sits below the user file. kreadconfig6:
        // "FromKdeglobals".
        let system = write("system.kdeglobals", "[Icons]\nTheme=FromSystemKdeglobals\n");
        let global = write("kdeglobals", "[Icons]\nTheme=FromKdeglobals\n");
        assert_eq!(
            resolve(&[system.clone(), global.clone()]).as_deref(),
            Some("FromKdeglobals")
        );

        // An UNLOCKED system value loses to the user file. kreadconfig6:
        // "UserWins".
        let user = write("user.kdeglobals", "[Icons]\nTheme=UserWins\n");
        assert_eq!(
            resolve(&[system.clone(), global.clone(), user.clone()]).as_deref(),
            Some("UserWins")
        );

        // A LOCKED one wins outright, from `system.kdeglobals` no less — the
        // file this code never used to open. kreadconfig6: "LockedSystem".
        let locked = write("locked.kdeglobals", "[Icons]\nTheme[$i]=LockedSystem\n");
        assert_eq!(
            resolve(&[locked, global.clone(), user.clone()]).as_deref(),
            Some("LockedSystem")
        );

        // The case that motivated splitting value from lock: a high-precedence
        // file that locks the key WITHOUT setting it. The value comes from the
        // lower system file and the user file is shut out entirely.
        // kreadconfig6: "LowPapirus" for both the group and the file lock.
        let low = write("low.kdeglobals", "[Icons]\nTheme=LowPapirus\n");
        for marker in ["[Icons][$i]\n", "[$i]\n"] {
            let high = write("high.kdeglobals", marker);
            assert_eq!(
                resolve(&[low.clone(), high, user.clone()]).as_deref(),
                Some("LowPapirus"),
                "lock marker {marker:?}"
            );
        }

        // A file that is missing entirely is skipped, not treated as a lock.
        assert_eq!(
            resolve(&[dir.join("absent.kdeglobals"), user.clone()]).as_deref(),
            Some("UserWins")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bounded_read_refuses_a_non_regular_file() {
        // A directory stands in for the FIFO case: both are non-regular, and
        // the point is that neither is opened.
        assert_eq!(read_config(Path::new("/tmp")), None);
        assert_eq!(
            read_config(Path::new("/nonexistent/definitely/not/here")),
            None
        );
    }
}
