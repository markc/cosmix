# Control flow

Mix has the control structures you expect — `if`, `while`, `for`, plus an ARexx-style `select` and a bare `loop` — with a few rules that bite anyone extrapolating from C, bash, or Python:

- **Every block closes with `end`.** There is no `}`, no `fi`/`esac`. (Legacy `next`/`done` still parse but warn — see [Legacy terminators](#legacy-terminators).)
- **There is no `do` keyword.** A bareword `do` after a loop header is just a discarded string: with a statement after it on the same line it's a *parse error*; alone at the end of the header line it silently does nothing. Drop the `do`.
- **Statements use newline or `;` separators.** Prefer newlines in files; `;` is the compact one-line form.
- **Conditions use Mix truthiness**, not a bool requirement (see [Truthiness](#truthiness)).
- **A loop var is a normal variable**, not block-scoped — it survives the loop at top level.

For transforming a list into another list, prefer a [HOF lambda](hof.md) (`map`/`filter`/`reduce`) over a hand-rolled loop; reach for the loops below for side effects (`print`, `send`) and stateful iteration.

---

## `if` / `elif` / `else if` / `else`

```mix
$x = 7
if $x > 10 then
  print("big")
elif $x > 5 then
  print("medium")
else
  print("small")
end
```
```text
medium
```

The header is `if <cond> then` — the `then` is **required** (`if $x` followed by a newline is `Parse error … expected Then, got Newline`). A middle branch is `elif <cond> then`, or equivalently the two-word `else if <cond> then` — both spellings parse into the same flat chain closed by a **single** `end` (they may even be mixed in one chain), and the final catch-all is a bare `else`. `elif` was added in 0.33.3; before that only `else if` existed. Only `end` closes an `if`; a legacy `done`/`next` is rejected:

```mix
if true then
  print("x")
done
```
```text
Parse error at line 3:1: unexpected token Done
```

`if` evaluates its branches lazily — only the first truthy condition's body runs, the rest are skipped.

### Truthiness

The condition does **not** have to be a boolean. `is_truthy` classifies every value:

| Value | Truthy? |
|---|---|
| `false`, `nil` | false |
| `0` (number) | false |
| `""` (empty string) | false |
| `"0"` (the one-char string) | **false** |
| any other string (`"0.0"`, `"false"`, `" "`) | true |
| `[]` / `{}` / empty bytes | false |
| non-empty list/map/bytes | true |
| any number `!= 0`, any function | true |

```mix
if "" then print("a") else print("empty string is falsy") end
if 0 then print("b") else print("zero is falsy") end
if "0" then print("c") else print("the string 0 is falsy") end
if "0.0" then print("but 0.0 as a string is truthy") end
if [] then print("d") else print("empty list is falsy") end
```
```text
empty string is falsy
zero is falsy
the string 0 is falsy
but 0.0 as a string is truthy
empty list is falsy
```

The `"0"` case is a deliberate shell-flavoured trap: `"0"` is falsy, `"0.0"` is truthy. When in doubt, compare explicitly (`if $s != "" then …`). For combining conditions use `and` / `or` / `not` (see [operators](operators.md)).

### `if` as an expression

Since 0.20 `if … then … else … end` can also be used **in expression position** — anywhere a value goes: an assignment, a call argument, inside a `..` concatenation. It short-circuits (only the taken branch runs). Each branch is a statement block whose value is its **last statement** (or nil if empty / no branch matched):

```mix
$code = 404
$msg = if $code == 200 then "ok" else if $code == 404 then "not found" else "?" end
print($msg)
print("status: " .. if $code >= 400 then "error" else "fine" end)
print(length(if false then "ab" else "abcd" end))
```
```text
not found
status: error
4
```

A multi-statement branch yields its last statement's value:

```mix
$v = if true then
  $a = 10
  $a * 2
else
  0
end
print($v)
```
```text
20
```

A **trailing operator binds to the if's value** — the `end` closes the expression first:

```mix
$x = if true then 1 else 2 end + 3
print($x)
```
```text
4
```

A leading `if` at the start of a statement is still a plain statement (its value is discarded); the expression form fires only when the `if` is nested inside an expression. For the terse one-line form, reach for the ternary `cond ? a : b`, and for "value or default" the nil-coalescing `??` (see [operators](operators.md)). Prefer either over the Lua-style `cond and a or b` idiom — Mix is broadly falsy, so that silently returns `b` whenever the middle value is falsy.

---

## `while`

Runs the body while the condition stays truthy, re-checking before each iteration (zero iterations if false at entry):

```mix
$n = 3
while $n > 0
  print($n)
  $n = $n - 1
end
```
```text
3
2
1
```

```mix
while false
  print("never")
end
print("skipped")
```
```text
skipped
```

There is no `do` after the condition — the body simply starts on the next line.

---

## `loop`

An unconditional loop — the ARexx/Smalltalk infinite loop. You exit it with [`break`](#break-and-continue) (or `return` from inside a function, or `die`):

```mix
$i = 0
loop
  $i = $i + 1
  if $i > 3 then
    break
  end
  print($i)
end
```
```text
1
2
3
```

`loop` is the right shape when the exit test is mid-body, not at the top (a `do … while` in other languages).

---

## `for $i = a to b [step s]`

A numeric counter loop. **Both bounds are inclusive**, the default step is `+1`. (There is no range *operator* — `..` is string concatenation; for a range as a list, see `range()` in [builtins](builtins.md).)

```mix
for $i = 1 to 3
  print($i)
end
```
```text
1
2
3
```

A negative `step` counts **down** (and the `to` bound is still inclusive):

```mix
for $i = 10 to 0 step -2
  print($i)
end
```
```text
10
8
6
4
2
0
```

The step may be fractional (numbers are f64 — see [math](math.md)):

```mix
for $x = 0 to 1 step 0.25
  print($x)
end
```
```text
0
0.25
0.5
0.75
1
```

Bounds and step are arbitrary expressions, evaluated **once** before the loop:

```mix
$lo = 2
$hi = 5
for $i = $lo to $hi
  print($i)
end
```
```text
2
3
4
5
```

### Direction and edge cases

The loop guard is direction-aware: with `step > 0` it stops once `$i > end`, with `step < 0` once `$i < end`. So a positive step over a descending range runs **zero** times rather than forever. **`step 0` never advances the counter — an infinite loop.** Mix does not promote a zero step to 1 (unlike shell brace expansion); don't compute a step that can reach 0.

A number or numeric string is accepted for each bound and step. A value that
`to_number` cannot parse raises `TYPE_MISMATCH`, naming whether it was the
start, end, or step; it never silently becomes `0`/`1`. This includes
`"inf"`, `"nan"`, and overflowing `"1e999"`, which are deliberately not Mix
numeric strings:

```mix
for $i = 1 to "x"
  print($i)
end
```
```text
TYPE_MISMATCH: for-loop end must be a number, got "x" (string)
```

A `NaN` start/end/step (e.g. from `sqrt(-1)`) is rejected up front (`for-loop start/end/step is NaN`) — it would otherwise be an unbounded loop, since every guard against `NaN` is false.

`to`, `step`, and `label` are keywords: they can never name a function or variable (`function step(…)` is a parse error — `expected identifier, got Step`), but since 0.21 a keyword is accepted anywhere it is unambiguously a name — a bare map key, a `.field` — so `for $i = $m.to to 7` parses. See [keywords](keywords.md).

---

## `for $x in <list>` (a.k.a. `for each`)

Iterates the elements of a list — the workhorse for collections. Since
0.63.0 the `each` keyword is **optional**: `for $x in $xs` and
`for each $x in $xs` are the same statement, and both stay supported
indefinitely (`each` is not deprecated). What disambiguates iteration from
the [counted loop](#for-i--a-to-b-step-s) is the token after the variable —
`in` (or a `,`) iterates, `=` counts:

```mix
for $fruit in ["apple", "pear", "fig"]
  print($fruit)
end
```
```text
apple
pear
fig
```

Add an **index variable** with a comma — `for $i, $x in …` or
`for each $i, $x in …` (the index is 0-based):

```mix
for each $i, $x in ["a", "b", "c"]
  print($i .. ": " .. $x)
end
```
```text
0: a
1: b
2: c
```

`for each` also accepts a **string** (yields one codepoint per iteration — emoji-correct since strings are codepoint-based, see [strings](strings.md)) and a **map** (yields the keys, in insertion order):

```mix
for each $ch in "héllo"
  print($ch)
end
```
```text
h
é
l
l
o
```

```mix
$m = { name: "Ada", lang: "Mix" }
for each $k in $m
  print($k .. " = " .. $m[$k])
end
```
```text
name = Ada
lang = Mix
```

### Two variables over a map bind (key, value) — changed in 0.68.0

The second variable is what changes by iterable. Over a **list, string or
bytes** two variables bind **(index, item)**. Over a **map** they bind
**(key, value)** — so the map form needs no `$m[$k]` lookup:

```mix
$m = { name: "Ada", lang: "Mix" }
for each $k, $v in $m
  print($k .. " = " .. $v)
end
```
```text
name = Ada
lang = Mix
```

**Before 0.68.0 this bound (index, key)** — the first variable was a counter
and the second was the key, which meant the value still had to be looked up
and the pairs form did not exist. Both spellings changed together
(`for each $k, $v` and the bare `for $k, $v`), and the **one-variable map
form still yields keys**, unchanged — it is what the release-cycle
`MIX-D3006` note pointed code at before the flip landed. (A map has no
position to bind, so there is no counter form; code that genuinely wanted a
running index over a map's keys counts one itself.)

Anything else (a number, nil) raises `cannot iterate over <type>`. An empty list/map/string runs the body zero times.

> **Iterating snapshots the source.** `for each` takes the items up front — for a map, **both the keys and the values** — so mutating the underlying variable mid-loop changes neither the iteration set nor what the remaining iterations bind. (This is also why `for each` is by-value friendly — see the loop-var scoping note below.)

### Prefer a HOF for value-producing bodies

If the loop body only builds a new collection or folds to a single value, a [HOF lambda](hof.md) is the idiom — it's shorter and side-effect-free:

```mix
$doubled = map([1, 2, 3, 4], fn($x) = $x * 2)
print($doubled)
```
```text
[2, 4, 6, 8]
```

Use `for each` for genuine side effects (printing, [`send`](bus.md), pushing into an accumulator). Reach for `map` / `filter` / `reduce` / `sort_by` when you're transforming data.

---

## `select` / `when` / `otherwise`

ARexx-style multi-way branch — Mix's `switch`. Each `when <expr> then` is tested for **equality** (`==`) against the selector value; the first match runs and the rest are skipped. `otherwise` is the optional default:

```mix
$x = 2
select $x
when 1 then
  print("one")
when 2 then
  print("two")
when 3 then
  print("three")
otherwise
  print("many")
end
```
```text
two
```

It works on strings just as well — a clean command dispatcher:

```mix
$cmd = "list"
select $cmd
when "add" then print("adding")
when "list" then print("listing")
otherwise print("unknown")
end
```
```text
listing
```

The `when` arms are **arbitrary expressions**, evaluated and compared at match time (not constant labels like C's `case`):

```mix
$x = 4
$two = 2
select $x
when $two * 2 then print("computed match: 4")
otherwise print("no")
end
```
```text
computed match: 4
```

Match is value equality (`==`). Be aware of the collection gotcha: **`List == List` is always `false`**, so a list selector never matches a list `when` — destructure or compare a scalar key instead:

```mix
$x = [1, 2]
select $x
when [1, 2] then print("matched list")
otherwise print("no match - lists never equal")
end
```
```text
no match - lists never equal
```

Each `when` arm takes exactly **one** expression (there is no `when 1, 2 then` multi-value arm — use `otherwise` plus an `if`, or normalise the selector first), and the `then` after it is required. With no matching `when` and no `otherwise`, `select` does nothing — no error.

---

## `break` and `continue`

`break` exits the innermost loop; `continue` skips to its next iteration. They work in `for`, `for each`, `while`, and `loop`:

```mix
$i = 0
loop
  $i = $i + 1
  if $i > 3 then
    break
  end
  print($i)
end
```
```text
1
2
3
```

```mix
for $i = 1 to 6
  if $i == 3 then
    continue
  end
  print($i)
end
```
```text
1
2
4
5
6
```

### Conditional `break if` / `continue if`

A one-line guarded form — `break if <cond>` / `continue if <cond>` — saves a wrapping `if … end`:

```mix
for $i = 1 to 10
  continue if $i < 3
  break if $i > 6
  print($i)
end
```
```text
3
4
5
6
```

### Labeled loops — break/continue an outer loop

Tag a loop header with `label <name>` — all four loop forms accept it (`for … label x`, `for each … label x`, `while <cond> label x`, `loop label x`) — then `break <name>` / `continue <name>` to act on that loop from inside a nested one:

```mix
for $i = 1 to 3 label outer
  for $j = 1 to 3
    if $i == 2 and $j == 2 then
      break outer
    end
    print($i .. "," .. $j)
  end
end
```
```text
1,1
1,2
1,3
2,1
```

```mix
for $i = 1 to 3 label rows
  for $j = 1 to 3
    continue rows if $j == 2
    print($i .. "," .. $j)
  end
end
```
```text
1,1
2,1
3,1
```

`continue rows` abandons the rest of the *inner* loop and jumps to the next iteration of the labeled *outer* loop. A `break`/`continue` whose label matches no enclosing loop propagates upward until one matches — and if **none** matches, it escapes every loop and aborts the script with a runtime error (`unexpected break outside loop`). A label typo is a crash, not a silent no-op.

---

## Inline (single-statement) bodies

A loop or `if` body may be a **single statement on the header line**, with no `do` and the same trailing `end`:

```mix
for $i = 1 to 3 print($i) end
```
```text
1
2
3
```

```mix
$c = 0
while $c < 3 $c = $c + 1 end
print($c)
```
```text
3
```

```mix
$a = []
for each $x in [1, 2, 3] push($a, $x * 10) end
print($a)
```
```text
[10, 20, 30]
```

Use `;` to put more than one body statement on the same physical line:

```mix
for each $x in [1, 2, 3]; print($x); print($x * 10); end
```

Critically, **`do` is not a keyword** — a bareword `do` parses as a discarded
string. Inline, it becomes the first body statement and the real body breaks the
parse:

```mix
for each $x in [1, 2, 3] do print($x) end
```
```text
Parse error at line 1:29: expected end of statement, got Print
```

Sneakier: with a **newline** after the `do` (the bash-style block header), the loop parses and runs — the `do` is just a silently discarded first statement. It looks like `do` is supported; it isn't. Drop it either way.

---

## Legacy terminators

The old block terminators still parse but emit a deprecation warning — **`next`** closes `for`/`for each`, **`done`** closes `while`/`loop`/`on` (the warning names the block kind: `'done' is deprecated for 'loop' blocks, …`). Migrate them to `end`:

```mix
for $i = 1 to 2
  print($i)
next
```
```text
warning: <mix> line 3: 'next' is deprecated for 'for' blocks, use 'end' instead
1
2
```

```mix
$n = 2
while $n > 0
  print($n)
  $n = $n - 1
done
```
```text
warning: <mix> line 5: 'done' is deprecated for 'while' blocks, use 'end' instead
2
1
```

`if` and `function`/`fn` accept **only** `end` — there is no legacy form for them. **Use `end` everywhere** in new code.

---

## Loop-variable scope (the one footgun)

Mix has **no per-iteration block scope** — the loop var is a single slot reused each pass, and it **persists after the loop** at top level:

```mix
for $i = 1 to 3
  $last = $i
end
print("last i was " .. $last)
```
```text
last i was 3
```

Inside a [function](functions.md) this is subtler: a loop var (and `for each` element/index var, and `catch $e`) binds **function-locally**, so a library's `for each $p in …` *shadows* rather than *clobbers* a same-named caller/global `$p`. But that also means a body assignment like `$total = $total + $x` inside a function binds a local and does **not** write through to an outer global — to propagate a result out, `return` it. See [functions](functions.md).

---

## See also

- [functions](functions.md) — `function`/`fn`, `return`, closures, the pass-in/return/reassign triad
- [hof](hof.md) — `map` / `filter` / `reduce` / `sort_by`: prefer these over value-producing loops
- [operators](operators.md) — `and` / `or` / `not`, comparisons, `..` concatenation
- [strings](strings.md) — codepoint iteration, interpolation rules
- [collections](collections.md) — lists & maps, `push`, indexing bases
- [error handling](errors.md) — `try` / `catch`, `die`
- [bus](bus.md) — `on … end` handlers (which also accept the legacy `done`)
- [keywords](keywords.md) — the reserved-word list; keywords as bare map keys/fields (0.21)
- [builtins index](builtins.md) — full builtin catalogue
- Lineage: [ARexx](https://en.wikipedia.org/wiki/ARexx) (`select`/`when`/`otherwise`, `do … end` blocks)
- `mix help` — one-line builtin summary · `mix what for` / `mix what select` — per-keyword help

```text
$ mix what for
for: Numeric loop: for $i = 1 to 10 ... end; iteration: for $x in LIST ... end ('each' optional: for each $x in LIST)
```
