# mix lint — semantic analysis

`mix --check` is a syntax check: it lexes and parses, nothing more. It happily
passes a script that reads an undefined variable, calls a function that exists
nowhere, or hands `substr` one argument. `mix lint` (0.29.0) is the semantic
layer: it builds the scope universe, resolves every call against the builtin
contract metadata and the file's own definitions, and reports machine-readable
diagnostics with **stable codes** — designed for an agent to consume without
parsing prose, and biased hard toward zero false positives on Mix's dynamic
seams.

```text
mix lint [--json | --data] [--deny-warnings]
         [--allow-global NAME]... [--allow-function NAME]...
         FILE...
```

- Exit codes: `0` no errors (warnings allowed unless `--deny-warnings`); `1` one or more errors, or any warning under `--deny-warnings`; `2` invalid usage, unreadable input, or internal failure. **Notes never affect the exit code** — a file whose only findings are notes exits `0` even under `--deny-warnings` (0.63.0; pinned by CLI tests, because `--deny-warnings` is a live fleet deploy gate).
- Diagnostics go to **stdout**; CLI errors go to **stderr**. `--deny-warnings` changes only the exit decision, never a severity.
- `-` reads stdin (at most once); relative `require()` paths from stdin resolve against the current directory.
- `--allow-global NAME` / `--allow-function NAME` declare names an embedder or environment provides (repeatable).
- If script parsing fails, lint tries the same strict-data parser as `load_data()`.
  A successful fallback exits cleanly and prints `validated as strict data (not
  as a script)`. If both parsers fail, an explicit top-level `key:` shape (or a
  conventional strict-data suffix as a tiebreak) gets the data error;
  otherwise the original script error is preserved.

## The rules (v1)

Codes are permanent — never reused, never repurposed — from three
namespaces: `MIX-E1xxx` errors, `MIX-W2xxx` warnings (born warnings), and
`MIX-D3xxx` **deprecations and release-transition advisories** (0.63.0) —
a severity-*independent* namespace: a deprecation starts at severity
`note` and may later be promoted to `warning` with the **code unchanged**
(only the wire `severity` field moves), so tooling that suppresses or
greps by code keeps working across the promotion.

### Notes (severity `note`, 0.63.0)

Rendered with a `note:` prefix, sorted after errors and warnings, counted
in their own summary field, and **never** gating. Current D-codes:

| code | what it flags |
|---|---|
| `MIX-D3001`–`D3005` | the five pattern-first legacy names `regex_match regex_find regex_replace regex_split grep` — use the subject-first `re_match re_find re_replace re_split grep_lines`. **The legacy names were DELETED in release B (0.73.0)** after the fleet-wide inventory read zero, so a surviving call also gets `MIX-E1102` (undefined function) and fails at runtime; these notes stay as the pointer to the replacement |
| ~~`MIX-D3006`~~ | **RETIRED in 0.68.0.** Watch note for the map-binding flip, which has landed: a two-variable loop over a MAP now binds (key, value). Code permanently spent, never reused |
| ~~`MIX-D3007`~~ | **RETIRED in 0.68.0.** Watch note for the equality flip, which has landed: `==`/`!=` with a map or list on **both** sides now raises `TYPE_ERROR` naming `deep_eq`. The shipped rule is narrower than this note's "either operand" wording — a collection compared to a *scalar* still answers, so `$m[$k] == nil` keeps working. Code permanently spent, never reused |
| `MIX-D3012` | an **`ssh_mix` body that could not be analysed** (0.69.0) — a non-literal second argument (a variable, a concatenation, an interpolated string, a `read_file`), or a literal that does not parse as Mix. Says so explicitly rather than passing silently, because an unreadable body counted as clean is exactly how an inventory reads zero while live sites exist |
| `MIX-D3013` | a **hand-rolled padding loop** (0.74.0) — `while len($o) < $n … $o = $o .. " "` — pointing at `lpad`/`rpad` (and the display-cell `lpad_w`/`rpad_w`). Four independent sessions wrote this loop while the builtins sat in the binary; the note is the discoverability fix that reaches the author at authoring time. Narrow by design: only a `<`/`<=` comparison of `len`/`length` of the same variable the body self-appends a string literal to |
| `MIX-D3008`–`D3011` | the REXX-style `pos lastpos byte_pos byte_lastpos` family, declared legacy — with a sharper message when composed as `substr(.., pos(..))` in one expression (the 1-based/0-based off-by-one). These stay notes until their own fleet count reads zero; they are NOT deleted in release B |

Member-call spellings are covered too: a builtin-named `.name(` desugars
to the same call at parse time.

