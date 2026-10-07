//! `archive` feature — `tar_list` / `tar_unpack` / `tar_pack` builtins.
//!
//! The distribution story (MixOS try-installer, distro doc §4/§5): a signed
//! manifest pins a digest, `hash_file` compares against it, and the SAME
//! file is unpacked in-language — no `importctl` (systemd 256+ only, v255
//! cannot read zstd, host `import-pubring.gpg` trust-store change). App
//! packs inside MixOS machines need archive reading too, where no external
//! tar/zstd exists at all.
//!
//! Codecs: zstd (structured-zstd, pure Rust, RFC 8878 — both directions),
//! gzip (flate2's gzip framing over its default Rust backend), none. Pure
//! Rust throughout: no C codec in the shipped binary. Encoder profile
//! contract: single frame, no dictionaries, preset levels only (their
//! windows stay far under the decoder's ceiling) — see docs/mix/tar.md.
//!
//! Safety posture (why this is not a thin `Archive::unpack` wrapper):
//! extraction is a classic escape hatch. Every member name is validated on
//! the crate-RESOLVED path (GNU longname/PAX smuggling lands here, not on
//! raw headers): no absolute/`..`/`.` components, UTF-8 only. Device, fifo
//! and unknown entry types are refused. Symlink targets are contained and
//! nothing is ever extracted THROUGH a created symlink; hardlinks may only
//! target an earlier regular file (hardlink-to-hardlink is refused). All
//! entry metadata — including directories' — is applied in a children-first
//! post-pass using lchown / utimensat(AT_SYMLINK_NOFOLLOW) so nothing
//! follows a link out of the staging tree. `max_stream_bytes` bounds the
//! DECODED stream (tar's internal longname/PAX buffers live inside it).
//! Xattrs are parsed and applied BY HAND: the tar crate's blanket
//! `set_unpack_xattrs` would restore `security.capability` even when the
//! caller asked for suid/sgid stripping, so `keep_special_bits:false` (the
//! default) skips `security.*` entirely — a capability xattr is a privilege
//! grant exactly like a suid bit. Entries land in a private 0700 staging
//! directory (dest must NOT exist) and are renamed into place only after
//! the compressed stream is drained to true EOF: the zstd frame checksum /
//! gzip CRC is actually verified, the decoded tail must be all zero (GNU
//! padding), AND the raw input must be fully consumed (a concatenated
//! second frame never decodes, so only the source position can see it).

use crate::builtins::{opt_invalid, sanitize_for_diag};
use crate::error::MixError;
use crate::value::Value;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use tar::{Archive, Builder, EntryType, Header};

fn runtime(msg: String) -> MixError {
    MixError::RuntimeError { span: None, msg }
}

// ---------------------------------------------------------------------------
// Codec + options
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Zstd,
    Gzip,
    None,
}

impl Codec {
    fn as_str(self) -> &'static str {
        match self {
            Codec::Zstd => "zstd",
            Codec::Gzip => "gzip",
            Codec::None => "none",
        }
    }
}

fn parse_codec(caller: &str, v: Option<&Value>) -> Result<Codec, MixError> {
    match v {
        None | Some(Value::Nil) => Ok(Codec::Zstd),
        Some(Value::String(s)) if s == "zstd" => Ok(Codec::Zstd),
        Some(Value::String(s)) if s == "gzip" => Ok(Codec::Gzip),
        Some(Value::String(s)) if s == "none" => Ok(Codec::None),
        other => Err(opt_invalid(
            caller,
            format!(
                "codec must be \"zstd\" (default), \"gzip\" or \"none\", got {}",
                other.map(Value::type_name).unwrap_or("nil")
            ),
        )),
    }
}

fn bool_opt(caller: &str, name: &str, v: Option<&Value>) -> Result<Option<bool>, MixError> {
    match v {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        other => Err(opt_invalid(
            caller,
            format!(
                "{name} must be a bool, got {}",
                other.map(Value::type_name).unwrap_or("nil")
            ),
        )),
    }
}

fn count_opt(caller: &str, name: &str, v: Option<&Value>) -> Result<Option<u64>, MixError> {
    match v {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Number(n)) if *n >= 1.0 && n.fract() == 0.0 => Ok(Some(*n as u64)),
        other => Err(opt_invalid(
            caller,
            format!(
                "{name} must be a whole number >= 1, got {}",
                other.map(Value::type_name).unwrap_or("nil")
            ),
        )),
    }
}

/// structured-zstd 0.0.58 exposes named presets, not a 1..=22 continuum —
/// accept the preset numbers only; the receipt names the preset used, never
/// a fictional level.
const ZSTD_PRESETS: &[i32] = &[1, 3, 7, 11];

