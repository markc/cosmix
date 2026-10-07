//! `tar_list` / `tar_unpack` / `tar_pack` tests — the round-trip contract
//! plus the ADVERSARIAL set: every archive here that escapes, lies or bombs
//! must be refused by name, and the refusal is the assertion. Malicious
//! archives are built with the raw `tar` crate as PLAIN tar (codec "none"):
//! the validation layer is codec-independent; the codec layers are covered
//! by the builtin's own round-trips (zstd/gzip/none) and the corruption
//! cases (truncation, trailing data).
#![cfg(feature = "archive")]

use cosmix_mix::value::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tar::{Builder, Header};

/// Run `source` and return the program's result value rendered briefly.
async fn run_ok(source: &str) -> Value {
    match cosmix_mix::run(source).await {
        Ok(v) => v,
        Err(e) => panic!("script failed: {e}\nsource: {source}"),
    }
}

async fn run_err(source: &str) -> String {
    match cosmix_mix::run(source).await {
        Ok(v) => panic!("expected an error, got {v:?}\nsource: {source}"),
        Err(e) => e.to_string(),
    }
}

/// Unique temp dir per test.
fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mix-archive-test-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Build a plain-tar archive from (name, bytes, mode) triples.
fn build_plain(path: &std::path::Path, members: &[(&str, &[u8], u32)]) {
    let file = fs::File::create(path).unwrap();
    let mut b = Builder::new(file);
    for (name, data, mode) in members {
        let mut h = Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(*mode);
        h.set_uid(1000);
        h.set_gid(1000);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, name, std::io::Cursor::new(data.to_vec()))
            .unwrap();
    }
    b.into_inner().unwrap().flush().unwrap();
}

// ---------------------------------------------------------------------------
// Round-trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_all_codecs() {
    let d = tmpdir("rt");
    let src = d.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("hello.txt"), b"hello archive\n").unwrap();
    fs::write(src.join("sub/deep.bin"), vec![0u8; 100_000]).unwrap();
    fs::set_permissions(src.join("hello.txt"), fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("hello.txt", src.join("link")).unwrap();

    for codec in ["zstd", "gzip", "none"] {
        let arc = d.join(format!("out-{codec}.tar"));
        let pack = run_ok(&format!(
            "tar_pack(\"{}\", \"{}\", {{codec:\"{}\"}})",
            src.display(),
            arc.display(),
            codec
        ))
        .await;
        assert!(matches!(pack, Value::Map(_)), "{codec} pack receipt");
        let list = run_ok(&format!(
            "tar_list(\"{}\", {{codec:\"{}\"}})",
            arc.display(),
            codec
        ))
        .await;
        let names = match list {
            Value::List(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::Map(m) => m.get("name").unwrap().to_mix_string(),
                    other => panic!("entry not a map: {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("list not a list: {other:?}"),
        };
        assert!(names.contains(&"hello.txt".to_string()), "{codec}: {names:?}");
        assert!(names.contains(&"sub/deep.bin".to_string()), "{codec}: {names:?}");
        assert!(names.contains(&"link".to_string()), "{codec}: {names:?}");

        let dest = d.join(format!("dest-{codec}"));
        run_ok(&format!(
            "tar_unpack(\"{}\", \"{}\", {{codec:\"{}\"}})",
            arc.display(),
            dest.display(),
            codec
        ))
        .await;
        assert_eq!(fs::read(dest.join("hello.txt")).unwrap(), b"hello archive\n");
        assert_eq!(fs::read(dest.join("sub/deep.bin")).unwrap().len(), 100_000);
        assert_eq!(
            fs::symlink_metadata(dest.join("hello.txt")).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            fs::read_link(dest.join("link")).unwrap().to_string_lossy(),
            "hello.txt"
        );
    }
}

#[tokio::test]
async fn pack_is_deterministic() {
    let d = tmpdir("det");
    let src = d.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a"), b"same").unwrap();
    fs::write(src.join("b"), b"bytes").unwrap();
    let a = d.join("a.tar");
    let b = d.join("b.tar");
    run_ok(&format!("tar_pack(\"{}\", \"{}\")", src.display(), a.display())).await;
    std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct mtime would break determinism if captured wrongly
    run_ok(&format!("tar_pack(\"{}\", \"{}\")", src.display(), b.display())).await;
    assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
}

// ---------------------------------------------------------------------------
// Adversarial: escapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refuse_parent_traversal() {
    let d = tmpdir("trav");
    let arc = d.join("evil.tar");
    build_plain(&arc, &[("../escape.txt", b"gotcha", 0o644)]);
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("..") || err.contains("parent"), "{err}");
    assert!(!d.join("../escape.txt").exists());
}

