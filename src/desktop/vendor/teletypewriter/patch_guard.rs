//! Compiled by cosmix-term's test target, never by vendored production code.
use sha2::{Digest, Sha256};

const RECORD: &str = include_str!("patch-record.json");
const SOURCE: &str = include_str!("src/unix/mod.rs");
const TERM: &str = include_str!("../../apps/term/Cargo.toml");
const VENDOR: &str = include_str!("Cargo.toml");
const WORKSPACE: &str = include_str!("../../Cargo.toml");
const REV_MOVED: &str = "upstream rev moved: re-diff the patch, update the recorded hash; see README.teletypewriter-patch.md";
const HUNK_DRIFT: &str = "patched hunk drift: re-diff the patch, update the recorded hash; see README.teletypewriter-patch.md";
const FILE_DRIFT: &str = "patched file drift outside recorded hunks: review the diff and update the file hash; see README.teletypewriter-patch.md";
const MANIFEST_SHAPE: &str = "manifest/record shape changed: inspect the pin, override and hash recording; see README.teletypewriter-patch.md";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify(term: &str, vendor: &str, workspace: &str, source: &str) -> Result<(), &'static str> {
    let record: serde_json::Value = serde_json::from_str(RECORD).map_err(|_| MANIFEST_SHAPE)?;
    let revision = record["upstream_rev"].as_str().ok_or(MANIFEST_SHAPE)?;
    for (manifest, dependency) in [
        (term, "teletypewriter ="),
        (term, "rio-vt ="),
        (vendor, "corcovado ="),
    ] {
        let line = manifest
            .lines()
            .find(|line| line.starts_with(dependency))
            .ok_or(MANIFEST_SHAPE)?;
        let pin = line
            .split_once("rev = \"")
            .and_then(|(_, tail)| tail.split('"').next())
            .ok_or(MANIFEST_SHAPE)?;
        if pin != revision {
            return Err(REV_MOVED);
        }
    }
    if !workspace.contains("[patch.\"https://github.com/raphamorim/rio\"]\nteletypewriter = { path = \"vendor/teletypewriter\" }")
    {
        return Err(MANIFEST_SHAPE);
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
        let begin = source.find(start).ok_or(HUNK_DRIFT)?;
        let finish = source[begin..].find(end).ok_or(HUNK_DRIFT)? + begin;
        // Hunk hashes trim trailing whitespace and append one LF. The full
        // file hash below is byte-exact and also guards edits outside hunks.
        let hunk = format!("{}\n", source[begin..finish].trim_end());
        if record[key].as_str() != Some(hash(hunk.as_bytes()).as_str()) {
            return Err(HUNK_DRIFT);
        }
    }
    if record["src/unix/mod.rs"].as_str() != Some(hash(source.as_bytes()).as_str()) {
        return Err(FILE_DRIFT);
    }
    Ok(())
}

#[test]
fn teletypewriter_pin_and_fd_patch_have_reviewed_hashes() {
    verify(TERM, VENDOR, WORKSPACE, SOURCE).unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn pin_guard_rejects_pin_hunk_and_other_file_drift() {
    assert_eq!(verify("", VENDOR, WORKSPACE, SOURCE), Err(MANIFEST_SHAPE));
    assert_eq!(verify(TERM, VENDOR, "", SOURCE), Err(MANIFEST_SHAPE));
    let record: serde_json::Value = serde_json::from_str(RECORD).unwrap();
    let moved = TERM.replace(record["upstream_rev"].as_str().unwrap(), &"0".repeat(40));
    assert_eq!(verify(&moved, VENDOR, WORKSPACE, SOURCE), Err(REV_MOVED));
    let changed = SOURCE.replace("libc::dup2(source, target)", "libc::dup2(target, source)");
    assert_eq!(verify(TERM, VENDOR, WORKSPACE, &changed), Err(HUNK_DRIFT));
    assert_eq!(
        verify(TERM, VENDOR, WORKSPACE, &format!("{SOURCE}\n")),
        Err(FILE_DRIFT)
    );
}
