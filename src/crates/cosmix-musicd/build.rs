// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME for the build-
// provenance surface (version-discovery contract). Captures this repo's (cos)
// HEAD. Runs for every feature set; only the `cosmix` daemon reads the values
// (via `cosmix_buildinfo::build_info!()`), so a pure-core build is unaffected.
fn main() {
    cosmix_buildinfo::emit();
}
