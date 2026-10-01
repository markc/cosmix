//! Local, immutable font/icon/emoji sets. No downloader, daemon or storage service.
//!
//! Native callers use [`AssetSet::discover`] once at startup, then keep that
//! selection while `current` changes. Servers open their configured system root
//! explicitly and call [`AssetSet::verify`] before publishing a set.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use cosmix_config::{CosmixDir, cosmix_path};
use serde::Deserialize;
use sha2::Digest;

const MANIFEST: &str = "manifest.conf.mix";
const MANIFEST_LIMIT: u64 = 256 * 1024;
const CATALOGUE_LIMIT: u64 = 1024 * 1024;

/// Locked installation data, parsed as strict non-executable Mix data.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub set_id: String,
    pub fonts: BTreeMap<String, String>,
    #[serde(default)]
    pub font_families: BTreeMap<String, String>,
    pub files: Vec<AssetFile>,
    pub web_css: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetFile {
    pub path: String,
    pub url: String,
    pub revision: String,
    pub upstream: String,
    pub licence: String,
    pub bytes: u64,
    pub sha256: String,
    pub blake3: String,
}

/// Lookup inputs are explicit so precedence can be tested without mutating env.
#[derive(Debug, Clone)]
pub struct DiscoveryPaths {
    pub home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_data_dirs: Option<OsString>,
    pub share: PathBuf,
}

impl DiscoveryPaths {
    pub fn from_environment() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS"),
            share: cosmix_path(CosmixDir::Share),
        }
    }

    pub fn asset_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(data_home) = self.xdg_data_home.as_ref().filter(|p| p.is_absolute()) {
            roots.push(data_home.join("cosmix/assets"));
        } else if let Some(home) = self.home.as_ref().filter(|p| p.is_absolute()) {
            roots.push(home.join(".local/share/cosmix/assets"));
        }
        let dirs = self.xdg_data_dirs.as_ref().filter(|dirs| !dirs.is_empty());
        let defaults = OsString::from("/usr/local/share:/usr/share");
        for dir in std::env::split_paths(dirs.unwrap_or(&defaults)) {
            if dir.is_absolute() {
                roots.push(dir.join("cosmix/assets"));
            }
        }
        roots.push(self.share.join("assets"));
        let mut seen = BTreeSet::new();
        roots.retain(|root| seen.insert(root.clone()));
        roots
    }
}

/// A complete selection, pinned to a concrete published directory.
#[derive(Debug, Clone)]
pub struct AssetSet {
    assets_root: PathBuf,
    root: PathBuf,
    manifest: Manifest,
    icons: BTreeMap<String, char>,
}

impl AssetSet {
    /// Choose the first installed complete set and verify its bytes once.
    /// A malformed existing override is an error; only absent roots fall through.
    pub fn discover() -> Result<Option<Self>> {
        Self::discover_with(&DiscoveryPaths::from_environment())
    }

    pub fn discover_with(paths: &DiscoveryPaths) -> Result<Option<Self>> {
        for root in paths.asset_roots() {
            if let Some(set) = Self::load_current(&root)? {
                set.verify()?;
                return Ok(Some(set));
            }
        }
        Ok(None)
    }

