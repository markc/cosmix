//! Opt-in, read-only publication of verified installed asset sets.
//!
//! A startup snapshot keeps request work bounded: each set is verified once,
//! then only allowlisted paths and unchanged file metadata are admitted. New
//! installations become visible after restart; `current` is never a URL.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::extract::{Extension, Path as RoutePath, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cosmix_assets::AssetSet;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::services::ServeFile;

const MAX_PUBLISHED_SETS: usize = 32;
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

#[derive(Default)]
pub(crate) struct Registry {
    sets: HashMap<String, PublishedSet>,
    cross_origin: bool,
}

struct PublishedSet {
    set: AssetSet,
    files: HashMap<String, PublishedFile>,
}

struct PublishedFile {
    stamp: FileStamp,
    etag: HeaderValue,
}

/// Include inode and ctime on Unix: replacing or rewriting a same-sized file
/// must not inherit the previously verified representation's cache identity.
#[derive(PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            bail!("asset file is not a regular file");
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            bytes: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        })
    }
}

impl Registry {
    pub(crate) fn with_cross_origin(mut self, enabled: bool) -> Self {
        self.cross_origin = enabled;
        self
    }

    pub(crate) fn load_optional(root: Option<&Path>) -> Result<Self> {
        let Some(root) = root else {
            return Ok(Self::default());
        };
        if !root.is_absolute() {
            bail!("shared asset installation must be an absolute path");
        }
        let mut registry = Self::default();
        let sets_dir = root.join("sets");
        for entry in std::fs::read_dir(&sets_dir)
            .with_context(|| format!("reading shared asset sets {}", sets_dir.display()))?
        {
            let entry = entry?;
            let Some(set_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Staging directories, locks and aliases cannot be published.
            if !cosmix_assets::valid_set_id(&set_id)
                || set_id == "current"
                || !entry.file_type()?.is_dir()
            {
                continue;
            }
            if registry.sets.len() >= MAX_PUBLISHED_SETS {
                bail!("at most {MAX_PUBLISHED_SETS} published asset sets may be served");
            }
            let set = AssetSet::open_published(root, &set_id)
                .with_context(|| format!("opening shared asset set {set_id}"))?;
            let mut files = HashMap::new();
            for file in &set.manifest().files {
                let path = set
                    .file_path(&file.path)?
                    .context("manifest asset is unavailable")?;
                files.insert(
                    file.path.clone(),
                    PublishedFile {
                        stamp: FileStamp::read(&path)?,
                        etag: HeaderValue::from_str(&format!("\"{}\"", file.sha256))?,
                    },
                );
            }
            for relative in ["fonts.css", "manifest.conf.mix"] {
                let path = set
                    .file_path(relative)?
                    .context("published asset metadata is unavailable")?;
                let stamp = FileStamp::read(&path)?;
                let bytes = std::fs::read(&path)?;
                if FileStamp::read(&path)? != stamp {
                    bail!("asset metadata changed while reading: {set_id}/{relative}");
                }
                files.insert(
                    relative.to_owned(),
                    PublishedFile {
                        stamp,
                        etag: HeaderValue::from_str(&format!("\"{:x}\"", Sha256::digest(bytes)))?,
                    },
                );
            }
            set.verify()
                .with_context(|| format!("verifying shared asset set {set_id}"))?;
            // A file changed while the verification ran: fail startup instead
            // of publishing a snapshot assembled from different generations.
            for (relative, file) in &files {
                let path = set.file_path(relative)?.context("asset disappeared")?;
                if FileStamp::read(&path)? != file.stamp {
                    bail!("asset changed during publication verification: {set_id}/{relative}");
                }
            }
            registry.sets.insert(set_id, PublishedSet { set, files });
        }
        Ok(registry)
    }
}

fn not_found() -> Response {
    let mut response = StatusCode::NOT_FOUND.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn mime_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "css" => "text/css; charset=utf-8",
        "txt" | "mix" | "codepoints" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(crate) async fn serve(
    registry: Option<Extension<Arc<Registry>>>,
    RoutePath((set_id, relative)): RoutePath<(String, String)>,
    mut request: Request,
) -> Response {
    let Some(Extension(registry)) = registry else {
        return not_found();
    };
    let Some(published) = registry.sets.get(&set_id) else {
        return not_found();
    };
    let Some(file) = published.files.get(&relative) else {
        return not_found();
    };
    let Ok(Some(path)) = published.set.file_path(&relative) else {
        return not_found();
    };
    if FileStamp::read(&path).ok().as_ref() != Some(&file.stamp) {
        return not_found();
    }
    let has_etag_condition = request.headers().contains_key(header::IF_NONE_MATCH);
    let matches_etag = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*"
                    || candidate.strip_prefix("W/").unwrap_or(candidate)
                        == file.etag.to_str().unwrap_or("")
            })
        });
    // RFC 9110 gives If-None-Match precedence over If-Modified-Since.
    if has_etag_condition {
        request.headers_mut().remove(header::IF_MODIFIED_SINCE);
    }
    let mut response = if matches_etag {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        match ServeFile::new(path).oneshot(request).await {
            Ok(response) => response.map(Body::new),
            Err(error) => match error {},
        }
    };
    let status = response.status();
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        if registry.cross_origin {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            );
        }
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(IMMUTABLE_CACHE),
        );
        headers.insert(header::ETAG, file.etag.clone());
        if status != StatusCode::NOT_MODIFIED {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(mime_type(&relative)),
            );
        }
    } else {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;

    const SET: &str = "test-core-1";
    const FONT: &[u8] = b"\x00\x01\x00\x00installed-test-font-bytes";
    const CSS: &str = "@font-face{font-family:Test;src:url(fonts/Test.ttf)}\n";

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sets").join(SET);
        std::fs::create_dir_all(root.join("fonts")).unwrap();
        std::fs::write(root.join("fonts/Test.ttf"), FONT).unwrap();
        std::fs::write(root.join("fonts.css"), CSS).unwrap();
        let manifest = serde_json::json!({
            "schema": "cosmix.static-assets.v1",
            "set_id": SET,
            "fonts": {"sans": "fonts/Test.ttf"},
            "files": [{
                "path": "fonts/Test.ttf",
                "url": "https://example.org/Test.ttf",
                "revision": "test-revision",
                "upstream": "https://example.org/fonts",
                "licence": "OFL-1.1",
                "bytes": FONT.len(),
                "sha256": format!("{:x}", Sha256::digest(FONT)),
                "blake3": blake3::hash(FONT).to_hex().to_string()
            }],
            "web_css": CSS
        });
        std::fs::write(
            root.join("manifest.conf.mix"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        temp
    }

    fn app(root: Option<&Path>) -> Router {
        let router =
            Router::new().route("/_cos/assets/{set_id}/{*path}", axum::routing::get(serve));
        match root {
            Some(root) => router.layer(Extension(Arc::new(
                Registry::load_optional(Some(root)).unwrap(),
            ))),
            None => router,
        }
    }

    fn request(path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cross_origin_public_font_reads_are_explicitly_enabled() {
        let temp = fixture();
        let registry = Registry::load_optional(Some(temp.path()))
            .unwrap()
            .with_cross_origin(true);
        let router = Router::new()
            .route("/_cos/assets/{set_id}/{*path}", axum::routing::get(serve))
            .layer(Extension(Arc::new(registry)));
        let mut req = request(&format!("/_cos/assets/{SET}/fonts/Test.ttf"));
        req.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://example.org"),
        );
        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
    }

    #[tokio::test]
    async fn published_bytes_mime_cache_and_conditional_get() {
        let temp = fixture();
        let router = app(Some(temp.path()));
        let path = format!("/_cos/assets/{SET}/fonts/Test.ttf");
        let response = router.clone().oneshot(request(&path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "font/ttf");
        assert_eq!(response.headers()[header::CACHE_CONTROL], IMMUTABLE_CACHE);
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
        let etag = response.headers()[header::ETAG].clone();
        let modified = response.headers()[header::LAST_MODIFIED].clone();
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            FONT
        );

        let mut conditional = request(&path);
        conditional
            .headers_mut()
            .insert(header::IF_NONE_MATCH, etag);
        let response = router.clone().oneshot(conditional).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], IMMUTABLE_CACHE);
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );

        let mut conditional = request(&path);
        conditional
            .headers_mut()
            .insert(header::IF_MODIFIED_SINCE, modified.clone());
        assert_eq!(
            router.clone().oneshot(conditional).await.unwrap().status(),
            StatusCode::NOT_MODIFIED
        );
        let mut mismatched = request(&path);
        mismatched
            .headers_mut()
            .insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        mismatched
            .headers_mut()
            .insert(header::IF_MODIFIED_SINCE, modified);
        assert_eq!(
            router.clone().oneshot(mismatched).await.unwrap().status(),
            StatusCode::OK
        );

        let response = router
            .oneshot(request(&format!("/_cos/assets/{SET}/fonts.css")))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            CSS
        );
    }

    #[tokio::test]
    async fn head_and_range_do_not_buffer_the_whole_font() {
        let temp = fixture();
        let router = app(Some(temp.path()));
        let path = format!("/_cos/assets/{SET}/fonts/Test.ttf");
        let mut head = request(&path);
        *head.method_mut() = axum::http::Method::HEAD;
        let response = router.clone().oneshot(head).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            FONT.len().to_string()
        );
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        let mut range = request(&path);
        range
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-3"));
        let response = router.oneshot(range).await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            &FONT[..4]
        );
    }

    #[tokio::test]
    async fn disabled_unlisted_alias_staging_and_traversal_are_denied() {
        let temp = fixture();
        let root = temp.path().join("sets").join(SET);
        std::fs::write(root.join("private.txt"), "not published").unwrap();
        let router = app(Some(temp.path()));
        for path in [
            format!("/_cos/assets/{SET}/private.txt"),
            format!("/_cos/assets/{SET}/../manifest.conf.mix"),
            format!("/_cos/assets/{SET}/%2e%2e/manifest.conf.mix"),
            format!("/_cos/assets/{SET}/fonts/%252e%252e/private.txt"),
            "/_cos/assets/current/fonts/Test.ttf".into(),
            "/_cos/assets/.stage-test/fonts/Test.ttf".into(),
        ] {
            let response = router.clone().oneshot(request(&path)).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let response = app(None)
            .oneshot(request(&format!("/_cos/assets/{SET}/fonts/Test.ttf")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn changed_or_symlinked_files_never_inherit_verified_cache_headers() {
        let temp = fixture();
        let router = app(Some(temp.path()));
        let path = temp.path().join("sets").join(SET).join("fonts/Test.ttf");
        let mut altered = FONT.to_vec();
        altered[4] ^= 1;
        std::fs::write(&path, altered).unwrap();
        let url = format!("/_cos/assets/{SET}/fonts/Test.ttf");
        let response = router.oneshot(request(&url)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::ETAG));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

        #[cfg(unix)]
        {
            let temp = fixture();
            let router = app(Some(temp.path()));
            let path = temp.path().join("sets").join(SET).join("fonts/Test.ttf");
            let outside = temp.path().join("outside.ttf");
            std::fs::write(&outside, FONT).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(outside, path).unwrap();
            assert_eq!(
                router.oneshot(request(&url)).await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[test]
    fn startup_rejects_bad_hashes_and_non_absolute_roots() {
        let temp = fixture();
        let path = temp.path().join("sets").join(SET).join("fonts/Test.ttf");
        let mut altered = FONT.to_vec();
        altered[4] ^= 1;
        std::fs::write(path, altered).unwrap();
        assert!(Registry::load_optional(Some(temp.path())).is_err());
        assert!(Registry::load_optional(Some(Path::new("relative/assets"))).is_err());
        assert!(Registry::load_optional(None).unwrap().sets.is_empty());
    }
}
