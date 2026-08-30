# hof — higher-order functions

The `hof` builtin category — twelve functions that take a **function value** as an
argument and run it over a list: `map`, `filter`, `reduce`, `sort_by`, `any`, `all`,
`count`, `min_by`, `max_by`, `sum_by`, `group_by`, `unique_by`. They are the idiom for
transforming and querying lists in Mix — reach for one of these instead of a
hand-rolled `for each … push … end` loop whenever the body produces a value.

List them live with `mix builtins hof`; one-line help for any single name with
`mix what NAME`.

```text
$ mix builtins hof
hof builtins:
  sort_by         Return list sorted ascending by key function (stable) (v0.2.0)
  filter          Return new list of items where predicate returns truthy (v0.2.0)
  map             Return new list of transform(item) results (v0.2.0)
  reduce          Fold list left with an explicit init: reduce($xs, $init, function($a, $b) = ...) (v0.2.0)
  any             Short-circuit: true if any item matches predicate (v0.2.0)
  all             Short-circuit: true if every item matches predicate (v0.2.0)
  count           Count items where predicate returns truthy (v0.2.0)
  min_by          Return the ITEM (not the key) with minimum key-function value (v0.2.0)
  max_by          Return the ITEM with maximum key-function value (v0.2.0)
  sum_by          Sum of key(item) across all items, returns number (v0.2.0)
  group_by        Map of stringified-key → list of items (first-seen key order) (v0.2.0)
  unique_by       Dedup list by key function, first occurrence wins (v0.2.0)
```

## The lambda argument — how to pass a function

Every HOF takes a list and a [function value](functions.md). The function is *called
per element*; Mix runs it on each item and collects/folds the results. There are three
ways to supply that function, and the rules matter — getting them wrong is the most
common HOF mistake.

### 1. Terse inline lambda `fn($x) = expr` — the everyday form

A single-expression lambda needs no `return` and no `end`. This is what you reach for
99% of the time:

```mix
$xs = [1, 2, 3, 4]
print(map($xs, fn($x) = $x * $x))
```

```text
[1, 4, 9, 16]
```

`fn` is just a short alias for `function` (see [functions](functions.md)); `fn($x) = expr`
and `function($x) return expr end` are the same thing.

A **branch fits in a terse lambda** — the ternary `cond ? a : b` and the
`if … then … else … end` *expression* (see [control-flow](control-flow.md)) are both
value-producing, so no block body is needed for a simple conditional:

```mix
print(map([-1, 0, 5], fn($n) = $n > 0 ? "pos" : "nonpos"))
print(map([-1, 0, 5], fn($n) = if $n > 0 then "pos" else "nonpos" end))
```

```text
[nonpos, nonpos, pos]
[nonpos, nonpos, pos]
```

### 2. A named function CANNOT be passed by bare name

A bareword is parsed as a **string**, not a function reference — so `map($xs, triple)`
fails:

```mix
function triple($x) return $x * 3 end
print(map([1, 2, 3], triple))
```

```text
Runtime error at line 2: map: expected function, got string
```

Wrap it in a lambda (which *is* an expression that evaluates to a function value) —
inline is fine, since the wrapper is a single expression:

```mix
function triple($x) return $x * 3 end
print(map([1, 2, 3], fn($x) = triple($x)))
```

```text
[3, 6, 9]
```

### 3. A multi-statement lambda must be VAR-BOUND first

An open call-paren breaks statement separation, so a lambda with a body of more than one
statement does **not** parse inline inside `map(...)`:

```mix
print(map([1, 2], function($x)
  $y = $x * 2
  return $y
end))
```

```text
Parse error at line 3:3: expected end of statement, got Return
```

Bind it to a variable, then pass the variable:

```mix
$probe = function($x)
  $y = $x * $x
  return $y + 1
end
print(map([1, 2, 3], $probe))
```

```text
[2, 5, 10]
```

A simple branch does **not** need this — the ternary and the `if`-expression fit in a
terse `fn($x) = …` body (form 1 above). Var-binding is for bodies that genuinely need
several *statements*: sequencing intermediate assignments, a loop, `try/catch`, or a
`send` followed by reading `$rc`. A block body needs an explicit `return` — it does
not implicitly return its last value (a `function($x) $x * $x end` body returns `nil`;
see [functions](functions.md)).

