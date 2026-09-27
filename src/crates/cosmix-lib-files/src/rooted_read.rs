//! Race-safe, read-only access beneath an operator-provisioned directory.
//!
//! The root descriptor pins its identity even if its pathname is renamed. Each
//! read is one Linux openat2 resolution: no symlinks (including magic links), no
//! parent/absolute escape. There is deliberately no path-based fallback on old
//! kernels. A returned descriptor, not a checked pathname, is the read authority.

use std::{fs::File, io, path::Path};

#[derive(Debug)]
pub struct ReadRoot {
    #[cfg(target_os = "linux")]
    directory: File,
}

impl ReadRoot {
    /// Open once at startup. The supplied root is operator configuration, never
    /// request data. Callers canonicalise configured roots first; this helper
    /// rejects symlinks in the supplied canonical path. Relative file reads
    /// beneath the pinned descriptor also reject symlinks.
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root must be absolute without parent components",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            let directory = open_at(
                libc::AT_FDCWD,
                path.as_os_str().as_bytes(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                NO_SYMLINKS | NO_MAGICLINKS,
            )?;
            Ok(Self { directory })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor-relative reads require Linux openat2",
            ))
        }
    }

    /// Open a regular file beneath the pinned root and return that exact handle.
    /// O_NONBLOCK prevents a FIFO from parking the worker before fstat rejects it.
    pub fn open_regular(&self, relative: &str) -> io::Result<File> {
        if relative.is_empty()
            || relative.len() > 4096
            || relative.contains('\\')
            || relative.chars().any(char::is_control)
            || relative
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe relative file path",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let file = open_at(
                self.directory.as_raw_fd(),
                relative.as_bytes(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
                BENEATH | NO_SYMLINKS | NO_MAGICLINKS,
            )?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "share source is not a regular file",
                ));
            }
            Ok(file)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor-relative reads require Linux openat2",
            ))
        }
    }
}

#[cfg(target_os = "linux")]
const NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
const NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const BENEATH: u64 = 0x08;

#[cfg(target_os = "linux")]
fn open_at(
    directory: libc::c_int,
    path: &[u8],
    flags: libc::c_int,
    resolve: u64,
) -> io::Result<File> {
    use std::{ffi::CString, os::fd::FromRawFd};
    // Linux UAPI open_how: three __u64 fields, zero mode without O_CREAT.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in file path"))?;
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: live NUL-terminated path and correctly sized UAPI structure. No
    // borrowed fd ownership changes. On success the new fd is owned exactly once.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat2 returned a fresh, owned file descriptor.
    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::{
        fs,
        io::Read,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rooted-read-{}", uuid::Uuid::now_v7()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn text(mut file: File) -> String {
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn reads_binary_and_refuses_escape_links_directories_and_fifo() {
        let tmp = Scratch::new();
        fs::create_dir(tmp.0.join("nested")).unwrap();
        fs::write(tmp.0.join("nested/file"), [0, 255, 128]).unwrap();
        let root = ReadRoot::open(&tmp.0).unwrap();
        let mut bytes = Vec::new();
        root.open_regular("nested/file")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, [0, 255, 128]);
        symlink("nested/file", tmp.0.join("in-root-link")).unwrap();
        symlink("/etc", tmp.0.join("escape")).unwrap();
        for relative in [
            "",
            "/etc/passwd",
            "../x",
            "nested/../file",
            "nested/./file",
            "nested//file",
            "nested",
            "in-root-link",
            "escape/passwd",
        ] {
            assert!(root.open_regular(relative).is_err(), "{relative}");
        }
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let fifo = CString::new(tmp.0.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: a live NUL-terminated pathname in this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(root.open_regular("fifo").is_err());
    }

    #[test]
    fn returned_file_and_root_stay_bound_across_path_replacements() {
        let tmp = Scratch::new();
        let path = tmp.0.join("root");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("file"), b"original").unwrap();
        let root = ReadRoot::open(&path).unwrap();
        let file = root.open_regular("file").unwrap();
        fs::write(path.join("new"), b"replacement").unwrap();
        fs::rename(path.join("new"), path.join("file")).unwrap();
        assert_eq!(text(file), "original");
        fs::rename(&path, tmp.0.join("moved")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("file"), b"outside").unwrap();
        assert_eq!(text(root.open_regular("file").unwrap()), "replacement");
    }

    #[test]
    fn concurrent_ancestor_swap_never_reads_outside_root() {
        let tmp = Scratch::new();
        let inside = tmp.0.join("root");
        let outside = tmp.0.join("outside");
        fs::create_dir_all(inside.join("dir")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(inside.join("dir/file"), b"inside").unwrap();
        fs::write(outside.join("file"), b"outside").unwrap();
        let root = ReadRoot::open(&inside).unwrap();
        assert_eq!(text(root.open_regular("dir/file").unwrap()), "inside");
        let running = Arc::new(AtomicBool::new(true));
        let run = running.clone();
        let worker = std::thread::spawn(move || {
            while run.load(Ordering::Acquire) {
                fs::rename(inside.join("dir"), inside.join("saved")).unwrap();
                symlink(&outside, inside.join("dir")).unwrap();
                fs::remove_file(inside.join("dir")).unwrap();
                fs::rename(inside.join("saved"), inside.join("dir")).unwrap();
            }
        });
        let mut escaped = false;
        for _ in 0..2000 {
            if let Ok(file) = root.open_regular("dir/file") {
                escaped |= text(file) != "inside";
            }
        }
        running.store(false, Ordering::Release);
        worker.join().unwrap();
        assert!(!escaped);
    }
}
