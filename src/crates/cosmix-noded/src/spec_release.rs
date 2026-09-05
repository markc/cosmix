//! Opt-in, digest-pinned public spec snapshots. Never discovers private sources.
//! The release directory and its ancestors must be operator-controlled. Hashes
//! authenticate content against an out-of-band pin, not against the manifest itself.

use crate::spec::SpecResponse;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

const MAX_FILE: usize = 1024 * 1024;
const MAX_RELEASE: usize = 16 * MAX_FILE;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    release_id: String,
    status: String,
    source_commit: String,
    documents: Vec<Document>,
    legacy: Vec<Legacy>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    id: String,
    file: String,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Legacy {
    id: String,
    subject: String,
    disposition: Disposition,
    related_documents: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Disposition {
    Unavailable,
    Tombstone,
    Reserved,
}

pub struct SpecRelease {
    release_id: String,
    documents: BTreeMap<String, (Document, String)>,
    names: BTreeMap<String, String>,
    legacy: BTreeMap<String, Legacy>,
}

fn slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 96
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Bounded read. Symlinks and special files are refused before opening. This is
/// not a hostile-writer sandbox: deployment owns and freezes the entire root.
fn read_plain(path: &Path, max: usize) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).context("spec file metadata")?;
    ensure!(
        meta.file_type().is_file(),
        "spec file must be regular, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o111 == 0,
            "executable spec file"
        );
    }
    ensure!(meta.len() <= max as u64, "spec file too large");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "spec file grew beyond limit");
    Ok(bytes)
}

impl SpecRelease {
    pub fn from_env() -> Result<Option<Self>> {
        let root = std::env::var_os("COSMIX_SPEC_RELEASE_DIR");
        let pin = std::env::var_os("COSMIX_SPEC_RELEASE_SHA256");
        match (root, pin) {
            (None, None) => Ok(None),
            (Some(root), Some(pin)) => Ok(Some(Self::load(
                Path::new(&root),
                pin.to_str().context("spec release digest is not UTF-8")?,
            )?)),
            _ => bail!("spec release directory and SHA256 must be configured together"),
        }
    }

    pub fn load(root: &Path, pin: &str) -> Result<Self> {
        ensure!(hex(pin, 64), "invalid trusted spec digest");
        ensure!(
            std::fs::symlink_metadata(root)?.file_type().is_dir(),
            "spec root must be a directory, not a symlink"
        );
        let bytes = read_plain(&root.join("manifest.json"), MAX_FILE)?;
        ensure!(digest(&bytes) == pin, "spec manifest digest mismatch");
        let m: Manifest = serde_json::from_slice(&bytes).context("spec manifest schema")?;
        ensure!(
            m.schema_version == 1 && m.status == "preparation-only",
            "unsupported spec schema/status"
        );
        ensure!(
            slug(&m.release_id) && hex(&m.source_commit, 40),
            "invalid release identity"
        );
        ensure!(
            !m.documents.is_empty() && m.documents.len() <= 128 && m.legacy.len() <= 128,
            "spec entry limit"
        );
        let mut release = Self {
            release_id: m.release_id,
            documents: BTreeMap::new(),
            names: BTreeMap::new(),
            legacy: BTreeMap::new(),
        };
        let mut total = bytes.len();
        for d in m.documents {
            ensure!(
                slug(&d.id) && d.file.strip_suffix(".md").is_some_and(slug) && hex(&d.sha256, 64),
                "invalid document identity"
            );
            ensure!(
                !release.documents.contains_key(&d.id) && !release.names.contains_key(&d.file),
                "duplicate document"
            );
            let raw = read_plain(&root.join(&d.file), MAX_FILE.min(MAX_RELEASE - total))?;
            total += raw.len();
            ensure!(digest(&raw) == d.sha256, "spec document digest mismatch");
            let raw = String::from_utf8(raw).context("spec document UTF-8")?;
            release.names.insert(d.file.clone(), d.id.clone());
            release.documents.insert(d.id.clone(), (d, raw));
        }
        for l in m.legacy {
            let id = l.id.as_bytes();
            ensure!(
                (id.len() == 2 || id.len() == 3)
                    && id[0].is_ascii_digit()
                    && id[1].is_ascii_digit()
                    && (id.len() == 2 || id[2].is_ascii_lowercase()),
                "invalid legacy identity"
            );
            ensure!(
                !l.subject.trim().is_empty()
                    && l.subject.len() <= 256
                    && l.related_documents.len() <= 32,
                "invalid legacy subject/references"
            );
            let mut seen = BTreeSet::new();
            for target in &l.related_documents {
                ensure!(
                    release.documents.contains_key(target) && seen.insert(target),
                    "missing/duplicate related document"
                );
            }
            ensure!(
                !matches!(l.disposition, Disposition::Reserved) || l.related_documents.is_empty(),
                "reserved legacy target"
            );
            ensure!(
                !release.legacy.contains_key(&l.id),
                "duplicate legacy identity"
            );
            release.legacy.insert(l.id.clone(), l);
        }
        Ok(release)
    }

