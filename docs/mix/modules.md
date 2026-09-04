# modules — require, include, source

Three ways to pull another `.mix` file into a running script, on a spectrum
from **isolated** to **splice-into-my-scope**:

| | `require(path)` | `include "path"` | `source "path"` |
|---|---|---|---|
| Form | expression (builtin, returns a value) | statement | statement |
| Caller's scope | **untouched** — exports come back as a value | file's fns + `$vars` land in the caller's scope | same as include |
| Runs | once per canonical path (cached) | once per canonical path (deduped, returns nil on repeat) | every time |
| Path resolution | script-relative, then CWD | script-relative, then CWD | CWD only |
| Parse failure | hard error | hard error | may fall back to per-line shell (`.mixrc`-style) |
| Capability | `fs-read` | `fs-read` | `fs-read` |

> **Script-relative follows symlinks** (0.74.0): the entry script's path is
> canonicalised before its directory is taken, so the symlinked-launcher
> pattern (`~/.local/bin/x -> repo/_bin/x.mix`) resolves `../_lib/…`
> against the repo the script lives in. Before this, every such launcher
> silently depended on being run from the right CWD.

`require` is the module loader: it evaluates the file in a **fresh, fully
isolated scope** and hands back its exports — the caller's variables and
functions cannot be clobbered, and the module cannot see them. `include` is
the trusting opposite: a splice that shares helper functions into your scope.
`source` is shell-style config loading.

## require in one example

```mix
-- strutil.mix ------------------------------------
$version = "1.0"
$_sep = "-"                          -- '_' prefix: private, not exported
function _squash($s)                 -- private helper
  return replace($s, "  ", " ")
end
function slugify($s)
  return lower(replace(_squash($s), " ", $_sep))
end

-- main.mix ---------------------------------------
$str = require("strutil.mix")
print($str.version)                  -- 1.0
print($str.slugify("Hello  World"))  -- hello-world
$f = $str.slugify                    -- extract-then-call works too
print($f("A B"))                     -- a-b
```

## What gets exported

- By default: a **map** of every top-level function and `$var`, minus names starting with `_` (private by convention), the implicit frame names (`rc`, `result`, `status`, `event`), and prelude functions the module didn't redefine.
- A top-level `return <expr>` **replaces** auto-export entirely — the file can return a map, a single function, a number, anything (Lua-style). Only an explicit `return` does this; an incidental value in the last statement does not.
- A side-effect-only module exports `{}` — `require` doubles as "run this file exactly once".

## Module semantics

- **Cached once per canonical path.** The module body runs on the first `require`; later requires (any relative spelling, symlinks included) return a fresh copy of the cached exports without re-running the body. Mutating the returned map never affects the cache. Exception: an exported `Buffer` is reference-semantic as always — every require-hit shares it.
- **Cycles are hard errors** (`a.mix -> b.mix -> a.mix` reports the chain), as is nesting deeper than 64 requires. A require that fails (missing file, parse error, runtime error) caches nothing — fix the module and require again.
- **Exported functions keep working**: they can call the module's other functions (including `_`-privates) and read the module's top-level `$vars`. Those vars are injected **by value per call** — writes inside a call don't persist to the next call. Module-level mutable state is not a thing; return state to the caller instead.
- **Nested `require` is module-relative**: a module requiring `"sibling.mix"` resolves next to itself, transitively.
- **What a module sees**: builtins, HOFs, the prelude (iff the program loaded it), extensions, `env()`/`args()`/process state, and further `require`s. It does NOT see the caller's `$vars`, functions, `~/.mixrc`, or an enclosing `address` block. It runs under the caller's capability policy and eval limits — `require` is not a sandbox escape.
- A free name in a module function that is **not** a module top-level resolves against the *requiring program's* globals at call time, like any named function. Define what you use at the module top level.
- `alias` and `export NAME=` at module top level affect the shared program state (documented, allowed); registering an `on` handler from a module is an **error**.

## Calling exports — precedence

`$m.foo(args)` dispatches **member-first** since 0.72.0: when `$m` has a
**function-valued** member `foo`, that member is called (the flip that made
exports named `sum`/`lines` reachable via dot-call — from 0.27.0 to 0.71 a
free/prelude name won instead and the prelude ran on the stringified map).
When no such member exists, the call is the usual UFCS sugar (`$list.map($f)`,
`$s.upper()`), then an address-block send. Two collisions survive the flip:

- An export named after a **builtin** must still be called via the index form (`$m["keys"](...)`) — a builtin-named dot-call desugars at parse time, before the member can be seen.
- Serve-mode **extension** verbs outrank members by design — a citizen's map can never fake an injected `props.*` verb.

The index form `$m["foo"](...)` calls the member unconditionally and remains
the collision-proof spelling.

`$m.foo` (no call) extracts the function value; `$f = $m.foo` then `$f(x)`
always calls the member, collisions or not.

Also new with require: a **Function-valued variable is callable bareword** —
`$b = function() ... end` then `b()` — wherever no builtin/named function of
that name exists.

## When to use which

- Reach for **`require`** for anything library-shaped: namespaced helpers, shared constants, run-once setup. It is the only loader safe against name collisions in both directions.
- Reach for **`include`** when you *want* the file's functions unprefixed in your scope (a project's helper set) and you own both files.
- Reach for **`source`** for `.mixrc`-style config that may mix Mix with bareword shell lines and must re-run each time.
