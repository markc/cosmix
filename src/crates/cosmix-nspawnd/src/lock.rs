//! Kernel-held per-instance lock shared by daemon and one-shot/admin paths.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use cosmix_nspawnd::core::InstanceName;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockHolder {
    pub op_id: String,
    pub verb: String,
    pub actor: String,
    pub pid: u32,
    pub started_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("instance lock busy")]
    Busy { holder: Option<LockHolder> },
    #[error("instance lock I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("cannot serialise lock holder: {0}")]
    Serialise(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct LockManager {
    root: PathBuf,
    owner: Option<(u32, u32)>,
}

impl LockManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            owner: None,
        }
    }

    pub fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some((uid, gid));
        self
    }

    pub fn ensure_root(&self) -> Result<(), LockError> {
        fs::create_dir_all(&self.root).map_err(|source| LockError::Io {
            path: self.root.clone(),
            source,
        })?;
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700)).map_err(|source| {
            LockError::Io {
                path: self.root.clone(),
                source,
            }
        })?;
        if let Some((uid, gid)) = self.owner {
            chown_path(&self.root, uid, gid).map_err(|source| LockError::Io {
                path: self.root.clone(),
                source,
            })?;
        }
        Ok(())
    }

    pub fn acquire(
        &self,
        name: &InstanceName,
        holder: LockHolder,
    ) -> Result<InstanceLock, LockError> {
        self.ensure_root()?;
        let path = self.root.join(format!("{name}.lock"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
        if let Some((uid, gid)) = self.owner {
            let rc = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
            if rc != 0 {
                return Err(LockError::Io {
                    path,
                    source: io::Error::last_os_error(),
                });
            }
        }
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let source = io::Error::last_os_error();
            if source.kind() == io::ErrorKind::WouldBlock {
                return Err(LockError::Busy {
                    holder: read_holder(&mut file),
                });
            }
            return Err(LockError::Io { path, source });
        }

        file.set_len(0).map_err(|source| LockError::Io {
            path: path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
        let mut bytes = serde_json::to_vec(&holder)?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|()| file.sync_data())
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(InstanceLock { file, path })
    }
}

pub struct InstanceLock {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_holder(file: &mut File) -> Option<LockHolder> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let rc = unsafe { libc::chown(path.as_ptr(), uid, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(op_id: &str) -> LockHolder {
        LockHolder {
            op_id: op_id.into(),
            verb: "nspawnd.start".into(),
            actor: "test".into(),
            pid: std::process::id(),
            started_at: "now".into(),
        }
    }

    #[test]
    fn cross_process_flock_contention_reports_holder_operation() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LockManager::new(temp.path());
        let name = InstanceName::parse("labspoke").unwrap();
        let first = manager.acquire(&name, holder("op-1")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock::tests::lock_helper_process"])
            .env("COSMIX_NSPAWND_LOCK_TEST_ROOT", temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        drop(first);
        assert!(manager.acquire(&name, holder("op-3")).is_ok());
    }

    #[test]
    fn lock_helper_process() {
        let Some(root) = std::env::var_os("COSMIX_NSPAWND_LOCK_TEST_ROOT") else {
            return;
        };
        let manager = LockManager::new(root);
        let name = InstanceName::parse("labspoke").unwrap();
        let error = match manager.acquire(&name, holder("op-child")) {
            Ok(_) => panic!("child process unexpectedly acquired held lock"),
            Err(error) => error,
        };
        match error {
            LockError::Busy {
                holder: Some(holder),
            } => assert_eq!(holder.op_id, "op-1"),
            other => panic!("unexpected child error: {other:?}"),
        }
    }

    #[test]
    fn root_owned_lock_output_is_reassigned_to_daemon_identity() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let manager = LockManager::new(temp.path().join("locks")).with_owner(518, 518);
        let name = InstanceName::parse("labspoke").unwrap();
        let _lock = manager.acquire(&name, holder("op-owned")).unwrap();
        let metadata = fs::metadata(temp.path().join("locks/labspoke.lock")).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (518, 518));
    }
}
