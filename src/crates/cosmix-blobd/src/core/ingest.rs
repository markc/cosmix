//! Descriptor-bound local ingest. Like cosmix-lib-files' rooted_read helper,
//! opened directories and the returned file are authority, not checked paths.
use super::*;
use std::ffi::{CString, OsStr};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Component;

const OWN_FILES: &[&str] = &[
    "blobd.sqlite",
    "blobd.sqlite-wal",
    "blobd.sqlite-shm",
    "blobs.sqlite",
    "blobs.sqlite-wal",
    "blobs.sqlite-shm",
    LOCK_FILE,
];

fn identity(file: &File) -> io::Result<(u64, u64)> {
    let md = file.metadata()?;
    Ok((md.dev(), md.ino()))
}

fn open_at(parent: &File, name: &OsStr, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
    // SAFETY: live parent descriptor and terminated path; on success the new
    // descriptor is owned exactly once. No path-based reopen follows this call.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a fresh descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) struct IngestRoot {
    path: PathBuf,
    directory: File,
    // Keep startup identities alive, even if a DB sidecar is unlinked later.
    protected: Vec<File>,
}

impl IngestRoot {
    pub(super) fn open(root: &Path) -> Result<Self> {
        let path = fs::canonicalize(root)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        let mut protected = Vec::new();
        for name in OWN_FILES {
            match open_at(
                &directory,
                OsStr::new(name),
                libc::O_PATH | libc::O_NOFOLLOW,
            ) {
                Ok(file) => {
                    file.metadata()?;
                    protected.push(file);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Self {
            path,
            directory,
            protected,
        })
    }

    pub(super) fn source(
        &self,
        caller: &Path,
        after_canonical: impl FnOnce(&Path),
    ) -> Result<(File, PathBuf)> {
        let canonical = fs::canonicalize(caller)?;
        if canonical.starts_with(&self.path) {
            return Err(StoreError::SourceInsideStore);
        }
        after_canonical(&canonical);
        let root_id = identity(&self.directory)?;
        // Walk each canonical ancestor through its pinned parent. NOFOLLOW on
        // every component prevents a symlink swap from changing the authority.
        // Comparing identities also catches a bind-mounted alias of the root.
        let mut directory = File::open("/")?;
        for component in canonical
            .parent()
            .ok_or_else(|| StoreError::BadRequest("source has no parent".into()))?
            .components()
        {
            if identity(&directory)? == root_id {
                return Err(StoreError::SourceInsideStore);
            }
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    directory = open_at(
                        &directory,
                        name,
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )?;
                }
                _ => return Err(StoreError::BadRequest("invalid canonical source".into())),
            }
        }
        if identity(&directory)? == root_id {
            return Err(StoreError::SourceInsideStore);
        }
        let name = canonical
            .file_name()
            .ok_or_else(|| StoreError::BadRequest("source has no filename".into()))?;
        let file = open_at(
            &directory,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?;
        let md = file.metadata()?;
        if !md.is_file() {
            return Err(StoreError::BadRequest(
                "source is not a regular file".into(),
            ));
        }
        let id = (md.dev(), md.ino());
        if id == root_id {
            return Err(StoreError::SourceInsideStore);
        }
        for own in &self.protected {
            if identity(own)? == id {
                return Err(StoreError::SourceInsideStore);
            }
        }
        // WAL/SHM can also be created after startup. Resolve them relative to
        // the pinned store root, and compare their current descriptor identity.
        for name in OWN_FILES {
            match open_at(
                &self.directory,
                OsStr::new(name),
                libc::O_PATH | libc::O_NOFOLLOW,
            ) {
                Ok(own) if identity(&own)? == id => return Err(StoreError::SourceInsideStore),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok((file, canonical))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::store;
    use super::*;

    #[test]
    fn store_database_hardlink_identity_is_refused_in_every_mode() {
        let (dir, store) = store();
        for name in OWN_FILES {
            let own = store.root().join(name);
            if !own.exists() {
                continue;
            }
            let alias = dir.path().join(format!("alias-{name}"));
            fs::hard_link(&own, &alias).unwrap();
            for mode in [PutMode::Copy, PutMode::Reflink, PutMode::HardLink] {
                let opts = PutOptions {
                    mode,
                    immutable: true,
                    ..PutOptions::new("tester")
                };
                assert!(
                    matches!(store.put(&alias, &opts), Err(StoreError::SourceInsideStore)),
                    "{name}: {mode:?}"
                );
            }
        }
    }

    #[test]
    fn symlink_swap_after_canonicalisation_never_ingests_store_bytes() {
        let (dir, store) = store();
        for mode in [PutMode::Copy, PutMode::Reflink, PutMode::HardLink] {
            let path = dir.path().join(format!("source-{mode:?}"));
            fs::write(&path, b"safe").unwrap();
            let opts = PutOptions {
                mode,
                immutable: true,
                ..PutOptions::new("tester")
            };
            let result = store.put_after_canonical(&path, &opts, |canonical| {
                fs::remove_file(canonical).unwrap();
                std::os::unix::fs::symlink(store.root().join("blobd.sqlite"), canonical).unwrap();
            });
            assert!(result.is_err());
        }
        assert_eq!(store.quota_report(None).unwrap().total.used, 0);
    }

    #[test]
    fn ancestor_symlink_swap_and_fifo_are_refused_without_blocking() {
        let (dir, store) = store();
        let parent = dir.path().join("outside");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("blobd.sqlite"), b"safe").unwrap();
        let result = store.put_after_canonical(
            &parent.join("blobd.sqlite"),
            &PutOptions::new("tester"),
            |_| {
                fs::rename(&parent, dir.path().join("saved")).unwrap();
                std::os::unix::fs::symlink(store.root(), &parent).unwrap();
            },
        );
        assert!(result.is_err());
        let fifo = dir.path().join("fifo");
        let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a terminated path within this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(store.put(&fifo, &PutOptions::new("tester")).is_err());
    }

    #[test]
    fn bind_mount_alias_of_store_is_refused_in_every_mode() {
        let (dir, store) = store();
        let staged = store.blobs_root().join(".uploads/bind-source");
        fs::write(&staged, b"uncommitted").unwrap();
        let hash = blob::put(&store.blobs_root(), b"CAS bytes").unwrap();
        let cas = blob::blob_path(&store.blobs_root(), &hash);
        let alias = dir.path().join("bind-alias");
        fs::create_dir(&alias).unwrap();
        let from = CString::new(store.root().as_os_str().as_bytes()).unwrap();
        let to = CString::new(alias.as_os_str().as_bytes()).unwrap();
        // SAFETY: private test mountpoint and valid paths; no existing mount is replaced.
        if unsafe {
            libc::mount(
                from.as_ptr(),
                to.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            eprintln!(
                "SKIP bind_mount_alias_of_store_is_refused_in_every_mode: mount unavailable: {}",
                io::Error::last_os_error()
            );
            return;
        }
        struct Mounted(CString);
        impl Drop for Mounted {
            fn drop(&mut self) {
                // SAFETY: this guard owns the private bind mount, not its source.
                unsafe {
                    libc::umount2(self.0.as_ptr(), libc::MNT_DETACH);
                }
            }
        }
        let _mount = Mounted(to);
        for mode in [PutMode::Copy, PutMode::Reflink, PutMode::HardLink] {
            let opts = PutOptions {
                mode,
                immutable: true,
                ..PutOptions::new("tester")
            };
            // Staging/CAS are not in the pinned database identity list: these
            // cases specifically prove descriptor-ancestry refusal.
            for relative in [
                Path::new("blobd.sqlite"),
                staged.strip_prefix(store.root()).unwrap(),
                cas.strip_prefix(store.root()).unwrap(),
            ] {
                assert!(
                    matches!(
                        store.put(&alias.join(relative), &opts),
                        Err(StoreError::SourceInsideStore)
                    ),
                    "{relative:?}: {mode:?}"
                );
            }
        }
    }
}
