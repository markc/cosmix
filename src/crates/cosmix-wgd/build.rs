// Emits the COSMIX_* build-provenance env (cos HEAD sha, build time, dirty
// flag) consumed by `cosmix_buildinfo::build_info!()` in the Bus register
// path — the mesh-wide version-discovery contract. Identical to every other
// citizen's build.rs.
fn main() {
    cosmix_buildinfo::emit();
}
