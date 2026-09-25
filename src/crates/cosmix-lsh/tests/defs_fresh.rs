//! `src/defs.rs` is generated and committed (ced E1 plan §1.2): regenerate it
//! in memory and require the committed bytes to match exactly.

use std::path::Path;

#[test]
fn committed_defs_match_the_definitions() {
    let fresh = cosmix_lsh::compiler::generate_defs().expect("the vendored definitions compile");
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(cosmix_lsh::compiler::DEFS_PATH);
    let committed = std::fs::read_to_string(&path).expect("src/defs.rs is readable");
    assert!(
        fresh == committed,
        "{} is stale: run `cargo run --release -p cosmix-lsh --example gen` and commit it",
        path.display()
    );
}

/// The compiler must not depend on hash-map iteration order (each map gets a
/// fresh random seed, so two runs in one process would disagree).
#[test]
fn generation_is_deterministic() {
    let a = cosmix_lsh::compiler::generate_defs().expect("compiles");
    let b = cosmix_lsh::compiler::generate_defs().expect("compiles");
    assert!(a == b, "two generations in one process differ");
}

#[test]
fn every_language_and_association_is_wired() {
    assert!(!cosmix_lsh::defs::LANGUAGES.is_empty());
    for (glob, language) in cosmix_lsh::defs::FILE_ASSOCIATIONS {
        assert!(
            cosmix_lsh::defs::LANGUAGES.iter().any(|l| std::ptr::eq(l, *language)),
            "{glob} points outside LANGUAGES"
        );
    }
    let rust = cosmix_lsh::language_for_path(Path::new("/src/main.rs")).expect("*.rs is associated");
    assert_eq!(rust.id, "rust");
    assert_eq!(cosmix_lsh::language_by_name("markdown").map(|l| l.id), Some("markdown"));
    assert_eq!(cosmix_lsh::language_by_id("rust").map(|l| l.name), Some("Rust"));
}
