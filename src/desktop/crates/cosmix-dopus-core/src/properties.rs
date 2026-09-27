//! Selection metadata. Filesystem calls run only in a properties worker.
use crate::model::{FileEntry, sanitise_display_path, sanitise_display_text};
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug)]
pub struct Metadata {
    pub kind: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub permissions: String,
    /// Numeric identities remain meaningful when no name service is available.
    pub owner_group: String,
    pub symlink_target: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Properties {
    Folder {
        path: String,
        summary: String,
    },
    Entry {
        entry: FileEntry,
        count_pending: bool,
        metadata: Option<Result<Box<Metadata>, String>>,
    },
}

#[derive(Default)]
pub(crate) struct Slot {
    pub in_flight: Option<(u64, PathBuf)>,
    pub cached: Option<(u64, PathBuf, Result<Metadata, String>)>,
}

pub(crate) fn read(path: &Path) -> Result<Metadata, String> {
    // lstat preserves the link's own permissions/owner/times, including for
    // broken links. A link's target is a separate, sanitised display field.
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| sanitise_display_text(&e.to_string()))?;
    let link = metadata.file_type().is_symlink();
    let kind = if link {
        "Symbolic link".into()
    } else if metadata.is_dir() {
        "Folder".into()
    } else {
        file_kind(path)
    };
    #[cfg(unix)]
    let (permissions, owner_group) = {
        use std::os::unix::fs::MetadataExt;
        (
            permissions(metadata.mode()),
            format!("{}:{}", metadata.uid(), metadata.gid()),
        )
    };
    #[cfg(not(unix))]
    let (permissions, owner_group) = (
        if metadata.permissions().readonly() {
            "Read only"
        } else {
            "Writable"
        }
        .into(),
        "—".into(),
    );
    Ok(Metadata {
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        accessed: metadata.accessed().ok(),
        permissions,
        owner_group,
        symlink_target: if link {
            Some(
                std::fs::read_link(path)
                    .map(|p| sanitise_display_path(&p))
                    .unwrap_or_else(|e| {
                        format!("Unavailable: {}", sanitise_display_text(&e.to_string()))
                    }),
            )
        } else {
            None
        },
    })
}

pub fn permissions(mode: u32) -> String {
    let mut chars = ['-'; 9];
    for (i, ch) in ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x']
        .into_iter()
        .enumerate()
    {
        if mode & (1 << (8 - i)) != 0 {
            chars[i] = ch;
        }
    }
    for (bit, i, lower, upper) in [
        (0o4000, 2, 's', 'S'),
        (0o2000, 5, 's', 'S'),
        (0o1000, 8, 't', 'T'),
    ] {
        if mode & bit != 0 {
            chars[i] = if chars[i] == 'x' { lower } else { upper };
        }
    }
    format!(
        "{} ({:04o})",
        chars.iter().collect::<String>(),
        mode & 0o7777
    )
}

/// Extension-only MIME hint, never content sniffing or a file read.
pub fn file_kind(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match extension.as_str() {
        "txt" | "log" | "conf" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "rs" | "mix" | "c" | "h" | "toml" | "yaml" | "yml" => "text/plain",
        _ => "application/octet-stream",
    };
    if extension.is_empty() {
        format!("File ({mime}, guessed)")
    } else {
        format!(
            "{} file ({mime}, guessed)",
            sanitise_display_text(&extension.to_uppercase())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_special_bits_are_not_lost() {
        assert_eq!(permissions(0o100644), "rw-r--r-- (0644)");
        assert_eq!(permissions(0o104755), "rwsr-xr-x (4755)");
        assert_eq!(permissions(0o107644), "rwSr-Sr-T (7644)");
    }
    #[test]
    fn unknown_mime_is_explicitly_a_guess() {
        assert!(file_kind(Path::new("movie.MKV")).contains("video/x-matroska"));
        assert!(file_kind(Path::new("unknown.xyz")).contains("application/octet-stream, guessed"));
    }
    #[cfg(unix)]
    #[test]
    fn broken_links_have_metadata_and_a_sanitised_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link");
        std::os::unix::fs::symlink("absent\nfile", &path).unwrap();
        let meta = read(&path).unwrap();
        assert_eq!(meta.kind, "Symbolic link");
        assert!(!meta.symlink_target.unwrap().contains('\n'));
        assert!(meta.owner_group.contains(':'));
    }
}
