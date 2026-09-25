//! Regenerates `src/defs.rs` from `vendor/lsh/definitions/*.lsh` — what
//! msedit's build script does, run by hand so the output is committed and
//! reviewable (ced E1 plan §1.2, D10).
//!
//!     cargo run --release -p cosmix-lsh --example gen            # writes src/defs.rs
//!     cargo run --release -p cosmix-lsh --example gen -- --stdout
//!
//! `tests/defs_fresh.rs` fails until the committed file matches.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let defs = match cosmix_lsh::compiler::generate_defs() {
        Ok(defs) => defs,
        Err(e) => {
            eprintln!("gen: failed to compile lsh definitions: {e}");
            return ExitCode::from(1);
        }
    };
    if std::env::args().any(|a| a == "--stdout") {
        print!("{defs}");
        return ExitCode::SUCCESS;
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join(cosmix_lsh::compiler::DEFS_PATH);
    if let Err(e) = std::fs::write(&out, defs) {
        eprintln!("gen: cannot write {}: {e}", out.display());
        return ExitCode::from(1);
    }
    eprintln!("gen: wrote {}", out.display());
    ExitCode::SUCCESS
}