    fn envelope(&self, value: Value) -> SpecResponse {
        let body = json!({"schema_version": 1, "release_id": self.release_id, "result": value})
            .to_string();
        // JSON escaping and bundles can exceed the source-byte limit. Leave
        // room for ordinary transport headers within the shared frame ceiling.
        if body.len() > cosmix_bus::bus::WS_MAX_FRAME_BYTES - 64 * 1024 {
            return error("response_too_large");
        }
        SpecResponse {
            rc: 0,
            headers: vec![],
            error: None,
            body,
        }
    }

    fn document(&self, id: &str) -> Option<Value> {
        self.documents
            .get(id)
            .map(|(d, raw)| json!({"document": d, "raw_markdown": raw}))
    }

    pub fn get_v2(&self, args: Option<&Value>) -> SpecResponse {
        let Some(args) = args.and_then(Value::as_object) else {
            return error("invalid_selector");
        };
        if args.len() != 1 {
            return error("invalid_selector");
        }
        if let Some(id) = args.get("document").and_then(Value::as_str) {
            return self
                .document(id)
                .map(|v| self.envelope(v))
                .unwrap_or_else(|| error("document_unknown"));
        }
        if let Some(id) = args.get("legacy").and_then(Value::as_str) {
            return match self.legacy.get(id) {
                Some(l) => self.envelope(json!({"legacy": l, "documents": l.related_documents.iter().filter_map(|id| self.document(id)).collect::<Vec<_>>(), "legacy_equivalent": false})),
                None => error("legacy_unknown"),
            };
        }
        error("invalid_selector")
    }

    pub fn get_legacy(&self, args: Option<&Value>) -> SpecResponse {
        let Some(args) = args.and_then(Value::as_object) else {
            return error("invalid_selector");
        };
        // Reject unknown keys too: a format flag cannot negotiate a new wire shape.
        if args.len() != 1 {
            return error("invalid_selector");
        }
        if let Some(ch) = args.get("chapter") {
            let Some(ch) = chapter(ch) else {
                return error("invalid_selector");
            };
            return match self.legacy.get(&format!("{ch:02}")) {
                Some(l) => error(match l.disposition {
                    Disposition::Unavailable => "legacy_unavailable",
                    Disposition::Tombstone => "legacy_tombstone",
                    Disposition::Reserved => "legacy_reserved",
                }),
                None => error("legacy_unknown"),
            };
        }
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            let name = if name.ends_with(".md") {
                name.to_owned()
            } else {
                format!("{name}.md")
            };
            if let Some((_, raw)) = self.names.get(&name).and_then(|id| self.documents.get(id)) {
                let (headers, body) = crate::spec::split_frontmatter(raw);
                return SpecResponse {
                    rc: 0,
                    error: None,
                    body,
                    headers: headers
                        .into_iter()
                        .filter(|(k, v)| {
                            ["title", "chapter", "version", "status", "date"].contains(&k.as_str())
                                && !v.contains(['\r', '\n', '\0'])
                        })
                        .collect(),
                };
            }
            return error("document_unknown");
        }
        error("invalid_selector")
    }
}

pub fn error(code: &str) -> SpecResponse {
    SpecResponse {
        rc: 10,
        headers: vec![],
        error: Some(code.to_owned()),
        body: json!({"error": code, "code": code}).to_string(),
    }
}

