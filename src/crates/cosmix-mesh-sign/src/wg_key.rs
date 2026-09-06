//! Node-local WG bootstrap using the native key implementation. No secret output.
use anyhow::{Context, Result, bail};
use cosmix_wg::keys::{WgKeyPair, WgPrivateKey};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub fn generate(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("private-file must be absolute");
    }
    let pair = WgKeyPair::generate();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .context("create new private key file (existing keys are never replaced)")?;
    writeln!(file, "{}", pair.private.to_base64()).context("write private key file")?;
    file.sync_all().context("sync private key file")?;
    File::open(path.parent().context("private file has no parent")?)?
        .sync_all()
        .context("sync private key directory")?;
    Ok(pair.public.to_base64())
}

pub fn public_key(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("private-file must be absolute");
    }
    // O_NONBLOCK avoids hanging on a FIFO before metadata rejects it.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open protected private key file")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        bail!("private key must be a regular file with no group/other permissions");
    }
    let mut encoded = String::new();
    file.take(129)
        .read_to_string(&mut encoded)
        .context("read private key file")?;
    if encoded.len() > 128 {
        bail!("private key encoding is too large");
    }
    let private = WgPrivateKey::from_base64(encoded.trim())
        .map_err(|_| anyhow::anyhow!("invalid private key encoding"))?;
    Ok(private.public_key().to_base64())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_key_is_private_create_only_and_recoverable() {
        let dir = std::env::temp_dir().join(format!(
            "cosmix-wg-key-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("wg.key");
        let public = generate(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(public_key(&path).unwrap(), public);
        assert!(generate(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(public_key(&link).is_err());
        assert!(generate(&link).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(public_key(&path).is_err());
        assert!(generate(Path::new("relative.key")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
