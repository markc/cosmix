//! Mix-local cosmix path resolver. Inlined so mix has no dependency
//! on the cos-side `cosmix-lib-config` crate; behaviour-parity with
//! that crate's `cosmix_root()` / `cosmix_path()` / `current_uid()`.
//! The rules match so a `mix` invocation and any cos daemon running on
//! the same node land on the same paths.
//!
//! **Everything is keyed off one root, `$COSMIX`.** The root is found by:
//!
//! 1. the `COSMIX` environment variable;
//! 2. self-location — an ancestor of the running binary that holds both
//!    `bootstrap` and `src/Cargo.toml` (so `$COSMIX/bin/mix` and a
//!    `cargo run` binary under `$COSMIX/src/target/…` both find their
//!    own checkout with no environment at all);
//! 3. otherwise the root is *unknown*.
//!
//! | Kind | Env override | Root known        | Root unknown (legacy FHS/XDG)        |
//! |------|--------------|-------------------|--------------------------------------|
//! | Src  | COSMIX_SRC   | `$COSMIX/src`     | `~/Projects/cosmix/src`              |
//! | Etc  | COSMIX_ETC   | `$COSMIX/etc`     | `~/.config/cosmix/` · `/etc/cosmix/` |
//! | Bin  | COSMIX_BIN   | `$COSMIX/bin`     | `~/.local/bin/` · `/usr/local/bin/`  |
//!
//! A system install (`/opt/cosmix/bin/mix`, no `$COSMIX`, no checkout
//! above it) therefore keeps the FHS defaults it always had. Mix keeps
//! only Src/Etc/Bin from the parent's full enum.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CosmixDir {
    Src,
    Etc,
    Bin,
}

struct ResolvedPaths {
    root: Option<PathBuf>,
    src: PathBuf,
    etc: PathBuf,
    bin: PathBuf,
}

static PATHS: OnceLock<ResolvedPaths> = OnceLock::new();

pub fn cosmix_path(kind: CosmixDir) -> PathBuf {
    let paths = PATHS.get_or_init(resolve_all);
    match kind {
        CosmixDir::Src => paths.src.clone(),
        CosmixDir::Etc => paths.etc.clone(),
        CosmixDir::Bin => paths.bin.clone(),
    }
}

/// The install root (`$COSMIX`) when it is known — from the environment
/// or by self-location — else `None`. Cached.
pub fn cosmix_root() -> Option<PathBuf> {
    PATHS.get_or_init(resolve_all).root.clone()
}

pub fn cosmix_src() -> PathBuf {
    cosmix_path(CosmixDir::Src)
}

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Default root when nothing names one: the documented clone location.
pub fn default_root(home: &Path) -> PathBuf {
    home.join("Projects/cosmix")
}

/// A directory is a CosMix root iff it carries the two files every
/// checkout has and no runtime tree does.
fn is_root(dir: &Path) -> bool {
    dir.join("bootstrap").is_file() && dir.join("src/Cargo.toml").is_file()
}

/// Root resolution shared with `cosmix-lib-config`: `$COSMIX`, else the
/// nearest of the running binary's first six ancestors that [`is_root`].
pub fn locate_root(env_root: Option<PathBuf>, exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = env_root {
        return Some(root);
    }
    exe?.ancestors().skip(1).take(6).find(|d| is_root(d)).map(Path::to_path_buf)
}

fn resolve_all() -> ResolvedPaths {
    let user_mode = current_uid() != 0;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root"));
    let exe = std::env::current_exe().ok();
    let root = locate_root(std::env::var_os("COSMIX").map(PathBuf::from), exe.as_deref());

    let src = env_or("COSMIX_SRC", || {
        root.clone().unwrap_or_else(|| default_root(&home)).join("src")
    });

    let etc = env_or("COSMIX_ETC", || match &root {
        Some(r) => r.join("etc"),
        None if user_mode => dirs::config_dir()
            .map(|d| d.join("cosmix"))
            .unwrap_or_else(|| home.join(".config/cosmix")),
        None => PathBuf::from("/etc/cosmix"),
    });

    let bin = env_or("COSMIX_BIN", || match &root {
        Some(r) => r.join("bin"),
        None if user_mode => home.join(".local/bin"),
        None => PathBuf::from("/usr/local/bin"),
    });

    ResolvedPaths { root, src, etc, bin }
}

fn env_or(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_root_wins_over_self_location() {
        let root = locate_root(Some(PathBuf::from("/srv/cosmix")), Some(Path::new("/nowhere/bin/mix")));
        assert_eq!(root, Some(PathBuf::from("/srv/cosmix")));
    }

    #[test]
    fn self_location_finds_the_checkout_above_bin_and_above_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("CosMix");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("src/target/release")).unwrap();
        std::fs::write(root.join("bootstrap"), "").unwrap();
        std::fs::write(root.join("src/Cargo.toml"), "").unwrap();
        assert_eq!(locate_root(None, Some(&root.join("bin/mix"))), Some(root.clone()));
        assert_eq!(
            locate_root(None, Some(&root.join("src/target/release/mix"))),
            Some(root.clone())
        );
        // a system install has no checkout above it
        assert_eq!(locate_root(None, Some(Path::new("/opt/cosmix/bin/mix"))), None);
    }

    #[test]
    fn a_runtime_tree_is_not_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
        assert_eq!(locate_root(None, Some(&tmp.path().join("bin/mix"))), None);
    }
}
