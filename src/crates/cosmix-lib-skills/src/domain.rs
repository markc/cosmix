//! Domain detection — walks up from $PWD to find the nearest CLAUDE.md,
//! then derives a domain key from the path relative to $HOME.
//!
//! Examples:
//!   ~/.cosmix/src/crates/foo  → "cosmix"
//!   ~/.ns/some/deep/path      → "ns"
//!   ~/.gh/wg-admin/src        → "gh/wg-admin"
//!   ~/.mix/src/crates         → "mix"
//!   ~/Dev/php/laravel/app     → "Dev/php/laravel"
//!   /tmp/random               → "general"

use std::path::{Path, PathBuf};

/// Detect the project domain from a working directory.
///
/// Walks up from `pwd` looking for a directory containing `CLAUDE.md`.
/// The domain is the path from `$HOME` to that directory, with leading
/// dots stripped from each segment (e.g. `.cosmix` → `cosmix`).
///
/// Returns `"general"` if no `CLAUDE.md` is found or `$HOME` is unset.
pub fn detect_domain(pwd: &Path) -> String {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return "general".into(),
    };

    // Walk up from pwd looking for CLAUDE.md
    let mut dir = pwd.to_path_buf();
    loop {
        if dir.join("CLAUDE.md").exists() {
            return path_to_domain(&dir, &home);
        }
        if !dir.pop() || dir == home || dir.as_os_str().is_empty() {
            break;
        }
    }

    "general".into()
}

/// Detect domain from the current working directory.
pub fn detect_domain_cwd() -> String {
    match std::env::current_dir() {
        Ok(pwd) => detect_domain(&pwd),
        Err(_) => "general".into(),
    }
}

/// Convert an absolute path to a domain key relative to $HOME.
/// Strips leading dots from each path segment.
fn path_to_domain(project_root: &Path, home: &Path) -> String {
    let rel = match project_root.strip_prefix(home) {
        Ok(r) => r,
        Err(_) => return "general".into(),
    };

    let segments: Vec<String> = rel
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            s.strip_prefix('.').unwrap_or(&s).to_string()
        })
        .collect();

    if segments.is_empty() {
        "general".into()
    } else {
        segments.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_domain() {
        let home = PathBuf::from("/home/cosmix");

        assert_eq!(
            path_to_domain(&PathBuf::from("/home/cosmix/.cosmix"), &home),
            "cosmix"
        );
        assert_eq!(
            path_to_domain(&PathBuf::from("/home/cosmix/.ns"), &home),
            "ns"
        );
        assert_eq!(
            path_to_domain(&PathBuf::from("/home/cosmix/.gh/wg-admin"), &home),
            "gh/wg-admin"
        );
        assert_eq!(
            path_to_domain(&PathBuf::from("/home/cosmix/Dev/php/laravel"), &home),
            "Dev/php/laravel"
        );
        assert_eq!(
            path_to_domain(&PathBuf::from("/tmp/random"), &home),
            "general"
        );
    }
}