```text
MIX-E1001  lexical error                    MIX-E1301  duplicate function parameter
MIX-E1002  script parse error               MIX-E1302  duplicate function definition in one scope
MIX-E1003  strict-data parse error          MIX-W2301  `+` stringifies a proven list
MIX-E1101  undefined variable               MIX-E1401  require() target missing/unreadable
MIX-E1102  undefined function               MIX-E1402  require() target invalid Mix
MIX-E1201  builtin arity mismatch           MIX-E1501  dead mutation (write is lost)
MIX-E1202  user-function arity mismatch     MIX-E1502  discarded pure transform
                                            MIX-W2101  unreachable statement
                                            MIX-W2201  discarded must-use result
                                            MIX-W2302  used implicit-nil function result
                                            MIX-W2303  assignment operand in hand-built chain AST
                                            MIX-W2304  unknown builtin-result key
                                            MIX-W2305  -1-sentinel builtin as a truth value
                                            MIX-W2306  escaped quotes in ssh command source
                                            MIX-W2401  source/include defeats analysis
                                            MIX-W2402  bare bound variable in heredoc
```

- **MIX-E1003** means the source was recognisably intended as strict data but
  failed the literal-data grammar. It is distinct from a broken executable
  script (`MIX-E1002`). A valid data file under any filename is recognised by
  content; the suffix is only a tiebreak when neither grammar succeeds.
