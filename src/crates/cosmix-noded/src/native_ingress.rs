//! BUS-013 Unix WebSocket endpoint, sharing Axum's HTTP/WS stack.
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use cosmix_bus::native_session::TransportIdentity;
use tokio::net::{UnixListener, UnixStream};

#[derive(Clone)]
pub(crate) struct UnixPeer(pub Option<TransportIdentity>);

impl Connected<IncomingStream<'_, UnixListener>> for UnixPeer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        // Called immediately after accept, before HTTP parsing. Linux
        // SO_PEERCRED is a connect-time snapshot, never refreshed after setuid.
        match stream.io().peer_cred() {
            Err(error) => {
                tracing::error!(%error, "Unix SO_PEERCRED failed; refusing upgrade");
                Self(None)
            }
            Ok(cred) => match cred.pid().and_then(|pid| u32::try_from(pid).ok()) {
                None => {
                    tracing::error!("Unix SO_PEERCRED has no usable PID; refusing upgrade");
                    Self(None)
                }
                Some(peer_pid) => Self(Some(TransportIdentity::LocalUnix {
                    uid: cred.uid(),
                    gid: cred.gid(),
                    peer_pid,
                })),
            },
        }
    }
}

/// Hold until the server stops. Never unlink a replacement endpoint on drop.
pub(crate) struct SocketGuard {
    _parent: std::fs::File,
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
    if !path.is_absolute()
        || path.to_str().is_none()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .split(|b| *b == b'/')
            .any(|s| s == b"." || s == b"..")
    {
        bail!("noded.unix_socket must be absolute without dot components");
    }
    let parent = path
        .parent()
        .context("Unix endpoint requires parent directory")?;
    // SAFETY: geteuid has no preconditions and reads only process credentials.
    let uid = unsafe { libc::geteuid() };
    let directory = anchored_parent(parent, uid)?;
    // Linux has no bindat. /proc/self/fd resolves through our held directory
    // descriptor, so ancestor renames cannot redirect bind/chmod/unlink.
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(
        path.file_name()
            .context("Unix endpoint requires a filename")?,
    );
    let path = anchored.as_path();
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() || meta.uid() != uid {
            bail!("refusing to replace non-socket or foreign-owned Unix endpoint");
        }
        match tokio::time::timeout(std::time::Duration::from_secs(2), UnixStream::connect(path))
            .await
            .context("existing Unix endpoint probe deadline")?
        {
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
        _parent: directory,
        path: path.to_owned(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let inode = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)?;
    let pinned = inode.metadata()?;
    if !pinned.file_type().is_socket() || pinned.dev() != meta.dev() || pinned.ino() != meta.ino() {
        bail!("Unix endpoint replaced after bind");
    }
    std::fs::set_permissions(
        format!("/proc/self/fd/{}", inode.as_raw_fd()),
        std::fs::Permissions::from_mode(0o666),
    )?;
    Ok((listener, guard))
}

fn anchored_parent(parent: &Path, uid: u32) -> Result<std::fs::File> {
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    let mut location = PathBuf::from("/");
    for component in parent.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        // Validate the anchor before using it. Sticky root-owned ancestors
        // protect broker-owned children; the immediate parent must be protected.
        check_directory(&directory, uid, false)?;
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: live directory fd and NUL-terminated component, no pointers retained.
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        let mut created = false;
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            // SAFETY: as above; only our new directory may have its mode changed.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error()).context("create socket ancestor");
            }
            created = true;
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open socket ancestor without following symlinks");
        }
        // SAFETY: openat returned a new owned fd.
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
        if created {
            // fchmod rejects O_PATH descriptors. Resolve the held inode via
            // procfs, as for socket chmod; no pathname component is re-walked.
            std::fs::set_permissions(
                format!("/proc/self/fd/{}", directory.as_raw_fd()),
                std::fs::Permissions::from_mode(0o755),
            )?;
        }
        location.push(component);
        check_directory(&directory, uid, location == parent)?;
    }
    check_directory(&directory, uid, true)?;
    Ok(directory)
}

fn check_directory(directory: &std::fs::File, uid: u32, immediate: bool) -> Result<()> {
    let meta = directory.metadata()?;
    if meta.uid() != uid && meta.uid() != 0 {
        bail!("socket ancestor is not broker/root-owned");
    }
    if meta.mode() & 0o022 != 0 && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0 && !immediate) {
        bail!("socket ancestor is group/world writable without a permitted sticky root");
    }
    if meta.mode() & 0o111 != 0o111 {
        bail!(
            "existing socket ancestor is not user-traversable; refusing to widen its permissions"
        );
    }
    Ok(())
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
    async fn traversal_only_ancestor_requires_no_read_permission() {
        let path = path();
        let root = path.parent().unwrap();
        std::fs::create_dir(root).unwrap();
        // Owner wx, everyone else x: simulate the unprivileged traversal of
        // a root-owned 0711 ancestor without requiring chown or setuid.
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o311)).unwrap();
        let endpoint = root.join("created/bus.sock");
        let (listener, guard) = bind(&endpoint).await.unwrap();
        // Also pins the traversal-only flags when this test runs as root.
        // SAFETY: guard owns this live fd; F_GETFL takes no variadic argument.
        assert_ne!(
            unsafe { libc::fcntl(guard._parent.as_raw_fd(), libc::F_GETFL) } & libc::O_PATH,
            0
        );
        assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, 0o311);
        drop(listener);
        drop(guard);
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir(root.join("created")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn preserves_existing_modes_and_rejects_intermediate_symlinks() {
        let path = path();
        let parent = path.parent().unwrap();
        std::fs::create_dir(parent).unwrap();
        for mode in [0o700, 0o750] {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                bind(&path)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("refusing to widen")
            );
            assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o777, mode);
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(parent, parent.join("alias")).unwrap();
        assert!(bind(&parent.join("alias/bus.sock")).await.is_err());
        std::fs::remove_file(parent.join("alias")).unwrap();
        let (listener, guard) = bind(&path).await.unwrap();
        let moved = parent.with_extension("moved");
        std::fs::rename(parent, &moved).unwrap();
        std::fs::create_dir(parent).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        drop(listener);
        drop(guard);
        assert!(!moved.join("bus.sock").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::fs::remove_dir(moved).unwrap();
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
