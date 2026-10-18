// Emit COSMIX_GIT_SHA / COSMIX_GIT_DIRTY / COSMIX_BUILD_TIME so `--version`
// reports this crate's build provenance (cosmix_buildinfo::exit_on_version!).
fn main() {
    cosmix_buildinfo::emit();
}
