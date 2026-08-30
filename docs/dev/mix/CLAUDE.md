# CLAUDE.md — markc/mix

Guidance for Claude Code sessions working in `$COSMIX/`.

## Agent language reference

The canonical Mix reference is the **manual** at `docs/_man/` (one page per topic,
live-verified against the binary — also `mix man TOPIC` in the terminal, or
<https://markc.github.io/mix/#_man/overview.md> on the web). `AGENTS.md` (repo
root) is the short orientation sheet on top of it: the mental model, the five
first errors, one screen of sharp edges, and the manual map. Mix has near-zero
training-data presence — read the relevant manual page before writing Mix. The
sheet is loaded here every session:

@AGENTS.md

## What this repo is

The `mix` scripting language, REPL, and login shell — a pure-Rust ARexx-flavoured interpreter with first-class Bus keywords (`send`, `address`, `emit`, `on … end`; legacy `done` still parses for `on`). `reply(...)` is a special-cased call available inside `on` handlers; `noded_register(...)`, `subscribe(...)`, `unsubscribe(...)`, `bus_reconnect()` are builtins on top of the keyword layer. Three workspace members:

- `cosmix-lib-mix` — the interpreter library (lexer, parser, evaluator, builtins, value model).
- `cosmix-mix` — the `mix` binary (REPL, shell layer, Bus wiring, `--serve` supervised citizen runtime).
- `mix-bench` — autoresearch / micro-benchmark harness for the interpreter.

## Four-repo split

Part of the Cosmix four-repo constellation (extracted 2026-05-29). One-way dependency order — **bus ← mix ← cos** (and bus ← cos directly); the private **cosmix** hub orchestrates all three and is depended on by none.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **bus** | `$COSMIX/` | public · markc/bus | Bus protocol family — `cosmix-lib-bus`, `cosmix-lib-client`, `cosmix-lib-props-core`, `cosmix-lib-log`, `cosmix-lib-buildinfo` (5 crates). Depends on nothing. |
| **mix** | `$COSMIX/` | public · markc/mix | Mix language — `cosmix-lib-mix` + `cosmix-mix` + `mix-bench` (3 crates). Depends on bus. |
| **cos** | `$COSMIX/` | public · markc/cos | Substrate libraries + daemon family (27 crates). Depends on bus + mix. |
| **cosmix** | `$COSMIX/` | private · markc/cosmix | Orchestration hub: docs, specs, journals, mesh-private operational state, deploy scripts. No code; drives the three public repos. |

**→ This repo is `mix`** — the scripting language + shell; needs bus present to build, never depends on cos.

## Layout

```
$COSMIX/src/
├── Cargo.toml                          workspace (3 members)
└── crates/
    ├── cosmix-lib-mix/                 interpreter library
    ├── cosmix-mix/                     mix binary
    └── mix-bench/                      benchmark harness
```

## Build / test / lint

