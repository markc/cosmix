//! Bounded native Bus object/snapshot client. No alternate remote transport.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use cosmix_client::NodedClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const CHUNK: usize = 256 * 1024;
const MANIFEST_LIMIT: usize = 128 * 1024;
const FILE_LIMIT: usize = 1000;

#[derive(Parser)]
#[command(
    version,
    about = "Store and restore immutable snapshots through the native Cosmix Bus"
)]
struct Args {
    #[arg(long)]
    service: String,
    #[arg(long)]
    collection: String,
    #[arg(long)]
    noded_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Store regular files; empty directories and filesystem metadata are not archived.
    Push {
        directory: PathBuf,
    },
    /// Restore into a new directory; a failed restore may leave verified partial files.
    Restore {
        snapshot: String,
        directory: PathBuf,
    },
    List,
    /// Discard an unfinished upload; committed objects and snapshots are untouched.
    Abort {
        sha256: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    files: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    sha256: String,
    size: u64,
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(manifest.schema_version == 1, "unsupported manifest version");
    ensure!(manifest.files.len() <= FILE_LIMIT, "too many files");
    ensure!(
        serde_json::to_vec(manifest)?.len() <= MANIFEST_LIMIT,
        "manifest exceeds 128 KiB"
    );
    let mut paths = BTreeSet::new();
    for entry in &manifest.files {
        ensure!(safe_path(&entry.path), "unsafe path: {}", entry.path);
        ensure!(valid_hash(&entry.sha256), "invalid object hash");
        ensure!(
            paths.insert(entry.path.as_str()),
            "duplicate path: {}",
            entry.path
        );
    }
    for path in &paths {
        for (pos, _) in path.match_indices('/') {
            ensure!(
                !paths.contains(&path[..pos]),
                "file/directory path conflict"
            );
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "not a regular file: {}",
        path.display()
    );
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        size = size.checked_add(n as u64).context("file size overflow")?;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<Entry>) -> Result<()> {
    ensure!(
        fs::symlink_metadata(directory)?.file_type().is_dir(),
        "not a real directory: {}",
        directory.display()
    );
    // Sorting gives stable manifests independent of filesystem enumeration order.
    let mut children = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        let kind = child.file_type()?;
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else if kind.is_file() {
            ensure!(files.len() < FILE_LIMIT, "snapshot exceeds 1000 files");
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF-8 path")?
                .to_owned();
            ensure!(safe_path(&relative), "unsafe path: {relative}");
            let (sha256, size) = hash_file(&path)?;
            files.push(Entry {
                path: relative,
                sha256,
                size,
            });
        } else {
            bail!(
                "symlinks and special files cannot be archived: {}",
                path.display()
            );
        }
    }
    Ok(())
}

struct Store {
    client: NodedClient,
    service: String,
    collection: String,
}

impl Store {
    async fn call(&self, operation: &str, mut args: Value) -> Result<Value> {
        args["collection"] = json!(self.collection);
        self.client
            .call(&self.service, &format!("webd.store.{operation}"), args)
            .await
            .with_context(|| format!("store {operation}"))
    }

    async fn push(&self, root: &Path) -> Result<Value> {
        let mut manifest = Manifest {
            schema_version: 1,
            files: Vec::new(),
        };
        collect(root, root, &mut manifest.files)?;
        validate_manifest(&manifest)?;
        let mut buffer = vec![0; CHUNK];
        for entry in &manifest.files {
            let path = root.join(&entry.path);
            let begin = self
                .call(
                    "object.begin",
                    json!({"sha256":entry.sha256,"size":entry.size}),
                )
                .await?;
            let mut offset = begin["offset"].as_u64().context("missing upload offset")?;
            ensure!(offset <= entry.size, "invalid resume offset");
            let complete = begin["complete"]
                .as_bool()
                .context("missing completion status")?;
            if !complete {
                ensure!(
                    fs::symlink_metadata(&path)?.file_type().is_file(),
                    "source replaced"
                );
                let mut file = File::open(&path)?;
                file.seek(SeekFrom::Start(offset))?;
                while offset < entry.size {
                    let length = (entry.size - offset).min(CHUNK as u64) as usize;
                    file.read_exact(&mut buffer[..length])
                        .context("source shortened during upload")?;
                    self.call("object.chunk", json!({"sha256":entry.sha256,"offset":offset,"data_base64":STANDARD.encode(&buffer[..length])})).await?;
                    offset += length as u64;
                }
                self.call("object.commit", json!({"sha256":entry.sha256}))
                    .await?;
            }
            let (hash, size) = hash_file(&path)?;
            ensure!(
                hash == entry.sha256 && size == entry.size,
                "source changed during upload: {}",
                entry.path
            );
            eprintln!("Verified {} ({} bytes)", entry.path, entry.size);
        }
        self.call("snapshot.commit", json!({"manifest":manifest}))
            .await
    }