struct UnpackOpts {
    codec: Codec,
    numeric_owner: bool,
    xattrs: bool,
    keep_special_bits: bool,
    max_entries: u64,
    max_bytes: u64,
    max_name: usize,
    max_stream_bytes: u64,
}

impl Default for UnpackOpts {
    fn default() -> Self {
        UnpackOpts {
            codec: Codec::Zstd,
            numeric_owner: true,
            xattrs: true,
            keep_special_bits: false,
            max_entries: 200_000,
            max_bytes: 64 << 30,
            max_name: 4096,
            // Bounds the DECODED stream — tar's internal GNU-longname/PAX
            // buffers are allocated inside it, so this is the memory-bomb
            // limit as much as the size limit.
            max_stream_bytes: 16 << 30,
        }
    }
}

fn known_keys_check(
    caller: &str,
    m: &IndexMap<String, Value>,
    known: &[&str],
) -> Result<(), MixError> {
    for k in m.keys() {
        if !known.contains(&k.as_str()) {
            return Err(opt_invalid(caller, format!("unknown option '{k}'")));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verified decode layer
// ---------------------------------------------------------------------------

/// `Read` wrapper bounding the DECODED stream; aborting mid-read kills
/// tar-rs's internal allocation loops (longname/PAX bombs) with an error
/// instead of an OOM.
struct LimitedRead<'a> {
    inner: &'a mut VerifiedDecode,
    remaining: u64,
    caller: String,
}

impl Read for LimitedRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read_some(buf)?;
        if n as u64 > self.remaining {
            return Err(std::io::Error::other(format!(
                "{}: exceeded max_stream_bytes ({})",
                self.caller, self.remaining
            )));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

enum VerifiedDecode {
    Zstd {
        dec: structured_zstd::decoding::StreamingDecoder<BufReader<File>>,
    },
    Gzip {
        dec: flate2::read::GzDecoder<BufReader<File>>,
    },
    None {
        src: BufReader<File>,
    },
}

impl VerifiedDecode {
    fn open(path: &Path, codec: Codec, caller: &str) -> Result<Self, MixError> {
        let file = File::open(path)
            .map_err(|e| runtime(format!("{caller}: open '{}': {e}", path.display())))?;
        let br = BufReader::with_capacity(1 << 20, file);
        Ok(match codec {
            Codec::Zstd => VerifiedDecode::Zstd {
                dec: structured_zstd::decoding::StreamingDecoder::new(br).map_err(|e| {
                    runtime(format!("{caller}: zstd init '{}': {e}", path.display()))
                })?,
            },
            Codec::Gzip => VerifiedDecode::Gzip {
                dec: flate2::read::GzDecoder::new(br),
            },
            Codec::None => VerifiedDecode::None { src: br },
        })
    }

    fn read_some(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            VerifiedDecode::Zstd { dec } => dec.read(buf),
            VerifiedDecode::Gzip { dec } => dec.read(buf),
            VerifiedDecode::None { src } => src.read(buf),
        }
    }

    /// After the tar reader finished: (a) drain the REMAINING decoded bytes
    /// to true stream EOF — the zstd frame checksum / gzip CRC are verified
    /// by the decoder there, and every trailing byte must be zero (GNU tar
    /// padding; a decoded second archive is refused); (b) assert the RAW
    /// input is fully consumed — a single-frame decoder never decodes a
    /// second frame/member, so only the source position can catch it.
    fn drain(&mut self, caller: &str) -> MixResult<u64> {
        let mut drained: u64 = 0;
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = self
                .read_some(&mut buf)
                .map_err(|e| runtime(format!("{caller}: stream verification failed: {e}")))?;
            if n == 0 {
                break;
            }
            drained += n as u64;
            if buf[..n].iter().any(|&b| b != 0) {
                return Err(runtime(format!(
                    "{caller}: non-zero data after the tar end-of-archive (bytes {}..{}) — \
                     single-frame artifacts only",
                    drained - n as u64,
                    drained
                )));
            }
        }
        let reader = match self {
            VerifiedDecode::Zstd { dec } => dec.get_mut(),
            VerifiedDecode::Gzip { dec } => dec.get_mut(),
            VerifiedDecode::None { src } => src,
        };
        let leftover = reader
            .fill_buf()
            .map_err(|e| runtime(format!("{caller}: tail read: {e}")))?;
        if !leftover.is_empty() {
            return Err(runtime(format!(
                "{caller}: {} raw bytes remain after the decoded stream ended — \
                 concatenated frames/members are refused",
                leftover.len()
            )));
        }
        Ok(drained)
    }
}

// ---------------------------------------------------------------------------
// Member validation (crate-RESOLVED paths)
// ---------------------------------------------------------------------------

fn clean_member(caller: &str, raw: &Path, max_name: usize) -> Result<PathBuf, MixError> {
    let bytes = raw.as_os_str().as_bytes();
    if bytes.len() > max_name {
        return Err(runtime(format!(
            "{caller}: member name exceeds max_name ({} > {})",
            bytes.len(),
            max_name
        )));
    }
    if bytes.is_empty() {
        return Err(runtime(format!("{caller}: empty member name")));
    }
    // Names are UTF-8 only in this builtin: silent mangling of invalid
    // bytes (to_string_lossy) would lie about what got extracted.
    if raw.to_str().is_none() {
        return Err(runtime(format!(
            "{caller}: member name is not valid UTF-8: '{}'",
            sanitize_for_diag(&String::from_utf8_lossy(bytes))
        )));
    }
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::Normal(part) => {
                let pb = part.as_bytes();
                if pb == b"." || pb == b".." || pb.is_empty() {
                    return Err(runtime(format!(
                        "{caller}: refusing member name '{}': '.' or '..' component",
                        raw.display()
                    )));
                }
                out.push(part);
            }
            _ => {
                return Err(runtime(format!(
                    "{caller}: refusing member name '{}': absolute or parent component",
                    raw.display()
                )));
            }
        }
    }
    Ok(out)
}