### Lambdas are closures

A lambda captures the scope where it is defined, so it can read outer variables:

```mix
$factor = 10
print(map([1, 2, 3], fn($x) = $x * $factor))
```

```text
[10, 20, 30]
```

(A closure can *read* captured state but a `$x = …` write inside binds a function-local
and does not propagate out — thread state via `return`. See
[functions](functions.md) for the full by-value / return-to-escape rules.)

## map — transform every element

`map(list, fn)` returns a **new list** of `fn(item)` for each item, same length, same
order. It does not mutate the input.

```mix
print(map([1, 2, 3, 4], fn($x) = $x * $x))
print(map(["a", "b", "c"], fn($s) = $s .. "!"))
print(map([], fn($x) = $x))
```

```text
[1, 4, 9, 16]
[a!, b!, c!]
[]
```

Pull one field out of a list of maps:

```mix
$nodes = [{name: "alpha", up: true}, {name: "beta", up: false}]
print(map($nodes, fn($n) = $n["name"]))
```

```text
[alpha, beta]
```

## filter — keep the elements a predicate accepts

`filter(list, pred)` returns a new list of the items for which `pred(item)` is **truthy**
(non-nil, non-false, non-zero, non-empty). Order is preserved.

```mix
print(filter([1, 2, 3, 4, 5, 6], fn($x) = $x > 3))
```

```text
[4, 5, 6]
```

Filter maps, then map to a field — the standard "names of the down nodes" one-liner:

```mix
$nodes = [{name: "alpha", up: true}, {name: "beta", up: false}, {name: "gamma", up: true}]
$down = filter($nodes, fn($n) = not $n["up"])
print(map($down, fn($n) = $n["name"]))
```

```text
[beta]
```

## reduce — fold a list to a single value

`reduce(list, init, fn($acc, $item))` folds **left**: it starts the accumulator at the
explicit `init`, then for each item computes `$acc = fn($acc, $item)`. Note the argument
order — list, **then init**, then the combiner — and the combiner takes
`(accumulator, item)` in that order.

```mix
print(reduce([1, 2, 3, 4], 0, fn($a, $b) = $a + $b))
print(reduce([1, 2, 3, 4, 5], 1, fn($a, $b) = $a * $b))
print(reduce(["a", "b", "c"], "", fn($acc, $x) = $acc .. $x))
```

```text
10
120
abc
```

`init` is mandatory — there is no two-argument `reduce`:

```mix
print(reduce([1, 2, 3], fn($a, $b) = $a + $b))
```

```text
Runtime error at line 1: reduce: expects 3 args, got 2
```

Folding an **empty list** returns `init` unchanged (`reduce([], 42, …)` → `42`).

For a plain numeric total you usually want `sum_by` (below). Reach for `reduce` when the
accumulator type differs from the element type (building a string, a map, a running max).

## sort_by — stable sort by a key function

`sort_by(list, key_fn)` returns a new list sorted **ascending** by `key_fn(item)`. The
key is computed once per element (cached), and the sort is **stable** — items with equal
keys keep their original relative order.

```mix
print(sort_by([3, 1, 2], fn($x) = $x))
print(sort_by(["banana", "apple", "cherry"], fn($s) = $s))
print(sort_by(["ccc", "a", "bb"], fn($s) = length($s)))
```

```text
[1, 2, 3]
[apple, banana, cherry]
[a, bb, ccc]
```

Sorting a list of maps by a field is the workhorse use:

```mix
$rows = [{name: "b", port: 80}, {name: "a", port: 443}, {name: "c", port: 22}]
$sorted = sort_by($rows, fn($r) = $r["port"])
for each $r in $sorted
  print($r["name"] .. " " .. $r["port"])
end
```

```text
c 22
b 80
a 443
```

