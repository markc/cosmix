# Vendored: msedit `lsh` + `stdext` (Microsoft Edit)

| Provenance | Value |
|---|---|
| upstream | `https://github.com/microsoft/edit` |
| commit | `826b4c097b6f14ba0a846dc56f2f0223a3aaf73a` (2026-08-27) |
| licence | MIT, Copyright (c) Microsoft Corporation — `LICENSE` in this directory, verbatim |
| pristine import | commit `403eba59` (byte-for-byte, not wired) |
| patch | the commit that follows it ("ced E1 Stage S: patch and wire vendored msedit") |

The two upstream crates are kept **as their own path crates** so their
sources compile unmodified: library names stay `lsh` and `stdext`, package
names are `cosmix-msedit-lsh` / `cosmix-msedit-stdext` (`publish = false`).
Only `cosmix-lsh` (the parent crate) depends on them.

## Files

| Here | Upstream path | Changes |
|---|---|---|
| `lsh/` | `crates/lsh/` (whole crate: `src/**`, `definitions/**`, `README.md`) | manifest rewritten (no msedit workspace inheritance); per-file provenance header |
| `stdext/` | `crates/stdext/` (whole crate) | manifest rewritten (feature `single-threaded` gone); patches 1–2; per-file provenance header |
| `edit-lsh/*.rs` | `crates/edit/src/lsh/{mod,highlighter,cache,definitions}.rs` | none — **reference only**, not compiled; `cosmix-lsh/src/{highlighter,cache}.rs` are the adapted versions |
| `LICENSE` | `LICENSE` | verbatim |

## Patch log (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §1.2, cmctl hub)

1. **No mutable statics — scratch arenas** (`stdext/src/arena/scratch.rs`):
   `mod single_threaded` (its arenas lived in a mutable static behind the
   `single-threaded` feature) is deleted; the thread-local variant, which
   already initialises lazily with a 128 MiB *virtual* reserve, is the only one.
2. **No mutable statics — memset dispatch** (`stdext/src/simd/memset.rs`):
   the fn pointer overwritten on first call (a data race) became a typed
   `OnceLock` filled by the same feature probe — the same patch E0 applied to
   `lines_fwd`/`lines_bwd`. The loongarch arms (nightly-only intrinsics) are
   removed; x86/x86_64 (AVX2/SSE2), aarch64 (NEON) and the fallback remain.
3. Every vendored `.rs` file gains a line naming its upstream path; three
   upstream files had no licence header and say so.
4. **Deterministic register allocation** (`lsh/src/compiler/backend.rs`,
   `compute_intervals`; ced Stage E1b): live intervals are sorted by
   `(start, vreg_id)` instead of `start` alone. Upstream's equal-start order
   came from a `HashMap`, whose iteration order is seeded per process, so the
   bytecode changed from run to run. Harmless inside msedit's build script;
   fatal for a committed `defs.rs` with a freshness test.

## Adapted, not vendored (`cosmix-lsh/src/`)

`highlighter.rs` and `cache.rs` are rewritten from `edit-lsh/` rather than
patched, so the differences are listed in their module docs: the
`LineSource` swap, plain byte scanning for newlines, owned `Vec<Span>`
output with the runtime's sentinel dropped, a fix for multi-chunk lines of
`MAX_LINE_LEN` or more (upstream left the read offset mid-line, shifting every
later line number by one), `INTERVAL` pinned to 1024, and a frontier state so
time-sliced seeks progress below one interval per call. `defs.rs` is the
generator's output with its Mermaid IR dump removed (it prints node
addresses) and `Hash` added to `HighlightKind`'s derives.

The lsh regex compiler still panics on unsupported patterns (upstream TODO);
it only runs in `examples/gen.rs` and `tests/defs_fresh.rs`, never at runtime.
