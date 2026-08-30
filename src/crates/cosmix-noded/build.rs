// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME for the
// build-provenance surface (SPEC 02 §4.1;
// cosmix-lib-bus::service_info). Captures
// the cos repo's HEAD because this build.rs runs in cos.
fn main() {
    cosmix_buildinfo::emit();
}
