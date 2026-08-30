# Functions, lambdas & modules

Mix functions come in two shapes that look almost identical but mean different things:

- **`function name(...)`** — a *statement* that registers a named function in the current scope.
- **`function(...)` / `fn(...)`** — an *expression* that evaluates to a first-class function **value** (a lambda) you can store, pass, return, or stash in a map.

Both share one tail grammar (`($params) ... end` or `($params) = expr`), one binding model (args by value, writes function-local), and one capture rule (closures see their defining scope for *reads*). Get those four facts straight and everything else follows.

> Verified against **mix 0.21.2**. Every example below was run with `mix -c` and shows its real output. The binary is the oracle — if anything here disagrees with live behaviour, the binary wins.

## Named functions — the statement form

```mix
function add($a, $b)
  return $a + $b
end
print(add(2, 3))
```
```text
5
```

A named `function` is a **statement**: evaluating it registers `add` in scope and returns nil. The body contains newline- or `;`-separated statements closed by `end` (there is no `do`; see [control flow](control-flow.md)). Use `return` to hand a value back; a block body that never returns yields nil (more on that below).

### `fn` alias and the `= expr` short form

`fn` is a Rust-style short alias for `function` — it works everywhere `function` does, and both spellings accept both body forms (block `... end` or `= expr`, named or anonymous — all eight combinations parse). The `= expr` form is a **single-expression body**: no `return`, no `end`, the expression's value *is* the result.

```mix
fn square($x) = $x * $x
print(square(7))
```
```text
49
```

