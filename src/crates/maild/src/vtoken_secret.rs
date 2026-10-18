//! The vtoken HMAC server secret + keyed-token hashing.
//!
//! The opaque capability token is looked up by `HMAC-SHA256(server_secret,
//! domain‖token)`, never by the raw token — so a leaked registry DB is useless
//! without the key, which lives OUTSIDE the DB and its backups. The secret is
//! loaded from (or created in) a maild-owned `0600` key file at startup, in Rust,
//! BEFORE any embedded Mix evaluator exists — so no in-proc script can `read_file`
//! it (the FsRead key-exfil gap the design calls out; the secret never enters Mix
//! scope).
//!
//! Dual-key shape: a `current` key signs new tokens; an optional `previous` key
//! still verifies during a rotation window, so a key roll does not invalidate
//! every row's `token_hmac` primary key at once.
//!
//! Prep only (C5) — wired into the opaque-token registry in C6.

use std::path::Path;

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Raw secret length in bytes (a 256-bit key).
const SECRET_LEN: usize = 32;

/// The loaded HMAC secret(s) for keyed token hashing.
pub struct VtokenSecret {
    current: [u8; SECRET_LEN],
    /// Still honoured at lookup during a rotation window.
    previous: Option<[u8; SECRET_LEN]>,
}

impl VtokenSecret {
    /// Load the secret from `path`, creating a fresh random `0600` key file if it
    /// does not exist. File format is hex, one key per line: the current key on
    /// line 1, an optional previous key (rotation window) on line 2.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            let key = random_secret();
            write_key_file(path, &key, None)?;
            return Ok(Self {
                current: key,
                previous: None,
            });
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read vtoken secret {}", path.display()))?;
        let mut lines = contents.lines().filter(|l| !l.trim().is_empty());
        let current = parse_key(
            lines
                .next()
                .with_context(|| format!("vtoken secret {} is empty", path.display()))?,
        )?;
        let previous = lines.next().map(parse_key).transpose()?;
        Ok(Self { current, previous })
    }

    /// `HMAC-SHA256(current, domain‖token)` as lowercase hex — the registry
    /// lookup key for an opaque token. `domain` binds the token to its vhost, so
    /// the same opaque string under two domains hashes differently.
    pub fn hmac_hex(&self, domain: &str, token: &str) -> String {
        hmac_hex_with(&self.current, domain, token)
    }

    /// The hashes to try at lookup, in order: the current key, then the previous
    /// key (rotation window) if present. The resolver checks each.
    pub fn candidate_hashes(&self, domain: &str, token: &str) -> Vec<String> {
        let mut v = vec![self.hmac_hex(domain, token)];
        if let Some(prev) = &self.previous {
            v.push(hmac_hex_with(prev, domain, token));
        }
        v
    }
}

/// `HMAC-SHA256(key, domain ‖ 0x00 ‖ token)` as lowercase hex. The NUL separator
/// makes the concatenation unambiguous — `("ab","c")` and `("a","bc")` cannot
/// collide.
fn hmac_hex_with(key: &[u8], domain: &str, token: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(domain.as_bytes());
    mac.update(b"\0");
    mac.update(token.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn random_secret() -> [u8; SECRET_LEN] {
    use rand::RngCore;
    let mut key = [0u8; SECRET_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

fn parse_key(hex_line: &str) -> Result<[u8; SECRET_LEN]> {
    let bytes = hex::decode(hex_line.trim()).context("vtoken secret is not valid hex")?;
    bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "vtoken secret must be {SECRET_LEN} bytes ({} hex chars)",
            SECRET_LEN * 2
        )
    })
}

fn write_key_file(
    path: &Path,
    current: &[u8; SECRET_LEN],
    previous: Option<&[u8; SECRET_LEN]>,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vtoken secret dir {}", parent.display()))?;
    }
    let mut body = hex::encode(current);
    body.push('\n');
    if let Some(prev) = previous {
        body.push_str(&hex::encode(prev));
        body.push('\n');
    }
    // create_new fails (rather than clobbering) if the key appeared concurrently —
    // a key roll must be deliberate, never an accidental overwrite at startup.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create vtoken secret {}", path.display()))?;
    f.write_all(body.as_bytes())
        .context("write vtoken secret")?;
    // Belt-and-braces: enforce 0600 even on a filesystem that ignored the mode.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context("chmod vtoken secret 0600")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_0600_key_file_and_is_stable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("secret.key");
        let s1 = VtokenSecret::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Reloading the persisted key yields the SAME hash.
        let s2 = VtokenSecret::load_or_create(&path).unwrap();
        assert_eq!(
            s1.hmac_hex("example.org", "k4m9p2"),
            s2.hmac_hex("example.org", "k4m9p2")
        );
    }

    #[test]
    fn hmac_binds_domain_and_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = VtokenSecret::load_or_create(tmp.path().join("k")).unwrap();
        let a = s.hmac_hex("example.org", "tok");
        assert_eq!(a.len(), 64); // hex SHA-256
        assert_ne!(a, s.hmac_hex("example.net", "tok")); // domain-bound
        assert_ne!(a, s.hmac_hex("example.org", "tok2")); // token-bound
        // The NUL separator prevents a concat collision.
        assert_ne!(s.hmac_hex("ab", "c"), s.hmac_hex("a", "bc"));
    }

    #[test]
    fn previous_key_gives_a_second_candidate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("k");
        let cur = [1u8; SECRET_LEN];
        let prev = [2u8; SECRET_LEN];
        write_key_file(&path, &cur, Some(&prev)).unwrap();
        let s = VtokenSecret::load_or_create(&path).unwrap();
        let cands = s.candidate_hashes("d", "t");
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0], hmac_hex_with(&cur, "d", "t"));
        assert_eq!(cands[1], hmac_hex_with(&prev, "d", "t"));
        // A single-key file yields exactly one candidate.
        let single = VtokenSecret::load_or_create(tmp.path().join("k2")).unwrap();
        assert_eq!(single.candidate_hashes("d", "t").len(), 1);
    }
}
