# variables — sigils, assignment & scope

Every Mix variable carries a `$` sigil. There is no separate declaration —
assignment **is** the declaration. Reading an unbound variable raises a runtime
error; the `$` sigil is also how Mix tells a variable apart from a bare command
word at the shell-first classifier. This page covers the sigil, assignment,
reading (bound and unbound), `${...}` string interpolation, the `env()` builtin,
positional `$1`/`$2` script arguments, and the one rule that trips up everyone
coming from bash or Python — **assignment inside a function does not write
through to a global**.

Related: [strings](strings.md) for the full quoting/interpolation rules,
[functions](functions.md) for the deep function-scope detail, and
[control flow](control-flow.md) for loop variables.

## The `$` sigil is mandatory

Read and write a variable with a leading `$`:

```mix
$x = 42
print($x)            -- 42
print($x + 8)        -- 50
```

```text
42
50
```

A **bare** `x = 1` is *not* a Mix assignment — the classifier is shell-first, so
it tries to run a command named `x`:

```text
$ mix -c 'x = 1'
mix: x: No such file or directory (os error 2)
```

The fix is always the same: write `$x = 1`. This is the single most common
first error. The sigil is what makes `$x = 1` parse as Mix rather than dispatch
to the shell.

### Variable names

A name after the `$` is ASCII letters, digits and underscores. Names are
**case-sensitive** — `$Foo` and `$foo` are different variables:

```text
$ mix -c '$Foo = 1
print($foo)'
Runtime error at line 2: undefined variable '$foo'
```

Because the sigil already marks the word as a variable, **keyword lexemes are
fine as variable names** — `$to`, `$end`, `$step`, even `$fn` all bind and read
normally. Un-sigiled name positions still reject keywords: `function step()` is
a parse error (`expected identifier, got Step`). See [keywords](keywords.md).

## Assignment, concatenation & reassignment

Assignment creates the variable in the current scope if new, or updates it in
place if it already exists in a visible scope:

```mix
$n = 1
$n = $n + 1
$n = $n + 1
print($n)            -- 3
```

```text
3
```

**Concatenation is `..`, never `+` or `.`** — `+` is numeric addition, and `.`
is field access:

```mix
$name = "world"
print("hello " .. $name)
```

```text
hello world
```

Statements are separated by a newline or `;`. Prefer newlines in script files;
the compact `mix -c '$x = 1; print($x)'` form is equivalent.

### Indexed & field assignment

Assignment also targets list elements and map fields:

```mix
$l = [1, 2, 3]
$l[0] = 9            -- lists are 0-indexed
$l[-1] = 7           -- a negative index writes from the end
print($l)

$m = { host: "a" }
$m.host = "b"        -- field assignment
$m.to = "kw"         -- keyword field names are fine
$m["port"] = 80      -- index assignment creates the key
print($m)
```

```text
[9, 2, 7]
{host: b, to: kw, port: 80}
```

Writes are **loud** where reads are forgiving: an out-of-range list *write*
(`$l[5] = 9` on a 1-element list → "list index 5 out of range") and an
index-assign into a scalar (`$s[0] = "H"` on a string → "cannot index-assign
into string") both raise, while the corresponding *reads* (`$l[5]`,
`$m["nope"]`) return `nil`. The asymmetry is deliberate — a lost write is data
loss, a missing read is often just a probe. Full indexing rules:
[collections](collections.md).

## Reading an unbound variable raises

Mix has no implicit `nil` for an unread name. Touching one that was never
assigned is a hard `RuntimeError`:

```text
$ mix -c 'print($nope)'
Runtime error at line 1: undefined variable '$nope'
```

This is deliberate — a typo in a variable name fails loudly instead of silently
reading empty. If you genuinely need a "maybe-set" variable (e.g. for an
`if $x == nil` guard), **pre-initialise it to `nil` first**:

```mix
$maybe = nil
if $maybe == nil then
  print("was nil")
end
```

```text
was nil
```