This is the form to reach for in [HOFs](#higher-order-functions): `map($xs, fn($x) = $x*2)`.

The body is one *expression* — which includes the value-producing conditionals (ternary `?:`, `if ... end` expressions, `??` — see [operators](operators.md) and [control flow](control-flow.md)), so a conditional one-liner needs no block:

```mix
$sgn = fn($x) = $x > 0 ? "pos" : "neg"
print($sgn(3))
print($sgn(-3))
```
```text
pos
neg
```

> **`fn` needs quoting as a map key.** A keyword is accepted anywhere it is unambiguously a NAME — bare map keys (`{label: 1, to: "x"}`), field access and assignment (`$m.to`, `$cfg.label = 2`). The ONE exception is `fn`: it lexes to the same token as `function`, so `{fn: 1}` and `$m.fn` are parse errors — use a quoted key (`{"fn": 1}`) and index access (`$m["fn"]`).

### Define before you call — no hoisting

A function must be defined **before the line that calls it**. There is no hoisting:

```mix
print(f())
function f()
  return 1
end
```
```text
Runtime error at line 1: undefined function 'f'
```

*Mutual* recursion is fine **as long as both are defined before either runs** — calls resolve at call-time, not def-time:

```mix
function is_even($n)
  if $n == 0 then return true end
  return is_odd($n - 1)
end
function is_odd($n)
  if $n == 0 then return false end
  return is_even($n - 1)
end
print(is_even(10))
print(is_odd(7))
```
```text
true
true
```

### Recursion is capped at 128 frames

Recursion works, but the evaluator caps nesting at **128** calls deep (127 nested recursive calls pass, 128 fail). Exceeding it is a clean, catchable runtime error — not a stack-overflow abort:

```mix
function f($n)
  if $n == 0 then return 0 end
  return f($n - 1)
end
print(f(100000))
```
```text
Runtime error at line 3: recursion depth exceeded (limit 128)
```

For deep counts, use a loop or a [HOF](hof.md) instead of recursion.

### Default parameters

A parameter can carry a default expression, used when the caller omits that argument:

```mix
function greet($name = "world")
  return "hello " .. $name
end
print(greet())
print(greet("mix"))
```
```text
hello world
hello mix
```

A missing argument with no default binds nil. Extra arguments are silently ignored (`f(1, 2, 3)` against `function f($a)` just binds `$a = 1`).

Since 0.29.0 that tolerance is a **mode**: `mix --strict-arity script.mix` (or an embedder's `ArityMode::Strict`) raises a catchable `ARITY_MISMATCH` structured error instead — before the function body runs — whenever a call's argument count falls outside `min..=max` (min = parameters without defaults). Strict mode also enforces every builtin's contract arity from the machine metadata, so `run_stream(["true"], {}, 5)`'s silently-ignored surplus argument becomes an error. (That example read `run_stream(argv, {timeout: 5})` until 0.51.0, when run_stream gained an options map and began refusing `timeout` **by name** as `OPTION_INVALID` — in every mode, not just this one.) The compatible binding stays the default; exact arity would only ever become the language default at an announced major compatibility boundary, after explicit variadic syntax lands. `mix lint` flags statically provable mismatches in either mode.

> **A keyword can't be a function name.** `function step(...)` and `fn to(...)` are parse errors (`expected identifier, got Step`) — and that includes non-obvious [reserved words](keywords.md) like `step`, `to`, `label`. Pick `phase`, `say_step`, etc. `$`-sigil *variable* and *parameter* names are unaffected — the sigil disambiguates, so `$step = 1` and `function f($to)` both work.

> **You can't shadow a builtin.** `function abs($x) return 999 end` parses fine, but calls still resolve to the builtin: `abs(-5)` is `5`, never `999`. A builtin always wins over a same-named Mix function — pick another name.

## Lambdas — the expression form (first-class values)

Drop the name and `function(...)` becomes an **expression** that evaluates to a function value. Store it, call it, pass it around:

```mix
$sq = function($x) return $x * $x end
print($sq(9))
```
```text
81
```

The terse form (`fn($x) = expr`) is a lambda too — anonymous, single-expression, no `end`:

```mix
$dbl = fn($x) = $x * 2
print($dbl(21))
```
```text
42
```

A lambda is an ordinary `Value`: put it in a list or map, return it from another function, hand it to a [builtin](builtins.md). That is the whole point — see [modules](#modules--a-map-of-lambdas) and [HOFs](#higher-order-functions). Stringified, a function value shows its parameter count: `"" .. $dbl` is `<function/1>`, a two-param lambda is `<function/2>`.

### Block bodies do not implicitly return

A `function ... end` **block** body does *not* return its last expression. If you want a value out of a block body, you must `return` it:

```mix
$f = function($x) $x * $x end
print("result: " .. ("" .. $f(5)))
```
```text
result: nil
```

The `= expr` short form is the opposite — it returns the expression directly. So `fn($x) = $x*$x` gives `25`, but `function($x) $x*$x end` gives `nil`. Reach for `= expr` whenever the body is one expression.

## The binding model — by value, local writes

Three rules govern how data flows in and out of a function. They are the most common source of surprise for anyone arriving from bash/python, so verify them against your own code.

### Arguments pass by value

Numbers, lists, and maps are **copied in**. Mutating a parameter — even `push()` on a list param — does not reach the caller:

```mix
function addit($p)
  push($p, 99)
  return length($p)
end
$l = [1, 2, 3]
print("inside len: " .. ("" .. addit($l)))
print("caller len: " .. ("" .. length($l)))
```
```text
inside len: 4
caller len: 3
```

Because this silently discards work, Mix **warns** (to stderr, once, when the
function is defined) about the provably-lost case — a `push($p, …)` statement
whose result is thrown away where `$p` is a parameter the body never reads again:

```text
mix: warning: in fat_snare() at line 2: push($drums, …) discards its result but
$drums is a by-value parameter never read again — the mutation is lost to the
caller (Mix lists pass by value). Return the list or use concat().
```

The warning is deliberately conservative (zero false positives): it stays silent
the moment `$p` is genuinely used — `return $p`, `length($p)`, `$p[i]`, capturing
`$y = push($p, …)`, `pop($p)`, or passing `$p` on — so `addit` above (which reads
`$p` via `length`) does **not** warn. It only fires when a push into a parameter
can have no observable effect at all.

> **Building a sequence across helpers** (e.g. accumulating MIDI events for a
> `.asc` score) is where this bites hardest — a helper that `push`es onto a
> list parameter silently drops every event onto the copy. Under by-value
> semantics, have the helper **return** its items and combine them at the call
> site with [`concat`](collections.md#concat) (`concat($a, $b, …)`
> joins lists into one new list, one level deep, O(total)):

```mix
function bar_events($b)
  return [$b * 10, $b * 10 + 1]   -- RETURN a list, never push a param
end
$track = []
for each $b in [0, 1, 2]
  $track = concat($track, bar_events($b))   -- combine at the caller
end
print("" .. $track)
```
```text
[0, 1, 10, 11, 20, 21]
```

### Writes inside a function are function-local

Any `$x = ...` *assignment* inside a function binds a **function-local** variable. It does not write through to an outer/global of the same name — and that includes `$rc`/`$result`: when those change after a `send`, it is the Bus runtime writing them natively, not a Mix assignment (see [Bus messaging](bus.md)); a literal `$rc = ...` inside a function is as local as any other write.

```mix
$g = 1
function w()
  $g = 99
end
w()
print($g)
```
```text
1
```

This includes the accumulator trap: a helper doing `$count = $count + 1` leaves the caller's `$count` untouched. A function cannot tick a shared global counter.

### Reads fall through (globals fallback)

A function may *read* an outer/global variable fine — lookups fall through to the global frame:

```mix
$g = 42
function r()
  return $g + 1
end
print(r())
```
```text
43
```

### The pass-in / return / reassign triad

Because writes are local and args are by value, the way to thread state *through* a function is the triad: **(1) pass the value in** (explicit read) → **(2) `return`** the new value (compute) → **(3) reassign at the call site** (write). There is no `global` keyword — this is the idiom by design.

```mix
function inc($n)
  return $n + 1
end
$count = 0
$count = inc($count)
$count = inc($count)
print($count)
```
```text
2
```

Return a **map** to update several values at once, then destructure at the call site. Net result: pure, self-contained functions — a helper can't accidentally read or stomp a caller's same-named variable.

## Closures — capture for reads

A lambda **captures its defining scope** for reads, so it carries state. Define a lambda where `$base` is in scope and it stays bound to `$base` even after the lambda escapes:

```mix
$base = 100
$add = function($x) return $x + $base end
print($add(5))
```
```text
105
```

This is what makes a **closure factory** work — each call to `adder` produces a lambda closed over that call's own `$n`:

```mix
function adder($n)
  return function($x) return $x + $n end
end
$add10 = adder(10)
$add100 = adder(100)
print($add10(5))
print($add100(5))
```
```text
15
105
```

A closure captures **only the variables its body actually references**, not the whole enclosing frame. So defining a key/predicate lambda inline inside a function that also holds a large value — a multi-thousand-element list, a whole generated score — costs nothing per call for the data it doesn't touch. That is what keeps `sort_by`/`map`/`filter` over big data `O(n log n)`/`O(n)` even when the lambda is written inside a heavy scope: `sort_by($track, fn($e) = $e["t"])` closes over `$e` alone, never the surrounding tracks.

### A closure can read captured state but not accumulate into it

The same local-write rule applies: a **write** to a captured/outer name binds a *local* and does **not** mutate the captured variable:

```mix
$c = 0
$bump = function() $c = $c + 1 end
$bump()
$bump()
print($c)
```
```text
0
```

So a closure can *read* shared state but can't *accumulate* into it. To carry a counter forward, thread it through the triad (`$n = $inc($n)`):

```mix
$inc = fn($x) = $x + 1
$n = 0
$n = $inc($n)
$n = $inc($n)
$n = $inc($n)
print($n)
```
```text
3
```

### Binders shadow, they don't clobber

Names a function *introduces* — a `for each $x` / `for $i = ...` loop var, a `catch $e`, a `parse ... with $a $b` target — bind in the function's own frame, so they **shadow** a same-named caller/global rather than overwriting it:

```mix
$x = "caller"
function loops()
  for each $x in [1, 2, 3]
    $unused = $x
  end
  return "done"
end
print(loops())
print($x)
```
```text
done
caller
```

## Higher-order functions

The payoff of first-class lambdas is the HOF builtins — no hand-rolled `for each ... push ... end` loops. Pass a lambda (inline or stored) as the transform:

```mix
$out = map([1, 2, 3], function($x) return $x * $x end)
print("" .. $out)
```
```text
[1, 4, 9]
```

```mix
$out = filter([1, 2, 3, 4, 5], fn($n) = $n > 2)
print("" .. $out)
```
```text
[3, 4, 5]
```

`reduce` takes the list, an **explicit init**, then `fn(acc, item)` — mind that order:

```mix
$sum = reduce([1, 2, 3, 4], 0, function($a, $b) return $a + $b end)
print($sum)
```
```text
10
```

The full HOF set (`mix what <name>` for each): `map` `filter` `reduce` `sort_by` `any` `all` `count` `min_by` `max_by` `sum_by` `group_by` `unique_by`. Each one is covered on the dedicated [hof](hof.md) page.

### Two HOF gotchas

**(1) You can't pass a named function by bare name.** A bareword is parsed as a string, not a function value:

```mix
function triple($x) return $x * 3 end
$out = map([1, 2, 3], triple)
print("" .. $out)
```
```text
Runtime error at line 2: map: expected function, got string
```

Wrap it in a lambda — `$f = function($x) return triple($x) end` — or define it as a lambda var in the first place.

**(2) A multi-statement lambda won't parse *inline* inside a HOF call.** The open call-paren breaks statement separation:

```mix
$out = map([1, 2, 3], function($x)
  $y = $x * 2
  return $y + 1
end)
print("" .. $out)
```
```text
Parse error at line 3:3: expected end of statement, got Return
```

Bind the lambda to a var first, then pass the var:

```mix
$f = function($x)
  $y = $x * 2
  return $y + 1
end
$out = map([1, 2, 3], $f)
print("" .. $out)
```
```text
[3, 5, 7]
```

A *single-expression* lambda (`function($x) return $x*$x end` or `fn($x) = ...`) is fine inline — only multi-statement bodies need the var-bind. A stored-var lambda also reads cleanly with `sort_by` and friends:

```mix
$ports = [{ port: 80 }, { port: 22 }, { port: 443 }]
$by_port = function($r) return $r["port"] end
$sorted = sort_by($ports, $by_port)
print("" .. $sorted)
```
```text
[{port: 22}, {port: 80}, {port: 443}]
```

## Modules — a map of lambdas

Mix has no module keyword, but a map of lambdas *is* a namespace. Functions are values, so you can group them:

```mix
$math = { square: function($x) return $x * $x end, cube: function($x) return $x * $x * $x end }
print($math["square"](5))
print($math["cube"](3))
```
```text
25
27
```

**Dot-call works — with name-first precedence** (0.27.0+). `$math.square(5)` calls the member when no builtin/HOF/free function named `square` exists; if one does, the *free* name wins and the member is skipped (the same UFCS sugar that makes `$list.map($f)` work). `$math["square"](5)` always calls the member, collisions or not — use the index form for any name that might collide:

```mix
$math = { square: function($x) return $x * $x end }
print($math.square(5))     -- 25 (member — no free 'square' exists)
print($math["square"](5))  -- 25 (index form: collision-proof)
```

Each lambda still closes over the defining scope for reads, so a module can share read-only state across its members:

```mix
$prefix = ">> "
$log = { line: function($s) return $prefix .. $s end }
print($log["line"]("hello"))
```
```text
>> hello
```

See [data structures](collections.md) for indexing details.

## Grouping functions across files — `require` vs `include` vs `source`

For libraries, split functions into their own `.mix` file and pull them in. Mix has three file-loaders with different semantics (full comparison + module semantics: [modules](modules.md)):

- **`$m = require("lib.mix")`** (0.27.0+) — evaluates the file in an **isolated scope** and returns its exports as a map (`$m.fn(...)`, `$m.var`). Cached once per file, cycle-safe, cannot clobber (or see) your scope. Use this for anything library-shaped.
- **`include "lib.mix"`** — **script-relative path**, **load-once** (re-`include` is a no-op), splices the file's fns + `$vars` INTO your scope. Use when you want the names unprefixed and own both files.
- **`source "lib.mix"`** — **CWD-relative path**, runs **every time**. Use this for re-running setup.

Given `util.mix`:

```mix
function lib_double($x)
  return $x * 2
end
print("[lib loaded]")
```

`include` runs the lib body **once** even if included twice:

```mix
include "util.mix"
include "util.mix"
print(lib_double(21))
```
```text
[lib loaded]
42
```

`source` re-runs it each time:

```mix
source "util.mix"
source "util.mix"
print(lib_double(21))
```
```text
[lib loaded]
[lib loaded]
42
```

### `include` drops names into a flat namespace

`include` injects the lib's functions straight into the caller's scope — the namespace is **flat**. A name collision across two libs **silently last-loaded-wins** (no warning). Prefix your function names, or use the module idiom to scope them.

Note `include` is **not expression-valued** — you can't capture its result:

```mix
$m = include "lib.mix"
```
```text
Parse error at line 1:6: unexpected token Include
```

So a "module library" assigns a well-known var name and the caller uses *that* after the `include`. Given `mathlib.mix`:

```mix
$mathlib = {
  add: function($a, $b) return $a + $b end,
  mul: function($a, $b) return $a * $b end
}
```

The caller:

```mix
include "mathlib.mix"
print($mathlib["add"](3, 4))
print($mathlib["mul"](3, 4))
```
```text
7
12
```

## Quick reference

| Form | Kind | Body | Returns |
|---|---|---|---|
| `function f($a) ... end` | statement (registers `f`) | block | nil unless `return` |
| `fn f($a) = expr` | statement (registers `f`) | one expression | the expression |
| `function($a) ... end` | expression (a value) | block | nil unless `return` |
| `fn($a) = expr` | expression (a value) | one expression | the expression |

`fn` ≡ `function` in every row — either keyword takes either body form, named or anonymous.

- **Sigil everywhere:** `$x`, params `$a`/`$b`. Bare `x = 1` is misparsed as a shell command.
- **Concat is `..`** (never `+`/`.`): `"hi " .. $name`.
- **Define before call** — no hoisting. Mutual recursion OK if both defined first; recursion caps at **128** frames (clean runtime error).
- **A keyword can't be a function name** (`function step(...)` errors; `$step = 1` is fine) and **a builtin can't be shadowed** (calls always resolve to the builtin).
- **Args by value; writes function-local; closures capture for reads only.** Propagate state with the pass-in/return/reassign triad — there is no `global`.
- **Modules = map of lambdas, index-called** (`$mod["fn"](args)`), not dot-called.
- **`include`** = script-relative, load-once (libs); **`source`** = CWD, every-time.
- Multi-statement lambdas must be **var-bound** before going into a HOF; named functions **can't** be passed by bare name.

## See also

- [hof](hof.md) — the twelve HOF builtins in depth (`map`/`filter`/`reduce`/`sort_by`/…)
- [strings](strings.md) — `..` concat, interpolation, the `'raw'` vs `"${...}"` split
- [control flow](control-flow.md) — `if`/`for`/`while`, `end`, why there is no `do`
- [operators](operators.md) — ternary `?:` and `??`, the expressions a `= expr` body can hold
- [maps](collections.md) / [lists](collections.md) — indexing, the data behind the module idiom
- [keywords](keywords.md) — the reserved words that can't be function names
- [builtins index](builtins.md) — everything callable out of the box
- [Bus messaging](bus.md) — `send`/`on`/`emit` keywords (where `$rc`/`$result` come from)
- The [mix repo](https://github.com/markc/mix) · [bus](https://github.com/markc/bus) · [cos](https://github.com/markc/cos)
- `mix help` — command overview · `mix what map` (and any builtin) — one-line signature + version
