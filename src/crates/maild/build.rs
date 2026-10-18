// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME for the
// build-provenance surface (version-discovery contract). Captures this
// repo's (cos) HEAD because this build.rs runs in cos.
fn main() {
    cosmix_buildinfo::emit();
}
