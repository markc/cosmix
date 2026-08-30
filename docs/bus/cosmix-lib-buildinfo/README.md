# cosmix-lib-buildinfo

`cosmix-lib-buildinfo` records compile-time build provenance for Cosmix daemons and other Rust consumers. It is a dependency-free leaf library at the `bus` end of the `bus ← mix ← cos` dependency chain, so crates in `mix` and `cos` can embed their own package version, Git state, and build time without reversing that dependency order.

## Synopsis

The crate has two cooperating entry points:

- `emit()` runs from the consumer's `build.rs` and captures provenance from the consumer's repository.
- `build_info!()` expands in the consumer crate and constructs a `BuildInfo` from package metadata and the values emitted by `emit()`.

It also provides `now_rfc3339()` for runtime start timestamps.

The crate contains one root module and no public submodules.

## What it provides

| Item | Kind | Purpose |
|---|---|---|
| `BuildInfo` | struct | Holds the calling crate's compile-time provenance |
| `BuildInfo::line()` | method | Formats the provenance as one human-readable line |
| `build_info!()` | macro | Constructs `BuildInfo` at the macro expansion site |
| `emit()` | function | Emits Cargo build-script environment and rerun directives |
| `now_rfc3339()` | function | Returns the current wall-clock time as RFC3339 UTC |

## BuildInfo

`BuildInfo` is `Debug`, `Clone`, `Copy`, `PartialEq`, and `Eq`.

All fields are public `&'static str` values or a boolean:

| Field | Meaning |
|---|---|
| `pkg` | `CARGO_PKG_NAME` of the crate that expands `build_info!()` |
| `version` | `CARGO_PKG_VERSION` of that crate |
| `git_sha` | Short Git SHA of the consumer repository, or `"unknown"` |
| `git_dirty` | Whether the consumer repository was dirty at build time |
| `build_time` | RFC3339 UTC build timestamp, or `"unknown"` |

Construct this type with `build_info!()` so Cargo package values resolve in the calling crate.

`BuildInfo::line()` returns:

```text
<pkg> <version> (<sha>[-dirty], built <time>)
```

For example:

```text
cosmix-demo 1.2.3 (abc123def456-dirty, built 2026-06-01T00:00:00Z)
```

## Consumer setup

Add the crate as both a normal dependency and a build dependency:

```toml
[dependencies]
cosmix-lib-buildinfo = { path = "..." }

[build-dependencies]
cosmix-lib-buildinfo = { path = "..." }
```

Call `emit()` from the consumer's `build.rs`:

```rust
fn main() {
    cosmix_lib_buildinfo::emit();
}
```

Expand `build_info!()` in the consumer crate:

```rust
let info = cosmix_lib_buildinfo::build_info!();
println!("{}", info.line());
```

The macro uses `env!` for Cargo package metadata and `option_env!` for the build-script values. Expansion therefore describes the calling crate rather than `cosmix-lib-buildinfo`.

## Build-script output

`emit()` writes these compile-time variables through `cargo:rustc-env`:

| Variable | Value |
|---|---|
| `COSMIX_GIT_SHA` | Output of `git rev-parse --short=12 HEAD`, or `"unknown"` |
| `COSMIX_GIT_DIRTY` | `1` when `git status --porcelain` is non-empty, otherwise `0` |
| `COSMIX_BUILD_TIME` | RFC3339 UTC build time |

Git capture is best-effort. A missing Git executable, a non-repository build, or a failed Git command does not fail the build.

The dirty check is repository-wide. It includes tracked changes and untracked, non-ignored files.

## Rebuild triggers

`emit()` asks Cargo to rerun the consumer build script when these inputs change:

- the consumer package's `src` directory;
- the consumer package's `Cargo.toml`;
- the Git directory's `HEAD`;
- the common Git directory's `packed-refs`;
- the current symbolic branch reference, when present;
- the `SOURCE_DATE_EPOCH` environment variable.

The Git-directory and common-directory watches support ordinary checkouts and linked worktrees.

Dirty-state freshness is best-effort between rebuilds. Changes elsewhere in the repository may not update the bit until one of the watched consumer-package inputs causes the build script to run.

## Reproducible build time

When `SOURCE_DATE_EPOCH` is unset, `emit()` uses the current system time.

When `SOURCE_DATE_EPOCH` contains a valid integer, that Unix timestamp becomes `COSMIX_BUILD_TIME`.

When it is set to an invalid value, `emit()` warns and uses Unix epoch zero:

```text
1970-01-01T00:00:00Z
```

This invalid-value fallback remains deterministic instead of silently using wall-clock time.

Timestamp formatting is implemented inside the crate and does not require `chrono`, `time`, or another date-time dependency.

## Runtime timestamps

`now_rfc3339()` returns the current wall-clock time as an RFC3339 UTC string:

```rust
let started_at = cosmix_lib_buildinfo::now_rfc3339();
```

This function is intended for live runtime provenance such as a process `started_at` value.

It ignores `SOURCE_DATE_EPOCH`; that variable affects build artifacts, not live timestamps.

## Degraded operation

A consumer may use `build_info!()` without calling `emit()` from `build.rs`.

The crate still compiles. The resulting values are:

- `git_sha`: `"unknown"`;
- `git_dirty`: `false`;
- `build_time`: `"unknown"`.

The package name and version remain available because Cargo supplies them at the macro expansion site.

## Cargo surface

The package name is `cosmix-lib-buildinfo`. Rust code imports it as `cosmix_lib_buildinfo`.

The crate declares no Cargo features.

The crate declares no dependencies and uses only the Rust standard library.

It is a library only. It defines no CLI, daemon process, configuration format, subcommands, or Bus verbs.
