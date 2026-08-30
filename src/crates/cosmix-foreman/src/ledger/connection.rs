use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;
use std::sync::{Mutex, OnceLock, Weak};

static OPEN_LEDGER_AUTHORITIES: OnceLock<Mutex<HashMap<PathBuf, Weak<LedgerOpenAuthority>>>> =
    OnceLock::new();

/// Prefix of SQLite's bundled Unix-VFS `unixFile`.
///
/// SQLite exposes the owning `sqlite3_file` through
/// `SQLITE_FCNTL_FILE_POINTER`, but its Unix descriptor belongs to the VFS
/// subclass rather than the public base struct. `connection_file_identity`
/// verifies the VFS name, allocation size and repeated prefix pointers before
/// reading `fd`. Rusqlite's bundled SQLite and this layout are compiled as
/// one pinned dependency.
#[repr(C)]
struct SqliteUnixFilePrefix {
    methods: *const rusqlite::ffi::sqlite3_io_methods,
    vfs: *mut rusqlite::ffi::sqlite3_vfs,
    inode: *mut c_void,
    fd: std::ffi::c_int,
}

impl LedgerOpenOptions {
    /// Open another SQLite connection only if SQLite binds it to the exact
    /// object selected by the primary connection.
    pub fn open(&self) -> Result<Ledger> {
        self.open_with(|path| {
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| {
                format!(
                    "reopening existing lane ledger {} without creating it",
                    path.display()
                )
            })
        })
    }

    fn open_with(&self, opener: impl FnOnce(&Path) -> Result<Connection>) -> Result<Ledger> {
        let conn = opener(&self.authority.path)?;
        let reopened = connection_file_identity(&conn)
            .context("reading the reopened SQLite main-file identity")?;
        anyhow::ensure!(
            reopened == self.authority.identity,
            "refusing ledger reopen: {} did not open the primary ledger object; expected device {} inode {}, SQLite opened device {} inode {}",
            self.authority.path.display(),
            self.authority.identity.device,
            self.authority.identity.inode,
            reopened.device,
            reopened.inode
        );
        // WAL and SHM are SQLite-managed sidecars, not separate ledger
        // identities. Only after the SQLite-owned main fd is verified do we
        // allow normal sidecar validation, locking, migration or ledger I/O.
        Ledger::finish_open(conn, self.clone(), &self.authority.path)
    }

    fn verify_requested_project_identity(
        &self,
        requested: Option<(&str, &str)>,
    ) -> Result<()> {
        let Some((name, repository)) = requested else {
            return Ok(());
        };
        anyhow::ensure!(
            self.authority
                .project_identity
                .as_ref()
                .is_some_and(|stored| stored.0 == name && stored.1 == repository),
            "active ledger {} is not bound to requested project {:?} repository {:?}",
            self.authority.path.display(),
            name,
            repository
        );
        Ok(())
    }
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_create(path, LedgerCreate::ParentsAndFile)
    }

    pub(crate) fn open_with_create(path: &Path, create: LedgerCreate) -> Result<Self> {
        Self::open_with_create_for_project(path, create, None)
    }

    pub(crate) fn open_with_create_for_project(
        path: &Path,
        create: LedgerCreate,
        project_identity: Option<(&str, &str)>,
    ) -> Result<Self> {
        // The first live open for a pathname is its primary authority. Every
        // later production open in this process is a reopen of that authority,
        // so worker/review callers cannot accidentally mint a new identity by
        // calling Ledger::open on a rebound pathname.
        let registry_key = std::path::absolute(path)
            .with_context(|| format!("resolving ledger path {}", path.display()))?;
        let authorities = OPEN_LEDGER_AUTHORITIES.get_or_init(Default::default);
        let mut authorities = authorities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(authority) = authorities.get(&registry_key).and_then(Weak::upgrade) {
            drop(authorities);
            let open_options = LedgerOpenOptions { authority };
            open_options.verify_requested_project_identity(project_identity)?;
            return open_options.open();
        }
        authorities.remove(&registry_key);

        match create {
            LedgerCreate::ParentsAndFile => {
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating ledger dir {}", parent.display()))?;
                }
            }
            LedgerCreate::FileOnly | LedgerCreate::Never => {}
        }
        let conn = match create {
            LedgerCreate::Never => Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| {
                format!(
                    "opening existing implicit ledger {} without creating it; only --db and FOREMAN_DB may select a creatable path",
                    path.display()
                )
            })?,
            LedgerCreate::ParentsAndFile | LedgerCreate::FileOnly => {
                Connection::open(path).with_context(|| match create {
                    LedgerCreate::FileOnly => format!(
                        "opening implicit ledger {} without creating parent directories; only --db and FOREMAN_DB may create parents",
                        path.display()
                    ),
                    LedgerCreate::ParentsAndFile => {
                        format!("opening ledger {}", path.display())
                    }
                    LedgerCreate::Never => unreachable!(),
                })?
            }
        };
        let identity =
            connection_file_identity(&conn).context("reading primary SQLite main-file identity")?;
        let open_options = LedgerOpenOptions {
            authority: std::sync::Arc::new(LedgerOpenAuthority {
                path: path.to_path_buf(),
                project_identity: project_identity
                    .map(|(name, repository)| (name.to_string(), repository.to_string())),
                identity,
            }),
        };
        let ledger = Self::finish_open(conn, open_options, path)?;
        authorities.insert(
            registry_key,
            std::sync::Arc::downgrade(&ledger.open_options.authority),
        );
        Ok(ledger)
    }

    fn finish_open(
        conn: Connection,
        open_options: LedgerOpenOptions,
        path: &Path,
    ) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        let ledger = Ledger { conn, open_options };
        ledger.migrate(path)?;
        if let Some((project_name, repository_identity)) =
            &ledger.open_options.authority.project_identity
        {
            ledger.bind_project_identity(path, project_name, repository_identity)?;
        }
        Ok(ledger)
    }

    /// Mint reopen-only authority from this successfully opened connection.
    pub fn open_options(&self) -> LedgerOpenOptions {
        self.open_options.clone()
    }

    /// Stamp a new manifest ledger with its one project identity, or refuse
    /// an existing ledger stamped by a different manifest instance. This is
    /// part of project-mode open, before any command can inspect task rows.
    fn bind_project_identity(
        &self,
        path: &Path,
        project_name: &str,
        repository_identity: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let stored: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT name, repository_identity
                 FROM project_identity WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let populated: i64 = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM tasks
                UNION ALL SELECT 1 FROM runs
                UNION ALL SELECT 1 FROM findings
                UNION ALL SELECT 1 FROM events
                UNION ALL SELECT 1 FROM reservations
                UNION ALL SELECT 1 FROM verifications
                LIMIT 1
            )",
            [],
            |row| row.get(0),
        )?;
        let stored = match stored {
            Some((stored_name, None)) => {
                anyhow::ensure!(
                    stored_name == project_name,
                    "ledger {} belongs to project {:?} with an unstamped repository identity; refusing manifest project {:?} ({:?})",
                    path.display(),
                    stored_name,
                    project_name,
                    repository_identity
                );
                anyhow::ensure!(
                    populated == 0,
                    "ledger {} contains legacy state under project {:?} but its repository identity is unstamped; refusing automatic adoption by repository {:?} — migrate it explicitly",
                    path.display(),
                    stored_name,
                    repository_identity
                );
                tx.execute(
                    "UPDATE project_identity SET repository_identity = ?1
                     WHERE singleton = 1 AND repository_identity IS NULL",
                    params![repository_identity],
                )?;
                (stored_name, repository_identity.to_string())
            }
            Some((stored_name, Some(stored_repository))) => (stored_name, stored_repository),
            None => {
                anyhow::ensure!(
                    populated == 0,
                    "ledger {} contains legacy state but has no project identity; refusing automatic adoption by manifest project {:?} ({:?}) — migrate it explicitly or use a fresh per-project ledger",
                    path.display(),
                    project_name,
                    repository_identity
                );
                tx.execute(
                    "INSERT INTO project_identity
                         (singleton, name, repository_identity)
                     VALUES (1, ?1, ?2)",
                    params![project_name, repository_identity],
                )?;
                (project_name.to_string(), repository_identity.to_string())
            }
        };
        anyhow::ensure!(
            stored.0 == project_name && stored.1 == repository_identity,
            "ledger {} belongs to project {:?} repository {:?}; refusing manifest project {:?} repository {:?}",
            path.display(),
            stored.0,
            stored.1,
            project_name,
            repository_identity
        );
        tx.commit()?;
        Ok(())
    }

    /// Fail before external work if a future refactor leaves any transaction
    /// open on this connection. In particular, refinery verifier/review/Git
    /// subprocesses must only run while the ledger is in autocommit mode.
    pub(crate) fn ensure_autocommit(&self, operation: &str) -> Result<()> {
        anyhow::ensure!(
            self.conn.is_autocommit(),
            "refusing {operation} while the ledger connection has an open transaction"
        );
        Ok(())
    }

}

