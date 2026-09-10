//! BUS-013 Unix WebSocket endpoint, sharing Axum's HTTP/WS stack.
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use cosmix_bus::native_session::TransportIdentity;
use tokio::net::{UnixListener, UnixStream};

// Activated with observation protection in the next coherent stage.
pub(crate) const ENABLED: bool = false;

#[derive(Clone)]
pub(crate) struct UnixPeer(pub Option<TransportIdentity>);

impl Connected<IncomingStream<'_, UnixListener>> for UnixPeer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        // Called immediately after accept, before HTTP parsing. Linux
        // SO_PEERCRED is a connect-time snapshot, never refreshed after setuid.
        Self(stream.io().peer_cred().ok().and_then(|cred| {
            Some(TransportIdentity::LocalUnix {
                uid: cred.uid(),
                gid: cred.gid(),
                peer_pid: u32::try_from(cred.pid()?).ok()?,
            })
        }))
    }
}

/// Hold until the server stops. Never unlink a replacement endpoint on drop.
pub(crate) struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) async fn bind(path: &Path) -> Result<(UnixListener, SocketGuard)> {
    if !path.is_absolute() {
        bail!("noded.unix_socket must be absolute");
    }
    let parent = path
        .parent()
        .context("Unix endpoint requires parent directory")?;
    // SAFETY: geteuid has no preconditions and reads only process credentials.
    let uid = unsafe { libc::geteuid() };
    std::fs::create_dir_all(parent).context("create broker socket directory")?;
    let dir = std::fs::symlink_metadata(parent)?;
    if !dir.is_dir() || (dir.uid() != uid && dir.uid() != 0) || dir.mode() & 0o022 != 0 {
        bail!("broker socket directory must be broker/root-owned and not group/world writable");
    }
    if dir.uid() == uid {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
    } else if dir.mode() & 0o111 != 0o111 {
        bail!("broker socket directory must be user-traversable");
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() || meta.uid() != uid {
            bail!("refusing to replace non-socket or foreign-owned Unix endpoint");
        }
        match UnixStream::connect(path).await {
            Ok(_) => bail!("Unix broker endpoint is already listening"),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let current = std::fs::symlink_metadata(path)?;
                if current.dev() != meta.dev() || current.ino() != meta.ino() {
                    bail!("Unix endpoint changed during stale-socket check");
                }
                std::fs::remove_file(path)?;
            }
            Err(e) => return Err(e).context("probe existing Unix endpoint"),
        }
    }
    let listener = UnixListener::bind(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    let guard = SocketGuard {
        path: path.to_owned(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok((listener, guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cosmix-uds-{:032x}", rand::random::<u128>()))
            .join("bus.sock")
    }

    #[tokio::test]
    async fn endpoint_modes_live_collision_and_cleanup() {
        let path = path();
        let (listener, guard) = bind(&path).await.unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o666);
        assert!(bind(&path).await.is_err());
        assert!(path.exists());
        drop(listener);
        drop(guard);
        assert!(!path.exists());
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refuses_files_symlinks_and_writable_directory() {
        let path = path();
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&path, b"keep").unwrap();
        assert!(bind(&path).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("absent.sock", &path).unwrap();
        assert!(bind(&path).await.is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(bind(&path).await.is_err());
        std::fs::remove_dir(parent).unwrap();
        assert!(bind(Path::new("relative.sock")).await.is_err());
    }

    #[tokio::test]
    async fn reclaims_stale_owned_socket_without_unlinking_successor() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(UnixListener::bind(&path).unwrap());
        let (listener, guard) = bind(&path).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"successor").unwrap();
        drop(listener);
        drop(guard);
        assert_eq!(std::fs::read(&path).unwrap(), b"successor");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
    }
}