    /// Follow the activation link exactly once; a dangling link is an error.
    pub fn load_current(root: &Path) -> Result<Option<Self>> {
        let current = root.join("current");
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("inspect {}", current.display())),
        };
        ensure!(
            metadata.file_type().is_symlink(),
            "asset current must be a symlink"
        );
        let target = fs::read_link(&current).context("read asset activation link")?;
        let text = target
            .to_str()
            .context("asset activation link is not UTF-8")?;
        let id = text
            .strip_prefix("sets/")
            .context("asset current must name sets/<id>")?;
        ensure!(valid_set_id(id), "invalid asset activation set ID");
        Ok(Some(Self::open_published(root, id)?))
    }

    /// Open only published data. Checks metadata and the generated stylesheet,
    /// but leaves streaming payload hashes to `verify`, avoiding per-request I/O.
    pub fn open_published(assets_root: &Path, set_id: &str) -> Result<Self> {
        ensure!(valid_set_id(set_id), "invalid static asset set ID");
        let assets_root = fs::canonicalize(assets_root).context("resolve static asset root")?;
        let root = checked_directory(&assets_root, &format!("sets/{set_id}"))?;
        let manifest_path = checked_file(&root, MANIFEST)?;
        let text = read_bounded(&manifest_path, MANIFEST_LIMIT)?;
        let manifest: Manifest = cosmix_config::from_conf_mix_str(&text)
            .context("parse static asset manifest as strict Mix data")?;
        validate_manifest(&manifest, set_id)?;
        for file in &manifest.files {
            let path = checked_file(&root, &file.path)?;
            ensure!(
                fs::metadata(path)?.len() == file.bytes,
                "asset size mismatch: {}",
                file.path
            );
        }
        let css = read_bounded(&checked_file(&root, "fonts.css")?, MANIFEST_LIMIT)?;
        ensure!(
            css == manifest.web_css,
            "asset stylesheet differs from locked manifest"
        );
        let icons = if let Some(font) = manifest.fonts.get("icons") {
            let catalogue = Path::new(font).with_extension("codepoints");
            let catalogue = catalogue.to_str().context("invalid icon catalogue path")?;
            ensure!(
                manifest.files.iter().any(|file| file.path == catalogue),
                "icon catalogue not locked"
            );
            parse_codepoints(&read_bounded(
                &checked_file(&root, catalogue)?,
                CATALOGUE_LIMIT,
            )?)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            assets_root,
            root,
            manifest,
            icons,
        })
    }

    pub fn set_id(&self) -> &str {
        &self.manifest.set_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn assets_root(&self) -> &Path {
        &self.assets_root
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn family(&self, role: &str) -> Option<&str> {
        self.manifest.font_families.get(role).map(String::as_str)
    }
    pub fn font_path(&self, role: &str) -> Option<PathBuf> {
        self.manifest
            .fonts
            .get(role)
            .map(|path| self.root.join(path))
    }
    pub fn font_paths(&self) -> Vec<PathBuf> {
        self.manifest
            .fonts
            .values()
            .map(|path| self.root.join(path))
            .collect()
    }
    pub fn icon(&self, name: &str) -> Option<char> {
        self.icons.get(name).copied()
    }

    /// Resolve only a manifest entry or its two known generated metadata files.
    /// Symlinks, directories and escapes remain forbidden on every lookup.
    pub fn file_path(&self, relative: &str) -> Result<Option<PathBuf>> {
        ensure!(valid_relative_path(relative), "invalid static asset path");
        if relative != MANIFEST
            && relative != "fonts.css"
            && !self.manifest.files.iter().any(|file| file.path == relative)
        {
            return Ok(None);
        }
        checked_directory(&self.assets_root, &format!("sets/{}", self.set_id()))?;
        let path = checked_file(&self.root, relative)?;
        if let Some(file) = self
            .manifest
            .files
            .iter()
            .find(|file| file.path == relative)
        {
            ensure!(
                fs::metadata(&path)?.len() == file.bytes,
                "asset size mismatch: {relative}"
            );
        }
        Ok(Some(path))
    }

    /// Verify both locked hashes using bounded streaming memory.
    pub fn verify(&self) -> Result<()> {
        let css = self
            .file_path("fonts.css")?
            .context("asset stylesheet unavailable")?;
        ensure!(
            read_bounded(&css, MANIFEST_LIMIT)? == self.manifest.web_css,
            "asset stylesheet differs from locked manifest"
        );
        for entry in &self.manifest.files {
            let path = self
                .file_path(&entry.path)?
                .context("locked asset unavailable")?;
            let mut file = File::open(path).context("open locked asset")?;
            let mut sha = sha2::Sha256::new();
            let mut b3 = blake3::Hasher::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut bytes = 0u64;
            loop {
                let length = file.read(&mut buffer)?;
                if length == 0 {
                    break;
                }
                bytes += length as u64;
                ensure!(
                    bytes <= entry.bytes,
                    "asset grew during verification: {}",
                    entry.path
                );
                sha.update(&buffer[..length]);
                b3.update(&buffer[..length]);
            }
            ensure!(bytes == entry.bytes, "asset size mismatch: {}", entry.path);
            ensure!(
                hex::encode(sha.finalize()) == entry.sha256,
                "asset SHA-256 mismatch: {}",
                entry.path
            );
            ensure!(
                b3.finalize().to_hex().as_str() == entry.blake3,
                "asset BLAKE3 mismatch: {}",
                entry.path
            );
        }
        Ok(())
    }
}

pub fn valid_set_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 96
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        })
}

