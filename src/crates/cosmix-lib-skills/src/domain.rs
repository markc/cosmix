//! Domain detection — resolves workspace paths to domain keys.
//!
//! Resolution order:
//! 1. Check `domains.toml` `map` (prefix match, longest wins)
//! 2. Walk up from path looking for CLAUDE.md, derive domain from relative path
//!
//! Examples:
//!   ~/.cosmix/src/crates/foo → "cosmix"  (via settings map)
//!   ~/.ns/some/deep/path     → "ns"      (via settings map)
//!   ~/.gh/wg-admin/src       → "gh/wg-admin" (via CLAUDE.md walk)
//!   /tmp/random              → "general"

use std::path::Path;

/// Detect the project domain from a working directory.
///
/// Tries config-based prefix matching first, then falls back to walking
/// up the directory tree looking for CLAUDE.md.
pub fn detect_domain(pwd: &Path) -> String {
    // Try settings-based resolution first
    if let Ok(domains) =
        cosmix_config::store::load_service::<cosmix_config::DomainsSettings>("domains")
        && let Some(domain) = domains.resolve(pwd)
    {
        return domain;
    }

    // Fallback: walk up looking for CLAUDE.md
    let home = match directories::BaseDirs::new() {
        Some(b) => b.home_dir().to_path_buf(),
        None => return "general".into(),
    };

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
    use std::path::PathBuf;

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