#[tokio::test]
async fn refuse_absolute_member() {
    let d = tmpdir("abs");
    let arc = d.join("evil.tar");
    build_plain(&arc, &[("/etc/gotcha", b"no", 0o644)]);
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("absolute"), "{err}");
}

#[tokio::test]
async fn refuse_symlink_then_write_through_it() {
    let d = tmpdir("sym");
    let arc = d.join("evil.tar");
    // A contained symlink first (valid on its own), then a member whose
    // path TRAVERSES it — the classic write-outside escape.
    let file = fs::File::create(&arc).unwrap();
    let mut b = Builder::new(file);
    let mut h = Header::new_gnu();
    h.set_size(0);
    h.set_mode(0o777);
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_mtime(0);
    h.set_cksum();
    h.set_link_name("../../outside").unwrap();
    b.append_data(&mut h, "pivot", std::io::empty()).unwrap();
    let mut h2 = Header::new_gnu();
    h2.set_size(5);
    h2.set_mode(0o644);
    h2.set_mtime(0);
    h2.set_cksum();
    b.append_data(&mut h2, "pivot/inside", std::io::Cursor::new(b"evil\n"))
        .unwrap();
    b.into_inner().unwrap().flush().unwrap();

    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    // The escaping TARGET is refused before the traversal check can bite.
    assert!(err.contains("symlink") && (err.contains("escaping") || err.contains("pivot")), "{err}");
    assert!(!d.join("../../outside").exists() || fs::read_link(d.join("dest/pivot")).is_err());
}

#[tokio::test]
async fn refuse_hardlink_to_unextracted() {
    let d = tmpdir("hard");
    let arc = d.join("evil.tar");
    let file = fs::File::create(&arc).unwrap();
    let mut b = Builder::new(file);
    let mut h = Header::new_gnu();
    h.set_size(0);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Link);
    h.set_mtime(0);
    h.set_cksum();
    h.set_link_name("never/extracted").unwrap();
    b.append_data(&mut h, "alias", std::io::empty()).unwrap();
    b.into_inner().unwrap().flush().unwrap();
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("hardlink"), "{err}");
}

#[tokio::test]
async fn refuse_device_and_fifo_entries() {
    let d = tmpdir("dev");
    for (tag, et) in [("chardev", tar::EntryType::Char), ("fifo", tar::EntryType::Fifo)] {
        let arc = d.join(format!("{tag}.tar"));
        let file = fs::File::create(&arc).unwrap();
        let mut b = Builder::new(file);
        let mut h = Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(et);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, tag, std::io::empty()).unwrap();
        b.into_inner().unwrap().flush().unwrap();
        let err = run_err(&format!(
            "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
            arc.display(),
            d.join("dest").display()
        ))
        .await;
        assert!(err.contains("refusing"), "{tag}: {err}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: stream integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refuse_trailing_data_after_tar_end() {
    let d = tmpdir("tail");
    let arc = d.join("plus.tar");
    build_plain(&arc, &[("a.txt", b"a", 0o644)]);
    // Append a second concatenated tar (non-zero bytes after the first
    // archive's end) — the single-frame policy must refuse.
    let mut f = fs::OpenOptions::new().append(true).open(&arc).unwrap();
    f.write_all(b"SECOND-ARCHIVE-GARBAGE").unwrap();
    drop(f);
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("non-zero data after"), "{err}");
}

#[tokio::test]
async fn refuse_truncated_gzip() {
    let d = tmpdir("trunc");
    let src = d.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("data"), vec![7u8; 50_000]).unwrap();
    let arc = d.join("data.tar.gz");
    run_ok(&format!(
        "tar_pack(\"{}\", \"{}\", {{codec:\"gzip\"}})",
        src.display(),
        arc.display()
    ))
    .await;
    let full = fs::read(&arc).unwrap();
    fs::write(&arc, &full[..full.len() - 200]).unwrap();
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"gzip\"}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("verification failed") || err.contains("unexpected"), "{err}");
}