fn connection_file_identity(conn: &Connection) -> Result<LedgerFileIdentity> {
    const MAIN: &[u8] = b"main\0";
    let mut file = std::ptr::null_mut::<rusqlite::ffi::sqlite3_file>();
    let mut vfs = std::ptr::null_mut::<rusqlite::ffi::sqlite3_vfs>();

    // SAFETY: `conn` remains live for this call, MAIN is NUL terminated, and
    // the output pointer has the exact type required by SQLite.
    let file_rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            MAIN.as_ptr().cast(),
            rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
            std::ptr::addr_of_mut!(file).cast(),
        )
    };
    anyhow::ensure!(
        file_rc == rusqlite::ffi::SQLITE_OK && !file.is_null(),
        "SQLite did not expose its main database file (file-control result {file_rc})"
    );

    // SAFETY: same live connection and correctly typed output storage as the
    // FILE_POINTER call above.
    let vfs_rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            MAIN.as_ptr().cast(),
            rusqlite::ffi::SQLITE_FCNTL_VFS_POINTER,
            std::ptr::addr_of_mut!(vfs).cast(),
        )
    };
    anyhow::ensure!(
        vfs_rc == rusqlite::ffi::SQLITE_OK && !vfs.is_null(),
        "SQLite did not expose its main database VFS (file-control result {vfs_rc})"
    );

    let vfs_ptr = vfs;
    // SAFETY: SQLite returned a non-null live sqlite3_vfs pointer above.
    let vfs = unsafe { &*vfs_ptr };
    anyhow::ensure!(!vfs.zName.is_null(), "SQLite main database VFS has no name");
    // SAFETY: SQLite VFS names are stable NUL-terminated strings for the VFS
    // lifetime.
    let vfs_name = unsafe { CStr::from_ptr(vfs.zName) }.to_string_lossy();
    anyhow::ensure!(
        vfs_name == "unix" || vfs_name.starts_with("unix-"),
        "ledger identity requires SQLite's Unix VFS, found {vfs_name:?}"
    );
    anyhow::ensure!(
        usize::try_from(vfs.szOsFile)
            .is_ok_and(|size| size >= size_of::<SqliteUnixFilePrefix>()),
        "SQLite Unix VFS file allocation is too small for its descriptor prefix"
    );

    // SAFETY: `file` is a non-null sqlite3_file returned above.
    let base_methods = unsafe { (*file).pMethods };
    // SAFETY: the guarded Unix VFS allocates the public sqlite3_file base
    // followed by the bundled unixFile prefix declared above. Its allocation
    // size was checked before this cast.
    let unix_file = unsafe { &*file.cast::<SqliteUnixFilePrefix>() };
    anyhow::ensure!(
        unix_file.methods == base_methods && unix_file.vfs == vfs_ptr,
        "SQLite Unix file prefix does not match its exposed base/VFS pointers"
    );
    anyhow::ensure!(
        unix_file.fd >= 0,
        "SQLite Unix VFS returned an invalid main-file descriptor"
    );

    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fd is owned and kept live by `conn`; fstat only reads it and
    // writes one initialized libc::stat to the provided storage.
    let stat_rc = unsafe { libc::fstat(unix_file.fd, metadata.as_mut_ptr()) };
    if stat_rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("fstat SQLite main database descriptor");
    }
    // SAFETY: successful fstat initialized the complete libc::stat.
    let metadata = unsafe { metadata.assume_init() };
    Ok(LedgerFileIdentity {
        device: metadata.st_dev,
        inode: metadata.st_ino,
    })
}
