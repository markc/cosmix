---
title: Mix Language Reference
chapter: 4
version: 0.3.0
status: draft
date: 2026-04-25
---

# Mix Language Reference

Mix is a scripting language designed for systems work — filesystem operations,
process management, HTTP, structured data handling — with first-class ABP mesh
messaging when running in a cosmix environment. In non-cosmix environments, ABP
operations (`send`, `emit`, `address`, `on`) raise a clear runtime error; the
rest of the language works identically. The `amp_available()` builtin lets
scripts detect the environment and branch when needed.

The language is defined by its reference implementation in `cosmix-lib-mix`.
There is one interpreter, shippable standalone as `mix` or compiled into cosmix
binaries. This chapter documents what that implementation actually accepts, not
an abstract specification. When this document and the source disagree, the
source wins.

**Source of truth:** `$COSMIX/src/crates/cosmix-lib-mix/src/{lexer,parser,ast,evaluator}.rs`.
Doc claims cite `file.rs:line` for any non-obvious behaviour.

**Scope:** This chapter covers the Mix language itself — syntax, operators,
control flow, scoping, and patterns. The standard library (~160 builtins as of
mix 0.15.6 — ~150 in the `builtins.rs` registry plus ~12 HOF helpers) is
documented inline where relevant but a
comprehensive builtin reference is planned as Chapter 04a. (The exact count
churns — the `builtins.rs` registry + `mix builtins` are canonical; this
spec deliberately does not pin a number.) ABP-specific
keywords are documented in §ABP below; everything else is available in any
environment.

Quick reference for writing Mix scripts correctly on the first try. Read this
before writing new Mix code — especially if you're coming from ARexx, Lua, or
shell, because Mix differs from all three in ways that are easy to get wrong.

> **v0.2.3 shipped — v0.2.x series complete.** All six phases of the
> stdlib-and-ergonomics plan have landed: anonymous functions + HOF
> list helpers (Phases 1–2), `fmt`/`printf` (Phase 3), recursive
> `glob`/`walk`/`path_parts` (Phase 4), terminator unification — `end`
> everywhere (Phase 5), and JSONL helpers `read_lines`/`read_json`/
> `read_jsonl` (Phase 6). Deferred to v0.3: destructuring assignment,
> lexical closures converging named-and-anonymous semantics, catchable
> `exit`, two-registry HOF unification.

## Quick gotcha list

Top-of-doc shortlist. If you only read this section, you'll avoid the common traps:

1. **Concatenation is `..` (not `||`)**. `||` is the statement-chain operator (run-if-failure), same as shell. String concat in expressions uses `..`.
2. **Command-line args are positional: `$1`, `$2`, ...** (ARexx/shell style). There is **no upper bound** — `$1` through `$N` for as many args as the script was invoked with. `args()` returns the same args as a list (use it when you need `length()` or iteration).
3. **`on` blocks use `on event.name` (unquoted) and close with `end`**. Example: `on dbview.page ... end`. `on` is statement-position only — it registers a global handler and is not legal inside `function` bodies.
4. **`send`/`emit`/`address` are ordinary statements** — they work at script top level, inside `on` handlers, **and inside `function` bodies**. Reuse via library helpers (e.g. `apps/lib/ui.mix`) is a style choice, not a requirement. (Earlier doc revisions claimed a top-level restriction; that was never enforced by the parser or evaluator. See evaluator.rs:1074–1088 and call_function at 1644.)
5. **String interpolation does NOT recurse**. If `$var = "hello"` and `$template = "${var}"`, then `"${template}"` produces the literal string `${var}`, not `hello`. This matters when passing complex strings through heredocs.
6. **Heredoc interpolation expands `${var}` eagerly** — one pass only. `${X}` (in both double-strings and heredocs) walks **scope → process env → `nil`**; bare `$X` outside `${...}` stays literal by design.
7. **`$var.field` dot access in strings walks the full chain**: `"host: ${cfg.host}"` and `"timeout: ${cfg.db.opts.timeout}"` both resolve identically to their expression-position counterparts (`$cfg.host`, `$cfg.db.opts.timeout`). Each step honours the `Value::Map` `*` fallback. Mid-chain non-map values yield `nil`. (Shipped in Phase 0 of v0.2.0; earlier v0.1.0 revisions only honoured the first dot.)
8. **Comparison operators have asymmetric coercion.** `==`/`!=` cross-type-coerce **string↔number only** via parse-as-f64 (value.rs:221–238), so `"5" == 5` is `true` but `false == 0` is **`false`** (Bool↔Number is not coerced). `<`/`>`/`<=`/`>=` route through `num_cmp`: if **both** operands parse as numbers (numbers or numeric strings) they compare **numerically**; if **both** are strings they compare **lexicographically by codepoint** (mix 0.15.6+, so `"apple" < "banana"` is `true` and `$ch >= "a" and $ch <= "z"` works); otherwise (a non-both-numeric string vs a number, or `nil`) it **errors** — so `nil > -1` is still a runtime error. `eq`/`ne` are strict string comparisons with no coercion.
9. **Keywords `and`/`or` for expressions, `&&`/`||` for statement chaining**. They are NOT interchangeable.
10. **All blocks close with `end`** (since v0.2.2). Legacy `done`/`next` still parse with deprecation warnings — removal deferred (still legacy-with-deprecation as of mix 0.15.6). See table below.
11. **Event fields live in `$event["headers"]`**, not top-level `$event`. `$event` has only three keys: `command`, `headers`, `body`.
12. **`Value::Map` field access falls back to the `*` key.** `$cfg.host` returns the map's `"*"` value if `"host"` isn't set (evaluator.rs:1547). This is a deliberate defaults pattern — set `$cfg.* = "default"` and unknown lookups return that. Surprising the first time you see it.
13. **`""`, `"0"`, `0`, empty list/map, and `nil` are all falsy** (`is_truthy`, value.rs:25–35). The canonical "missing config / blank value" guard is `if not $x then ... end`. Use `if is_empty($x) then ... end` when `"0"` should count as *present* (it ignores the `"0" => false` rule). Don't reach for `if $x == ""` — it misses `nil`. Truthiness rules are documented in full in `_doc/mix/overview.md` §3.1.