Mix depends on the sibling [bus](https://github.com/markc/bus) repo at `$COSMIX/` for the five Bus-family path-deps. Both must be present:

```sh
git clone https://github.com/markc/bus $COSMIX           # if not already present
cd $COSMIX/src
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The zero-warning baseline is enforced.

**On the workstation build host, wrap every `cargo build`/`cargo test` in `memguard`** (on PATH;
`~/.mc/_bin/memguard.mix`) — it runs the command in a systemd user scope capped at
`MemoryMax=48G` so a runaway parallel build OOMs its own scope instead of the desktop.
An unguarded overnight release build triggered a kernel OOM storm on 2026-07-07 that
killed thunderbird/plasmashell/the claude session. If a memguard-wrapped build dies
with exit 137/143 (cgroup OOM), retry with fewer jobs: `memguard cargo build --release -j8`.

For release builds:

```sh
cd $COSMIX/src && memguard cargo build --release          # plain `cargo build --release` off-box
sudo install -o 0 -g 0 -m 0755 target/release/mix /opt/cosmix/bin/mix   # root:root 0755
```

## Release / version-bump discipline

`cosmix-lib-mix` and `cosmix-mix` carry the same `version` in lockstep — the `mix` binary the user installs is one semver unit:

1. Bump both `Cargo.toml` `version` fields together.
2. Commit: `chore: bump mix to X.Y.Z`.
3. `cargo build --release`.
4. Verify `./target/release/mix --version` matches.

Don't ship release builds without bumping both.

## Where to put new functionality

- **New language syntax (keywords, operators, statements)** → `cosmix-lib-mix/src/lexer.rs` + `parser.rs` + `evaluator.rs`. Always add unit tests covering parse + eval.
- **New builtin function** (`foo()`, `bar()`) → `cosmix-lib-mix/src/builtins.rs`. Prefer extending Mix over writing a wrapper script in the calling project — if a builtin is missing, it's a Mix bug worth fixing in the interpreter.
- **New REPL command or shell-mode feature** → `cosmix-mix/src/main.rs` + `repl.rs`. Keep Bus wiring isolated to `bus.rs` and `serve_runtime.rs`.
- **New Bus-runtime behaviour (`--serve` mode)** → `cosmix-mix/src/serve_runtime.rs`. Touches the supervised reconnect / re-register path; needs the `MixBusHandler` lazy-probe state-machine semantics preserved.

## Feature flags on `cosmix-lib-mix`

The interpreter is feature-gated for opt-in capabilities so embedders can pull only what they need:

| Feature | Adds |
|---|---|
| `json` | `json_*` builtins + jq-style jaq evaluator |
| `regex` | `regex_match`, `regex_find`, `regex_replace`, `regex_split` builtins (plain `replace`/`split` are always present) |
| `toml` | TOML parse/serialize |
| `datetime` | `chrono`-backed date/time |
| `url` | URL parsing |
| `crypto` | `blake3` / `sha2` / `base64` / `uuid` |
| `http` | `http_get` / `http_post` via `ureq` |
| `sqlite` | embedded sqlite via `rusqlite` |
| `dkim` | DKIM keypair + DNS TXT helper |
| `tokio-sleep` | alias-only (tokio itself is now unconditional core) |

The `mix` binary turns all on.

## Auto-upgrade story (don't break this)

A standalone `mix` install runs as a scripting shell. When `cosmix-noded` (from [cos](https://github.com/markc/cos)) is installed later on the same host, the **same `mix` binary** becomes mesh-viable on next invocation — no recompile. A runtime lazy-probe state machine in `MixBusHandler` makes the bare-vs-mesh distinction at execution time. Bus forms degrade gracefully on bare hosts (silent `nil` for `send`/`emit`; explicit `mesh unavailable` for the bare-incoherent ones like `noded_register` / `subscribe` / `reply`).

When touching `bus.rs` / `serve_runtime.rs`: preserve the `Unprobed` → `NeverPresent | Connected(handle)` → `Lost` semantics. Collapsing states, removing the lazy-probe boundary, or making `send` fail loudly on bare hosts breaks the auto-upgrade contract.

## Cross-repo dep direction

- mix → bus (one-way, via path-dep on the five bus crates).
- mix has **no** build dependency on cos. (cos depends back on `cosmix-lib-mix`, but that's a cos-side requirement — cos needs mix present, not the other way around.)
- Both `$COSMIX/` and `$COSMIX/` must be cloned for mix to build; `$COSMIX/` is not needed.

## License

MIT.


## Graduated Skills (auto-generated)

### mix-man-online-local-resolution

**When:** When changing Mix manual resolution, offline fallback, caching, or `mix man` environment controls.

**Approach:** For `mix man` online-primary resolution in `cosmix-mix/src/meta.rs`: use ureq 2 with https_only(true) and redirects(2) for safe resolution; set separate connect/read/overall deadlines (noting DNS is uninterruptible and only TCP handshake respects timeout_connect); run blocking ureq calls on a detached worker thread with caller-side recv_timeout to unblock the main event loop; in long-lived REPL contexts, enforce a one-in-flight/outstanding-worker guard (permit only one resolver at a time per source) or use a killable helper process to prevent accumulation of stuck resolver threads on repeated timeouts; validate HTTP 200 plus bounded UTF-8 body that is non-empty and not an HTML error page; use a 24-hour XDG-aware cache with same-directory atomic writes (create-new temp, sync, rename), skipping entries with mtime >60s in the future; namespace non-canonical base URLs by stable URL fingerprint; resolve auto as fresh-cache -> online (detached worker) -> local-checkout candidates -> stale-cache, and local as checkout-only -> any-cache (no HTTP); prioritize explicit COSMIX_SRC/mix over defaults ($COSMIX/mix) in candidate list and remove non-adjacent duplicates to prevent shadowing. Enforce https_only to prevent HTTPS-to-HTTP downgrade. Keep URL/source/topic/cache lookups pure with unit tests covering online/offline/cache transitions. Document resolver contract (resolution order, timeouts, cache semantics, worker bounds, thread accumulation guard) in mix/cli.md alongside implementation.

**Watch out for:**
- Do not use the stale $COSMIX_SRC/_man layout; published pages are under $COSMIX_SRC/mix.
- A shared cache namespace lets COSMIX_MAN_URL development content poison canonical pages; isolate overrides by stable URL fingerprint.
- Relative XDG_CACHE_HOME values are invalid and should fall back to ~/.cache.
- ureq returns HTTP status errors via call(), but explicitly require status 200 to preserve the non-200 fallback contract.
- DNS resolution is uninterruptible by ureq timeouts; timeout_connect only covers TCP handshake, not DNS lookup, so 2s is not a strict wall-clock cap for network operations.
- Treating cached files with future mtimes as fresh can pin stale cache content; validate mtime is not >60s in the future or skip the entry.
- HTTP 200 with empty or HTML-formatted error bodies (soft errors from slow servers/proxies) are cached as valid; validate response body is non-empty valid UTF-8 content, not an error page.
- ureq allows HTTPS-to-HTTP redirects by default; enforce https_only(true) and configure redirects(2) to prevent protocol downgrade and limit chains.
- Detached worker threads for blocking ureq calls require caller-side recv_timeout; inner connect/read/overall bounds remain but only TCP handshake respects timeout_connect.
- The resolver contract (resolution order, timeouts, cache semantics, worker blocking bounds) must be documented in mix/cli.md alongside code; deviations cause confusion.
- man_dir_candidates orders $COSMIX/mix before COSMIX_SRC/mix, shadowing explicit checkouts; prioritize COSMIX_SRC/mix and remove non-adjacent duplicates.
- In long-lived REPLs, repeated timed-out `mix man` calls accumulate stuck detached resolver threads even when recv_timeout protects the caller, because the thread itself remains alive waiting on network operations; mitigate with a one-in-flight guard (only one resolver per source permitted) or a killable helper process instead of native threads. One-shot CLI invocations exit and do not exhibit the accumulation problem.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._
