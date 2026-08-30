//! Mint a self-signed leaf certificate for an internal / `.localhost` vhost.
//!
//! `<node>.localhost` (and other internal-only names) can never obtain a Let's
//! Encrypt cert, so an appliance's first boot needs a self-signed one for
//! webd/maild TLS. This replaces the previous `openssl req` shell-out in the
//! NS5 `firstboot.mix`, which was the sole reason the appliance dragged in the
//! `openssl` package — moving it to `rcgen` (already a `tls`-feature dep via
//! the ACME wrapper) lets the image stay pure-glibc.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use time::{Duration, OffsetDateTime};

/// Generate a P-256 self-signed certificate for `fqdn` and write it as PEM.
///
/// Mirrors the retired `openssl req -x509 -newkey ec
/// -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj /CN=<fqdn>
/// -addext subjectAltName=DNS:<fqdn>,IP:127.0.0.1 -days 3650`: an EC P-256 key,
/// `CN=<fqdn>`, SANs `DNS:<fqdn>` + `IP:127.0.0.1`, ~10-year validity. The
/// certificate PEM is written to `cert_path` (0644) and the private key PEM to
/// `key_path` (0600); the caller is responsible for any further ownership
/// change (e.g. `root:cosmix-tls`).
pub fn write_self_signed(fqdn: &str, cert_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    // `new()` adds `fqdn` as a DNS SAN; add the loopback IP SAN alongside it.
    let mut params = CertificateParams::new(vec![fqdn.to_string()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, fqdn);
    params.distinguished_name = dn;
    params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));

    // ~10 years, backdated a day to tolerate a slightly-fast peer clock.
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3650);

    let key = KeyPair::generate()?; // rcgen's default algorithm is ECDSA P-256
    let cert = params.self_signed(&key)?;

    write_pem(cert_path, cert.pem().as_bytes(), 0o644)?;
    write_pem(key_path, key.serialize_pem().as_bytes(), 0o600)?;
    Ok(())
}

/// Write `bytes` to `path` at exactly `mode`, atomically.
///
/// Writes to a fresh same-directory temp opened `O_EXCL` with `mode`, fsyncs,
/// then renames over the destination. This guarantees `mode` even when the
/// target already exists (a plain `create(true).truncate(true)` keeps the
/// *old* file's permissions, so the private key could briefly be readable);
/// the rename is atomic (no reader ever sees a torn PEM); and `O_EXCL` refuses
/// to follow a symlink planted at the temp path.
fn write_pem(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp); // clear a stale temp from a crashed run
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}