Key comparison: numbers compare numerically, strings lexicographically by codepoint,
bools `false` before `true`, `nil` sorts before everything; a genuinely mixed set of keys
falls back to string comparison so the sort stays total (it never panics). To sort
**descending**, negate a numeric key (`fn($r) = -$r["port"]`) or `reverse()` the result.

## any / all / count — predicate queries

`any(list, pred)` short-circuits to `true` on the first match; `all(list, pred)` short-circuits
to `false` on the first miss; `count(list, pred)` returns how many items match. All three
test for **truthiness**.

```mix
print(any([1, 2, 3], fn($x) = $x > 2))
print(all([2, 4, 6], fn($x) = $x > 1))
print(count([1, 2, 3, 4, 5], fn($x) = $x > 2))
```

```text
true
true
3
```

Empty-list conventions follow standard logic — `any([])` is `false` (no match exists),
`all([])` is `true` (vacuously, no counterexample):

```mix
print(any([], fn($x) = true))
print(all([], fn($x) = false))
```

```text
false
true
```

`count` with any truthy test — here, non-empty strings:

```mix
print(count(["", "x", "", "y"], fn($s) = not is_empty($s)))
```

```text
2
```

## min_by / max_by — the extreme ITEM

`min_by(list, key_fn)` and `max_by(list, key_fn)` return the **item** (not the key) whose
`key_fn` value is smallest / largest. This is the point of the `_by` suffix — you get the
whole record back, no second lookup needed.

```mix
$rows = [{n: "b", p: 80}, {n: "a", p: 443}, {n: "c", p: 22}]
$lo = min_by($rows, fn($r) = $r["p"])
$hi = max_by($rows, fn($r) = $r["p"])
print($lo["n"] .. " " .. $lo["p"])
print($hi["n"] .. " " .. $hi["p"])
```

```text
c 22
a 443
```

On a **tie**, the first item with the extreme key wins — both `min_by` and `max_by`
replace the running best only on a strictly smaller/larger key.

An empty list returns `nil`:

```mix
print(min_by([], fn($x) = $x))
```

```text
nil
```

## sum_by — numeric total of a key

`sum_by(list, key_fn)` adds up `key_fn(item)` across the list and returns a **number**.
It is the right tool for "total of a field" — clearer than a `reduce`.

```mix
$rows = [{p: 80}, {p: 443}, {p: 22}]
print(sum_by($rows, fn($r) = $r["p"]))
```

```text
545
```

The key function must return something numeric — a non-number key **raises**:

```mix
print(sum_by([{n: "x"}], fn($r) = $r["n"]))
```

```text
Runtime error at line 1: sum_by: key function returned non-number: string
```

"Numeric" follows Mix's usual coercion: a numeric **string** (`"5"`) and a bool
(`true` = 1) sum fine — `sum_by(["5", "6"], fn($x) = $x)` is `11`. Coercion is strict:
`"inf"` / `"nan"` / `"1e999"` are **not** numeric strings and raise. An empty list
sums to `0`.

## group_by — bucket by a key

`group_by(list, key_fn)` returns a **map** of `key → list of items`. The key is
**stringified** (it becomes a map key), and the buckets are in **first-seen key order**
(backed by an insertion-ordered map), so iterating the result is stable.

```mix
$people = [{name: "al", team: "red"}, {name: "bo", team: "blue"}, {name: "cy", team: "red"}]
print(group_by($people, fn($p) = $p["team"]))
```

```text
{red: [{name: al, team: red}, {name: cy, team: red}], blue: [{name: bo, team: blue}]}
```

Because the key is stringified, a numeric key works too:

```mix
print(group_by([1, 2, 3, 4, 5, 6], fn($x) = $x % 2))
```

```text
{1: [1, 3, 5], 0: [2, 4, 6]}
```

For a labelled even/odd grouping, branch with a ternary right in the terse lambda:

```mix
print(group_by([1, 2, 3, 4, 5, 6], fn($x) = ($x % 2) == 0 ? "even" : "odd"))
```

```text
{odd: [1, 3, 5], even: [2, 4, 6]}
```

An empty list groups to the empty map `{}`.

## unique_by — dedup, first occurrence wins