#[tokio::test]
async fn refuse_corrupt_zstd_checksum() {
    let d = tmpdir("cksum");
    let src = d.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("data"), vec![9u8; 50_000]).unwrap();
    let arc = d.join("data.tar.zst");
    run_ok(&format!(
        "tar_pack(\"{}\", \"{}\")",
        src.display(),
        arc.display()
    ))
    .await;
    // Flip one payload byte well inside the compressed body.
    let mut bytes = fs::read(&arc).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    fs::write(&arc, bytes).unwrap();
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\")",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(
        err.contains("verification failed") || err.contains("checksum") || err.contains("corrupt"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Limits and modes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enforce_max_entries() {
    let d = tmpdir("max");
    let arc = d.join("three.tar");
    build_plain(&arc, &[("a", b"1", 0o644), ("b", b"2", 0o644), ("c", b"3", 0o644)]);
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\", max_entries:2}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("max_entries"), "{err}");
}

#[tokio::test]
async fn suid_stripped_by_default_kept_opt_in() {
    let d = tmpdir("suid");
    let arc = d.join("s.tar");
    build_plain(&arc, &[("rootish", b"x", 0o4755)]);
    let dest = d.join("dest");
    run_ok(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        dest.display()
    ))
    .await;
    let mode = fs::metadata(dest.join("rootish")).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o755, "suid must be stripped by default");
    let dest2 = d.join("dest2");
    run_ok(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\", keep_special_bits:true}})",
        arc.display(),
        dest2.display()
    ))
    .await;
    let mode2 = fs::metadata(dest2.join("rootish")).unwrap().permissions().mode();
    assert_eq!(mode2 & 0o7777, 0o4755);
}

#[tokio::test]
async fn refuse_any_existing_dest() {
    let d = tmpdir("dest");
    let arc = d.join("one.tar");
    build_plain(&arc, &[("a", b"1", 0o644)]);
    let dest = d.join("dest");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("existing"), b"keep me").unwrap();
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        dest.display()
    ))
    .await;
    assert!(err.contains("exists"), "{err}");
    // An empty dir is also refused: extraction is staged, never merged, and
    // a pre-existing dir is indistinguishable from a mount point.
    let empty = d.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let err2 = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\"}})",
        arc.display(),
        empty.display()
    ))
    .await;
    assert!(err2.contains("exists"), "{err2}");
    assert!(empty.read_dir().unwrap().next().is_none(), "staging must not leak into a refused dest");
}

#[tokio::test]
async fn unknown_option_refused() {
    let d = tmpdir("opts");
    let arc = d.join("one.tar");
    build_plain(&arc, &[("a", b"1", 0o644)]);
    let err = run_err(&format!(
        "tar_unpack(\"{}\", \"{}\", {{codec:\"none\", frobnicate:true}})",
        arc.display(),
        d.join("dest").display()
    ))
    .await;
    assert!(err.contains("frobnicate"), "{err}");
}

// ---------------------------------------------------------------------------
// Capability round-trip — root only (security.* xattrs need euid 0);
// skipped in normal dev runs, exercised by the root canary flow instead.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capability_roundtrip_as_root() {
    if unsafe { libc::geteuid() } != 0 {
        return; // documented skip
    }
    let d = tmpdir("cap");
    let src = d.join("src");
    fs::create_dir_all(&src).unwrap();
    let prog = src.join("prog");
    fs::write(&prog, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&prog, fs::Permissions::from_mode(0o755)).unwrap();
    let st = fs::File::open(&prog).unwrap();
    // set security.capability v3 (rootid 0)
    let mut cap = vec![0u8; 20];
    cap[0] = 0; // version 1 for simplicity
    unsafe {
        let c = std::ffi::CString::new(prog.as_os_str().as_bytes()).unwrap();
        let r = libc::setxattr(
            c.as_ptr(),
            b"security.capability\0".as_ptr() as *const libc::c_char,
            cap.as_ptr() as *const libc::c_void,
            cap.len(),
            0,
        );
        if r != 0 {
            panic!("setxaddr failed: {}", std::io::Error::last_os_error());
        }
    }
    drop(st);
    let arc = d.join("cap.tar");
    run_ok(&format!(
        "tar_pack(\"{}\", \"{}\")",
        src.display(),
        arc.display()
    ))
    .await;
    let dest = d.join("dest");
    run_ok(&format!(
        "tar_unpack(\"{}\", \"{}\")",
        arc.display(),
        dest.display()
    ))
    .await;
    // The restored xattr must exist and round-trip byte-exact.
    let mut got = vec![0u8; 64];
    let n = unsafe {
        let c = std::ffi::CString::new(dest.join("prog").as_os_str().as_bytes()).unwrap();
        libc::getxattr(
            c.as_ptr(),
            b"security.capability\0".as_ptr() as *const libc::c_char,
            got.as_mut_ptr() as *mut libc::c_void,
            got.len(),
        )
    };
    assert_eq!(n as usize, cap.len(), "capability xattr must round-trip");
    got.truncate(n as usize);
    assert_eq!(got, cap);
}
