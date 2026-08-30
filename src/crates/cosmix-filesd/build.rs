fn main() {
    // Stamp git sha / version / build time into the binary for the Bus INFO triple.
    cosmix_buildinfo::emit();
}