## Block terminators

**v0.2.2:** every block closes with `end`. The legacy `done` / `next`
forms for `while` / `loop` / `for` / `for each` / `on` still parse
but emit a deprecation warning to stderr. (Removal was originally
targeted at v0.3 but deferred — they still parse as of mix 0.15.6.)

| Block | Terminator |
|---|---|
| `if...then...end` | `end` |
| `for...to` | `end` |
| `for each...in` | `end` |
| `while` | `end` |
| `loop` | `end` |
| `function` | `end` |
| `try...catch` | `end` |
| `select...when` | `end` |
| `address` | `end` |
| `on` | `end` |

Migration: new scripts should use `end` universally. Existing scripts
will keep working but should be migrated when next touched. A Mix
script that does the line-level rewrite is in the tree:

```bash
mix _bin/mix_migrate_terminators.mix path/to/script.mix [...]
```

The migration script only rewrites *bare* `done` / `next` lines
(trimmed content equals the keyword) — it won't touch keywords in
strings or mid-line comments. Always run under version control so
you can diff the result.

## Lexical elements

### Variables

- `$name` — identifier starting with `$`, then `[a-zA-Z_][a-zA-Z0-9_]*`
- `$1`..`$9` — positional command-line arguments. Return `nil` when not provided.
- `$_` — result of pipe LHS in pipe expressions
- `$event` — only available inside `on` handlers, read-only, map with `command`/`headers`/`body` keys
- `$rc` — return code from most recent ABP `send` statement

### String literals

**Single-quoted** `'...'` — **no interpolation**. Escapes: `\'` and `\\` only.

```mix
$literal = 'This is ${not} interpolated'
```

**Double-quoted** `"..."` — interpolation enabled.

```mix
$greeting = "Hello, ${name}!"
$cmd_output = "Today is $(date +%Y-%m-%d)"
```

Escapes: `\n \t \r \e \" \\ \$` (`\$` produces literal `$`, preventing interpolation).

**Heredoc** `<<TAG ... TAG` — multiline, interpolation enabled.

```mix
$body = <<MD
# Title

Content line with ${variable} interpolated.
MD
```

Closing tag must be on its own line, exact match. Trailing newline is removed automatically.

### Comments

Both `#` and `--` run to end of line. No block comments.

### Statement separators (v0.31.0)

Executable Mix statements are separated by a physical newline or `;`.
Semicolons are emitted even inside `()`/`[]`/`{}` and are accepted only where
the nested grammar contains a real statement boundary (for example an
expression-position `if` or function body); ordinary call arguments, lists,
maps, and parenthesized expressions reject them. Leading, trailing, and repeated
semicolons are empty statements. Comments continue to own the rest of their
physical line. Strict-data does not accept `;`: its separators remain newline
and comma. Shell-dispatch lines retain shell command-list semantics, and the
classifier never mixes shell and Mix segments within one line.

`;` is the loosest executable-Mix boundary: `&&`/`||` bind before it. A Mix
chain operand can be a pipeline, but after `|` the parser captures raw external
command text through newline, `;`, or EOF, so any later `&&`/`||` belongs to
that external tail. Unlike newline, which permits a Mix `&&`/`||` chain to
continue across the physical line boundary, `;` is hard: `a; && b` and
`a &&; b` are parse errors.

