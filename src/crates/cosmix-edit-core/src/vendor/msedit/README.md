# Vendored: msedit (Microsoft Edit)

| Provenance | Value |
|---|---|
| upstream | `https://github.com/microsoft/edit` |
| commit | `826b4c097b6f14ba0a846dc56f2f0223a3aaf73a` (2026-08-27) |
| licence | MIT, Copyright (c) Microsoft Corporation — `LICENSE` in this directory, verbatim |
| pristine import | commit `b46ac3b6` (byte-for-byte, not wired into the crate) |
| patch | the commit that follows it ("cosmix-edit-core: patch and wire vendored msedit") |

## Files

| Here | Upstream path | Changes |
|---|---|---|
| `gap_buffer.rs` | `crates/edit/src/buffer/gap_buffer.rs` | patched (see below) |
| `document.rs` | `crates/edit/src/document.rs` | subset: the two traits + `&[u8]` / `String` impls |
| `helpers.rs` | `crates/edit/src/helpers.rs` | subset: `KIBI`, `MEBI`, `GIBI`, `CoordType` |
| `simd/mod.rs` | `crates/edit/src/simd/mod.rs` | `memchr2` not vendored |
| `simd/lines_fwd.rs` | `crates/edit/src/simd/lines_fwd.rs` | patched (see below) |
| `simd/lines_bwd.rs` | `crates/edit/src/simd/lines_bwd.rs` | patched (see below) |
| `stdext/helpers.rs` | `crates/stdext/src/helpers.rs` | subset: `slice_copy_safe`, `ReplaceRange` + `Vec` impl + private `vec_replace_impl` |
| `stdext/sys_unix.rs` | `crates/stdext/src/sys/unix.rs` | header only |

Every file keeps its Microsoft header and gains a line naming its upstream
path. `crate::…` paths are rewritten to `crate::vendor::msedit::…` / `super::…`.

## Patch log (ced E0 plan `_plan/2026-09-26-ced-e0-implementation.md` §2.2, cmctl hub)

1. **simd dispatch** (`lines_fwd.rs`, `lines_bwd.rs`): upstream kept the
   AVX2-or-fallback choice in a `static mut` function pointer overwritten on
   first call — a data race in a multi-threaded process. Replaced by a typed
   `OnceLock<unsafe fn(..)>` filled by the same `is_x86_feature_detected!` probe.
2. **loongarch arms removed** from both simd files (they need nightly
   `stdarch_loongarch` features). x86_64 AVX2 + fallback, aarch64 NEON and the
   scalar fallback remain.
3. **All-or-nothing memory commit** (`gap_buffer.rs`): upstream
   `allocate_gap` deleted text and then called an `enlarge_gap` that returned
   silently when the reserve was exhausted or `virtual_commit` failed, leaving
   a short gap and lost text. Now `allocate_gap` commits any memory it needs
   *before* moving the gap or deleting, and returns `io::Result` with the
   buffer untouched on failure; `replace` returns `io::Result<()>`;
   `enlarge_gap` only moves bytes (debug-asserts the commit exists). Added
   `ensure_commit`, `committed`, `commit_needed` (so a transaction can commit
   its peak up front), `commit_calls` (so phase 2 can assert it made no
   commits), and a `#[cfg(test)]` commit-failure injection switch.
   `copy_from` (unused, and not all-or-nothing) was removed.
4. Lint allowances for upstream style are scoped to `mod msedit` in `../mod.rs`.
5. **`unsafe impl Send for GapBuffer`** (`gap_buffer.rs`, ced E0b need: a
   buffer lives in a per-buffer actor task and moves between threads).
   Sound because the buffer exclusively owns its reservation via `NonNull`
   (released only in its own `Drop`), with no aliasing or interior sharing.
   Deliberately not `Sync`. `lib.rs` asserts at compile time that
   `Text` and `Buffer` are `Send`.

## E1 additions (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §1.3)

| pristine import | commit `403eba59` (byte-for-byte, not wired) |
|---|---|

| Here | Upstream path | Changes |
|---|---|---|
| `unicode/mod.rs` | `crates/edit/src/unicode/mod.rs` | header only |
| `unicode/measurement.rs` | `crates/edit/src/unicode/measurement.rs` | patched (6) |
| `unicode/tables.rs` | `crates/edit/src/unicode/tables.rs` | header only (generated UCD tables, Unicode 16.0.0) |
| `navigation.rs` | `crates/edit/src/buffer/navigation.rs` | header + path |
| `stdext/unicode/utf8.rs` | `crates/stdext/src/unicode/utf8.rs` | header only (`Utf8Chars`) |
| `helpers.rs` | `crates/edit/src/helpers.rs` | subset grows by `Point` (+ its `Ord`) |
| `stdext/helpers.rs` | `crates/stdext/src/helpers.rs` | subset grows by `cold_path` |

6. **Per-measurement ambiguous width** (`measurement.rs`): upstream's
   process-global `static mut AMBIGUOUS_WIDTH` + `setup_ambiguous_width`
   became a `MeasurementConfig` field set by `with_ambiguous_width(1|2)`.

`simd/memchr2.rs` is **not** vendored after all: nothing in measurement or
navigation calls it (upstream only uses it from the TUI buffer and the VT
parser). `crates/lsh` and the `stdext` crate it needs are vendored as their
own path crates under `cosmix-lsh/vendor/` (see that crate's README).

`GapBuffer` does not honour `ReadableDocument`'s "never split a grapheme
cluster across chunks" promise (`document.rs:23`). The measurement code is
therefore only ever handed the grapheme-safe adapter in `crate::view`
(ced E1 plan §1.3(3)), never a `GapBuffer` directly.