fn clean_link_target(
    caller: &str,
    name: &str,
    target: &std::ffi::OsStr,
    max_name: usize,
) -> Result<PathBuf, MixError> {
    clean_member(caller, Path::new(target), max_name).map_err(|_| {
        runtime(format!(
            "{caller}: refusing symlink '{}' with absolute or escaping target '{}'",
            name,
            target.to_string_lossy()
        ))
    })
}

// ---------------------------------------------------------------------------
// tar_list
// ---------------------------------------------------------------------------

pub fn builtin_tar_list(args: Vec<Value>) -> MixResult<Option<Value>> {
    let caller = "tar_list()";
    if args.is_empty() || args.len() > 2 {
        return Err(runtime(format!(
            "{caller} expects 1 or 2 args (path[, opts]), got {}",
            args.len()
        )));
    }
    let path = args[0].to_mix_string();
    let mut opts = UnpackOpts::default();
    match args.get(1) {
        None | Some(Value::Nil) => {}
        Some(Value::Map(m)) => {
            if let Some(v) = m.get("codec") {
                opts.codec = parse_codec(caller, Some(v))?;
            }
            opts.max_stream_bytes =
                count_opt(caller, "max_stream_bytes", m.get("max_stream_bytes"))?
                    .unwrap_or(opts.max_stream_bytes);
            opts.max_name =
                count_opt(caller, "max_name", m.get("max_name"))?.unwrap_or(opts.max_name) as usize;
            known_keys_check(caller, m, &["codec", "max_stream_bytes", "max_name"])?;
        }
        other => {
            return Err(opt_invalid(
                caller,
                format!("options must be a map or nil, got {}", other.type_name()),
            ));
        }
    }

    let mut dec = VerifiedDecode::open(Path::new(&path), opts.codec, caller)?;
    let mut limited = LimitedRead {
        inner: &mut dec,
        remaining: opts.max_stream_bytes,
        caller: caller.to_string(),
    };
    let mut archive = Archive::new(&mut limited);
    let mut out: Vec<Value> = Vec::new();
    let mut count: u64 = 0;
    let iter = archive
        .entries()
        .map_err(|e| runtime(format!("{caller}: {e}")))?;
    for entry in iter {
        let entry = entry.map_err(|e| runtime(format!("{caller}: {e}")))?;
        count += 1;
        if count > opts.max_entries {
            return Err(runtime(format!(
                "{caller}: exceeded max_entries ({})",
                opts.max_entries
            )));
        }
        let header = entry.header();
        let raw = entry
            .path()
            .map_err(|e| runtime(format!("{caller}: member name: {e}")))?
            .to_path_buf();
        let name = clean_member(caller, &raw, opts.max_name)?;
        let mut map = IndexMap::new();
        map.insert("name".to_string(), Value::String(name.to_string_lossy().to_string()));
        map.insert(
            "size".to_string(),
            Value::Number(header.size().unwrap_or(0) as f64),
        );
        map.insert("mode".to_string(), Value::Number(header.mode().unwrap_or(0) as f64));
        map.insert("uid".to_string(), Value::Number(header.uid().unwrap_or(0) as f64));
        map.insert("gid".to_string(), Value::Number(header.gid().unwrap_or(0) as f64));
        map.insert(
            "mtime".to_string(),
            Value::Number(header.mtime().unwrap_or(0) as f64),
        );
        map.insert(
            "kind".to_string(),
            Value::String(kind_str(header.entry_type()).to_string()),
        );
        out.push(Value::Map(map));
    }
    drop(archive);
    dec.drain(caller)?;
    Ok(Some(Value::List(out)))
}