### Keywords (case-sensitive)

Control: `if` `then` `else` `end` `for` `each` `in` `to` `step` `next` `while` `done` `loop` `break` `continue`  
Functions: `function` `return`  
Case: `select` `when` `otherwise`  
Logic: `and` `or` `not` `true` `false` `nil`  
Parse: `parse` `with`  
ABP: `send` `address` `emit` `on`  
Errors: `try` `catch` `die`  
Env: `export` `alias` `source` `sh`  
I/O: `print` `eprint`  

## Operators

Precedence low to high (parser.rs:908-947):

| Prec | Op | Name | Notes |
|---|---|---|---|
| 1 | `or` | Logical OR | Short-circuit |
| 2 | `and` | Logical AND | Short-circuit |
| 3 | `==` `!=` `<` `>` `<=` `>=` | Comparison | Cross-type coerce |
| 3 | `eq` `ne` | String comparison | No coercion |
| 4 | `??` | Nil coalesce | `x ?? y` — y if x is nil |
| 5 | `..` | **Concatenation** | String concat, both sides coerced |
| 6 | `+` `-` | Add / subtract | `+` also concats if either side isn't numeric |
| 7 | `*` `/` `%` | Multiply / divide / modulo | Numeric only |
| 8 | `**` | Power | Right-associative |

Unary: `-` (negation), `not` or `!` (logical NOT).

**Statement-level chain operators** (NOT expression operators):

- `stmt && other` — run `other` only if `$rc == 0` after `stmt`
- `stmt || other` — run `other` only if `$rc != 0` after `stmt`
- `stmt | external_cmd` — pipe `stmt` stdout to external command

**These are the operators that caused dbview bugs**: `||` is run-if-failure (like shell), not concat. Use `..` for string concatenation.

## Statements

### Assignment

```mix
$var = expression
$obj.field = value
$arr[0] = value
$obj.* = value      -- sets all fields of a map
```

### Conditionals

```mix
if condition then
    body
else if other_cond then
    body
else
    body
end
```

```mix
select $value
when "a" then
    body
when "b" then
    body
otherwise
    default_body
end
```

### Loops

```mix
for $i = 1 to 10 step 2
    print $i
next
```

```mix
for each $item in $list
    print $item
next

for each $i, $item in $list   -- with index
    print $i, $item
next
```

```mix
while $condition
    body
done

loop
    body
    break if $done
done
```

`break [label] [if cond]` and `continue [label] [if cond]` support optional conditions and loop labels.

### Functions

```mix
function name($arg1, $arg2)
    body
    return $value
end
```

Expression form:

```mix
function double($x) = $x * 2
```

Parameters can have defaults: `function greet($name, $greeting = "Hello")`.

#### Anonymous functions (v0.2.0)

`function` is also valid in expression position with no name, producing
a first-class `function` value that can be stored in a variable,
passed as an argument, or returned from another function. Both body
forms work:

```mix
$double = function($x) = $x * 2
print $double(5)                        -- 10

$inc = function($x)
    return $x + 1
end
print $inc(4)                           -- 5
```

