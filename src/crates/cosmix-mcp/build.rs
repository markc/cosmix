// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME so
// `cosmix-mcp --version` can report build provenance. mcp connects
// anonymously (no noded.register), so --version is its ONLY version
// surface — the 2026-06-01 stale-binary case.
fn main() {
    cosmix_buildinfo::emit();
}