fn kind_str(t: EntryType) -> &'static str {
    match t {
        EntryType::Regular | EntryType::Continuous => "file",
        EntryType::Directory => "dir",
        EntryType::Symlink => "symlink",
        EntryType::Link => "hardlink",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// tar_unpack
// ---------------------------------------------------------------------------

pub fn builtin_tar_unpack(args: Vec<Value>) -> MixResult<Option<Value>> {
    let caller = "tar_unpack()";
    if args.len() < 2 || args.len() > 3 {
        return Err(runtime(format!(
            "{caller} expects 2 or 3 args (path, dest[, opts]), got {}",
            args.len()
        )));
    }
    let path = args[0].to_mix_string();
    let dest = args[1].to_mix_string();
    let mut opts = UnpackOpts::default();
    match args.get(2) {
        None | Some(Value::Nil) => {}
        Some(Value::Map(m)) => {
            if let Some(v) = m.get("codec") {
                opts.codec = parse_codec(caller, Some(v))?;
            }
            if let Some(v) = bool_opt(caller, "numeric_owner", m.get("numeric_owner"))? {
                opts.numeric_owner = v;
            }
            if let Some(v) = bool_opt(caller, "xattrs", m.get("xattrs"))? {
                opts.xattrs = v;
            }
            if let Some(v) = bool_opt(caller, "keep_special_bits", m.get("keep_special_bits"))? {
                opts.keep_special_bits = v;
            }
            opts.max_entries =
                count_opt(caller, "max_entries", m.get("max_entries"))?.unwrap_or(opts.max_entries);
            opts.max_bytes =
                count_opt(caller, "max_bytes", m.get("max_bytes"))?.unwrap_or(opts.max_bytes);
            opts.max_name =
                count_opt(caller, "max_name", m.get("max_name"))?.unwrap_or(opts.max_name) as usize;
            opts.max_stream_bytes =
                count_opt(caller, "max_stream_bytes", m.get("max_stream_bytes"))?
                    .unwrap_or(opts.max_stream_bytes);
            known_keys_check(
                caller,
                m,
                &[
                    "codec",
                    "numeric_owner",
                    "xattrs",
                    "keep_special_bits",
                    "max_entries",
                    "max_bytes",
                    "max_name",
                    "max_stream_bytes",
                ],
            )?;
        }
        other => {
            return Err(opt_invalid(
                caller,
                format!("options must be a map or nil, got {}", other.type_name()),
            ));
        }
    }

    let dest_path = Path::new(&dest);
    if dest_path.exists() {
        return Err(runtime(format!(
            "{caller}: dest '{dest}' exists; refusing (extraction is staged, never merged)"
        )));
    }
    let parent = dest_path
        .parent()
        .ok_or_else(|| runtime(format!("{caller}: dest has no parent directory")))?;
    if !parent.is_dir() {
        return Err(runtime(format!(
            "{caller}: dest parent '{}' does not exist",
            parent.display()
        )));
    }

    let staging = parent.join(format!(
        ".mixtar-stage-{}-{}",
        std::process::id(),
        STAGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir(&staging).map_err(|e| runtime(format!("{caller}: staging: {e}")))?;
    // Private staging regardless of umask: content exists briefly before
    // the header mode is applied.
    let _ = fs::set_permissions(&staging, fs::Permissions::from_mode(0o700));

    match unpack_staged(&path, &staging, &opts, caller) {
        Ok(receipt) => {
            fs::rename(&staging, dest_path).map_err(|e| {
                let _ = fs::remove_dir_all(&staging);
                runtime(format!("{caller}: commit '{dest}': {e}"))
            })?;
            Ok(Some(Value::Map(receipt)))
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

static STAGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Metadata deferred to a post-pass: directory mtime/owner must be applied
/// children-first, and symlink metadata must never follow the link.
struct DeferredMeta {
    rel: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    is_symlink: bool,
}

fn unpack_staged(
    path: &str,
    staging: &Path,
    opts: &UnpackOpts,
    caller: &str,
) -> MixResult<IndexMap<String, Value>> {
    let mut dec = VerifiedDecode::open(Path::new(path), opts.codec, caller)?;
    let mut limited = LimitedRead {
        inner: &mut dec,
        remaining: opts.max_stream_bytes,
        caller: caller.to_string(),
    };
    let mut archive = Archive::new(&mut limited);
    let iter = archive
        .entries()
        .map_err(|e| runtime(format!("{caller}: {e}")))?;

    let mut files: u64 = 0;
    let mut dirs: u64 = 0;
    let mut symlinks: u64 = 0;
    let mut hardlinks: u64 = 0;
    let mut bytes: u64 = 0;
    let mut xattrs_restored: u64 = 0;
    let mut entry_count: u64 = 0;
    let mut symlinks_created: HashSet<PathBuf> = HashSet::new();
    let mut extracted_files: HashSet<PathBuf> = HashSet::new();
    let mut deferred: Vec<DeferredMeta> = Vec::new();

    for entry in iter {
        let mut entry = entry.map_err(|e| runtime(format!("{caller}: {e}")))?;
        entry_count += 1;
        if entry_count > opts.max_entries {
            return Err(runtime(format!(
                "{caller}: exceeded max_entries ({})",
                opts.max_entries
            )));
        }
        let header = entry.header().clone();
        let raw_path = entry
            .path()
            .map_err(|e| runtime(format!("{caller}: member name: {e}")))?
            .to_path_buf();
        let rel = clean_member(caller, &raw_path, opts.max_name)?;
        let target_abs = staging.join(&rel);

        // Never extract THROUGH a symlink this archive created.
        for ancestor in rel.ancestors().skip(1) {
            if !ancestor.as_os_str().is_empty() && symlinks_created.contains(ancestor) {
                return Err(runtime(format!(
                    "{caller}: refusing to extract '{}' through symlink '{}'",
                    rel.display(),
                    ancestor.display()
                )));
            }
        }

        let mode = header.mode().unwrap_or(0o644);
        let masked = if opts.keep_special_bits {
            mode & 0o7777
        } else {
            mode & !0o6000 & 0o7777
        };
        let meta = DeferredMeta {
            rel: rel.clone(),
            mode: masked,
            uid: header.uid().unwrap_or(0) as u32,
            gid: header.gid().unwrap_or(0) as u32,
            mtime: header.mtime().unwrap_or(0) as i64,
            is_symlink: false,
        };

        match header.entry_type() {
            EntryType::Directory => {
                // 0700 & umask during extraction; the post-pass applies the
                // real (masked) mode so nothing is ever wider than asked.
                fs::create_dir(&target_abs)
                    .map_err(|e| runtime(format!("{caller}: mkdir '{}': {e}", rel.display())))?;
                dirs += 1;
                deferred.push(meta);
            }
            EntryType::Regular | EntryType::Continuous => {
                if let Some(parent) = target_abs.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| runtime(format!("{caller}: mkdir parent: {e}")))?;
                }
                if opts.xattrs {
                    xattrs_restored +=
                        apply_pax_xattrs(&mut entry, opts, &target_abs, &rel, caller)?;
                }
                entry.set_preserve_mtime(true);
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target_abs)
                    .map_err(|e| runtime(format!("{caller}: create '{}': {e}", rel.display())))?;
                let written = std::io::copy(&mut entry, &mut out)
                    .map_err(|e| runtime(format!("{caller}: write '{}': {e}", rel.display())))?;
                bytes += written;
                if bytes > opts.max_bytes {
                    return Err(runtime(format!(
                        "{caller}: exceeded max_bytes ({})",
                        opts.max_bytes
                    )));
                }
                files += 1;
                extracted_files.insert(rel.clone());
                deferred.push(meta);
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .ok_or_else(|| {
                        runtime(format!("{caller}: symlink '{}' has no target", rel.display()))
                    })?
                    .clone();
                let clean = clean_link_target(caller, &rel.to_string_lossy(), &target, opts.max_name)?;
                std::os::unix::fs::symlink(&clean, &target_abs)
                    .map_err(|e| runtime(format!("{caller}: symlink '{}': {e}", rel.display())))?;
                symlinks += 1;
                symlinks_created.insert(rel.clone());
                let mut m = meta;
                m.is_symlink = true;
                deferred.push(m);
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .ok_or_else(|| {
                        runtime(format!("{caller}: hardlink '{}' has no target", rel.display()))
                    })?
                    .clone();
                let clean_target = clean_member(caller, &target.to_path_buf(), opts.max_name)?;
                if !extracted_files.contains(&clean_target) {
                    return Err(runtime(format!(
                        "{caller}: hardlink '{}' targets '{}' which is not an earlier \
                         regular file in this archive",
                        rel.display(),
                        clean_target.display()
                    )));
                }
                fs::hard_link(staging.join(&clean_target), &target_abs)
                    .map_err(|e| runtime(format!("{caller}: hardlink '{}': {e}", rel.display())))?;
                hardlinks += 1;
            }
            other => {
                return Err(runtime(format!(
                    "{caller}: refusing member '{}' of type {other:?} \
                     (device/fifo/special entries are not extracted)",
                    rel.display()
                )));
            }
        }
    }
    drop(archive);
    let trailing = dec.drain(caller)?;

    // Deferred metadata, children-first so directory mtimes stick, never
    // following symlinks (lchown / utimensat AT_SYMLINK_NOFOLLOW).
    for m in deferred.iter().rev() {
        let abs = staging.join(&m.rel);
        if !m.is_symlink {
            let _ = fs::set_permissions(&abs, fs::Permissions::from_mode(m.mode));
        }
        if opts.numeric_owner {
            let _ = lchown(&abs, m.uid, m.gid);
        }
        let _ = set_mtime_nofollow(&abs, m.mtime);
    }

    let mut map = IndexMap::new();
    map.insert("files".to_string(), Value::Number(files as f64));
    map.insert("dirs".to_string(), Value::Number(dirs as f64));
    map.insert("symlinks".to_string(), Value::Number(symlinks as f64));
    map.insert("hardlinks".to_string(), Value::Number(hardlinks as f64));
    map.insert("bytes".to_string(), Value::Number(bytes as f64));
    map.insert(
        "xattrs_restored".to_string(),
        Value::Number(xattrs_restored as f64),
    );
    map.insert("entries".to_string(), Value::Number(entry_count as f64));
    map.insert("trailing_padding".to_string(), Value::Number(trailing as f64));
    map.insert("codec".to_string(), Value::String(opts.codec.as_str().to_string()));
    Ok(map)
}

/// Parse the entry's PAX records by hand and apply `SCHILY.xattr.*` keys.
/// `security.*` is SKIPPED unless `keep_special_bits` — a capability xattr
/// is a privilege grant exactly like a suid bit, and the tar crate's
/// blanket `set_unpack_xattrs` would restore it regardless of the caller's
/// choice. EPERM is tolerated (non-root extraction is a documented
/// capability limit); every other failure raises.
fn apply_pax_xattrs<E: Read>(
    entry: &mut tar::Entry<E>,
    opts: &UnpackOpts,
    target_abs: &Path,
    rel: &Path,
    caller: &str,
) -> MixResult<u64> {
    let pax = match entry.pax_extensions() {
        Ok(Some(p)) => p,
        _ => return Ok(0),
    };
    let mut restored = 0u64;
    for kv in pax.flatten() {
        let key = kv.key();
        if !key.starts_with(b"SCHILY.xattr.") {
            continue;
        }
        let xname = &key[b"SCHILY.xattr.".len()..];
        if !opts.keep_special_bits && xname.starts_with(b"security.") {
            continue;
        }
        let cname = match std::ffi::CString::new(xname.to_vec()) {
            Ok(c) => c,
            Err(_) => continue, // xattr names cannot contain NUL by definition
        };
        let cpath = std::ffi::CString::new(target_abs.as_os_str().as_bytes())
            .map_err(|_| runtime(format!("{caller}: member path contains NUL")))?;
        let rc = unsafe {
            libc::lsetxattr(
                cpath.as_ptr(),
                cname.as_ptr(),
                kv.value().as_ptr() as *const libc::c_void,
                kv.value().len(),
                0,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(1) {
                continue; // EPERM: non-root; documented limit, not an error
            }
            return Err(runtime(format!(
                "{caller}: xattr '{}' on '{}': {err}",
                String::from_utf8_lossy(xname),
                rel.display()
            )));
        }
        restored += 1;
    }
    Ok(restored)
}

fn lchown(path: &Path, uid: u32, gid: u32) -> bool {
    match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => unsafe { libc::lchown(c.as_ptr(), uid, gid) == 0 },
        Err(_) => false,
    }
}

fn set_mtime_nofollow(path: &Path, mtime: i64) -> bool {
    let ts = libc::timespec {
        tv_sec: mtime,
        tv_nsec: 0,
    };
    let times = [ts, ts];
    match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
        },
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// tar_pack
// ---------------------------------------------------------------------------

struct PackOpts {
    codec: Codec,
    level: i32,
    keep_special_bits: bool,
}

pub fn builtin_tar_pack(args: Vec<Value>) -> MixResult<Option<Value>> {
    let caller = "tar_pack()";
    if args.len() < 2 || args.len() > 3 {
        return Err(runtime(format!(
            "{caller} expects 2 or 3 args (source, path[, opts]), got {}",
            args.len()
        )));
    }
    let source = args[0].to_mix_string();
    let path = args[1].to_mix_string();
    let mut opts = PackOpts {
        codec: Codec::Zstd,
        level: 7,
        keep_special_bits: false,
    };
    match args.get(2) {
        None | Some(Value::Nil) => {}
        Some(Value::Map(m)) => {
            if let Some(v) = m.get("codec") {
                opts.codec = parse_codec(caller, Some(v))?;
            }
            if let Some(v) = m.get("level") {
                match v {
                    Value::Number(n) if ZSTD_PRESETS.contains(&(*n as i32)) => {
                        opts.level = *n as i32;
                    }
                    _ => {
                        return Err(opt_invalid(
                            caller,
                            format!(
                                "level must be one of the structured-zstd presets 1/3/7/11 \
                                 (gzip clamps 11 to 9), got {}",
                                v.type_name()
                            ),
                        ));
                    }
                }
            }
            if let Some(v) = bool_opt(caller, "keep_special_bits", m.get("keep_special_bits"))? {
                opts.keep_special_bits = v;
            }
            known_keys_check(caller, m, &["codec", "level", "keep_special_bits"])?;
        }
        other => {
            return Err(opt_invalid(
                caller,
                format!("options must be a map or nil, got {}", other.type_name()),
            ));
        }
    }
    let source_path = Path::new(&source);
    if !source_path.is_dir() {
        return Err(runtime(format!(
            "{caller}: source '{source}' is not a directory"
        )));
    }
    let out_path = Path::new(&path);
    if let Some(parent) = out_path.parent() {
        if !parent.is_dir() {
            return Err(runtime(format!(
                "{caller}: output parent '{}' does not exist",
                parent.display()
            )));
        }
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out_path)
        .map_err(|e| runtime(format!("{caller}: create '{path}': {e}")))?;
    let buf = BufWriter::with_capacity(1 << 20, file);
    match pack_stream(source_path, buf, &opts, caller) {
        Ok(receipt) => Ok(Some(Value::Map(receipt))),
        Err(e) => {
            let _ = fs::remove_file(out_path);
            Err(e)
        }
    }
}

enum PackSink {
    Zstd(structured_zstd::encoding::StreamingEncoder<BufWriter<File>>),
    Gzip(flate2::write::GzEncoder<BufWriter<File>>),
    None(BufWriter<File>),
}

impl Write for PackSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            PackSink::Zstd(w) => w.write(buf),
            PackSink::Gzip(w) => w.write(buf),
            PackSink::None(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            PackSink::Zstd(w) => w.flush(),
            PackSink::Gzip(w) => w.flush(),
            PackSink::None(w) => w.flush(),
        }
    }
}

impl PackSink {
    /// Finalise the CODEC before the buffered writer flushes: gzip without
    /// `finish` omits the CRC32/ISIZE trailer (our own unpack would reject
    /// the artifact), and ruzstd-lineage encoders finalise the frame on
    /// `finish`, not on flush.
    fn finish(self) -> std::io::Result<()> {
        match self {
            PackSink::Zstd(w) => w.finish().and_then(|mut b| b.flush()),
            PackSink::Gzip(w) => w.finish().and_then(|mut b| b.flush()),
            PackSink::None(mut w) => w.flush(),
        }
    }
}

fn pack_stream(
    source: &Path,
    buf: BufWriter<File>,
    opts: &PackOpts,
    caller: &str,
) -> MixResult<IndexMap<String, Value>> {
    let sink = match opts.codec {
        Codec::Zstd => {
            let preset = match opts.level {
                1 => structured_zstd::encoding::CompressionLevel::Fastest,
                3 => structured_zstd::encoding::CompressionLevel::Fast,
                11 => structured_zstd::encoding::CompressionLevel::Best,
                _ => structured_zstd::encoding::CompressionLevel::Default,
            };
            let enc = structured_zstd::encoding::StreamingEncoder::new(buf, preset)
                .map_err(|e| runtime(format!("{caller}: zstd encode: {e}")))?;
            PackSink::Zstd(enc)
        }
        Codec::Gzip => PackSink::Gzip(flate2::write::GzEncoder::new(
            buf,
            flate2::Compression::new(opts.level.min(9) as u32),
        )),
        Codec::None => PackSink::None(buf),
    };
    walk_and_append(source, sink, opts, caller)
}

fn walk_and_append(
    source: &Path,
    sink: PackSink,
    opts: &PackOpts,
    caller: &str,
) -> MixResult<IndexMap<String, Value>> {
    let mut builder = Builder::new(sink);
    let mut files: u64 = 0;
    let mut dirs: u64 = 0;
    let mut symlinks: u64 = 0;
    let mut bytes: u64 = 0;
    let mut caps: u64 = 0;

    let mut queue: Vec<PathBuf> = vec![source.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let mut children: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|e| runtime(format!("{caller}: read_dir '{}': {e}", dir.display())))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        children.sort();
        for child in children {
            let meta = fs::symlink_metadata(&child)
                .map_err(|e| runtime(format!("{caller}: stat '{}': {e}", child.display())))?;
            let rel = child
                .strip_prefix(source)
                .map_err(|e| runtime(format!("{caller}: internal path: {e}")))?;
            if meta.is_dir() {
                let mut header = Header::new_gnu();
                header.set_size(0);
                set_header_meta(&mut header, &meta, opts);
                builder
                    .append_data(&mut header, rel, std::io::empty())
                    .map_err(|e| runtime(format!("{caller}: append '{}': {e}", rel.display())))?;
                dirs += 1;
                queue.push(child);
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(&child).map_err(|e| {
                    runtime(format!("{caller}: readlink '{}': {e}", child.display()))
                })?;
                // Contained targets only, so packed archives always
                // round-trip through the safe unpacker.
                clean_link_target(caller, &rel.to_string_lossy(), &target, 4096)?;
                let mut header = Header::new_gnu();
                header.set_size(0);
                header.set_entry_type(EntryType::Symlink);
                set_header_meta(&mut header, &meta, opts);
                header
                    .set_link_name(&target)
                    .map_err(|e| runtime(format!("{caller}: link name: {e}")))?;
                builder
                    .append_data(&mut header, rel, std::io::empty())
                    .map_err(|e| runtime(format!("{caller}: append '{}': {e}", rel.display())))?;
                symlinks += 1;
            } else if meta.is_file() {
                if let Some(raw) = xattr_security_capability(&child) {
                    append_pax_xattr(&mut builder, rel, &raw).map_err(|e| {
                        runtime(format!("{caller}: pax xattr '{}': {e}", rel.display()))
                    })?;
                    caps += 1;
                }
                let mut header = Header::new_gnu();
                header.set_size(meta.len());
                set_header_meta(&mut header, &meta, opts);
                let f = File::open(&child)
                    .map_err(|e| runtime(format!("{caller}: open '{}': {e}", child.display())))?;
                builder
                    .append_data(&mut header, rel, f)
                    .map_err(|e| runtime(format!("{caller}: append '{}': {e}", rel.display())))?;
                files += 1;
                bytes += meta.len();
            } else {
                return Err(runtime(format!(
                    "{caller}: refusing to pack special file '{}' (device/fifo); \
                     source trees may contain only files, dirs and symlinks",
                    rel.display()
                )));
            }
        }
    }
    let sink = builder
        .into_inner()
        .map_err(|e| runtime(format!("{caller}: finish archive: {e}")))?;
    sink.finish()
        .map_err(|e| runtime(format!("{caller}: finalise codec: {e}")))?;

    let mut map = IndexMap::new();
    map.insert("files".to_string(), Value::Number(files as f64));
    map.insert("dirs".to_string(), Value::Number(dirs as f64));
    map.insert("symlinks".to_string(), Value::Number(symlinks as f64));
    map.insert("bytes".to_string(), Value::Number(bytes as f64));
    map.insert("capabilities".to_string(), Value::Number(caps as f64));
    map.insert("codec".to_string(), Value::String(opts.codec.as_str().to_string()));
    map.insert("level".to_string(), Value::Number(opts.level as f64));
    Ok(map)
}