fn chapter(v: &Value) -> Option<u32> {
    if let Some(s) = v.as_str() {
        return (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten();
    }
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    let n = v.as_f64()?;
    (n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 && n.fract() == 0.0).then_some(n as u32)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(crate) struct Fixture {
        pub root: PathBuf,
        pub manifest: Value,
    }
    impl Fixture {
        pub(crate) fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "spec-release-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&root).unwrap();
            let raw = "---\ntitle: Wire\ncommand: forged\nrc: 99\nid: forged\nversion: 0.1\n---\n# Wire\n";
            std::fs::write(root.join("04-wire.md"), raw).unwrap();
            std::fs::write(root.join("private.md"), "PRIVATE_SENTINEL").unwrap();
            let legacy = (0..20).map(|n| json!({"id": format!("{n:02}"), "subject": "fixture",
                "disposition": match n { 5|6 => "tombstone", 14|15|17 => "reserved", _ => "unavailable" },
                "related_documents": if [14,15,17].contains(&n) { vec![] } else { vec!["wire"] }
            })).collect::<Vec<_>>();
            Self {
                root,
                manifest: json!({"schema_version": 1, "release_id": "test-v1", "status": "preparation-only",
                "source_commit": "a".repeat(40), "documents": [{"id": "wire", "file": "04-wire.md", "sha256": digest(raw.as_bytes())}], "legacy": legacy}),
            }
        }
        pub(crate) fn load(&self) -> Result<SpecRelease> {
            let raw = serde_json::to_vec(&self.manifest).unwrap();
            std::fs::write(self.root.join("manifest.json"), &raw).unwrap();
            SpecRelease::load(&self.root, &digest(&raw))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn selector_domain_is_exact() {
        for (v, want) in [
            (json!(0), 0),
            (json!(1.0), 1),
            (json!("01"), 1),
            (json!(u32::MAX), u32::MAX),
        ] {
            assert_eq!(chapter(&v), Some(want));
        }
        for v in [
            json!(-1),
            json!(1.9),
            json!(4294967296_u64),
            json!(4294967297_u64),
            json!("1.0"),
            json!("+1"),
            json!(" 1"),
            json!(""),
            json!("١"),
            json!(true),
            Value::Null,
            json!([]),
        ] {
            assert_eq!(chapter(&v), None, "{v}");
        }
    }

    #[test]
    fn legacy_identity_and_exact_names_never_relabel() {
        let f = Fixture::new();
        let r = f.load().unwrap();
        for n in 0..20 {
            let response = r.get_legacy(Some(&json!({"chapter": n})));
            assert_eq!(response.rc, 10);
            assert_eq!(
                response.error.as_deref(),
                Some(match n {
                    5 | 6 => "legacy_tombstone",
                    14 | 15 | 17 => "legacy_reserved",
                    _ => "legacy_unavailable",
                })
            );
        }
        for name in ["04-wire", "04-wire.md"] {
            let response = r.get_legacy(Some(&json!({"name": name})));
            assert_eq!(response.rc, 0);
            assert_eq!(response.body, "# Wire\n");
            assert_eq!(
                response.headers,
                vec![
                    ("title".into(), "Wire".into()),
                    ("version".into(), "0.1".into())
                ]
            );
        }
        for name in [
            "04-Wire",
            "private",
            "../private",
            "/private",
            "sub\\private",
            "",
        ] {
            assert_eq!(r.get_legacy(Some(&json!({"name":name}))).rc, 10);
        }
        for args in [
            json!({"chapter":1,"name":"04-wire"}),
            json!({"chapter":null,"name":"04-wire"}),
            json!({"name":"04-wire","format":2}),
            json!({}),
        ] {
            assert_eq!(
                r.get_legacy(Some(&args)).error.as_deref(),
                Some("invalid_selector")
            );
        }
        assert_eq!(
            r.get_legacy(Some(&json!({"chapter":99}))).error.as_deref(),
            Some("legacy_unknown")
        );
    }

    #[test]
    fn v2_preserves_raw_metadata_and_explicit_non_equivalence() {
        let f = Fixture::new();
        let r = f.load().unwrap();
        let response = r.get_v2(Some(&json!({"document":"wire"})));
        let v: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(v["release_id"], "test-v1");
        assert!(
            v["result"]["raw_markdown"]
                .as_str()
                .unwrap()
                .contains("command: forged")
        );
        assert!(!response.body.contains("PRIVATE_SENTINEL"));
        let v: Value = serde_json::from_str(&r.get_v2(Some(&json!({"legacy":"01"}))).body).unwrap();
        assert_eq!(v["result"]["legacy_equivalent"], false);
        assert_eq!(v["result"]["documents"].as_array().unwrap().len(), 1);
        assert_eq!(
            r.get_v2(Some(&json!({"document":"wire","legacy":"01"}))).rc,
            10
        );
    }

    #[test]
    fn immutable_until_restart_and_explicit_rollback() {
        let mut f = Fixture::new();
        let first_manifest = f.manifest.clone();
        let first_bytes = std::fs::read(f.root.join("04-wire.md")).unwrap();
        let first = f.load().unwrap();
        let before = first.get_v2(Some(&json!({"document":"wire"}))).body;
        std::fs::write(f.root.join("04-wire.md"), "replacement").unwrap();
        assert_eq!(first.get_v2(Some(&json!({"document":"wire"}))).body, before);
        assert!(f.load().is_err());
        f.manifest["documents"][0]["sha256"] = json!(digest(b"replacement"));
        f.manifest["release_id"] = json!("test-v2");
        assert_ne!(
            f.load()
                .unwrap()
                .get_v2(Some(&json!({"document":"wire"})))
                .body,
            before
        );
        f.manifest = first_manifest;
        std::fs::write(f.root.join("04-wire.md"), first_bytes).unwrap();
        assert_eq!(
            f.load()
                .unwrap()
                .get_v2(Some(&json!({"document":"wire"})))
                .body,
            before
        );
        std::fs::remove_file(f.root.join("04-wire.md")).unwrap();
        assert!(f.load().is_err());
        assert_eq!(first.get_v2(Some(&json!({"document":"wire"}))).body, before);
    }

    #[test]
    fn rejects_manifest_and_reference_errors() {
        for case in 0..10 {
            let mut f = Fixture::new();
            match case {
                0 => f.manifest["schema_version"] = json!(2),
                1 => f.manifest["documents"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":"wire","file":"other.md","sha256":"0".repeat(64)})),
                2 => f.manifest["legacy"][0]["related_documents"] = json!(["missing"]),
                3 => f.manifest["documents"][0]["file"] = json!("../private.md"),
                4 => f.manifest["documents"][0]["sha256"] = json!("0".repeat(64)),
                5 => f.manifest["legacy"][0]["disposition"] = json!("serve"),
                6 => f.manifest["unknown"] = json!(true),
                7 => f.manifest["legacy"][1]["id"] = json!("00"),
                8 => f.manifest["legacy"][0]["related_documents"] = json!(["wire", "wire"]),
                _ => f.manifest["legacy"][14]["related_documents"] = json!(["wire"]),
            }
            assert!(f.load().is_err(), "case {case}");
        }
        let f = Fixture::new();
        f.load().unwrap();
        assert!(SpecRelease::load(&f.root, &"0".repeat(64)).is_err());
    }

    #[test]
    fn rejects_oversized_and_non_utf8_content() {
        let mut f = Fixture::new();
        std::fs::write(f.root.join("04-wire.md"), vec![b'a'; MAX_FILE + 1]).unwrap();
        assert!(f.load().is_err());
        std::fs::write(f.root.join("04-wire.md"), [255]).unwrap();
        f.manifest["documents"][0]["sha256"] = json!(digest(&[255]));
        assert!(f.load().is_err());
    }

    #[test]
    fn oversized_encoded_bundle_is_an_application_error() {
        let mut f = Fixture::new();
        let raw = vec![0u8; MAX_FILE];
        std::fs::write(f.root.join("04-wire.md"), &raw).unwrap();
        std::fs::write(f.root.join("other.md"), &raw).unwrap();
        f.manifest["documents"][0]["sha256"] = json!(digest(&raw));
        f.manifest["documents"].as_array_mut().unwrap().push(json!({
            "id":"other", "file":"other.md", "sha256":digest(&raw)
        }));
        f.manifest["legacy"][0]["related_documents"] = json!(["wire", "other"]);
        let release = f.load().unwrap();
        assert_eq!(release.get_v2(Some(&json!({"document":"wire"}))).rc, 0);
        assert_eq!(
            release
                .get_v2(Some(&json!({"legacy":"00"})))
                .error
                .as_deref(),
            Some("response_too_large")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_executables() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let f = Fixture::new();
        let file = f.root.join("04-wire.md");
        std::fs::remove_file(&file).unwrap();
        symlink(f.root.join("private.md"), &file).unwrap();
        assert!(f.load().is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "text").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(f.load().is_err());
    }
}