fn validate_manifest(manifest: &Manifest, id: &str) -> Result<()> {
    ensure!(
        manifest.schema == "cosmix.static-assets.v1",
        "unsupported asset manifest schema"
    );
    ensure!(manifest.set_id == id, "asset manifest set ID mismatch");
    ensure!(
        !manifest.files.is_empty() && manifest.files.len() <= 256,
        "invalid locked asset count"
    );
    ensure!(
        !manifest.fonts.is_empty() && manifest.fonts.len() <= 32,
        "invalid font role count"
    );
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        ensure!(
            valid_relative_path(&file.path) && file.path != MANIFEST && file.path != "fonts.css",
            "invalid locked asset path"
        );
        ensure!(
            paths.insert(file.path.clone()),
            "duplicate locked asset path"
        );
        ensure!(
            file.bytes > 0 && file.bytes <= 256 * 1024 * 1024,
            "invalid asset size"
        );
        for hash in [&file.sha256, &file.blake3] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                "invalid asset hash"
            );
        }
        ensure!(
            file.url.starts_with("https://") && file.upstream.starts_with("https://"),
            "asset provenance must use HTTPS"
        );
        ensure!(
            !file.revision.is_empty() && !file.licence.is_empty(),
            "missing asset provenance"
        );
    }
    for (role, path) in &manifest.fonts {
        ensure!(
            valid_set_id(role) && paths.contains(path),
            "font role references an unlocked asset"
        );
        ensure!(
            path.ends_with(".ttf") || path.ends_with(".otf"),
            "native role must name a font"
        );
    }
    for (role, family) in &manifest.font_families {
        ensure!(
            manifest.fonts.contains_key(role)
                && !family.is_empty()
                && family.len() <= 128
                && !family.chars().any(char::is_control),
            "invalid font family metadata"
        );
    }
    Ok(())
}

fn checked_directory(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        valid_relative_path(relative),
        "invalid asset directory path"
    );
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "asset directories must not be symlinks"
        );
    }
    Ok(path)
}

fn checked_file(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(valid_relative_path(relative), "invalid static asset path");
    let relative_path = Path::new(relative);
    let parent = relative_path.parent().context("asset has no parent")?;
    let directory = if parent.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        checked_directory(root, parent.to_str().context("non-UTF8 asset parent")?)?
    };
    // Recheck the pinned set itself, including after an administrative update.
    let root_metadata = fs::symlink_metadata(root)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "asset set must not be a symlink"
    );
    let path = directory.join(
        relative_path
            .file_name()
            .context("asset has no file name")?,
    );
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "asset must be a regular file: {relative}"
    );
    Ok(path)
}

fn read_bounded(path: &Path, limit: u64) -> Result<String> {
    ensure!(
        fs::metadata(path)?.len() <= limit,
        "asset metadata file too large"
    );
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= limit, "asset metadata file too large");
    Ok(text)
}