`unique_by(list, key_fn)` removes later items whose stringified `key_fn` value has already
been seen. The **first** occurrence of each key is kept, in original order.

```mix
print(unique_by([1, 2, 2, 3, 3, 3, 1], fn($x) = $x))
```

```text
[1, 2, 3]
```

Dedup a list of maps by a field (e.g. unique by email, keeping the first row seen —
row 3 duplicates row 1's email and is dropped):

```mix
$rows = [{id: 1, email: "a@example.com"}, {id: 2, email: "b@example.com"}, {id: 3, email: "a@example.com"}]
$uniq = unique_by($rows, fn($r) = $r["email"])
print(map($uniq, fn($r) = $r["id"]))
```

```text
[1, 2]
```

## Composing — chain them like a pipeline

HOFs return plain lists / numbers / maps, so they chain by binding each stage to a
variable. There is no pipe operator — the variable *is* the pipe.

```mix
$xs = [1, 2, 3, 4, 5, 6]
$doubled = map($xs, fn($x) = $x * 2)
$big = filter($doubled, fn($x) = $x > 5)
$total = reduce($big, 0, fn($a, $b) = $a + $b)
print($total)
```

```text
36
```

Nest a HOF inside a var-bound lambda to operate on a list of lists:

```mix
$grid = [[1, 2], [3, 4]]
$dbl_row = function($row)
  return map($row, fn($x) = $x * 2)
end
print(map($grid, $dbl_row))
```

```text
[[2, 4], [6, 8]]
```

## Notes & gotchas

- **`map`/`filter`/`sort_by`/`unique_by`/`group_by` return new collections;** they never mutate the input list. (`reduce`/`sum_by`/`count`/`any`/`all` return a scalar; `min_by`/`max_by` return an item.)
- **First argument must be a list** — passing a non-list raises (`map: expected list, got …`). To HOF over a **map**, go through `keys($m)` / `values($m)` (see [collections](collections.md)). The function argument must be a function value (`expected function, got string` is the bareword-name mistake above). Arity is strict too — a surplus argument raises (`map: expects 2 args, got 3`).
- **`reduce` takes three args** — list, **init**, combiner — and the combiner is `fn($acc, $item)` in that order. Easy to misremember as two-arg or `(item, acc)`.
- **`group_by` / `unique_by` keys are stringified.** Two keys that print the same collapse into one bucket — fine for strings/numbers/bools, but the number `1` and the string `"1"` are the SAME key: `group_by([1, "1"], fn($x) = $x)` is `{1: [1, 1]}`.
- **Truthiness** drives `filter`/`any`/`all`/`count`: a predicate returning `nil`, `false`, `0`, `""`, or `[]` counts as a miss; anything else is a hit.
- **An error inside the lambda aborts the HOF and propagates** — a `map` over a list where one element makes the body raise (say, a division by zero) produces no partial result. Catch it with `try/catch` (see [errors](errors.md)).
- Prefer a HOF over a `for each … push … end` loop for any value-producing body — it is the Mix idiom and avoids the [`push` returns nil / list-by-value](functions.md) loop footguns. Use a bare `for each` only for pure side-effects (`print`, `send`).
- These twelve live in a separate evaluator-aware registry (they call back into Mix code), but to a script they are ordinary builtins — same call syntax, same `mix what` help.

## See also

- [functions](functions.md) — `fn`/`function`, lambdas, closures, the by-value / return-to-escape rules HOF lambdas obey
- [control-flow](control-flow.md) — the ternary `?:` and `if`-expression that let a branch live in a terse lambda
- [collections](collections.md) — lists, maps, indexing, `push`/`length`/`join`, `keys`/`values`
- [math](math.md) — numeric builtins (`min`/`max`/`floor`/`round`) that pair with the `_by` family
- [strings](strings.md) — string ops for `map`/`filter` over text
- [errors](errors.md) — `try/catch` for an error raised inside a HOF lambda
- [builtins index](builtins.md) — every builtin by category

```text
mix builtins hof      list every HOF with its one-line description
mix what NAME         one-line description of a single builtin (e.g. mix what reduce)
mix help              the full categorized builtin reference
```