    async fn restore(&self, snapshot: &str, destination: &Path) -> Result<Value> {
        ensure!(valid_hash(snapshot), "invalid snapshot hash");
        let reply = self
            .call("snapshot.get", json!({"sha256":snapshot}))
            .await?;
        ensure!(
            reply["sha256"].as_str() == Some(snapshot),
            "snapshot identity mismatch"
        );
        let value = reply
            .get("manifest")
            .context("missing snapshot manifest")?
            .clone();
        let bytes = cosmix_files::object_store::canonical_manifest_bytes(&value)
            .map_err(anyhow::Error::msg)?;
        ensure!(bytes.len() <= MANIFEST_LIMIT, "oversized manifest");
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == snapshot,
            "snapshot manifest hash mismatch"
        );
        let manifest: Manifest = serde_json::from_value(value)?;
        validate_manifest(&manifest)?;
        // create_dir fails for any existing target, including dangling symlinks.
        fs::create_dir(destination)
            .context("restore destination must not exist and its parent must exist")?;
        for entry in &manifest.files {
            let path = destination.join(&entry.path);
            let parent = path.parent().context("missing parent")?;
            fs::create_dir_all(parent)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            let mut offset = 0;
            let mut hasher = Sha256::new();
            while offset < entry.size {
                let max = (entry.size - offset).min(CHUNK as u64);
                let reply = self
                    .call(
                        "object.read",
                        json!({"sha256":entry.sha256,"offset":offset,"max":max}),
                    )
                    .await?;
                ensure!(
                    reply["sha256"].as_str() == Some(entry.sha256.as_str())
                        && reply["offset"].as_u64() == Some(offset)
                        && reply["size"].as_u64() == Some(entry.size),
                    "object identity or offset mismatch"
                );
                let encoded = reply["data_base64"]
                    .as_str()
                    .context("missing object data")?;
                ensure!(
                    encoded.len() <= CHUNK.div_ceil(3) * 4,
                    "oversized object chunk"
                );
                let data = STANDARD.decode(encoded)?;
                ensure!(
                    !data.is_empty() && data.len() as u64 <= max,
                    "invalid object chunk size"
                );
                temporary.write_all(&data)?;
                hasher.update(&data);
                offset += data.len() as u64;
            }
            ensure!(
                format!("{:x}", hasher.finalize()) == entry.sha256,
                "restored hash mismatch: {}",
                entry.path
            );
            temporary.as_file().sync_all()?;
            temporary.persist_noclobber(&path)?;
            File::open(parent)?.sync_all()?;
        }
        Ok(
            json!({"snapshot":snapshot,"restored_files":manifest.files.len(),"directory":destination}),
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let store = Store {
        client: match &args.noded_url {
            Some(url) => NodedClient::connect("store-client", url).await?,
            None => cosmix_config::client_helpers::connect_default("store-client").await?,
        },
        service: args.service,
        collection: args.collection,
    };
    let result = match args.command {
        Command::Push { directory } => store.push(&directory).await,
        Command::Restore {
            snapshot,
            directory,
        } => store.restore(&snapshot, &directory).await,
        Command::List => store.call("snapshot.list", json!({})).await,
        Command::Abort { sha256 } => {
            ensure!(valid_hash(&sha256), "invalid SHA-256");
            store.call("object.abort", json!({"sha256":sha256})).await
        }
    };
    store.client.close().await;
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_and_ambiguous_paths() {
        for path in [
            "",
            "/absolute",
            "../escape",
            "a/../b",
            "a//b",
            "a/./b",
            "a\\b",
            "C:drive",
            "a\0b",
        ] {
            assert!(!safe_path(path), "{path:?}");
        }
        assert!(safe_path("coast/source/sea.glb"));
    }

    #[test]
    fn rejects_duplicate_and_conflicting_paths() {
        let entry = |path: &str| Entry {
            path: path.into(),
            sha256: "a".repeat(64),
            size: 0,
        };
        for paths in [["a", "a"], ["a", "a/b"]] {
            let manifest = Manifest {
                schema_version: 1,
                files: paths.into_iter().map(entry).collect(),
            };
            assert!(validate_manifest(&manifest).is_err());
        }
    }

    #[test]
    fn inventories_nested_binary_and_rejects_symlinks() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/file.bin"), [0, 255, 1]).unwrap();
        let mut files = Vec::new();
        collect(root.path(), root.path(), &mut files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "nested/file.bin");
        assert_eq!(files[0].size, 3);
        assert_eq!(
            files[0].sha256,
            format!("{:x}", Sha256::digest([0, 255, 1]))
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("nested/file.bin", root.path().join("link")).unwrap();
            assert!(collect(root.path(), root.path(), &mut Vec::new()).is_err());
        }
    }
}
