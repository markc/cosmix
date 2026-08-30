// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME for mix's
// build provenance (version-discovery contract). Runs in the mix repo,
// so it captures mix's HEAD — the provenance a mix --serve citizen
// (e.g. statecache) reports for itself via noded.register.
fn main() {
    cosmix_buildinfo::emit();
}