fn parse_codepoints(text: &str) -> Result<BTreeMap<String, char>> {
    let mut icons = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let name = parts.next().context("missing icon name")?;
        let hex = parts.next().context("missing icon codepoint")?;
        ensure!(
            parts.next().is_none() && valid_set_id(name),
            "invalid icon catalogue row"
        );
        let scalar = u32::from_str_radix(hex, 16).context("invalid icon codepoint")?;
        let character = char::from_u32(scalar).context("icon codepoint is not a Unicode scalar")?;
        if icons.insert(name.to_owned(), character).is_some() {
            bail!("duplicate icon catalogue name: {name}");
        }
    }
    ensure!(!icons.is_empty(), "empty icon catalogue");
    Ok(icons)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture(root: &Path, id: &str) -> PathBuf {
        let dir = root.join("sets").join(id);
        fs::create_dir_all(dir.join("fonts")).unwrap();
        fs::create_dir_all(dir.join("icons")).unwrap();
        let files = [
            ("fonts/Sans.ttf", b"font bytes".as_slice()),
            ("icons/Symbols.ttf", b"icon font".as_slice()),
            (
                "icons/Symbols.codepoints",
                b"delete e872\nfolder e2c7\n".as_slice(),
            ),
        ];
        let mut entries = Vec::new();
        for (path, bytes) in files {
            fs::write(dir.join(path), bytes).unwrap();
            entries.push(serde_json::json!({
                "path": path, "bytes": bytes.len(),
                "url": "https://example.org/font", "upstream": "https://example.org/",
                "revision": "pinned", "licence": "OFL-1.1",
                "sha256": hex::encode(sha2::Sha256::digest(bytes)),
                "blake3": blake3::hash(bytes).to_hex().to_string()
            }));
        }
        fs::write(
            dir.join(MANIFEST),
            serde_json::to_string(&serde_json::json!({
                "schema": "cosmix.static-assets.v1", "set_id": id,
                "fonts": { "sans": "fonts/Sans.ttf", "icons": "icons/Symbols.ttf" },
                "font_families": { "sans": "Fixture Sans", "icons": "Fixture Symbols" },
                "files": entries, "web_css": "/* fixture */\n"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(dir.join("fonts.css"), "/* fixture */\n").unwrap();
        dir
    }

    fn activate(root: &Path, id: &str) {
        let current = root.join("current");
        if fs::symlink_metadata(&current).is_ok() {
            fs::remove_file(&current).unwrap();
        }
        symlink(format!("sets/{id}"), current).unwrap();
    }

    #[test]
    fn published_roles_icons_and_allowlist_are_shared() {
        let temp = tempfile::tempdir().unwrap();
        let directory = fixture(temp.path(), "one");
        let set = AssetSet::open_published(temp.path(), "one").unwrap();
        set.verify().unwrap();
        assert_eq!(set.root(), directory);
        assert_eq!(
            set.font_path("sans"),
            Some(directory.join("fonts/Sans.ttf"))
        );
        assert_eq!(set.font_path("missing"), None);
        assert_eq!(set.family("sans"), Some("Fixture Sans"));
        assert_eq!(set.font_paths().len(), 2);
        assert_eq!(set.icon("delete"), Some('\u{e872}'));
        assert_eq!(set.icon("unknown"), None);
        assert!(set.file_path("fonts.css").unwrap().is_some());
        assert!(set.file_path(MANIFEST).unwrap().is_some());
        fs::write(directory.join("unlocked.txt"), "private").unwrap();
        assert!(set.file_path("unlocked.txt").unwrap().is_none());
        assert!(set.file_path("../unlocked.txt").is_err());
    }

    #[test]
    fn selection_survives_current_activation_swap() {
        let temp = tempfile::tempdir().unwrap();
        let one = fixture(temp.path(), "one");
        fixture(temp.path(), "two");
        activate(temp.path(), "one");
        let selected = AssetSet::load_current(temp.path()).unwrap().unwrap();
        activate(temp.path(), "two");
        assert_eq!(selected.root(), one);
        assert_eq!(selected.set_id(), "one");
        assert_eq!(
            AssetSet::load_current(temp.path())
                .unwrap()
                .unwrap()
                .set_id(),
            "two"
        );
        selected.verify().unwrap();
    }

    #[test]
    fn lookup_respects_xdg_order_and_ignores_relative_entries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let user = temp.path().join("user");
        let dist = temp.path().join("dist");
        let share = temp.path().join("share");
        let paths = DiscoveryPaths {
            home: Some(home.clone()),
            xdg_data_home: Some(user.clone()),
            xdg_data_dirs: Some(
                std::env::join_paths([Path::new("relative"), dist.as_path()]).unwrap(),
            ),
            share: share.clone(),
        };
        assert_eq!(
            paths.asset_roots(),
            vec![
                user.join("cosmix/assets"),
                dist.join("cosmix/assets"),
                share.join("assets")
            ]
        );
        fixture(&share.join("assets"), "system");
        activate(&share.join("assets"), "system");
        assert_eq!(
            AssetSet::discover_with(&paths).unwrap().unwrap().set_id(),
            "system"
        );
        fixture(&dist.join("cosmix/assets"), "distribution");
        activate(&dist.join("cosmix/assets"), "distribution");
        assert_eq!(
            AssetSet::discover_with(&paths).unwrap().unwrap().set_id(),
            "distribution"
        );
        fixture(&user.join("cosmix/assets"), "personal");
        activate(&user.join("cosmix/assets"), "personal");
        assert_eq!(
            AssetSet::discover_with(&paths).unwrap().unwrap().set_id(),
            "personal"
        );
        let mut relative = paths.clone();
        relative.xdg_data_home = Some(PathBuf::from("relative"));
        assert_eq!(
            relative.asset_roots()[0],
            home.join(".local/share/cosmix/assets")
        );
        let mut empty = paths;
        empty.xdg_data_dirs = Some(OsString::new());
        assert!(
            empty
                .asset_roots()
                .contains(&PathBuf::from("/usr/share/cosmix/assets"))
        );
    }

    #[test]
    fn malformed_or_dangling_override_does_not_fall_through() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("user");
        let root = data.join("cosmix/assets");
        let share = temp.path().join("shared");
        fixture(&root, "broken");
        activate(&root, "broken");
        fs::write(root.join("sets/broken/manifest.conf.mix"), "{bad: true}").unwrap();
        fixture(&share.join("assets"), "valid");
        activate(&share.join("assets"), "valid");
        let paths = DiscoveryPaths {
            home: None,
            xdg_data_home: Some(data),
            xdg_data_dirs: Some(OsString::from("relative")),
            share,
        };
        assert!(AssetSet::discover_with(&paths).is_err());
        activate(&root, "absent");
        assert!(AssetSet::discover_with(&paths).is_err());
    }

    #[test]
    fn payload_tampering_and_stylesheet_changes_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let dir = fixture(temp.path(), "one");
        let set = AssetSet::open_published(temp.path(), "one").unwrap();
        fs::write(dir.join("fonts/Sans.ttf"), b"tampered!!").unwrap();
        assert!(set.verify().unwrap_err().to_string().contains("SHA-256"));
        fs::write(dir.join("fonts.css"), "/* altered */\n").unwrap();
        assert!(AssetSet::open_published(temp.path(), "one").is_err());
    }

    #[test]
    fn symlink_payloads_directories_and_activation_escapes_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let dir = fixture(temp.path(), "one");
        let outside = temp.path().join("outside.ttf");
        fs::write(&outside, b"font bytes").unwrap();
        let font = dir.join("fonts/Sans.ttf");
        fs::remove_file(&font).unwrap();
        symlink(&outside, &font).unwrap();
        assert!(AssetSet::open_published(temp.path(), "one").is_err());
        fs::remove_file(&font).unwrap();
        fs::write(&font, b"font bytes").unwrap();
        let fonts = dir.join("fonts");
        let renamed = dir.join("original-fonts");
        fs::rename(&fonts, &renamed).unwrap();
        symlink(&renamed, &fonts).unwrap();
        assert!(AssetSet::open_published(temp.path(), "one").is_err());
        symlink("../outside", temp.path().join("current")).unwrap();
        assert!(AssetSet::load_current(temp.path()).is_err());
    }

    #[test]
    fn catalogue_rejects_duplicate_names_and_non_scalars() {
        assert!(parse_codepoints("delete e872\ndelete e873").is_err());
        assert!(parse_codepoints("bad d800").is_err());
        assert!(parse_codepoints("bad 110000").is_err());
        assert!(parse_codepoints("bad e872 extra").is_err());
        assert_eq!(
            parse_codepoints("folder e2c7\n").unwrap()["folder"],
            '\u{e2c7}'
        );
    }

    #[test]
    fn schemas_duplicates_and_unlocked_roles_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let dir = fixture(temp.path(), "one");
        let manifest_path = dir.join(MANIFEST);
        let original: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        for bad in [
            "unknown-field",
            "duplicate",
            "unlocked-role",
            "escape",
            "wrong-id",
        ] {
            let mut json = original.clone();
            match bad {
                "unknown-field" => json["unexpected"] = serde_json::json!(true),
                "duplicate" => {
                    let duplicate = json["files"][0].clone();
                    json["files"].as_array_mut().unwrap().push(duplicate);
                }
                "unlocked-role" => json["fonts"]["sans"] = serde_json::json!("fonts/Absent.ttf"),
                "escape" => json["files"][0]["path"] = serde_json::json!("../outside.ttf"),
                _ => json["set_id"] = serde_json::json!("other"),
            }
            fs::write(&manifest_path, serde_json::to_string(&json).unwrap()).unwrap();
            assert!(
                AssetSet::open_published(temp.path(), "one").is_err(),
                "{bad}"
            );
        }
        let mut legacy = original;
        legacy.as_object_mut().unwrap().remove("font_families");
        fs::write(&manifest_path, serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(
            AssetSet::open_published(temp.path(), "one")
                .unwrap()
                .family("sans"),
            None
        );
    }
}
