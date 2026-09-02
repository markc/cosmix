# AGENTS.md — Mix orientation for AI agents

Read `CLAUDE.md` before substantive repository changes for repo identity, dependency direction, release/version discipline, and the runtime auto-upgrade contract. Use `$mix-memory` for targeted historical fixes only; the live binary and current code override memory.

**The Mix language reference is the live manual, read straight from the binary —
not this file.** Mix has near-zero presence in model training data, so don't
extrapolate from bash/python; ask the binary, which is the oracle:

- `mix man` — manual index; `mix man TOPIC` opens one live-verified page.
  **Start with `mix man overview` and `mix man syntax`** — the mental model, the
  newline rule, and the shell-vs-Mix classifier (the mistakes an agent makes
  first). The same pages render at
  <https://github.com/markc/cosmix/tree/main/docs/mix> and under `$COSMIX/docs/mix/`.
- `mix builtins` — every builtin with `name(args) -> ret` signatures
  (`mix builtins --json` machine-readable · `mix builtins <name>` for one ·
  `mix builtins <category>` to filter).
- `mix help` / `mix --help` — the `mix` command surface and CLI flags.
- `mix -c '<probe>'` — run a one-liner (`;` separates multiple Mix statements)
  to check live behaviour. When any doc
  conflicts with what the binary does, the binary wins.

These read from the binary and never drift — prefer them over any static
cheat-sheet, this file included.

## Repo workflow (for agents editing this repo)

Build/test/lint from `src/` — the sibling `$COSMIX/` checkout must also be present:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # zero-warning baseline
```

Where new functionality goes (crate `cosmix-lib-mix` unless noted):

- **New syntax** (keyword / operator / statement) → `lexer.rs` + `parser.rs` +
  `evaluator.rs`; add parse + eval unit tests.
- **New builtin** (`foo()`) → `builtins.rs`. Prefer extending Mix over a wrapper
  script — a missing builtin is a Mix bug.
- **REPL / shell-mode feature** → `cosmix-mix/src/main.rs` + `repl.rs`.
- **Bus `--serve` runtime** → `cosmix-mix/src/serve_runtime.rs` (preserve the
  `MixBusHandler` lazy-probe state machine).

**Doc discipline:** when you change Mix syntax, semantics, or builtins, update the
affected `docs/_man/` page **in the same commit**. The manual is the reference
every agent reads, so it must stay live-verified against the binary.
