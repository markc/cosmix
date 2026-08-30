# cosmix-lib-mix fuzz targets

P2 of the mix tokenizer fuzz/property corpus
(`_doc/planned/mix-tokenizer-fuzz-corpus.md` in the cosmix hub). Coverage-guided
libFuzzer harnesses for the **Mix lexer and parser** — the robustness floor
beneath the example tests and the P0/P1 property suites.

This crate is its **own workspace** (the empty `[workspace]` in `Cargo.toml`), so
it is *not* built or tested by the parent `~/.mix/src` workspace. Fuzzing is
**on-demand**, never part of the fast `cargo test` gate.

## Requirements

- A nightly toolchain (`cargo +nightly`) — libFuzzer needs `-Z` flags.
- `cargo install cargo-fuzz`.
- `clang` + `llvm-symbolizer` on `PATH` (for the sanitizer + symbolized traces).

## Targets

| Target | Exercises |
|---|---|
| `fuzz_lex` | `Lexer::tokenize` on arbitrary UTF-8 — must never panic/hang/trip ASan. |
| `fuzz_parse` | lex → `Parser::parse_program` (parser fed only clean lexes, as in the real pipeline). |

## Run

```sh
cd crates/cosmix-lib-mix
cargo +nightly fuzz run fuzz_lex   -- -max_total_time=60
cargo +nightly fuzz run fuzz_parse -- -max_total_time=60
```

Drop `-max_total_time` to fuzz until interrupted. The committed seeds under
`corpus/<target>/` prime the search; libFuzzer writes newly-discovered inputs
back there (left untracked — `git add` ones worth keeping, or `git clean` them).

## On a crash

libFuzzer writes the failing input to `artifacts/<target>/crash-*` (gitignored).
Reproduce and minimize:

```sh
cargo +nightly fuzz run fuzz_lex artifacts/fuzz_lex/crash-<hash>   # reproduce
cargo +nightly fuzz tmin fuzz_lex artifacts/fuzz_lex/crash-<hash>  # shrink
```

A crash is a real bug in the lexer/parser. Triage, fix in `cosmix-lib-mix`, then
add the minimized input as a permanent seed (`corpus/<target>/`) so it becomes a
regression guard.
