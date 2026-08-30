//! First-run SoundFont fetch: download → verify SHA-256 → atomic install.
//!
//! Mirrors the indexd "load on demand, serve regardless" posture: the daemon
//! calls this off the Bus-connect path so a slow ~151 MB download never blocks
//! broker registration, and a failure is fail-soft (the daemon keeps serving;
//! render/play just report the SoundFont is unavailable).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

/// Where + what to fetch.
pub struct SoundfontSpec<'a> {
    pub state_dir: &'a str,
    pub name: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

/// Ensure `state_dir/name` exists and matches `sha256`, fetching on first run.
/// Idempotent: a present, hash-matching file returns immediately (no network).
/// A present-but-mismatched file is re-fetched over (handles a truncated or
/// substituted artifact).
pub fn ensure_soundfont(spec: &SoundfontSpec) -> Result<PathBuf> {
    let dir = Path::new(spec.state_dir);
    fs::create_dir_all(dir).with_context(|| format!("create state dir {}", dir.display()))?;
    let target = dir.join(spec.name);

    if target.exists() && verify_sha256(&target, spec.sha256)? {
        return Ok(target);
    }
    download_verified(spec.url, &target, spec.sha256)?;
    Ok(target)
}

/// Stream-hash an existing file and compare to the expected hex digest.
fn verify_sha256(path: &Path, expect: &str) -> Result<bool> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(hasher.finalize()).eq_ignore_ascii_case(expect))
}

/// GET `url` → `target.new`, hashing as it streams; verify, then atomic-rename.
/// The partial `.new` is removed on ANY failure path (network, write, checksum,
/// rename), so a failed fetch never leaves an orphaned partial behind.
fn download_verified(url: &str, target: &Path, expect: &str) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.new", target.display()));
    let result = download_to_tmp(url, &tmp, target, expect);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn download_to_tmp(url: &str, tmp: &Path, target: &Path, expect: &str) -> Result<()> {
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file = fs::File::create(tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("read response body")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).context("write soundfont")?;
        total += n as u64;
    }
    file.flush().ok();
    let got = hex(hasher.finalize());
    if !got.eq_ignore_ascii_case(expect) {
        bail!("soundfont sha256 mismatch ({total} bytes): got {got}, expected {expect}");
    }
    fs::rename(tmp, target).with_context(|| format!("install {}", target.display()))?;
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
