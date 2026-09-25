// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/simd/mod.rs; see vendor/msedit/README.md.
// Patched: `memchr2` is not vendored in E0 (it serves navigation, which lands in E1).

//! Provides various high-throughput utilities.

pub mod lines_bwd;
pub mod lines_fwd;

pub use lines_bwd::*;
pub use lines_fwd::*;

#[cfg(test)]
mod test {
    // Knuth's MMIX LCG
    pub fn make_rng() -> impl FnMut() -> usize {
        let mut state = 1442695040888963407u64;
        move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state as usize
        }
    }

    pub fn generate_random_text(len: usize) -> String {
        const ALPHABET: &[u8; 20] = b"0123456789abcdef\n\n\n\n";

        let mut rng = make_rng();
        let mut res = String::new();

        for _ in 0..len {
            res.push(ALPHABET[rng() % ALPHABET.len()] as char);
        }

        res
    }

    pub fn count_lines(text: &str) -> usize {
        text.lines().count()
    }
}
