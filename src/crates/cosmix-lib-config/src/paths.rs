//! Centralized FHS-aligned path resolver for the cosmix stack.
//!
//! All path resolution flows through `cosmix_path(kind)`, which checks
//! a dedicated env var first, then falls back to XDG user dirs or system
//! defaults depending on context.
//!
//! **Everything is keyed off one root, `$COSMIX`.** The root is found by:
//!
//! 1. the `COSMIX` environment variable;
//! 2. self-location — an ancestor of the running binary that holds both
//!    `bootstrap` and `src/Cargo.toml` (so `$COSMIX/bin/<daemon>` and a
//!    `cargo run` binary under `$COSMIX/src/target/…` both find their own
//!    checkout with no environment at all);
//! 3. otherwise the root is *unknown* and the legacy FHS/XDG defaults apply,
//!    which is what a system install at `/opt/cosmix/bin` gets.
//!
//! | Kind | Env override | Root known      | Root unknown: user (XDG) · system  |
//! |------|--------------|-----------------|------------------------------------|
//! | Src  | COSMIX_SRC   | `$COSMIX/src`   | `~/Projects/cosmix/src` (both)     |
//! | Etc  | COSMIX_ETC   | `$COSMIX/etc`   | ~/.config/cosmix/ · /etc/cosmix/   |
//! | Var  | COSMIX_VAR   | `$COSMIX/var`   | ~/.local/share/cosmix/ · /var/lib/cosmix/ |
//! | Bin  | COSMIX_BIN   | `$COSMIX/bin`   | ~/.local/bin/ · /usr/local/bin/    |
//! | Run  | COSMIX_RUN   | `$COSMIX/run`   | $XDG_RUNTIME_DIR/cosmix/ · /run/cosmix/ |
//! | Log  | COSMIX_LOG   | `$COSMIX/log`   | $COSMIX_VAR/log/ · /var/log/cosmix/ |
//! | Tmp  | COSMIX_TMP   | `$COSMIX/tmp`   | /tmp/cosmix/ (both)                |
//!
//! `cosmix-mix` carries a verbatim copy of the root rule in its
//! `cosmix_paths.rs` (mix must not depend on this crate); keep them in step.

use std::path::PathBuf;
use std::sync::OnceLock;

use directories::{BaseDirs, ProjectDirs};

/// Directory categories for the cosmix stack, mapped to FHS conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CosmixDir {
    /// Source tree (project root).
    Src,
    /// Configuration files.
    Etc,
    /// Persistent variable data (databases, blobs).
    Var,
    /// Installed binaries.
    Bin,
    /// Runtime sockets and PIDs.
    Run,
    /// Log files.
    Log,
    /// Temporary/ephemeral files.
    Tmp,
}

/// Resolved paths for all 7 directory categories.
struct ResolvedPaths {
    root: Option<PathBuf>,
    src: PathBuf,
    etc: PathBuf,
    var: PathBuf,
    bin: PathBuf,
    run: PathBuf,
    log: PathBuf,
    tmp: PathBuf,
}

static PATHS: OnceLock<ResolvedPaths> = OnceLock::new();

/// Resolve a cosmix directory path. Cached after first call.
///
/// Resolution order: `COSMIX_<KIND>` env var → `$COSMIX/<kind>` when the
/// root is known → XDG user dir → system default.
pub fn cosmix_path(kind: CosmixDir) -> PathBuf {
    let paths = PATHS.get_or_init(resolve_all);
    match kind {
        CosmixDir::Src => paths.src.clone(),
        CosmixDir::Etc => paths.etc.clone(),
        CosmixDir::Var => paths.var.clone(),
        CosmixDir::Bin => paths.bin.clone(),
        CosmixDir::Run => paths.run.clone(),
        CosmixDir::Log => paths.log.clone(),
        CosmixDir::Tmp => paths.tmp.clone(),
    }
}

/// Real user ID of the current process.
///
/// The audited home for the `getuid(2)` FFI call for every crate that
/// already depends on `cosmix-lib-config`. Callers that branch on
/// root-vs-user (path resolution, `/run/user/<uid>` socket paths) route
/// through this instead of each open-coding `unsafe { libc::getuid() }`
/// — one reviewed `unsafe` boundary instead of the scattered ones.
/// `getuid` is always successful and cannot fail (POSIX), so the
/// wrapper is infallible.
///
/// One deliberate exception: `cosmix-lib-bus`'s `native_port` keeps its
/// own open-coded call. Routing it here would add a
/// `cosmix-lib-bus → cosmix-lib-config` edge, and `cosmix-lib-config`
/// already depends on `cosmix-lib-mix`; lib-bus must stay free of that
/// subtree. That is the only intentional FFI duplicate — every other
/// crate routes through here.
pub fn current_uid() -> u32 {
    // SAFETY: `getuid()` takes no arguments, never fails, and returns
    // a plain `uid_t` — it cannot violate any memory or thread safety
    // invariant. POSIX guarantees it is async-signal-safe.
    unsafe { libc::getuid() }
}

