//! The `--version` contract, shared by every terminal frontend.
//!
//! **Mark's contract (2026-09-21): `--version` does nothing except report the
//! version and the build hash, even if the program is already running.**
//!
//! Three things follow, and each one is a bug that existed before this:
//!
//! - It is answered from `main()`'s first statement, before the inherited-fd
//!   quarantine, the config read, the Bus connection and the window. Nothing
//!   it prints can depend on startup succeeding.
//! - It needs no display. `bterm --version` over ssh used to die with
//!   `term requires a native Wayland session`, because the Wayland check came
//!   first.
//! - It takes no name. A running frontend already owns the Bus name it serves
//!   under, so anything that registered — or refused to start because it could
//!   not — would make a second `--version` lie or fail. This path registers
//!   nothing, so a second copy is always truthful.
//!
//! The hash is not decoration: a semver alone cannot tell a stale binary from
//! a fresh one (the 2026-06-01 stale-binary incident, which is why
//! `cosmix-lib-buildinfo` exists). It lives here rather than in either
//! frontend so `bterm` and the incoming iced `term` cannot answer differently.

use cosmix_buildinfo::BuildInfo;

/// Answer a version query, or `None` when `args` is not one.
///
/// `args` is the whole argv. `bi` must come from `build_info!()` expanded in
/// the **binary's** crate — the macro captures the sha of the crate it expands
/// in, so calling it here would report `cosmix-term-core`'s provenance for
/// every frontend.
///
/// Unlike `--help` and `--print-config`, which scan the whole argv, a version
/// query is `argv[1]` only: a terminal forwards the rest of its argv to the
/// program it runs, and `bterm -e mycmd --version` must run `mycmd`.
pub fn version_request(args: &[String], bi: BuildInfo) -> Option<String> {
    if !matches!(args.get(1).map(String::as_str), Some("--version" | "-V")) {
        return None;
    }
    if args.get(2).map(String::as_str) == Some("--json") {
        return Some(
            serde_json::json!({
                "component": bi.pkg,
                "version": bi.version,
                "git_sha": bi.git_sha,
                "git_sha_full": bi.git_sha_full,
                "git_dirty": bi.git_dirty,
                "build_time": bi.build_time,
            })
            .to_string(),
        );
    }
    Some(version_line(bi))
}

/// `<component> <version> (<sha>)`, with `-dirty` when the tree was modified
/// at compile time. Mirrors `mix --version` so an operator reads one shape
/// across the substrate.
pub fn version_line(bi: BuildInfo) -> String {
    let dirty = if bi.git_dirty { "-dirty" } else { "" };
    format!("{} {} ({}{dirty})", bi.pkg, bi.version, bi.git_sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> BuildInfo {
        BuildInfo {
            pkg: "cosmix-bterm",
            version: "9.9.9",
            git_sha: "abc1234",
            git_sha_full: "abc1234000000000000000000000000000000000",
            git_dirty: false,
            build_time: "2026-09-21T00:00:00Z",
        }
    }

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("bterm")
            .chain(rest.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn reports_version_and_build_hash() {
        assert_eq!(
            version_request(&argv(&["--version"]), info()).as_deref(),
            Some("cosmix-bterm 9.9.9 (abc1234)")
        );
        assert_eq!(
            version_request(&argv(&["-V"]), info()).as_deref(),
            Some("cosmix-bterm 9.9.9 (abc1234)")
        );
    }

    #[test]
    fn a_dirty_build_says_so() {
        let mut bi = info();
        bi.git_dirty = true;
        assert!(version_line(bi).ends_with("(abc1234-dirty)"));
    }

    #[test]
    fn json_form_carries_the_full_sha_and_the_component() {
        let out = version_request(&argv(&["--version", "--json"]), info()).expect("a request");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["component"], "cosmix-bterm");
        assert_eq!(v["git_sha_full"].as_str().unwrap().len(), 40);
        assert_eq!(v["git_dirty"], false);
    }

    /// Reach is `argv[1]`, so a version token in the command a terminal is
    /// asked to RUN is not a version query.
    #[test]
    fn a_later_version_token_is_not_a_query() {
        assert_eq!(version_request(&argv(&[]), info()), None);
        assert_eq!(version_request(&argv(&["-e", "cmd", "--version"]), info()), None);
        assert_eq!(version_request(&argv(&["--print-config"]), info()), None);
    }
}