**All-digit names** (`$0`, `$1`, `$2`, … — the positional script arguments) are
the one exception — an unset positional reads as `nil` rather than raising (see
[Positional arguments](#positional-arguments)).

## `${...}` string interpolation — scope → env → nil

Inside **double quotes**, `${name}` interpolates. The lookup walks
**scope first, then the process environment, then literal `nil`**:

```mix
$user = "alice"
print("hi ${user}")          -- scope hit
```

```text
hi alice
```

```text
$ GREETING=howdy mix -c 'print("env says: ${GREETING}")'
env says: howdy
```

A scope binding always wins over an environment variable of the same name — the
env fallback only fires when the name is **truly unbound in scope**. A variable
bound to `nil` is still a scope hit, so it shadows the env:

```text
$ ZED=fromenv mix -c '$ZED = nil
print("[${ZED}]")'
[nil]

$ ZED=fromenv mix -c 'print("[${ZED}]")'
[fromenv]
```

An entirely unknown name interpolates as the text `nil`:

```text
$ mix -c 'print("[${missing}]")'
[nil]
```

### A bare `$name` in double quotes is LITERAL

This is the opposite of bash, and a prime trap. Only `${...}` (with braces)
interpolates; a bare `$name` is literal text:

```mix
$name = "bob"
print("literal $name here")
```

```text
literal $name here
```

To emit a literal `$` next to braces, escape it: `"\$5"` → `$5`. When in doubt,
prefer `..` concatenation over interpolation — `"hi " .. $name` is unambiguous.

`${...}` is **string-only syntax** — it is not a general expression form.
`print(${x})` outside a string is a lexer error:

```text
$ mix -c '$x = 9
print(${x})'
Lexer error at line 2:7: expected variable name after '$'
```

### Dotted paths interpolate into maps

`${map.field}` walks a map by dotted path inside the interpolation:

```mix
$cfg = { host: "node1", port: 8080 }
print("connect ${cfg.host}:${cfg.port}")
```

```text
connect node1:8080
```

A missing map, or a missing key on a bound map (`${cfg.missing}`), interpolates
as the text `nil`, same as an entirely unknown name.

### `'single quotes'` are fully raw

Single-quoted strings interpolate **nothing** — no `${...}`, no `~`, no escapes
beyond `\'` and `\\`. Use them when you want the literal characters:

```text
$ mix -c "\$x = 9
print('raw \${x} \$x')"
raw ${x} $x
```

See [strings](strings.md) for the complete quoting matrix.

## `env()` — explicit environment read

`${X}` consults the environment only as a *fallback* after scope. When you want
to read the environment unconditionally — ignoring any same-named Mix variable —
call the `env()` builtin. A missing variable returns the **empty string** (it
never raises):

```text
$ MYVAR=hello mix -c 'print(env("MYVAR"))
print("[" .. env("NOSUCH") .. "]")'
hello
[]
```

| Form | Looks up | Missing → | Same-named Mix var |
|---|---|---|---|
| `"${X}"` | scope, then env | `nil` text | scope **wins** |
| `env("X")` | env only | `""` (empty string) | ignored |

## Positional arguments

When you run a script file, its path lands in `$0` and the command-line
arguments in `$1`, `$2`, … as strings:

```mix
-- pos.mix
print("script: " .. $0)
print("first: " .. $1)
print("second: " .. $2)
```

```text
$ mix pos.mix apple banana
script: pos.mix
first: apple
second: banana
```

An unset positional reads as `nil` (not an error — the one exception to the
unbound-raises rule). The exemption is by **name shape**: any all-digit name
reads `nil` when unset — `$15` is `nil` even when only two args were passed,
and `$0` is `nil` under `mix -c` (no script file):

```text
$ mix -c 'print($1)'
nil
```

Positionals interpolate in double quotes like any other name —
`"first is ${1}"` → `first is apple`.

For the whole argument vector as a list, use the `args()` builtin — the same
arguments the positionals index, so `args()[0]` is `$1`:

```mix
-- argv.mix
print(args())
print(length(args()))
```

```text
$ mix argv.mix one two three
[one, two, three]
3
```

`args()` pairs with `getopt(args(), spec)` for flag parsing — see
[builtins index](builtins.md).

## Scope: top level vs functions

At the **top level** there is one global frame. A variable assigned anywhere at
the top level is visible everywhere after it, and a loop variable still holds its
last value after the loop ends:

```mix
for each $x in [1, 2, 3]
  $last = $x
end
print($last)
```

```text
3
```

### Assignment in a function does NOT write through to a global

A function gets its **own frame**. It can *read* an outer/global variable
(reads fall through to the global frame), but **any `$x = …` assignment inside a
function binds a function-local** — it never updates the outer variable:

```mix
$g = 1
function bump()
  $g = 99           -- binds a LOCAL $g, does NOT touch the global
end
bump()
print($g)           -- still 1
```

```text
1
```

Reading through to an outer variable works fine:

```mix
$base = 100
function show()
  print($base)      -- reads the global
end
show()
```

```text
100
```

This holds for accumulator patterns — a helper doing `$count = $count + 1`
leaves the caller's `$count` unchanged — and for `$rc` / `$result` too: on a
`send` the Bus runtime writes them **into the current frame**, so a `send`
inside a function updates them for that function's body only. The caller's
`$rc` is untouched (and still unbound at the top level if no top-level `send`
has run). There is **no `global` keyword** by design.

### Binders shadow inside a function

Names a function *introduces* — a `for each $x` / `for $i = …` loop variable, a
`catch $e`, a `parse … with $a $b` target — bind in the function's own frame,
**shadowing** a same-named caller/global variable rather than clobbering it:

```mix
$x = "outer"
function f()
  for each $x in [1, 2, 3]
    $y = $x           -- $x is function-local here
  end
end
f()
print($x)             -- still "outer"
```

```text
outer
```

The same holds for `catch $e` and `parse "one two" with $a $b` inside a
function — the outer `$e` / `$a` read unchanged after the call. At the **top
level** there is no shadowing: everything is one global frame, so a top-level
loop variable overwrites a same-named global and persists after the loop.

### The pass-in / return / reassign triad

To propagate state out of a function, thread it through three steps:
**(1) pass the value in** (explicit read), **(2) `return`** the new value, and
**(3) reassign at the call site** (the write):

```mix
$g = 5
function inc($n)
  return $n + 1
end
$g = inc($g)        -- pass in, return, reassign
print($g)           -- 6
```

```text
6
```

Return a map to update several values at once. Parameters pass **by value**
(a number/list/map is copied in), so mutating a parameter inside the function
never reaches the caller either — the `return` + reassign is always the channel.
Lambdas and closures follow the same rules (a closure *captures* its defining
scope for reads, but a write still binds a local). The full function-scope
detail — closures, captures, recursion, HOF lambdas — lives in
[functions](functions.md).

## Quick reference

| Thing | Rule | Example |
|---|---|---|
| Sigil | every variable is `$name` | `$x = 1` |
| Bare name | misparsed as a shell command | `x = 1` → "No such file" |
| Names | ASCII letters/digits/`_`, case-sensitive | `$Foo` ≠ `$foo` |
| Keyword names | fine with the sigil | `$to = 1`, `$end = 2` |
| Concat | `..` (not `+` or `.`) | `"a " .. $x` |
| Unbound read | raises `RuntimeError` | `$nope` → error |
| Unbound positional | all-digit names read as `nil` | `$1` (unset) → `nil` |
| `${X}` in `"..."` | scope → env → `nil` text | `"${user}"` |
| Bare `$X` in `"..."` | **literal** text | `"$user"` → `$user` |
| `'...'` | fully raw, no interpolation | `'${x}'` → `${x}` |
| `env("X")` | env only, missing → `""` | `env("HOME")` |
| Indexed write | loud on out-of-range; the read is `nil` | `$l[5] = 9` → error |
| Assign in function | binds a **local**, never the global | use return + reassign |
| Binders in function | shadow, never clobber the outer var | loop var, `catch $e` |

## See also

- [strings](strings.md) — quoting, `${...}` vs `$name`, escapes, `~` expansion
- [functions](functions.md) — function-local binding, closures, the triad in depth
- [control flow](control-flow.md) — loops and the variables they bind
- [collections](collections.md) — lists/maps, indexing, the read-`nil` / write-error asymmetry
- [keywords](keywords.md) — reserved words and where they are allowed as names
- [builtins index](builtins.md) — `env`, `args`, `getopt`, and the rest
- [Bus messaging](bus.md) — `$result` / `$rc` after a `send`

```
mix help              the full categorized builtin reference
mix what env          one-line description of env() (or any name)
mix what args         one-line description of args()
mix keywords          list the reserved words
```
