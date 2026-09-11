//! Compiled by cosmix-term's test target, never by vendored production code.
use sha2::{Digest, Sha256};

const RECORD: &str = include_str!("patch-record.json");
const SOURCE: &str = include_str!("src/unix/mod.rs");
const TERM: &str = include_str!("../../apps/term/Cargo.toml");
const VENDOR: &str = include_str!("Cargo.toml");
const WORKSPACE: &str = include_str!("../../Cargo.toml");
const REVIEW: &str = "upstream rev moved: re-diff the patch, update the recorded hash (also required when patched files change); see README.teletypewriter-patch.md";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify(term: &str, vendor: &str, workspace: &str, source: &str) -> Result<(), &'static str> {
    let record: serde_json::Value = serde_json::from_str(RECORD).map_err(|_| REVIEW)?;
    let revision = record["upstream_rev"].as_str().ok_or(REVIEW)?;
    for (manifest, dependency) in [
        (term, "teletypewriter ="),
        (term, "rio-vt ="),
        (vendor, "corcovado ="),
    ] {
        let line = manifest
            .lines()
            .find(|line| line.starts_with(dependency))
            .ok_or(REVIEW)?;
        if line
            .split_once("rev = \"")
            .and_then(|(_, tail)| tail.split('"').next())
            != Some(revision)
        {
            return Err(REVIEW);
        }
    }
    if !workspace.contains("[patch.\"https://github.com/raphamorim/rio\"]\nteletypewriter = { path = \"vendor/teletypewriter\" }")
        || record["src/unix/mod.rs"].as_str() != Some(hash(source.as_bytes()).as_str()) {
        return Err(REVIEW);
    }
    for (key, start, end) in [
        (
            "launch_api_hunk",
            "    create_pty_with_spawn_fd(shell",
            "    #[cfg(not(any(target_os = \"macos\", target_os = \"freebsd\")))]",
        ),
        (
            "child_mapping_hunk",
            "            if let Some((source, target)) = inherit_fd {",
            "            libc::signal(libc::SIGCHLD",
        ),
    ] {
        let begin = source.find(start).ok_or(REVIEW)?;
        let finish = source[begin..].find(end).ok_or(REVIEW)? + begin;
        // Hunk hashes trim trailing whitespace and append one LF. The full
        // file hash above is byte-exact and also guards edits outside hunks.
        let hunk = format!("{}\n", source[begin..finish].trim_end());
        if record[key].as_str() != Some(hash(hunk.as_bytes()).as_str()) {
            return Err(REVIEW);
        }
    }
    Ok(())
}

#[test]
fn teletypewriter_pin_and_fd_patch_have_reviewed_hashes() {
    verify(TERM, VENDOR, WORKSPACE, SOURCE).unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn pin_guard_rejects_pin_hunk_and_other_file_drift() {
    let record: serde_json::Value = serde_json::from_str(RECORD).unwrap();
    let moved = TERM.replace(record["upstream_rev"].as_str().unwrap(), &"0".repeat(40));
    assert_eq!(verify(&moved, VENDOR, WORKSPACE, SOURCE), Err(REVIEW));
    let changed = SOURCE.replace("libc::dup2(source, target)", "libc::dup2(target, source)");
    assert_eq!(verify(TERM, VENDOR, WORKSPACE, &changed), Err(REVIEW));
    assert_eq!(
        verify(TERM, VENDOR, WORKSPACE, &format!("{SOURCE}\n")),
        Err(REVIEW)
    );
}
