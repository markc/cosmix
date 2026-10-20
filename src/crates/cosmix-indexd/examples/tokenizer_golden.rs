// SPDX-License-Identifier: MIT
//! Generates the token-id golden for the MixOS indexd port (stage 7 of
//! ~/.mix/_plan/2026-10-20-indexd-port.md), using cosmix-indexd's own pinned
//! tokenizers (0.21.x, onig). It mirrors mixos services/indexd/tests/tokenizer_parity.rs
//! exactly: skip `#` lines, `encode(text, true)`, then a JSON array of id arrays.
//!   cargo run --release -p cosmix-indexd --example tokenizer_golden -- <tokenizer.json> <corpus.txt>
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tokenizer = tokenizers::Tokenizer::from_file(&args[1]).expect("read tokenizer.json");
    let corpus = std::fs::read_to_string(&args[2]).expect("read corpus");
    let ids: Vec<Vec<u32>> = corpus
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|text| tokenizer.encode(text, true).expect("tokenize").get_ids().to_vec())
        .collect();
    println!("{}", serde_json::to_string(&ids).expect("encode json"));
}
