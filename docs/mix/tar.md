# tar — archives in-language: tar_list, tar_unpack, tar_pack

Archives are the distribution substrate: MixOS images and app packs arrive
as tar + a compression codec, and inside a MixOS machine there is no
external `tar` or `zstd` at all — `/bin/sh` is mix, so the language has to
read them itself. On stock hosts the same builtins replace `importctl`
(which needs systemd ≥ 256, cannot read zstd on 255, and would touch the
host's `import-pubring.gpg`): the signed manifest pins a digest,
`hash_file` compares against it, and `tar_unpack` consumes the SAME
verified file. No C codec ships in the binary — zstd is the pure-Rust
RFC 8878 implementation (structured-zstd), gzip framing is flate2 over its
Rust backend.

List the family live with `mix builtins tar`; one-line help with
`mix builtins NAME`.

## Codecs and the artifact profile

`codec` is explicit on every entry point: `"zstd"` (default), `"gzip"`,
`"none"`. The artifact profile is a contract BOTH encoders must satisfy,
because the builtin decoder is the universal reader:

- **single frame** — `tar_unpack`/`tar_list` drain the stream to true EOF:
  the zstd frame checksum / gzip CRC are verified there, any non-zero
  decoded bytes after the tar end are refused, and raw input left unconsumed
  (a concatenated second frame) is refused by source position;
- **no dictionaries**;
- zstd levels are the structured-zstd **presets `1/3/7/11`** (default 7),
  whose windows stay far under the decoder's 128 MiB ceiling — system zstd
  on workers must never use `--long` beyond that, and every worker-produced
  artifact must decode with the pinned builtin before release.

For artifacts humans unpack with stock tools, `codec:"gzip"` (`tar -xzf`
works everywhere); `level` for gzip clamps 11 to 9.

## The builtins

| Call | Result |
|---|---|
| `tar_list(path[, opts])` | list of `{name,size,mode,uid,gid,kind,mtime}` — streaming, nothing extracted, stream verified |
| `tar_unpack(path, dest[, opts])` | receipt `{files,dirs,symlinks,hardlinks,bytes,xattrs_restored,entries,trailing_padding,codec}` |
| `tar_pack(source, path[, opts])` | receipt `{files,dirs,symlinks,bytes,capabilities,codec,level}` |

`opts` keys are validated strictly — unknown keys raise.

## tar_unpack is safe by default, and staged

- `dest` **must not exist**. Extraction happens in a private `0700`
  staging sibling, renamed into place only after the whole stream verifies;
  any failure removes the staging tree and leaves nothing behind.
- Member names are validated on the *resolved* path (GNU longname/PAX
  smuggling lands there): no absolute paths, no `.`/`..` components,
  UTF-8 only (invalid bytes raise — the extractor never guesses what got
  extracted).
- Device, fifo and unknown entry types are **refused**.
- Symlink targets must be contained (no absolute, no `..`). Nothing is
  ever extracted *through* a symlink the archive created. Hardlinks may
  only target an earlier **regular file** (hardlink-to-hardlink is
  refused).
- `numeric_owner` (default true) restores uid/gid — via `lchown`, never
  following a link; non-root callers get the documented capability limit
  (silently skipped) rather than an error.
- `xattrs` (default true) restores `SCHILY.xattr.*` records by hand —
  EXCEPT `security.*`, which is skipped unless `keep_special_bits:true`:
  a capability xattr is a privilege grant exactly like a suid bit, and
  suid/sgid are stripped from modes by the same rule.
- Directory and symlink metadata (mode/mtime/owner) is applied in a
  children-first post-pass so directory mtimes stick.
- Limits: `max_entries` (200k), `max_bytes` (64 GiB file bytes),
  `max_name` (4096), and `max_stream_bytes` (16 GiB) — the last bounds the
  **decoded** stream, which is where tar's internal longname/PAX buffers
  live, so it is the memory-bomb limit as much as a size limit.

## tar_pack

Deterministic (sorted, parents-first) walk of a source **directory**;
numeric owner, mtime and mode preserved (suid/sgid stripped unless
`keep_special_bits:true`); `security.capability` is captured into a
`SCHILY.xattr` PAX record exactly as GNU `tar --xattrs` writes it, so
capability canaries round-trip (root-only to apply on unpack). Refuses
device/fifo members. Output is created new (`0600`, refuses to overwrite).
Two packs of an unchanged tree are byte-identical.

## Round-trip

```
tar_pack("/var/tmp/stage", "/var/tmp/app.tar.zst", {codec:"zstd", level:11})
$receipt = tar_unpack("/var/tmp/app.tar.zst", "/var/tmp/app")
-- receipt.xattrs_restored counts applied records; capabilities only when
-- keep_special_bits was set on BOTH ends.
```
