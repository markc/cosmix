//! Durable Foreman state path resolution.
//!
//! The ledger anchors the governor's stop file. Operational units continue
//! to select the clone explicitly with `--repo`; this module only removes the
//! unsafe cwd-relative default. An existing legacy cwd ledger is still
//! honoured with a deprecation note; a new ledger is never created there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::ledger::{Ledger, LedgerCreate};

pub const LEDGER_FILE: &str = "ledger.db";
const LEGACY_DB_RELPATH: &str = ".foreman/ledger.db";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbSource {
    Explicit,
    StateDirectory,
    LegacyCwd,
    Var,
}

/// SQLite creation authority inherited by Foreman child processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DbCreateMode {
    ParentsAndFile,
    FileOnly,
    Never,
}

impl DbCreateMode {
    pub fn as_cli_value(self) -> &'static str {
        match self {
            Self::ParentsAndFile => "parents-and-file",
            Self::FileOnly => "file-only",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedDbPath {
    path: PathBuf,
    source: DbSource,
    create: DbCreateMode,
}

/// Resolve the ledger path in operator-precedence order.
///
/// `--db` and `FOREMAN_DB` are explicit file paths. `STATE_DIRECTORY` is the
/// directory systemd creates for the service. An existing legacy cwd ledger
/// wins over the final XDG/FHS fallback, but this resolver never creates a
/// cwd-relative ledger. Only explicitly named ledger paths may have missing
/// parent directories created later by [`ResolvedDbPath::open`].
pub fn db_path(
    explicit: Option<&Path>,
    inherited_create: Option<DbCreateMode>,
) -> Result<ResolvedDbPath> {
    anyhow::ensure!(
        inherited_create.is_none() || explicit.is_some(),
        "--db-create requires --db"
    );
    let cwd = std::env::current_dir().context("resolving current directory for legacy ledger")?;
    let mut resolved = db_path_with(
        explicit,
        |key| std::env::var_os(key),
        current_uid(),
        &cwd,
        |path| {
            path.try_exists()
                .with_context(|| format!("checking {}", path.display()))
        },
    )?;
    if let Some(create) = inherited_create {
        resolved.create = create;
    }

    if resolved.source == DbSource::LegacyCwd {
        eprintln!(
            "foreman: deprecated cwd-relative ledger {}; select it explicitly with --db or FOREMAN_DB",
            resolved.path.display()
        );
    }
    Ok(resolved)
}

impl ResolvedDbPath {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_mode(&self) -> DbCreateMode {
        self.create
    }

    /// Open the selected ledger with the create authority carried by its
    /// resolution rung. Explicit paths may create parents. Systemd and
    /// XDG/FHS paths may create only the ledger file in an existing parent.
    /// A selected legacy ledger must still exist at the actual SQLite open.
    pub fn open(&self) -> Result<Ledger> {
        self.open_for_project(None)
    }

    /// Open and, when a manifest is active, atomically stamp/check the
    /// ledger's project-name and repository-history pair before returning it
    /// to the command.
    pub fn open_for_project(&self, project_identity: Option<(&str, &str)>) -> Result<Ledger> {
        let create = match self.create {
            DbCreateMode::ParentsAndFile => LedgerCreate::ParentsAndFile,
            DbCreateMode::FileOnly => {
                require_existing_parent(&self.path)?;
                LedgerCreate::FileOnly
            }
            DbCreateMode::Never => LedgerCreate::Never,
        };
        Ledger::open_with_create_for_project(&self.path, create, project_identity)
    }

    /// Refuse state-using commands before [`crate::ledger::Ledger::open`] can
    /// create a directory selected by an implicit environment or fallback
    /// path. Stateless commands may resolve the path without requiring it.
    pub fn require_existing_implicit_parent(&self) -> Result<()> {
        if self.create != DbCreateMode::ParentsAndFile {
            require_existing_parent(&self.path)?;
        }
        Ok(())
    }
}

fn db_path_with(
    explicit: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
    uid: u32,
    cwd: &Path,
    exists: impl Fn(&Path) -> Result<bool>,
) -> Result<ResolvedDbPath> {
    if let Some(path) = explicit {
        return Ok(ResolvedDbPath {
            path: path.to_path_buf(),
            source: DbSource::Explicit,
            create: DbCreateMode::ParentsAndFile,
        });
    }
    if let Some(path) = env("FOREMAN_DB") {
        return Ok(ResolvedDbPath {
            path: PathBuf::from(path),
            source: DbSource::Explicit,
            create: DbCreateMode::ParentsAndFile,
        });
    }
    if let Some(dir) = env("STATE_DIRECTORY") {
        return Ok(ResolvedDbPath {
            path: absolute_dir("STATE_DIRECTORY", dir)?.join(LEDGER_FILE),
            source: DbSource::StateDirectory,
            create: DbCreateMode::FileOnly,
        });
    }

    let legacy = cwd.join(LEGACY_DB_RELPATH);
    if exists(&legacy)? {
        return Ok(ResolvedDbPath {
            path: legacy,
            source: DbSource::LegacyCwd,
            create: DbCreateMode::Never,
        });
    }

    let var = if let Some(dir) = env("COSMIX_VAR") {
        absolute_dir("COSMIX_VAR", dir)?
    } else if uid == 0 {
        PathBuf::from("/var/lib/cosmix")
    } else if let Some(dir) = env("XDG_DATA_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            dir.join("cosmix")
        } else {
            user_var_dir(&env)?
        }
    } else {
        user_var_dir(&env)?
    };
    Ok(ResolvedDbPath {
        path: var.join("foreman").join(LEDGER_FILE),
        source: DbSource::Var,
        create: DbCreateMode::FileOnly,
    })
}

fn require_existing_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| format!("implicit ledger path {} has no parent", path.display()))?;
    anyhow::ensure!(
        parent.is_dir(),
        "implicit ledger directory {} does not exist; create it first or select a ledger with --db or FOREMAN_DB",
        parent.display()
    );
    Ok(())
}