`type($double)` returns `"function"`. Functions compare as never-equal
(identity comparison at the Mix level is deliberately disabled —
it's rarely useful and tends to encourage bugs).

Calling a function value uses the same `$var(args)` syntax as
calling a named function, but dispatched through the value in scope
rather than a bareword function name:

```mix
-- $sort_by is the function VALUE in a variable (whatever that means
-- for your script — maybe built by a factory). Dispatches via scope.
$sort_by($items, $key_fn)
```

Bareword calls like `sort_by($items, $key_fn)` still dispatch through
the builtin / user-function tables as before.

#### Closure semantics (v0.2.0)

Two-tier rule:

**Top-level lambdas** (constructed at script top level) see globals
**live** at call time. Mutating a global after capture is visible to
the lambda:

```mix
$t = 5
$gt = function($x) = $x > $t
$t = 100
print $gt(50)                           -- false (uses current $t, not 5)
```

**Inner-frame lambdas** (constructed inside a named function) see a
**frozen snapshot** of the enclosing function's frame. This is
capture-by-value, taken at the moment the `function(...)` expression
evaluates. It's how closures over a caller's parameters work:

```mix
function make_adder($n)
    return function($x) = $x + $n    -- $n captured by value
end

$plus40 = make_adder(40)
print $plus40(2)                        -- 42 (even though make_adder has returned)
```

The divergence between "top-level = live" and "inner-frame = frozen"
is acknowledged and narrower than the v0.1.0 state (where inner
lambdas saw nothing useful at all). v0.3 will converge both named
and anonymous functions on a single rule; until then, inner-frame
lambdas are capture-by-value and cannot observe later mutations to
the captured frame.

Named functions continue to see globals live and cannot capture
enclosing function locals — the `captures` slot is only populated
for anonymous `function(...)` expressions.

### Event handlers (`on`)

```mix
on noded.ping
    print "received:", $event["body"]
done

on dbview.page
    $page = to_number($event["page"])
    -- handle page change
done

-- Class C handler: yields at every `send` so concurrent callers
-- interleave through a slow downstream `send` (SPEC 18 §3.7 Phase 2).
on slow.echo async
    $body = json_parse($event["body"])
    $r = send "slowsvc" slow.sleep ms=$body["ms"]
    reply rc=0 body=$r["body"]
done
```

- **No quotes around the event name.** Dotted identifiers work: `dbview.page`, `noded.ping`.
- **Closes with `done`, not `end`.**
- `$event` is a map with exactly three keys:
  - `$event["command"]` → the full command string (e.g. `"dbview.sort"`)
  - `$event["headers"]` → map of ABP headers. **All user-defined event fields live here** (e.g. `$event["headers"]["column"]`)
  - `$event["body"]` → raw body string (if any)
- **Event fields are in `headers`, not top-level.** A common mistake: `$event["column"]` is `nil`. The correct access is `$event["headers"]["column"]`, or destructure first: `$h = $event["headers"]` then `$h["column"]`.
- **Default handlers are Class S (sequential).** Without the `async`
  modifier, handlers run to completion before the next event is
  dispatched. A `send` inside a Class S handler blocks the dispatch
  loop until the reply arrives; concurrent callers serialise behind
  it (head-of-line blocking). This preserves the run-to-completion
  atomicity every pre-Phase-2 Mix script (`sysmon.mix`, the CMM
  scheduler scripts) was authored under.
- **`async` modifier (SPEC 18 §3.7 Phase 2, since mix 0.2.x).** A
  handler declared `on <cmd> async ... done` is Class C: it yields
  the dispatch reader at every `send`, `reply`, and `sleep_ms` await
  point, letting another invocation of the same citizen (typically a
  different caller's event chain) interleave through. The handler is
  never re-entered while it is suspended — a Class C invocation's
  *frame* is its own; the yield is at the scheduler boundary, not
  inside the handler's local state. Synchronous request cycles
  (A→B→A across two single-threaded citizens) remain prohibited *by
  design* (SPEC 18 §3.7 deadlock corollary; the deadlock is silent —
  the broker does not detect cycles).
  - `async` is a *contextual* identifier at handler-header position,
    not a global reserved word — existing scripts that use `async` as
    a variable or function name (`$async = 1`, `async()`) are
    unaffected.
  - Place after the optional filter clauses if present (Ch05 §7.3
    inline `on ui.event from "X" action "Y" async` shape).
  - **When to use:** pick `async` when a handler issues a `send` to
    a downstream that may be slow AND the citizen is registered
    (request-addressable, structurally concurrently-callable). Leave
    it off for pure-local-state handlers, fast handlers, or
    unregistered transient ABP clients (cron/oneshot/CLI invocations
    — they have no concurrent callers to head-of-line-block; see
    SPEC 18 §3.7 sole-caller carve-out).
- **Per-`send` `timeout=<sec>` kwarg (SPEC 18 Phase 2 WS4).** Any
  `send` may carry a `timeout=<sec>` kwarg (fractional seconds
  permitted). On timeout the `send` writes the ARexx-shaped result
  vars and returns `nil` to its expression position:
  - `$rc = "-1"` (string — same convention as other transport
    failures; not a new `rc="timeout"` namespace)
  - `$result = "timeout: send to <target> exceeded <sec>s"`
  Cancellation is cooperative: the citizen does *not* abort the
  in-flight downstream request, it just stops waiting. The
  pending-reply slot is freed immediately so the caller's frame
  doesn't leak — a late downstream reply arrives at the broker
  with no matching correlation id and is dropped. `timeout=` is
  scoped to a single `send`; there is no handler-wide timeout.
  ```mix
  on slow.echo.timed async
      send "slowsvc" slow.sleep ms="2000" timeout=0.5
      if to_number($rc) != 0 then
          reply(to_number($rc), "timed out: " .. to_string($result))
      else
          reply(0, $result)
      end
  done
  ```

### ABP messaging (`send`, `emit`, `address`)

```mix
-- Statement form
send "target" command.name arg1 arg2 key=value

-- Expression form: assign result
$result = send "noded" noded.ping

-- Fire-and-forget (no $rc)
emit "display" ui.window id="main" body=$content

-- Multiple sends to same target
address "display"
    ui.status target="main" body="Ready"
    ui.data target="list" body=$json
end
```

`send`/`emit`/`address` are ordinary statements and work in any statement position, including inside `function` bodies. Wrapping them in helpers is a style choice for reuse, not a workaround for a parser restriction:

```mix
-- This is fine — define and call a helper directly.
function window($id, $title, $body)
    emit "display" ui.window id=$id title=$title body=$body
end

window("main", "My Window", $body)
```

Library files in `apps/lib/` exist to share these helpers across scripts via `source`, not because top-level is required.

Two real constraints to remember:

- **`on` IS statement-position only** — it registers a global event handler and is not legal inside a `function` body. Define handlers at script top level.
- **Closures do not capture enclosing function frames** (v0.1.0). A function defined inside another function sees globals + its own params only — not the outer function's locals. v0.2.0 will likely add capture-by-value at construction time for inner-frame lambdas; named functions may keep live-binding lookup. Plan tracks this.

### Error handling

```mix
try
    $result = risky_op()
catch $err
    print "failed:", $err
end

die "fatal: " .. $reason
```

### Parse statement

```mix
parse "host:port" with $host ":" $port
```

Captures `$host = "host"` and `$port = "port"` using literal delimiters.

### Source, sh, export, alias

```mix
source "path/to/lib.mix"              -- run another mix file in current scope
sh "ls -la /tmp"                       -- statement form, inherits stdio
$output = sh "ls -la /tmp"             -- expression form, captures stdout
export PATH = "/opt/cosmix/bin:${PATH}"
alias ll = "ls -la"
```

## Expressions

### Literals

- Numbers: `42`, `3.14`, `1_000_000` (underscores as separators), always f64
- Strings: `'single'`, `"double"`, `<<HEREDOC...HEREDOC`
- Booleans: `true`, `false`
- Nil: `nil`
- Lists: `[1, 2, 3]`, `["a", "b"]` — trailing comma allowed
- Maps: `{host: "localhost", port: 8080}` — keys are bare identifiers or strings, colon separator

### Function calls

```mix
length($list)
sqlexec($db, $sql, [$id])
$list.length()          -- method form, desugars to length($list)
func()[0]               -- postfix chaining
```

### Field / index access

```mix
$config.host
$config["host"]
$list[0]
$list[$i + 1]
$list[-1]                -- negative indices count from the end (v0.2.0)
$s[-1]                   -- same for strings (unicode-safe, returns a char)
```

Out-of-bounds index returns `nil` silently — index expressions never
raise. Use `length($xs)` if you need to check bounds explicitly.

### Send / sh in expression position

```mix
$info = send "noded" noded.ping
$files = sh "ls /tmp"
```

### Command substitution in strings

```mix
"today is $(date +%F)"
$result = $(whoami)
```

Balanced parens are tracked, so `$(echo (foo))` works.

## String interpolation details

**Variable expansion** in double-quoted strings and heredocs:

- `${name}` — variable lookup: **scope first, then process env, then literal `nil`**. The three-step chain is the same for both the sync and async evaluator arms (evaluator.rs `Expr::InterpolatedString` consumers) and fires for *every* head shape, not just uppercase env-var-shaped names. The `nil` sentinel is preserved when neither scope nor env has the name — keeping the loud typo signal intact for the genuine-missing case. Heredoc `${X}` walks the same path because the lexer's heredoc producer (`lex_heredoc`) emits the same `Token::InterpString` consumed by both evaluator arms.
- `${cfg.host}` — single-level dot access for maps. The head (`cfg`) walks the same scope → env → nil chain as a bare `${cfg}`; the dotted suffix then walks `Value::Map` fields. When env-fallback resolves the head to a `String`, the next dot step has no map to descend into and yields `nil` for the rest of the chain (same "non-map intermediate → nil" rule that applies inside scope-bound maps).
- `${cfg.db.opts.timeout}` — chained dot access; walks the full path. Each step looks the field up against the current `Value::Map`, falling back to the `*` key if present. A non-map intermediate value yields `nil` for the rest of the chain. Resolves identically to expression-position `$cfg.db.opts.timeout`.
- `$(cmd)` — command substitution
- `\$` — literal `$` (prevents interpolation)
- **Bare `$X` outside `${...}` is literal** by design — `"$5"`, `"$cwd"`, `"$path"` all print verbatim. The env-fallback chain applies *only* to the `${...}` form, so existing scripts that print awk-style `$5` or shell-doc-style `$cwd` are unaffected.

**No recursive expansion.** This is the trap:

```mix
$var = "hello"
$template = "${var} world"
$result = "${template}"      -- prints "${var} world", not "hello world"
```

Interpolation happens once at expression evaluation time. Once a string is stored in a variable, its content is literal text — even if that text contains `${...}`, it won't expand when re-interpolated.

**Practical implication:** when building markdown bodies with JSON inside, prefer building the full string with `..` concatenation over nesting variables in heredocs. The heredoc form works *if* all variables are simple scalars, but it's easy to forget one case and produce malformed output.

## Truthiness

- **Falsy:** `nil`, `false`, `""`, `0`, `"0"`, empty list `[]`, empty map `{}`
- **Truthy:** everything else

## Command-line arguments

```mix
-- Positional (ARexx/shell style)
$cmd = $1
$arg = $2
if $1 == nil then
    print "usage: script <cmd>"
    exit(1)
end

-- Or as a list
$argv = args()
print length($argv), "arguments"
```

`args()` skips the binary name and script path — it returns just the user-provided args.

**No cap on positional indexes.** `$1` through `$N` work for any `N` the script was invoked with. The lexer accepts `$<digits>` (lexer.rs:595–619) and `main.rs:64` binds every script arg as `(i+1).to_string()`. Out-of-range positions return `nil`, not an error (evaluator.rs:1192–1195).

## Common patterns

### Building markdown for a widget with JSON content

Use `..` concatenation when the value contains `${}`-like characters (JSON):

```mix
$col_defs = '[{"key":"name","label":"Name"}]'
$dt = "~~~datatable id=data page_size=" .. $page_size .. " total_rows=" .. $total
$body = "# " .. $title .. "\n\n" .. $dt .. "\ncolumns: " .. $col_defs .. "\n~~~"
```

### Loading a library

```mix
-- Resolve absolute path so scripts work from any CWD
$_cos = env("COSMIX_SRC")
if $_cos == "" then
    $_cos = env("HOME") .. "/.cosmix"
end
source "${_cos}/apps/lib/ui.mix"
```

### Event loop keep-alive

```mix
-- At end of a GUI script, sleep forever; on handlers still fire
sleep(86400)
```

### Functional list helpers (v0.2.0)

All 12 higher-order helpers take a function value as their last
argument. Lambdas are the idiomatic source:

```mix
-- Transform
$doubled = map($xs, function($x) = $x * 2)
$odds = filter($xs, function($x) = $x % 2 == 1)
$sum = reduce($xs, 0, function($acc, $x) = $acc + $x)

-- Sort. Key function returns the sort key (number or string).
-- Descending: negate a numeric key, or build a reversed string.
$by_score_desc = sort_by($users, function($u) = 0 - $u.score)

-- Top-N: chain sort_by with slicing
$top10 = take(sort_by($users, function($u) = 0 - $u.score), 10)

-- Folds
print any($xs, function($x) = $x > 100)
print all($xs, function($x) = $x > 0)
print count($xs, function($x) = $x % 2 == 0)

-- _by family returns the ITEM, not the key
$worst = min_by($users, function($u) = $u.score)
$total = sum_by($users, function($u) = $u.score)

-- Grouping / dedup
$by_role = group_by($users, function($u) = $u.role)
$by_email = unique_by($users, function($u) = $u.email)
```

Pure list helpers (no function argument):

```mix
slice($xs, 1, 4)         -- sublist [1, 4) — end exclusive
slice($xs, -3, nil)      -- last three — nil end means "to end"
take($xs, 5)             -- first 5 (or last 5 with take($xs, -5))
drop($xs, 5)             -- skip first 5 (or drop last 5 with drop($xs, -5))
zip($keys, $vals)        -- list of [k, v] pairs
```

`slice`, `take`, `drop` all clamp on out-of-range boundaries rather
than erroring — `slice($xs, -100, 100)` on a 3-element list returns
the whole list. `slice` also works on strings (unicode-safe).

### Reading structured files (v0.2.3)

Three helpers avoid the `read_file` + parse pattern scripts were
writing by hand:

```mix
-- Line-oriented text: trailing newline stripped, empty trailing
-- line dropped so callers don't filter it out.
$lines = read_lines("/var/log/auth.log")
print length($lines), "lines"

-- Single-record JSON file → Mix value directly
$cfg = read_json("/etc/cosmix/config.json")
print $cfg.listen

-- JSON-lines (jsonl): one record per line → list
-- Strict by default: a single malformed line aborts the read.
$events = read_jsonl("/srv/mail/msg/alice/.spamlite-stats.jsonl")
print length($events)

-- Lenient mode: silently skip malformed lines. Use when log rotation
-- can leave a truncated tail and you'd rather lose one record than
-- fail the whole aggregation.
$clean = read_jsonl("/srv/rotating/events.jsonl", {skip_errors: true})
```

Strict-by-default for `read_jsonl` is deliberate: the canonical use
case is aggregating stats across dozens of files, and silently
dropping bad lines would make aggregate numbers unreliable without
telling the script author. Lenient mode is the explicit escape
hatch for known-noisy inputs.

### Filesystem ergonomics (v0.2.1)

Recursive `glob`, `walk`, and `path_parts`:

```mix
-- Single-component (unchanged from v0.1.0)
glob("*.txt")                           -- files in CWD
glob("/var/log/*.gz")                   -- one wildcard component

-- Multi-component and recursive (v0.2.1)
glob("/srv/*/msg/*/.spamlite-stats.jsonl")   -- * in any component
glob("src/**/*.rs")                           -- ** = zero-or-more dirs

-- Enumerate every file under a tree
walk("/srv/mail")                       -- files only, sorted
walk("/srv/mail", {max_depth: 2})       -- cap depth (0 = direct children)
walk("/srv/mail", {include_dirs: true}) -- add dirs to the list
walk("/srv/mail", {follow_symlinks: true})  -- with loop protection

-- Decompose a path
path_parts("/home/user/report.tar.gz")
-- → {dir: "/home/user", base: "report.tar.gz", stem: "report.tar", ext: "gz"}
```

**`glob` rules:**
- `*` and `?` work in any path component, not just the last.
- `**` matches zero or more directory levels (think shell globstar).
- Leading `/` → absolute pattern; otherwise relative to CWD.
- Results are sorted lexicographically and `./foo` normalizes to `foo`.
- Nonexistent intermediate components silently return an empty list.

**`walk` rules:**
- Returns a flat list of paths, sorted lexicographically.
- Unreadable subdirectories are skipped silently — a single bad file
  in a deep tree doesn't abort the walk. The top-level `$dir` errors
  only if it doesn't exist.
- `max_depth: 0` means "direct children only, don't descend." `None`
  (omit the key) means unlimited.
- Symlink loop protection only activates with `follow_symlinks: true`;
  uses `(dev, inode)` tracking on Unix.

**`path_parts` rules:**
- Pure, no filesystem access. Works on nonexistent paths.
- `ext` is WITHOUT the leading dot. `extname` keeps the dot —
  they're different consumers (`ext` for comparison, `extname` for
  reconstruction).
- `stem` is everything before the *last* dot: `path_parts("foo.tar.gz").stem`
  returns `"foo.tar"`, not `"foo"`.

### Formatted output (v0.2.0)

Three functions share the same minimal format grammar:

```mix
fmt("hello %s", "world")         -- returns "hello world"
printf("count=%d\n", 42)         -- writes to stdout, returns nil
eprintf("error: %s\n", $msg)     -- writes to stderr, returns nil
```

| Spec     | Meaning                                   |
|----------|-------------------------------------------|
| `%s`     | any value via `to_mix_string`             |
| `%d`     | integer (truncates floats)                |
| `%f`     | float, default 6 decimals                 |
| `%.Nf`   | float, N decimals                         |
| `%Nd`    | integer, min-width N (right-aligned)      |
| `%Ns`    | string, min-width N (right-aligned)       |
| `%-Ns`   | string, min-width N (left-aligned)        |
| `%%`     | literal `%`                               |

No `{}`-style templates — Mix already has `${...}` interpolation;
a second template syntax would confuse scripts. No `%x`/`%o`/`%e`/`%g`
— add them if a real script needs them.

Unknown specifiers and too-few-args errors raise `RuntimeError`
rather than substituting silently, so typos surface loudly. `printf`
does **not** add a trailing newline — include `\n` explicitly, like C.

The canonical tabular-output pattern:

```mix
for each $u in $users
    printf("%-12s %5d %5d\n", $u.name, $u.fn, $u.fp)
next
```

`..` concatenation already coerces every value through `to_mix_string`
(so `"count=" .. 5` works), which means `printf` is sugar for
readability rather than an escape from a broken behaviour. Prefer
`printf` for tabular output where alignment matters; prefer `${}`
interpolation or `..` concat for simple string building.

## Differences from ARexx / Lua / shell

### From ARexx

- **Concat is `..`, not `||`.** `||` is the run-if-failure chain operator.
- Variables need `$` prefix. Bare identifiers are function names or keywords.
- Strings don't auto-concatenate: `"a" "b"` is a parse error; use `"a" .. "b"`.

### From Lua

- Maps use `{key: value}` with colon (not `=`).
- No metatables, no `local` scope (all vars in current function frame).
- `end` closes every block (since v0.2.2). Legacy `done`/`next` still parse with deprecation warnings; removal deferred (still legacy-with-deprecation as of mix 0.15.6).

### From shell

- `$var` in expressions is OK; `${var}` only works inside strings.
- No glob expansion: `*.txt` is literal. Use `glob("*.txt")`.
- No word splitting: `"a b c"` is one string, not three. Use `split(x, " ")`.
- `&&`/`||`/`|` are statement-level only.
- Errors don't abort the script — use `try/catch` or `die`.

## Mix Outside Cosmix

Mix works as a standalone shell and scripting language without the cosmix stack.
The interpreter detects at startup whether an ABP broker (cosmix-noded) is
available (via `COSMIX_NODED` env var or local socket probe). The detection
result is stored in the interpreter context and exposed via `amp_available()`.

### Environment detection

```mix
if amp_available() then
    send "maild" "account.list"
else
    print "No ABP broker — reading from file"
    $accounts = read_file("/tmp/accounts.json")
end
```

### ABP keyword behaviour without a broker

| Keyword | Behaviour | Error message |
|---------|-----------|---------------|
| `send` | Runtime error | `send: no ABP broker available` |
| `emit` | Runtime error | `emit: no ABP broker available` |
| `address` | Runtime error | `address: no ABP broker available` |
| `on` | Runtime error | `on: no ABP broker available` |

Scripts that don't use ABP keywords run identically in both environments. The
All builtins (filesystem, HTTP, JSON, regex, crypto, process management) are
available without a broker.

### The ARexx precedent

ARexx was the Amiga's scripting language but also worked as a general-purpose
language — standalone REXX scripts that did nothing Amiga-specific ran fine.
The ARexx port system was the standout feature, but the language existed in
both contexts. Mix follows the same model: a useful shell on its own, with
mesh messaging as a capability that activates when a broker is present.

### Distribution

The `mix` binary is a single statically-linked Rust executable. It can be
distributed independently of the cosmix stack. The ABP wire-format library
(`cosmix-lib-bus`) is linked but dormant without a broker connection. Binary size
overhead for the ABP code is minimal (~1000 lines of Rust).

## Known sharp edges

1. **Heredoc + JSON / `${...}` content**: if the value contains literal `${...}` sequences that should *not* expand, prefer explicit `..` concat. Heredocs interpolate eagerly and there's no opt-out per-segment.
2. **Comparison asymmetry**: `==` cross-type-coerces (so `"5" == 5` is `true`), but `<`/`>` error on coerce failure (so `nil > -1` raises a runtime error rather than returning `false`). This is value.rs:79 vs evaluator.rs:2022 — two code paths, two rules. If you might compare `nil`, guard with `??` or an explicit `if $x == nil`.
3. **Mid-chain non-map in interpolation yields nil**: `${cfg.host.foo}` returns nil if `$cfg.host` is a string, not an error. Same rule as expression-position field access. The chain walks as far as it can on `Value::Map`, then short-circuits to `nil`.
4. **`Value::Map` falls back to `*`**: `$cfg.unknown_key` returns the value of `$cfg.*` if set. Convenient as a defaults pattern, surprising if you forgot you set `*`.
5. **`exit(N)` is not catchable**: `builtin_exit` (builtins.rs:745) calls `std::process::exit` directly. It bypasses `try/catch`, skips Rust-side cleanup, and kills the host process if Mix is embedded. Use `die "msg"` (catchable, raises `MixError::DieError`) when you want intercept-able termination. v0.3 will convert `exit` to a catchable error variant.
6. **Named functions vs anonymous lambdas capture differently (v0.2.0)**: **Named** `function foo(...)` definitions still see globals + their own params only — they do **not** capture an enclosing function's locals. **Anonymous** `function(...)` expressions built inside a named function capture that frame by value (frozen snapshot). Top-level `$f = function(...)` sees globals live — no capture needed. The divergence is deliberate for v0.2.0 and is acknowledged in the plan's Q2. v0.3 will converge both forms on a single lexical-closure rule.
7. **`args()` vs `$N`**: both work, but `$1` is idiomatic for command scripts. `args()` is better when you need `length()` or iteration.

## See also

- `$COSMIX/src/crates/cosmix-lib-mix/src/parser.rs` — authoritative grammar
- `$COSMIX/src/crates/cosmix-lib-mix/src/builtins.rs` — all builtins (future Ch 04a)
- `apps/sshm.mix` — complete example of a Mix GUI app
- `apps/lib/ui.mix` — library of `panel`/`data`/`status` helpers
- `mix/dbview` — SQLite viewer using the datatable features
- `_doc/2026-03-29-mix-language-plan.md` — original design doc (historical)
- `_doc/2026-04-11-mix-structured-pipelines.md` — planned pipeline syntax (historical)
