# cosmix-lib-buildinfo — build provenance

**`cosmix-lib-buildinfo` embeds a consumer's compile-time identity without
adding runtime dependencies.** It records the package and version, git
revision, dirty-tree state, and build timestamp used for fleet inventory and
Bus service registration.

## What it is

`BuildInfo` contains five compile-time fields:

- `pkg` and `version` from the crate where `build_info!()` expands;
- `git_sha`, a 12-character revision captured from the consumer repository;
- `git_dirty`, reporting uncommitted repository content at build time; and
- `build_time`, an RFC3339 UTC timestamp.

`BuildInfo::line()` renders the standard one-line form. `now_rfc3339()` is the
separate runtime helper used for a citizen's process start timestamp.

## How consumers use it

Add the crate as both a normal dependency and a build dependency. The
consumer's `build.rs` calls `emit()`:

```rust
fn main() {
    cosmix_lib_buildinfo::emit();
}
```

The consumer then expands the macro in its own crate:

```rust
let build = cosmix_lib_buildinfo::build_info!();
println!("{}", build.line());
```

This two-stage arrangement is deliberate. `emit()` runs git in the consumer's
repository and exports `COSMIX_GIT_SHA`, `COSMIX_GIT_DIRTY`, and
`COSMIX_BUILD_TIME`; `build_info!()` combines them with that consumer crate's
`CARGO_PKG_*` values. It therefore describes the binary being built, not the
bus checkout that supplies the helper.

Missing git or a missing `emit()` call degrades provenance to `unknown` rather
than failing the build. `SOURCE_DATE_EPOCH` pins the build timestamp for
reproducible builds.

For broker inventory, consumers pass these fields and one process-start
timestamp to `RegisterProvenance::from_parts` from
[cosmix-lib-bus](wire-format.md), then connect with
`NodedClient::connect_with_provenance` or the supervised equivalent.

## See also

- [wire format](wire-format.md) — `RegisterProvenance`, `ServiceInfo`, and `NodeInfo`
- [client](client.md) — provenance-aware registration and reconnect
- [overview](overview.md) — the crate family