- **MIX-E1101** flags a `$name` read only when the name is bound **nowhere in its visible universe** — function bodies see params + their own binders + everything bound anywhere at file level (Mix has no block scoping and no read-before-assign rule, so lexical order is deliberately ignored). `${name}` interpolation is never flagged (it falls back to the process environment), nor are `$1`-style positionals or the runtime-injected `rc` / `result` / `status` / `event` / `_`.
- **MIX-E1102** resolves bareword calls against builtins, HOFs, evaluator special forms, every `function` definition in the file, the embedded prelude, `--allow-function` names, AND any assigned variable (a bareword call can dispatch to a function-valued variable). Calls inside `address ... end` blocks are sends and are never flagged; `MethodCall`/`ValueCall` are dynamic dispatch and are skipped.
- **MIX-E1201** checks calls against the structured contract metadata (`mix builtins --json`), including non-contiguous exact-arity sets — `random(1)` is an error, `random()`/`random(min, max)` are not. The contract is the documented surface; some older builtins tolerate surplus arguments at runtime, and lint is deliberately stricter (`mix --strict-arity` makes the runtime agree).
- **MIX-E1501** flags a discarded `push`/`pop`/`shift` whose first argument is **not a bare variable** — `push($m["a"], $v)`, `push($m.a, $v)`, `$m["a"].push($v)`. These builtins mutate through the variable slot, so given any other expression they append to a temporary copy and the write is **lost in silence**. It is an ERROR, not a warning: the statement does nothing while reading as though it did. The fix **differs by builtin**: `push` returns the appended list, so assign it back (`$m["a"] = push($m["a"], $v)`); `pop`/`shift` return the **removed element**, not the list, so assigning that back replaces the list with the element (data corruption) — hoist first instead (`$l = $m[$k]; $x = pop($l); $m[$k] = $l`). For maps of maps, write the [nested assignment](collections.md) directly. A by-value **parameter** is a bare variable, so that case stays with its own definition-time dead-push warning and is not double-reported.
- **MIX-E1502** flags a discarded `delete` / `merge` — both are **pure** (they return a new container and change nothing in place), so a bare call is a no-op. Assign it back: `$m = delete($m, "k")`.
- **MIX-W2201** fires when an operation whose failure signal lives in its RETURN VALUE (`effects.must_use`: `run_rc`, `run_argv`, `run_pipeline`, `run_parallel`, `ssh_run`, `ssh_exec`, `ssh_mix`, `http_*`, `kill`, `run_stream`) is a bare expression statement — the bug class where a failed remote step silently vanishes. Bind the result and branch on it; some have a fail-fast twin that raises (`run_argv`→`run_argv_must`, `run_pipeline`→`run_pipeline_must`, `ssh_run`→`ssh_must`). The last statement of a block is exempt (it may be the block's value).
- **MIX-W2301** warns that `+` coerces lists to strings; it does not append or
  concatenate list values. It fires for a list literal operand, or a variable
  proven by straight-line analysis to hold a directly assigned list literal.
  Use `concat(list_a, list_b)` or `push(list, value)`.
- **MIX-W2302** warns when the result of a uniquely defined named function is
  consumed, its block body's final statement is a bare expression, and the
  body contains no value-returning `return`. Block functions implicitly return
  `nil`; add `return`. A discarded call is quiet, as are mixed-return bodies,
  terminating final expressions, and calls whose name can be redirected
  through a variable.
- **MIX-W2303** is defence-in-depth for Rust embedders that construct the public
  AST directly and pass it to `analyze()`: it warns if any operand of a
  hand-built `StmtKind::Chain` is an assignment. Ordinary Mix source cannot
  reach this warning because the parser rejects the same shape first as
  `MIX-E1002`. The code remains reserved for this public-API path and is not
  repurposed.
- **MIX-W2304** checks a literal field/index key against the builtin's
  documented result-map fields from `mix builtins --json`. It works on a
  direct builtin call or a variable proven by straight-line assignment to
  hold that result. The hint names the closest documented key (for example,
  `exit_code` rather than `code`). Dynamic keys, generic maps and result
  shapes without declared fields are deliberately silent.
- **MIX-W2305** flags `index_of()` / `byte_index_of()` / `bytes_find()` (added
  v0.64.0) used **bare as a truth
  value**. They return `-1` for "not found" and `0` for "found at the first
  position", and Mix treats `0` as falsy and every non-zero number — `-1`
  included — as truthy. So a bare call in a condition is wrong on *both*
  branches:

  ```mix
  if index_of("abc", "z") then …   -- -1 is TRUTHY  → absent reads as present
  if index_of("abc", "a") then …   --  0 is FALSY   → found-at-0 reads as absent
  ```

  Compare explicitly (`index_of(..) >= 0`), or use `contains()` for the yes/no
  question — **except for `byte_find`/`bytes_find`**, whose bytes subject
  `contains()` rejects, so those take the `>= 0` comparison only. Their 1-based
  twins — `pos`, `lastpos`, `byte_pos`,
  `byte_lastpos` — are **safe** in the same position because their not-found
  sentinel is `0` and therefore falsy; that asymmetry is exactly what makes
  the trap easy to walk into, and why the rule exists. Fires in `if`/`elif`,
  `while`, `break if`/`continue if`, expression-position `if`, the ternary
  condition, and through `not`/`and`/`or`. Any explicit comparison is already
  correct code and stays silent.
- **MIX-W2306** flags a literal command passed to `ssh_run` or `ssh_must` when
  its source spelling contains `\"`. That escape is the high-signal mark of
  nested Mix source which the remote shell will parse again. Ship the source
  verbatim with [`ssh_mix` + a heredoc](remote.md#headline-idiom-ssh_mix--heredoc).
  Simple command strings, computed commands, `ssh_exec`, `ssh_mix`, and
  single-quoted strings containing ordinary `"` stay quiet.
- **MIX-W2401**: one `source`/`include` anywhere disables the undefined-name checks for the whole file (the loaded file can define anything) — reported once so you know analysis is degraded. Prefer `require()`: it is isolated, statically resolvable, and E1401/E1402 verify literal-path modules parse.
- **MIX-W2402** warns when a heredoc literal contains bare `$NAME` and `NAME` is bound somewhere in the same visible universe. Heredocs interpolate `${NAME}`, not `$NAME`, so the bare form often means a generated config was silently corrupted. It does not fire for `${NAME}`, `$(` command substitution, explicitly escaped `\$NAME`, all-digit names such as `$1`, unknown names, or ordinary double-quoted strings. The warning is lint-only: bare `$NAME` still evaluates to literal `$NAME`, and intentional literal output requires no change.
- **MIX-W2403** (0.74.0) warns at the *definition* of a function whose name is a builtin: the builtin wins at every call site (a builtin-named dot-call even desugars at parse time), so the definition is unreachable by name (only an extracted function value or an exports-map index still reaches it). The worst shape this produces is a script that keeps running while its own function quietly stops being called — every release that adds a builtin name arms it again for older scripts. Deliberately a warning, never an error: a compat shim written for an older mix that lacks the builtin is legitimate authoring — but on the mix doing the linting it is dead, and the author should know.

## `mix explain MIX-XXXX` — the offline diagnostics explainer

Every human-readable lint run that reports anything ends with one trailer line:

```text
explain any code with: mix explain MIX-XXXX
```

`mix explain MIX-W2305` (the `MIX-` prefix is optional, case-insensitive — `mix
explain w2305` works too) prints the code's full story — what it flags, why the
rule exists, the shape it catches, and the fix — from a registry embedded in the
binary, so an agent that hits a code it has never seen gets the whole rationale
in one call without leaving the terminal or reaching the network. An unknown but
code-shaped argument lists the known codes rather than failing blankly; a
non-code argument (`mix explain round`) falls through to the AI builtin
explainer. The registry is the same prose as this page; a build-time test
asserts every code the analyzer can emit has a record, so a new diagnostic
cannot ship without its explanation.

## Machine output

`--json` (D3 schema, `schema_version: 2` since 0.63.0 — the severity
domain gained `"note"` and `summary` gained `notes`; no in-tree consumer
parsed v1, inventoried 2026-09-03):

```json
{
  "schema_version": 2,
  "tool": "mix lint",
  "mix_version": "0.63.0",
  "files": ["worker.mix"],
  "strict_data_files": [],
  "diagnostics": [{
    "code": "MIX-E1101", "severity": "error", "file": "worker.mix",
    "line": 412, "column": null,
    "message": "undefined variable '$DOMAIN' (assigned nowhere in this file)",
    "hint": "assign it, use env(\"DOMAIN\") for environment values, or pass --allow-global DOMAIN"
  }],
  "capabilities": ["fs-read", "network", "process"],
  "summary": {"errors": 1, "warnings": 0, "notes": 0, "denied_warnings": false}
}
```

`--data` emits the same report as strict-data Mix source (parse with
`data_parse`). `line`/`column` are 1-based or `null`. Most diagnostics are
statement-level and carry a `null` column. Lexical and parse errors generally do
carry one — the assignment-chain error, for instance, points at the offending
`&&`/`||`.

`capabilities` is the inventory of capability classes the script's calls
exercise — data, not warnings.
`strict_data_files` lists inputs validated by the strict-data fallback rather
than by the script analyzer.

## Remote bodies — lint sees inside `ssh_mix` (0.69.0)

[`ssh_mix(host, source[, opts])`](remote.md) ships its **second argument** to
a remote `mix -`. That argument is Mix source, and since 0.69.0 lint treats it
as such: when it is a **string literal** it is parsed and analysed, and its
diagnostics are reported against the enclosing file with a
`[inside ssh_mix body]` prefix and the line mapped into the outer file.

```
deploy_thing.mix:283: MIX-D3001 note: [inside ssh_mix body] `regex_match` is
  pattern-first legacy: use `re_match(s, pattern)` (subject first)
```

Before this, a deploy script's entire remote half was one opaque string —
invisible to lint and, more consequentially, to every **inventory built from
lint**. That is not a hypothetical: the `MIX-D3006` inventory that gated
0.68.0's map-binding flip reported *zero* sites for `deploy_vhost.mix`,
locally and on 27/27 fleet nodes, while line 283 of that file is a
two-variable loop over a map living inside such a body.

**A body that cannot be analysed says so** — `MIX-D3012`, for a non-literal
argument (variable, concatenation, interpolation, `read_file`) or a literal
that does not parse as Mix. This is the rule that matters more than the
analysis: an unreadable body silently counted as clean is precisely how an
inventory reads zero while live sites exist.

**Name resolution is suppressed inside the body.** Its free names come from
`ssh_mix`'s `bindings` option and from the remote's own environment, neither
visible locally, so `MIX-E1101`-class findings would be pure noise — and a
linter that cries wolf about remote bodies gets switched off. The enclosing
file keeps all of its own name checks. Everything else — legacy-name notes,
arity, the truthiness trap — applies normally, and **errors from inside a
body gate exactly as they would anywhere else**.

Only `ssh_mix` carries Mix source this way. `run`/`run_argv`/`run_pipeline`
execute *shell* commands, Mix has no heredoc syntax, there is no
`source`/`include`/`eval` builtin taking a string, and `--serve` runs a script
*file* (which lint reaches directly).

## What lint deliberately does NOT do (v1)

- Straight-line facts do not cross branches, loops, function-call boundaries,
  `source`/`include`, or function frames. This deliberately misses lists and
  builtin-result maps produced indirectly, returned by helpers, assigned in a
  branch, or reached through a dynamic key. Suspicious `nil` coercion into
  paths/hostnames remains out of scope. `MIX-W2302` also stays silent for a
  mixed-return function even when one fallthrough path may still yield `nil`.
  Shell-vs-argv advice remains out of scope — use
  [`validate`](data.md) at boundaries for the nil class today.
- No reachability model for `E1102`/`E1101` beyond the universe rules above.
- Dynamic `require(expr)` paths, extension calls, and Bus dispatch are never hard errors.

## See also

- [invocation](invocation.md) — `mix --check` (syntax only), `--strict-arity`
- [errors](errors.md) — the structured errors the flagged code will raise at runtime
- [builtins](builtins.md) — the contract metadata lint checks against