fn resolve_all() -> ResolvedPaths {
    let user_mode = current_uid() != 0;
    let base = BaseDirs::new();
    let proj = ProjectDirs::from("", "", "cosmix");
    let home = base
        .as_ref()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/root"));

    let exe = std::env::current_exe().ok();
    let root = locate_root(std::env::var_os("COSMIX").map(PathBuf::from), exe.as_deref());

    let src = env_or("COSMIX_SRC", || {
        root.clone().unwrap_or_else(|| default_root(&home)).join("src")
    });

    let etc = env_or("COSMIX_ETC", || match &root {
        Some(r) => r.join("etc"),
        None if user_mode => proj
            .as_ref()
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| home.join(".config/cosmix")),
        None => PathBuf::from("/etc/cosmix"),
    });

    let var = env_or("COSMIX_VAR", || match &root {
        Some(r) => r.join("var"),
        None if user_mode => proj
            .as_ref()
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| home.join(".local/share/cosmix")),
        None => PathBuf::from("/var/lib/cosmix"),
    });

    let bin = env_or("COSMIX_BIN", || match &root {
        Some(r) => r.join("bin"),
        None if user_mode => home.join(".local/bin"),
        None => PathBuf::from("/usr/local/bin"),
    });

    let run = env_or("COSMIX_RUN", || match &root {
        Some(r) => r.join("run"),
        None if user_mode => base
            .as_ref()
            .and_then(|d| d.runtime_dir().map(|r| r.join("cosmix")))
            .unwrap_or_else(|| PathBuf::from("/tmp/cosmix-run")),
        None => PathBuf::from("/run/cosmix"),
    });

    let log = env_or("COSMIX_LOG", || match &root {
        Some(r) => r.join("log"),
        None if user_mode => var.join("log"),
        None => PathBuf::from("/var/log/cosmix"),
    });

    let tmp = env_or("COSMIX_TMP", || match &root {
        Some(r) => r.join("tmp"),
        None => PathBuf::from("/tmp/cosmix"),
    });

    ResolvedPaths {
        root,
        src,
        etc,
        var,
        bin,
        run,
        log,
        tmp,
    }
}

fn env_or(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(fallback)
}

/// The install root (`$COSMIX`) when it is known — from the environment
/// or by self-location — else `None`. Cached with the rest.
pub fn cosmix_root() -> Option<PathBuf> {
    PATHS.get_or_init(resolve_all).root.clone()
}

/// Default root when nothing names one: the documented clone location.
pub fn default_root(home: &std::path::Path) -> PathBuf {
    home.join("Projects/cosmix")
}

/// A directory is a CosMix root iff it carries the two files every
/// checkout has and no runtime tree does.
fn is_root(dir: &std::path::Path) -> bool {
    dir.join("bootstrap").is_file() && dir.join("src/Cargo.toml").is_file()
}

/// Root resolution: `$COSMIX`, else the nearest of the running binary's
/// first six ancestors that [`is_root`] (covers `$COSMIX/bin/<exe>` and
/// `$COSMIX/src/target/<profile>/<exe>`).
pub fn locate_root(env_root: Option<PathBuf>, exe: Option<&std::path::Path>) -> Option<PathBuf> {
    if let Some(root) = env_root {
        return Some(root);
    }
    exe?.ancestors()
        .skip(1)
        .take(6)
        .find(|d| is_root(d))
        .map(std::path::Path::to_path_buf)
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn env_root_wins_over_self_location() {
        assert_eq!(
            locate_root(Some(PathBuf::from("/srv/cosmix")), Some(Path::new("/nowhere/bin/noded"))),
            Some(PathBuf::from("/srv/cosmix"))
        );
    }

    #[test]
    fn self_location_finds_the_checkout_above_bin_and_above_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("CosMix");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("src/target/release")).unwrap();
        std::fs::write(root.join("bootstrap"), "").unwrap();
        std::fs::write(root.join("src/Cargo.toml"), "").unwrap();
        assert_eq!(locate_root(None, Some(&root.join("bin/noded"))), Some(root.clone()));
        assert_eq!(
            locate_root(None, Some(&root.join("src/target/release/noded"))),
            Some(root.clone())
        );
        assert_eq!(locate_root(None, Some(Path::new("/opt/cosmix/bin/noded"))), None);
    }
}