fn user_var_dir(env: &impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let Some(home) = env("HOME") else {
        bail!(
            "cannot resolve Foreman state: HOME is unset; set --db, FOREMAN_DB, STATE_DIRECTORY, or COSMIX_VAR"
        );
    };
    absolute_dir("HOME", home).map(|home| home.join(".local/share/cosmix"))
}

fn absolute_dir(name: &str, value: OsString) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        bail!("{name} must be an absolute path, got {}", path.display());
    }
    Ok(path)
}

fn current_uid() -> u32 {
    // SAFETY: getuid(2) takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn resolve(
        explicit: Option<&Path>,
        entries: &[(&str, &str)],
        uid: u32,
        legacy_exists: bool,
    ) -> Result<ResolvedDbPath> {
        let env: BTreeMap<&str, OsString> = entries
            .iter()
            .map(|(key, value)| (*key, OsString::from(value)))
            .collect();
        db_path_with(
            explicit,
            |key| env.get(key).cloned(),
            uid,
            Path::new("/work"),
            |_| Ok(legacy_exists),
        )
    }

    #[test]
    fn precedence_is_flag_then_foreman_then_systemd_then_cosmix() {
        let explicit = Path::new("/flag/ledger.db");
        let env = [
            ("FOREMAN_DB", "/env/ledger.db"),
            ("STATE_DIRECTORY", "/state"),
            ("COSMIX_VAR", "/var-override"),
        ];
        assert_eq!(
            resolve(Some(explicit), &env, 1000, true).unwrap().path,
            explicit.to_path_buf()
        );
        assert_eq!(
            resolve(None, &env, 1000, true).unwrap().path,
            PathBuf::from("/env/ledger.db")
        );
        assert_eq!(
            resolve(None, &env[1..], 1000, true).unwrap().path,
            PathBuf::from("/state/ledger.db")
        );
        assert_eq!(
            resolve(None, &env[2..], 1000, false).unwrap().path,
            PathBuf::from("/var-override/foreman/ledger.db")
        );
    }

    #[test]
    fn existing_legacy_ledger_wins_only_after_explicit_and_systemd_paths() {
        let resolved = resolve(None, &[("COSMIX_VAR", "/var-override")], 1000, true).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/work/.foreman/ledger.db"));
        assert_eq!(resolved.source, DbSource::LegacyCwd);

        let resolved = resolve(None, &[("COSMIX_VAR", "/var-override")], 1000, false).unwrap();
        assert_eq!(
            resolved.path,
            PathBuf::from("/var-override/foreman/ledger.db")
        );
        assert_eq!(resolved.source, DbSource::Var);
    }

    #[test]
    fn final_fallback_is_xdg_for_users_and_fhs_for_root() {
        assert_eq!(
            resolve(
                None,
                &[("HOME", "/users/operator"), ("XDG_DATA_HOME", "/state")],
                1000,
                false,
            )
            .unwrap(),
            ResolvedDbPath {
                path: PathBuf::from("/state/cosmix/foreman/ledger.db"),
                source: DbSource::Var,
                create: DbCreateMode::FileOnly,
            }
        );
        assert_eq!(
            resolve(None, &[("HOME", "/root")], 0, false).unwrap().path,
            PathBuf::from("/var/lib/cosmix/foreman/ledger.db")
        );
    }

    #[test]
    fn derived_roots_cannot_fall_back_to_cwd() {
        let error = resolve(None, &[("STATE_DIRECTORY", "relative")], 1000, false).unwrap_err();
        assert!(error.to_string().contains("absolute"));

        let error = resolve(None, &[], 1000, false).unwrap_err();
        assert!(error.to_string().contains("HOME is unset"));
    }

    #[test]
    fn vanished_legacy_ledger_is_refused_at_open_and_not_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_dir = tmp.path().join(".foreman");
        let legacy = legacy_dir.join(LEDGER_FILE);
        std::fs::create_dir(&legacy_dir).unwrap();
        drop(Ledger::open(&legacy).unwrap());

        let resolved = db_path_with(
            None,
            |_| None,
            1000,
            tmp.path(),
            |path| path.try_exists().map_err(Into::into),
        )
        .unwrap();
        assert_eq!(resolved.source, DbSource::LegacyCwd);
        assert_eq!(resolved.create_mode(), DbCreateMode::Never);
        std::fs::remove_file(&legacy).unwrap();

        let error = match resolved.open() {
            Ok(_) => panic!("vanished implicit ledger must not be recreated"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains(legacy.to_str().unwrap()), "{message}");
        assert!(message.contains("--db"), "{message}");
        assert!(message.contains("FOREMAN_DB"), "{message}");
        assert!(!legacy.exists());
    }
}
