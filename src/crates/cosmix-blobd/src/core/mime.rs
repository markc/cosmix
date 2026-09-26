//! Mime sniffing by file extension.
//!
//! The `infer` crate is not in `src/Cargo.lock`, so this is the
//! documented fallback: an extension map for the formats the mesh
//! actually moves. Unknown extensions and absent extensions return
//! `application/octet-stream`.

/// Sniff a mime type from a path or name's extension (lowercased).
pub fn sniff(path_or_name: &str) -> &'static str {
    let ext = path_or_name
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "tiff" | "tif" => "image/tiff",
        // Video
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        // Audio
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        // Text and documents
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // Archives
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "xz" => "application/x-xz",
        "zst" => "application/zstd",
        "br" => "application/x-brotli",
        "7z" => "application/x-7z-compressed",
        // Fonts
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_extensions_map() {
        assert_eq!(sniff("shot.png"), "image/png");
        assert_eq!(sniff("photo.JPG"), "image/jpeg");
        assert_eq!(sniff("clip.mkv"), "video/x-matroska");
        assert_eq!(sniff("a.b/c.json"), "application/json");
        assert_eq!(sniff("notes.md"), "text/markdown");
        assert_eq!(sniff("site.tar.gz"), "application/gzip");
    }

    #[test]
    fn unknown_and_absent_extensions_are_octet_stream() {
        assert_eq!(sniff("blob.bin"), "application/octet-stream");
        assert_eq!(sniff("weird.zzy"), "application/octet-stream");
        assert_eq!(sniff("no-extension"), "application/octet-stream");
        assert_eq!(sniff(""), "application/octet-stream");
    }
}
