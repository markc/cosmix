# Lua-to-Mix Migration

**Date:** 2026-03-30
**Status:** Complete

## What Changed

The cosmix scripting layer replaced Lua (via the `mlua` crate, which wraps C-based Lua 5.4) with Mix, a pure-Rust scripting language purpose-built for the cosmix ecosystem. Mix lives at `github.com/markc/mix`.

## Why Mix

- **AMP IPC as language primitives.** Mix has `send`, `address`, and `emit` built into the language, making AMP messaging first-class rather than bolted on through a Lua FFI bridge.
- **Pure Rust, no C dependencies.** Eliminates `lua5.4-dev` build dependency and `lua54` feature flags. Builds cleanly on any Rust target without a C toolchain.
- **WASM compilation.** Mix compiles to WebAssembly, enabling scripts to run in the browser-based DCS shell and on remote mesh nodes.
- **Purpose-built for cosmix.** Mix is designed around the AMP protocol and cosmix runtime model, not adapted from a general-purpose embedded language.

## What Was Removed

- **`cosmix-portd` crate** -- the Lua-based port/service daemon, fully retired.
- **`mlua` dependency** and all `lua54` feature flags across the workspace.
- **`lua5.4-dev`** system build dependency (was required for mlua's C bindings).

## What Replaced It

| Crate | Role |
|-------|------|
| `cosmix-lib-script` | Mix runtime bridge -- script discovery, TOML definitions, variable substitution, menu generation, and hub connectivity via `cosmix_client` |
| `cosmix-scripts` | Mix + Bash script manager GUI -- list, run, edit, delete scripts |

## Path Dependencies

Both crates depend on `mix-core` as a path dependency:

```toml
# cosmix-lib-script (optional, behind "mix" feature, on by default)
mix-core = { path = "../../../../.mix/mix/crates/mix-core", features = ["json"] }

# cosmix-scripts (required)
mix-core = { path = "../../../../.mix/mix/crates/mix-core", features = ["json"] }
```

From `crates/<crate>/` the path traverses up four levels to `~/` then into `.mix/mix/crates/mix-core`.

## Historical Documents

All files in `_doc/` and `_journal/` dated before 2026-03-28 that reference Lua scripting, `mlua`, `cosmix-portd`, or Lua-based IPC are historical context only. Mix supersedes all prior Lua integration work.

## Name Etymology

With this migration, "cosmix" gains a second reading: **CachyOS + Mix shell** -- the CachyOS desktop powered by the Mix scripting language and DCS shell environment.
