# collections — lists & maps

Mix has two compound data types: the **list** (an ordered, 0-indexed sequence)
and the **map** (an ordered key→value table, insertion order preserved). Both are
first-class values — store them in variables, nest them, pass them to functions,
return them, iterate them, and serialise them with [data](data.md). Most list +
map builtins live in the `list` and `map` categories — list them live with
`mix builtins list` / `mix builtins map` — but a few multi-type ones
(`contains`, `reverse`, `length`/`len`) sit in the `string` category since they
also operate on strings, so `mix builtins list` won't show every name used here.
Get one-line help for any name with `mix what NAME`.

> One-line model: **lists are `[a, b, c]` 0-indexed (negatives count from the
> end, for reads AND writes); maps are `{key: value}` read with `.key` or
> `["key"]`. `push`/`pop`/`shift` and index-assignment mutate in place; everything
> else returns a fresh copy. Lists pass into functions by value, and
> `List == List` **raises** — use `deep_eq(a, b)`.**

## Literals & printing

```mix
$nums = [10, 20, 30]
$user = { name: "ada", age: 36 }
print("" .. $nums)
print("" .. $user)
print("" .. [])
print("" .. {})
```
```text
[10, 20, 30]
{name: ada, age: 36}
[]
{}
```

A list literal is `[`…`]` with `,`-separated elements; a map literal is `{`…`}`
with `key: value` pairs. **Map keys are bare identifiers, keyword lexemes, or
string literals** — `{ name: "ada" }` and `{ "name": "ada" }` are the same map.
Since 0.21 a [keyword](keywords.md) is accepted as a bare key wherever it is
unambiguously a name: `{ label: 1, to: "x", on: true, parse: 2 }` all parse, and
the same keywords work in dot-field reads and writes (`$m.to`, `$m.label = 2`).
The **one exception is `fn`** — it lexes to the same token as `function`, so it
needs the quoted key `{ "fn": 1 }` and bracket access `$m["fn"]` (`.fn` is a
parse error).

**Map keys are always strings.** A bare *number* is not a valid literal key —
`{ 22: "ssh" }` is a parse error; quote it (`{ "22": "ssh" }`). Indexing with a
number coerces to the key's string form, so `$m[22] = "ssh"` stores key `"22"`
and `$m[22]` / `$m["22"]` both read it back.

Elements and values are any Mix value, so nesting is free:

```mix
$cfg = { server: { host: "node1", ports: [80, 443] }, tls: true }
print("" .. $cfg)
```
```text
{server: {host: node1, ports: [80, 443]}, tls: true}
```

Note the print form is **debug-ish, not round-trippable**: strings inside a
container print *without* quotes (`[a, b]`, not `["a", "b"]`). For a parseable
representation use [`data_encode`](data.md) / `json_encode`.

## Indexing