fn set_header_meta(header: &mut Header, meta: &fs::Metadata, opts: &PackOpts) {
    let mode = PermissionsExt::from(&meta.permissions()).mode();
    let masked = if opts.keep_special_bits {
        mode
    } else {
        mode & !0o6000
    };
    header.set_mode(masked & 0o7777);
    header.set_uid(meta.uid());
    header.set_gid(meta.gid());
    header.set_mtime(meta.mtime().max(0) as u64);
    if meta.is_dir() {
        header.set_entry_type(EntryType::Directory);
    }
}

/// Emit one PAX extended-header entry (`typeflag 'x'`) carrying a single
/// `SCHILY.xattr.security.capability=<raw bytes>` record for the member
/// that FOLLOWS it — the same form GNU tar --xattrs writes (RAW value
/// bytes; PAX records are length-delimited, so binary values are legal) and
/// `apply_pax_xattrs` reads back. Framing: the record is
/// `<len> <key>=<value>\n` where `<len>` counts itself, the space, the
/// key=value and the newline.
fn append_pax_xattr<W: Write>(
    builder: &mut Builder<W>,
    next_member: &Path,
    raw: &[u8],
) -> std::io::Result<()> {
    let key = b"SCHILY.xattr.security.capability=";
    let payload = key.len() + raw.len() + 1;
    let mut digits = 1usize;
    loop {
        let total = digits + 1 + payload;
        if total.to_string().len() == digits {
            let mut body = Vec::with_capacity(total);
            body.extend_from_slice(total.to_string().as_bytes());
            body.push(b' ');
            body.extend_from_slice(key);
            body.extend_from_slice(raw);
            body.push(b'\n');
            let mut h = Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_entry_type(EntryType::XHeader);
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(0);
            let name = format!("PaxHeaders/mix/{}", next_member.display());
            return builder.append_data(&mut h, name, std::io::Cursor::new(body));
        }
        digits += 1;
    }
}

/// Size-query first, then read: a fixed buffer would silently truncate
/// VFS_CAP_REVISION_3 values (~1 KB) into corrupt capabilities.
fn xattr_security_capability(path: &Path) -> Option<Vec<u8>> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let name = b"security.capability\0".as_ptr() as *const libc::c_char;
    let size = unsafe { libc::getxattr(c.as_ptr(), name, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::getxattr(
            c.as_ptr(),
            name,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}