Lists are **0-indexed**. A **negative** index counts from the end (`-1` is the
last element). An **out-of-range** index returns `nil` (never an error) — the
same three rules hold for a string (yielding a one-character string) and, since
**v0.64.0**, for `bytes`/`buffer` (yielding the byte as a **number** 0-255; see
[io](io.md#bytes-as-a-sequence-v0640)):

```mix
$l = [10, 20, 30]
print($l[0])
print($l[2])
print($l[-1])
print($l[5])
```
```text
10
30
30
nil
```

Maps are read with **dot** (`$m.key`) or **bracket** (`$m["key"]`) syntax; they
are equivalent. Use the bracket form when the key is computed or held in a
variable, or when it isn't a bare identifier. A **missing key** returns `nil`:

```mix
$m = { name: "ada", age: 36 }
print($m.name)
print($m["age"])
print($m.missing)
$k = "name"
print($m[$k])
```
```text
ada
36
nil
ada
```

### The `"*"` wildcard key

If a map has a key `"*"`, an indexing miss falls back to that value instead of
`nil`. Handy for default/catch-all tables (e.g. a port→service map — note the
quoted numeric keys, and that the numeric index `80` coerces to the string key
`"80"`):

```mix
$svc = { "22": "ssh", "80": "http", "*": "unknown" }
print($svc[80])
print($svc[12345])
```
```text
http
unknown
```

### Footgun: `pos` is 1-based, indexing is 0-based

`index_of` (and list/`substr` indexing) are **0-based**; the string-search
builtin `pos` is **1-based** and returns `0` for "not found". Don't mix them up:

```mix
$fruits = ["apple", "pear", "plum"]
print("index_of (0-based): " .. ("" .. index_of($fruits, "pear")))
print("index_of miss: " .. ("" .. index_of($fruits, "fig")))
print("pos (1-based): " .. ("" .. pos("pear", join($fruits, " "))))
```
```text
index_of (0-based): 1
index_of miss: -1
pos (1-based): 7
```

`index_of(list, value)` → 0-based index, or **-1** when absent. `pos(needle,
haystack)` is a *string* op (see [strings](strings.md)) — 1-based, **0** when
absent.

## Iterating

`for each` walks a list directly. Over a **map** with ONE variable it yields
the **keys** (in insertion order) — `for each $k in $m` and
`for each $k in keys($m)` are equivalent; look values up with `$m[$k]`. With
**two** variables it yields **(key, value)** since 0.68.0, so no lookup is
needed:

```mix
$ports = [22, 80, 443]
for each $p in $ports
  print("- port " .. ("" .. $p))
end

$m = { host: "node1", port: 8080, tls: true }
for each $k in $m
  print($k .. " = " .. ("" .. $m[$k]))
end
```
```text
- port 22
- port 80
- port 443
host = node1
port = 8080
tls = true
```

Over a **string** `for each` yields one-character strings, and since **v0.64.0**
over a `bytes`/`buffer` it yields the bytes as **numbers** 0-255 (see
[io](io.md#bytes-as-a-sequence-v0640)). Every source is materialised at loop
entry, so mutating it in the body never extends the loop.

A **second variable** binds the position for a list, string or bytes, and the
**value** for a map — see
[control-flow](control-flow.md#two-variables-over-a-map-bind-key-value--changed-in-0680)
for the 0.68.0 change and what stayed put.

For value-producing transforms prefer a [hof](hof.md) (`map`/`filter`/`reduce`/
`sort_by`) over a hand-rolled `for each` + `push`.

## Length & emptiness

`length` (alias `len`) returns the **element count** for a list and the
**key count** for a map (for a *string* it counts codepoints — see
[strings](strings.md); for a **`bytes`/`buffer`** it counts bytes, v0.64.0 —
see [io](io.md#bytes-as-a-sequence-v0640)). `is_empty` tests for zero length:

```mix
print("" .. length([10, 20, 30]))
print("" .. len({ a: 1, b: 2 }))
print("" .. is_empty([]))
print("" .. is_empty({ a: 1 }))
```
```text
3
2
true
false
```

## Mutating builtins (in place)

Three builtins **mutate the list in place**. `push` appends and **returns
`nil`**; `pop`/`shift` remove and **return the removed element** (or `nil` on an
empty list).

```mix
$l = [1, 2, 3]
push($l, 4)
print("after push: " .. ("" .. $l))
print("pop -> " .. ("" .. pop($l)))
print("shift -> " .. ("" .. shift($l)))
print("now: " .. ("" .. $l))
```
```text
after push: [1, 2, 3, 4]
pop -> 4
shift -> 1
now: [2, 3]
```

### Footgun: what `push` mutates depends on what you hand it

`push`/`pop`/`shift` reach the list **through the variable slot**. That makes
their behaviour shape-dependent, and it is the single most-tripped-over corner in
Mix:

- **First argument is a bare variable** (`push($l, 3)`) → mutates `$l` in place, and `push` **returns `nil`**. So `$l = push($l, 3)` ✗ clobbers `$l` to `nil`. Call it as a statement: `push($l, 3)` ✓.
- **First argument is anything else** — `push($m["a"], 3)`, `push($m.a, 3)` — there is no slot to mutate, so it appends to a **copy** and returns the new list. A bare `push($m["a"], 3)` therefore does **nothing at all**: the write is lost, silently. Assign the result back instead: `$m["a"] = push($m["a"], 3)` ✓.

Since 0.33.0 `mix lint` catches the dead form as **MIX-E1501**, and the same
release lets you skip the idiom entirely for maps of maps by writing the nested
assignment directly (see [Nested assignment](#nested-assignment)).

```mix
$l = [1, 2]
print("push returns: " .. ("" .. push($l, 3)))
$m = { a: [1] }
$m["a"] = push($m["a"], 2)
print("" .. $m)
```
```text
push returns: nil
{a: [1, 2]}
```

### Index-assignment

Assigning to an index mutates in place. **Maps auto-create** the key (so this is
how you build a map incrementally) — via bracket (`$m["a"] = 1`) or single-level
dot (`$m.b = 2`), which are equivalent. **List writes resolve signed indices
exactly like reads**: a negative index writes from the end (`$l[-1] = v` sets the
last element):

```mix
$m = {}
$m["a"] = 1
$m.b = 2
print("" .. $m)

$l = [10, 20, 30]
$l[1] = 99
$l[-1] = 77
print("" .. $l)
```
```text
{a: 1, b: 2}
[10, 99, 77]
```

**Out-of-range and mistyped list writes are loud errors** (since 0.20.5 — they
were previously silent no-ops, and a negative write corrupted element 0):

- an out-of-range index (either direction) raises `list index 7 out of range (length 3)` — a lost write is data loss; use `push` to grow a list;
- only a **number** indexes a list on write — `$l["a"] = v` raises `cannot index list with string`; a NaN/infinite index is rejected too;
- index-assigning into a scalar (`$x = 5` then `$x[0] = 1`) raises `cannot index-assign into number` — and since 0.33.0 the **dot form raises too** (`$x.f = 1` previously discarded the write in silence).

This is a **deliberate read/write asymmetry**: a missing *read* still returns
`nil` (safe default), a missing *write* errors (silent data loss).

### Nested assignment

Since 0.33.0 an assignment target may carry **any number of accessors**, in any
mix of `.field` and `["key"]`, over maps and lists alike:

```mix
$m = {}
$m["u"]["k"] = 1
$m.a.b.c = "deep"
$l = [[1, 2]]
$l[0][1] = 99
print("" .. $m)
print("" .. $l)
```
```text
{u: {k: 1}, a: {b: {c: deep}}}
[[1, 99]]
```

This is not sugar for the old read-mutate-write-back dance: it walks the path and
mutates **in place**, so building a map of maps is linear where the workaround was
quadratic (it deep-copied the inner container on every write).

**A missing or `nil` intermediate is auto-created as a map** — but only when the
next accessor is a name or a string key, which is the only case where the intent
is unambiguous:

- `$m["g1"]["k"] = 1` on an empty `$m` creates `$m["g1"] = {}` first ✓
- `$m["a"][0] = 1` **raises**: `[0]` could mean a list slot or the map key `"0"`, and guessing would freeze one interpretation into your data. Assign an explicit `[]` or `{}` first.

**Nothing else is auto-created, and nothing is auto-overwritten:**

- an existing scalar intermediate raises (`$m = { a: 5 }` then `$m["a"]["b"] = 1` → `cannot index-assign into number at $m[a]`) — a map is never dropped on top of your data;
- **lists never extend**, at any depth — `$m["a"][5] = 9` on a 1-element list raises, exactly as a single-level write does;
- the `"*"` wildcard is a **read** fallback only: `$m.missing.k = 1` creates `missing`, it does not mutate `$m["*"]`.

**A failed nested write creates nothing.** The path is validated before it is
touched, so a write that dies halfway leaves no half-built maps behind — which
matters because `catch` can resume after it.

## Pure list builtins (return a copy)

Every builtin below leaves its argument **unchanged** and returns a **new** list.

### sort / unique / reverse

```mix
print("" .. sort([3, 1, 20, 2]))
print("" .. unique([1, 2, 2, 3, 1]))
print("" .. reverse([1, 2, 3]))
```
```text
[1, 2, 3, 20]
[1, 2, 3]
[3, 2, 1]
```

> **`sort` orders a homogeneous number list numerically; everything else is
> lexicographic.** A list whose every element is a number sorts by value
> (`sort([2, 10, 1, 20])` → `[1, 2, 10, 20]`). A list of strings — or a *mixed*
> list — compares by the **string** form of each element instead, so
> `sort([2, "a", 1])` → `[1, 2, a]`. For any other key (a map field, a computed
> value, descending order) use [`sort_by`](hof.md):

```mix
print("numbers:  " .. ("" .. sort([2, 10, 1, 20])))
print("strings:  " .. ("" .. sort(["banana", "apple", "cherry"])))
print("by field: " .. ("" .. sort_by([{p: 80}, {p: 22}], fn($r) = $r["p"])))
```
```text
numbers:  [1, 2, 10, 20]
strings:  [apple, banana, cherry]
by field: [{p: 22}, {p: 80}]
```

`unique` removes duplicates **keeping first-seen order**. `reverse` works on a
list or a string.

### contains / index_of

```mix
$l = ["x", "y", "z"]
print("" .. contains($l, "y"))
print("" .. contains($l, "q"))
print("" .. index_of($l, "z"))
print("" .. index_of($l, "q"))
```
```text
true
false
2
-1
```

`contains(list, v)` → bool (it also accepts a *string* haystack — but **not** a
map; use `has_key` for maps). `index_of(list, v)` → 0-based position or `-1`.

### range

`range(start, end)` is **inclusive of both ends**, with an optional step. Step
direction must match the start→end direction or you get an empty list. A step of
`0`, or a range over 10M elements, errors.

Bounds and step must be **whole numbers within the i64 range** (fractional,
non-finite, or beyond ±2^63 raises `VALUE_OUT_OF_RANGE` naming the argument).
Before 0.59.0 an oversized bound silently saturated: `range(1e30, 1e30)`
answered `[9223372036854775807]` — a value the caller never wrote.

All supplied bounds and the optional step must coerce to numbers. Numeric
strings remain valid (`range("1", "3")` → `[1, 2, 3]`); an unparseable value
raises `TYPE_MISMATCH` with its argument position instead of silently becoming
`0` (or `1` for step).

```mix
print("" .. range(1, 5))
print("" .. range(0, 10, 2))
print("" .. range(5, 1, -1))
print("" .. range(5, 1))
```
```text
[1, 2, 3, 4, 5]
[0, 2, 4, 6, 8, 10]
[5, 4, 3, 2, 1]
[]
```

### flat

`flat` recursively flattens nested lists into one flat list:

```mix
print("" .. flat([1, [2, 3], [4, [5, 6]]]))
```
```text
[1, 2, 3, 4, 5, 6]
```

### concat

`concat($a, $b, …)` joins **2+ lists** into one new list, **one level deep** —
unlike `flat`, it does not recurse into nested elements, so a list *of lists*
(or of maps) is concatenated without disturbing the inner structure:

```mix
print("" .. concat([1, 2], [3, 4], [5]))
print("" .. concat([[1, 2]], [[3, 4]]))
```
```text
[1, 2, 3, 4, 5]
[[1, 2], [3, 4]]
```

It returns a fresh list and mutates nothing — the clean, O(total) way to
**accumulate a sequence across helpers** under Mix's by-value list model: a
helper returns its items, the caller `concat`s them in, sidestepping the
[`push($param, …)`-into-a-copy trap](#footgun-lists-pass-into-functions-by-value)
entirely. Each argument must be a list (a non-list arg is a loud error); pass at
least two.

### slice / take / drop

`slice(list, start[, end])` returns the half-open range `[start, end)`. **End is
exclusive**; omit it (or pass `nil`) to slice to the end. Negative indices count
from the end, and all bounds **clamp** (out-of-range never errors):

```mix
$l = [10, 20, 30, 40, 50]
print("" .. slice($l, 1, 3))
print("" .. slice($l, -2))
print("" .. slice($l, -100, 100))
```
```text
[20, 30]
[40, 50]
[10, 20, 30, 40, 50]
```

`slice` also accepts a **string** (`slice("hello", 1, 3)` → `"el"`, counting
codepoints — see [strings](strings.md)) and, since **v0.64.0**, a
**`bytes`/`buffer`** value (counting bytes, returning a new value-semantic
`bytes` — see [io](io.md#bytes-as-a-sequence-v0640)). A **reversed** range is
empty in every case: `slice($l, 3, 1)` is `[]`, not a wrap-around.

`take(list, n)` keeps the **first** `n` (negative `n` → **last** `|n|`);
`drop(list, n)` skips the **first** `n` (negative `n` → drops the **last** `|n|`):

```mix
$l = [10, 20, 30, 40, 50]
print("take 2:   " .. ("" .. take($l, 2)))
print("take -2:  " .. ("" .. take($l, -2)))
print("drop 2:   " .. ("" .. drop($l, 2)))
print("drop -2:  " .. ("" .. drop($l, -2)))
```
```text
take 2:   [10, 20]
take -2:  [40, 50]
drop 2:   [30, 40, 50]
drop -2:  [10, 20, 30]
```

### zip

`zip(a, b)` pairs corresponding elements into 2-element lists, stopping at the
shorter input. Destructure each pair with `[0]` / `[1]`:

```mix
print("" .. zip([1, 2, 3], ["a", "b"]))
for each $pair in zip(["host", "port"], ["node1", 8080])
  print($pair[0] .. " = " .. ("" .. $pair[1]))
end
```
```text
[[1, a], [2, b]]
host = node1
port = 8080
```

## Pure map builtins

### keys / values / has_key

```mix
$m = { host: "node1", port: 8080, tls: true }
print("" .. keys($m))
print("" .. values($m))
print("" .. has_key($m, "port"))
print("" .. has_key($m, "user"))
```
```text
[host, port, tls]
[node1, 8080, true]
true
false
```

`keys`/`values` return lists in **insertion order**. `has_key(map, key)` is the
map-membership test — remember `contains` does **not** accept a map and will
raise `contains() expects a string or list`.

### merge / delete

Both are **non-mutating** — they return a new map and leave the original alone.
`merge(a, b)` overlays `b` onto `a` (b wins on conflicts); `delete(map, key)`
returns a copy without that key.

```mix
$base = { host: "node1", port: 8080 }
$out = merge($base, { port: 9090, tls: true })
print("merged: " .. ("" .. $out))
print("original: " .. ("" .. $base))
print("deleted:  " .. ("" .. delete($base, "port")))
print("still:    " .. ("" .. $base))
```
```text
merged: {host: node1, port: 9090, tls: true}
original: {host: node1, port: 8080}
deleted:  {host: node1}
still:    {host: node1, port: 8080}
```

## `List == List` raises (0.68.0) — use `deep_eq`

`==` and `!=` raise `TYPE_ERROR` when **both** operands are a map or list —
including a self-compare (`$a == $a`) and a list-vs-map cross-compare.

Until 0.68.0 they answered `false`/`true` instead — always, regardless of
contents. That is not a comparison; it is a constant wearing a comparison's
clothes, and it read as working code right up until someone depended on it.
The raise names the fix:

```
TYPE_ERROR: `==` is not defined for list and list — it would always answer
false, not compare them. Use deep_eq(a, b) for structural comparison
```

**Use `deep_eq(a, b)` (0.63.0)** — structural equality: maps compare by key
set with insertion order ignored, lists elementwise in order, scalars as
`==`:

```mix
print(deep_eq([1, {x: 2}], [1, {x: 2}]))   -- true
print(deep_eq({a: 1, b: 2}, {b: 2, a: 1})) -- true (map order ignored)
print(deep_eq([1, 2], [2, 1]))             -- false (list order matters)
```

Scalars compare as `==`, with the corners that implies: a **function**
value is never equal — even to itself, so a callback-bearing map is not
`deep_eq` its own copy — and Buffer-vs-Bytes is false (`freeze()` first).
Nesting deeper than 512 levels raises (catchable) rather than recursing
unboundedly.

Or compare a derived value (length, `join`, an element) — those are scalars,
so `==` applies normally:

```mix
$a = [1, 2, 3]
$b = [1, 2, 3]
print("deep_eq:       " .. ("" .. deep_eq($a, $b)))
print("same length:   " .. ("" .. (length($a) == length($b))))
print("same joined:   " .. ("" .. (join($a, ",") == join($b, ","))))
```
```text
deep_eq:       true
same length:   true
same joined:   true
```

### A collection compared to a SCALAR still answers — `$m[$k] == nil` works

Only collection-vs-collection raises. A same-type `false` **misleads** — it
invites the reading "these two lists differ", which is not what was computed.
A collection and a scalar differ by *type*, so `false` there is truthful,
exactly as `1 == "a"` is honestly false:

```mix
$reg = {}
print($reg["k"] == nil)   -- true  (key absent)
$reg.k = [1, 2]
print($reg["k"] == nil)   -- false (key present, value is a list)
print([1] == "text")      -- false
print([1] != nil)         -- true
```

That first pattern is the whole reason for the narrower rule: `$map[$key] ==
nil` is **the** key-absence idiom, and the value on the left is a list or map
precisely when the key IS populated. Raising there would fail
data-dependently — the same line working until the map fills up.

### The raise is scoped to the OPERATORS — searching builtins are unchanged

`contains`, `index_of`, `last_index_of` and `unique` compare elements
internally, without going through `==`. They do **not** raise — and they also
still treat two equal-looking collections as different:

```mix
print(contains([{a: 1}], {a: 1}))      -- false, NOT a raise
print(index_of([[1], [2]], [2]))       -- -1
print(length(unique([[1], [1], [2]]))) -- 3 — no dedup of equal lists
```

That asymmetry is deliberate but worth knowing: since 0.68.0 `==` is loud
about collections while these stay quiet. Scoping the change to the operators
is what keeps `contains($list, $scalar)` — the overwhelmingly common use —
working, and there is no `deep_contains` yet. When you need a structural
search, drive it yourself:

```mix
$found = false
for each $item in $haystack
  if deep_eq($item, $needle) then $found = true end
end
```

## Footgun: lists pass into functions by value

A list (or map) handed to a function is **copied in**. Mutating the parameter —
`push`, `pop`, index-assignment — changes only the function's local copy; the
caller's value is untouched. The idiom is **pass in → `return` the new value →
reassign at the call site** (the triad — see [functions](functions.md)):

```mix
function with_tail($list)
  push($list, 99)
  return $list
end

$xs = [1, 2, 3]
$ys = with_tail($xs)
print("caller (unchanged): " .. ("" .. $xs))
print("returned:           " .. ("" .. $ys))
$xs = with_tail($xs)
print("after reassign:     " .. ("" .. $xs))
```
```text
caller (unchanged): [1, 2, 3]
returned:           [1, 2, 3, 99]
after reassign:     [1, 2, 3, 99]
```

## Quick reference

```text
LISTS
[a, b, c]            list literal (0-indexed)
$l[i]   $l[-1]       index READ (negative from end; OOB -> nil)
$l[i] = v            index WRITE (negative from end; OOB/non-number = ERROR; grow with push)
length($l) / len     element count
is_empty($l)         true when zero length
push($l, v)          append, MUTATES, returns nil
pop($l) / shift($l)  remove last / first, MUTATES, returns element (nil if empty)
sort($l)             all-number list→numeric; else lexicographic (sort_by for keys)
unique($l)           dedupe, first-seen order
reverse($l)          reversed copy
contains($l, v)      bool membership
index_of($l, v)      0-based pos, or -1
range(a, b[, step])  inclusive both ends
flat($l)             recursively flatten nested lists
concat($a, $b, …)    join 2+ lists into one new list (one level)
slice($l, s[, e])    half-open [s, e); negatives + clamp; reversed range = empty
                     (also string -> string, bytes/buffer -> bytes, v0.64.0)
take($l, n) drop($l, n)   first/last n  (negative n flips the end)
zip($a, $b)          element-wise [a, b] pairs, min length

MAPS
{k: v, "x": w}       map literal (insertion-ordered; keys are STRINGS; keywords ok bare, "fn" not)
$m.key  $m["key"]    read (missing -> nil; "*" key = wildcard fallback)
$m["key"] = v        assign / auto-create key (dot form $m.key = v equivalent, one level only)
keys($m) values($m)  lists, insertion order (for each $k in $m iterates keys)
has_key($m, k)       membership (NOT contains — that errors on a map)
merge($a, $b)        new map, b wins on conflicts (non-mutating)
delete($m, k)        new map without k       (non-mutating)
length($m)           key count
```

## See also

- [hof](hof.md) — `map` / `filter` / `reduce` / `sort_by` / `group_by` / `zip`-style transforms over collections (and the numeric `sort_by` that fixes lexical `sort`)
- [functions](functions.md) — the pass-by-value triad and closures
- [strings](strings.md) — `split` (string → list), `join` (list → string), `pos`, codepoint vs byte semantics
- [data](data.md) — `data_encode`, `json_parse` / `json_encode`, `jq` for round-trippable serialisation
- [keywords](keywords.md) — the keyword list, and which lexemes double as bare map keys / fields
- [builtins index](builtins.md) — every builtin by category

```text
mix builtins list     list every list builtin with its one-line description
mix builtins map      list every map builtin
mix what NAME         one-line description of a single builtin
mix help              the full categorized builtin reference
```

Source of truth: the Mix repo at <https://github.com/markc/cosmix> (verified
against **mix 0.21.2** — the binary is the oracle).
