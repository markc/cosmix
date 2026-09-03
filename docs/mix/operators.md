# operators — arithmetic, concat, comparison & boolean

Every operator Mix's expression grammar knows, with verified precedence and the
exact coercion rules. Statements use newline or `;` separators, every
variable is `$`-sigil, and string concatenation is **`..`** — not `+`, not `.`.
Verified against **mix 0.56.0**; the binary is the oracle.

## Precedence & associativity

From the parser (`parse_binary_rhs`, `parser.rs`). **All binary operators are
left-associative** — including `**`. Higher number = binds tighter.

| Prec | Operators | Meaning |
|---|---|---|
| 0 | `? :` | ternary conditional (short-circuit, **right-associative**) |
| 1 | `or` | logical OR (short-circuit) |
| 2 | `and` | logical AND (short-circuit) |
| 3 | `==` `!=` `<` `>` `<=` `>=` `eq` `ne` | comparison (no chaining) |
| 4 | `??` | nil-coalesce (short-circuit) |
| 5 | `..` | string concatenation |
| 6 | `+` `-` | add / subtract (or `+` string-fallback) |
| 7 | `*` `/` `%` | multiply / divide / modulo |
| 8 | `**` | power |

Unary `-` (negate) and `not` / `!` (logical not) bind tighter than every binary
operator. Parenthesise to override.

`&&` and `||` are **statement-chain operators**, and `;` is a statement
separator rather than an expression operator, so none appears in this table.
An assignment cannot be any operand of a **Mix-classified** chain: use `and` /
`or` inside an assigned logical expression, or put shell-style conditional
execution in a separate statement. This rejects both `$ok = $a.ok && $b.ok` and
`print("gate") && $ok = false`, and neither line's operands run.
A line keeps bash semantics instead only when the classifier routes it to the
shell *and* the Mix parse gives out before reaching the assignment — a
path-shaped head, a redirect, an env prefix. Neither half is enough alone:
`print(1); echo hi` fails to parse yet stays a Mix error, and `true && $x =
false || cmd` has a shell-command head yet parses far enough to be rejected.
See `mix man syntax` for the exact boundary.

`;` is the loosest hard boundary. A Mix chain operand can be a pipeline, but
after `|` the raw external command tail owns any later `&&`/`||` until newline
or `;`; there is no symmetric pipeline/chain precedence rule. See the
authoritative [statement-separator contract](syntax.md).

**What a chain tests is `$rc`.** After the left statement runs, `&&` executes
its right side only when `$rc` reads as success and `||` only when it reads as
failure, where (since 0.60.0):

- **absent `$rc` is success** — pure Mix statements like `print` never set it,
  so a cold `print(...) && next` runs its right side. Mind the scope corner:
  when **no global `$rc` exists yet**, an rc-setting statement inside a
  function writes a *function-local* `$rc` that dies with the call, so
  `p() && next` can read absent — and run `next` — off a failure that
  happened inside `p()`. (Once a global `$rc` exists, in-function setters
  update it and the gate sees it.) Chain on rc-setting statements directly,
  or return the rc from the function, rather than chaining on a call that
  buries them;
- a **finite whole Number** compares against `0` (`0` = success; anything
  else, including the negative Bus transport bands, = failure);
- **anything else raises**: a non-Number `$rc` (string — numeric strings
  included — bool, nil, list, map) is `TYPE_MISMATCH`, and a non-finite or
  fractional Number is `VALUE_OUT_OF_RANGE`, both catchable as normal errors.
  Before 0.60.0 the gate cast through `to_number().unwrap_or(0)`, which
  fabricated an **arbitrary verdict** from every corrupt shape, never an
  error: anything whose coerced-then-truncated value was `0` read as
  *success* — non-coercible values and unparseable strings (`"inf"`
  included), `false`, NaN, fractions inside `(-1, 1)` and the numeric
  strings `"0"`/`"0.5"` — so `&&` ran a destructive right side; while
  `true`, `Number` infinities and anything truncating to non-zero read as
  *failure*, so `||` did. Raising stops both.

```mix
print(2 + 3 * 4)          -- * before +
print((2 + 3) * 4)        -- parens win
print("sum=" .. 2 + 3)    -- + (prec 6) before .. (prec 5)
print(1 + 2 == 3)         -- arith before comparison
```
```text
14
20
sum=5
true
```

**`**` is left-associative** (unlike maths convention and unlike Python):

```mix
print(2 ** 3 ** 2)        -- (2**3)**2 = 64, NOT 2**(3**2) = 512
print(-2 ** 2)            -- unary minus binds tighter: (-2)**2 = 4
```
```text
64
4
```

Comparisons **do not chain** — they are left-associative at the same precedence,
so `1 < 2 == true` parses as `(1 < 2) == true`:

```mix
print(1 < 2 == true)      -- (true) == true
```
```text
true
```

`and` (prec 2) binds tighter than `or` (prec 1):

```mix
print(true or false and false)   -- true or (false and false)
```
```text
true
```

## Arithmetic: `+` `-` `*` `/` `%` `**`

Mix numbers are **f64** (see [numbers](numbers.md) / [math](math.md)). Operands are
coerced with `to_number`: a numeric **string** or a **bool** is accepted
(`true` → 1, `false` → 0); a non-numeric value **raises** a runtime error.
Numeric strings include scientific notation (`"1e3" + 1` → `1001`), but the
coercion is **strict**: `"inf"`, `"nan"`, and overflow spellings like `"1e999"`
are *not* numeric strings — they behave as ordinary text (so `"inf" + 1`
falls back to concat, and `"nan" < 5` errors).

```mix
print(7 + 3)
print(7 - 3)
print(7 * 3)
print(7 / 3)
print(7 % 3)
print(2 ** 10)
```
```text
10
4
21
2.3333333333333335
1
1024
```

`**` is real `powf`, so fractional and negative exponents work; bools coerce:

```mix
print(9 ** 0.5)           -- square root via power
print(true + true)        -- 1 + 1
print(true * 5)
```
```text
3
2
5
```

**Modulo** follows Rust's `%` (truncated; the result takes the sign of the
dividend) and works on floats:

```mix
print(-7 % 3)
print(7 % -3)
print(7.5 % 2)
```
```text
-1
1
1.5
```

**Division and modulo by zero raise** — catchable runtime errors, not
`NaN`/`inf` (`%` by zero returned `NaN` before 0.20.5; both error now):

```mix
print(5 / 0)
```
```text
Runtime error at line 1: division by zero
```
```mix
print(5 % 0)
```
```text
Runtime error at line 1: modulo by zero
```

Wrap a risky divide in [try/catch](errors.md) if the divisor might be zero. (Note:
the `math` builtins like `ln(0)` *do* return `-inf` per IEEE-754 — only the `/`
and `%` operators hard-error. See [math](math.md).)

A genuinely non-numeric operand to `-` `*` `/` `%` `**` raises:

```mix
print("ab" * 3)           -- no string-repeat operator
```
```text
Runtime error at line 1: cannot use 'ab' as number
```

Numeric strings still coerce (`"8" / "2"` → `4`). In 0.55.0, `/` and `%`
were brought into line with the other arithmetic operators: an unparseable
right operand now reports that operand as non-numeric, rather than being
silently treated as zero and misreported as division/modulo by zero; an
unparseable left operand no longer becomes a plausible numeric `0` result.

There is **no string `*` repeat and no list `+` merge** — use the
[`repeat()`](builtins.md) builtin for strings and [`push()`](collections.md) /
[`concat`-style helpers](collections.md) for lists.

### `+` has a string fallback — prefer `..` for text

`+` first tries to coerce **both** sides to numbers. If both succeed it adds; only
when at least one side is non-numeric does it fall back to string concatenation.
This makes `+` ambiguous on data of unknown type — **always use `..` for text**:

```mix
print("5" + 3)            -- both numeric -> 8
print("5" + "10")         -- both numeric strings -> 15, NOT "510"
print("a" + "b")          -- not numeric -> string concat
print("inf" + 1)          -- "inf" is NOT a numeric string -> concat
print([1] + [2])          -- neither numeric -> renders + joins as text
```
```text
8
15
ab
inf1
[1][2]
```

That `"5" + "10"` → `15` surprise is exactly why the concat operator is separate.

## Concatenation: `..`

`..` always converts **both** operands to their Mix string form and joins them.
It never does arithmetic — this is the unambiguous way to build text. (In `"…"`
strings, `${name}` interpolation is the other path; a bare `$name` is literal. See
[strings](strings.md).)

```mix
$name = "world"
print("hello " .. $name)
print("x" .. 1 .. "y")    -- numbers stringify
print(1 .. 2)             -- "12", NOT 3
```
```text
hello world
x1y
12
```

**`..` is not a range operator** (unlike Rust/Ruby): `1 .. 5` is the *string*
`"15"`, never a sequence. Use the `range()` [builtin](builtins.md) or a
`for` loop for numeric sequences.

`..` is the workhorse for assembling messages, paths, and Bus targets:

```mix
$node = "node1"
$addr = "noded." .. $node .. ".bus"
print($addr)
```
```text
noded.node1.bus
```

When `..` ends a physical line, the expression continues on the following line.
Comments and blank lines may separate the operator from its right operand:

```mix
$message = "prefix: " .. -- trailing concat requests more input

  "body"
```

Only the trailing form is continuation syntax. A line beginning with `..` is
not joined to the previous statement, so `../tool` remains a relative command
path and `source ../file.mix` / `include ../file.mix` retain their path grammar.
No other binary operator gains newline continuation; use `\` for the general
case. A `;` after `..` remains a parse error, not a continuation boundary.

## Comparison: `==` `!=` `<` `>` `<=` `>=`

### Equality `==` / `!=`

Value equality with **one cross-type rule**: a `Number` compared to a `String`
coerces the string to a number and compares numerically (when it parses);
otherwise unequal. Same-type compares structurally. No `Number`↔`Bool` coercion.

```mix
print(2 == 2.0)           -- numbers
print(5 == "5")           -- string coerces to number
print(5 == "5.0")         -- still numeric: 5.0 == 5.0
print(1 == true)          -- NO number/bool coercion -> false
print(nil == nil)
print(3 != 4)
```
```text
true
true
true
false
true
true
```

**`==` / `!=` with a map or list on BOTH sides raises `TYPE_ERROR`** (0.68.0).
Use **`deep_eq(a, b)`** for structural comparison:

```mix
print(deep_eq([1, 2], [1, 2]))   -- true
print([1, 2] == [1, 2])          -- raises TYPE_ERROR, naming deep_eq
```
```text
true
```

Until 0.68.0 the comparison answered — always, whatever the contents: `false`
for `==`, `true` for `!=`. That is a constant wearing a comparison's clothes,
which is why it now raises rather than quietly misleading.

Only **both**-collection comparisons raise. A collection against a **scalar**
still answers, because that is a genuine type difference and `false` is
truthful — which keeps the key-absence idiom working:

```mix
$reg = {}
print($reg["k"] == nil)   -- true
$reg.k = [1, 2]
print($reg["k"] == nil)   -- false  (present; the value is a list)
print([1, 2] == "text")   -- false
```

See [collections](collections.md#list--list-raises-0680--use-deep_eq) for the
full rule, including which builtins are and are not affected.

### Ordering `<` `>` `<=` `>=` — numeric-first, lexicographic fallback

The rule (`num_cmp` in `evaluator.rs`):

1. If **both** operands coerce to numbers (numbers, or numeric strings like `"5"`), compare **numerically**.
2. Else if **both** are strings, compare **lexicographically by Unicode codepoint** (byte order for valid UTF-8 — byte-exact, no case-fold, no normalization).
3. Otherwise (a non-numeric string vs a number) → **runtime error**.

```mix
print(5 < 10)             -- numeric
print("5" < "10")         -- both numeric strings -> NUMERIC -> true
print("apple" < "banana") -- both strings -> lexicographic
print("Zebra" < "apple")  -- 'Z' (0x5A) < 'a' (0x61) -> true
print("abc" <= "abc")
print("5" < "abc")        -- not both numeric, both strings -> lexicographic
```
```text
true
true
true
true
true
true
```

Note `"5" < "10"` stays **numeric** (→ `true`), the opposite of a naive
string compare. The lexicographic path is handy for character-class tests:

```mix
$ch = "m"
print($ch >= "a" and $ch <= "z")   -- is it a lowercase letter?
```
```text
true
```

A non-numeric string against a number is genuinely incomparable and **errors**:

```mix
print(5 < "abc")
```
```text
Runtime error at line 1: cannot compare 'abc' as number
```

## String equality keywords: `eq` / `ne`

`eq` and `ne` compare the **rendered Mix string forms** of both operands
(`to_mix_string()`) — no numeric coercion. Use them when you want a *textual*
match regardless of type, and `==` when you want value/numeric equality. The
difference shows on numbers that print differently:

```mix
print(5 eq "5")           -- "5" eq "5" -> true
print("5" eq 5)           -- symmetric
print(5 eq "5.0")         -- "5" eq "5.0" -> DIFFERENT text -> false
print(5 == "5.0")         -- numeric: 5.0 == 5.0 -> true
print("a" ne "b")
```
```text
true          <- 5 eq "5"
true          <- "5" eq 5   (symmetric)
false         <- 5 eq "5.0"  (different printed text: "5" vs "5.0")
true          <- 5 == "5.0"  (numeric: 5.0 == 5.0)
true          <- "a" ne "b"
```

Rule of thumb: **`==` for "same value", `eq` for "same printed text".**

## Boolean: `and` `or` `not` (and `!`)

`and` / `or` **short-circuit** and **return the deciding operand value itself**,
not a coerced bool — like Lua/Python, not C:

- `a or b` → `a` if `a` is truthy, else `b`.
- `a and b` → `a` if `a` is falsy, else `b`.

```mix
print(0 or "fallback")    -- 0 is falsy -> right side
print("first" or "second")-- left truthy -> left, right not evaluated
print(1 and "kept")       -- left truthy -> right side
print("" and "skipped")   -- left falsy ("") -> left, prints empty
```
```text
fallback
first
kept

```

This makes `or` a clean default-picker and `and` a guard. (`not` / `!` is the only
one that *always* yields a real bool — see below.)

### `not` / `!` and truthiness

`not x` (keyword) and `!x` (symbol) both return a real `Bool` — the logical
negation of `x`'s truthiness. **Truthiness** (`is_truthy`):

| Value | Truthy? |
|---|---|
| `nil` | false |
| `false` / `true` | as-is |
| `0` (number) | false; any other number true |
| `""` **and `"0"`** | false; any other string true |
| `[]` / `{}` empty | false; non-empty true |
| function | always true |

Note the two string traps: an **empty string and the string `"0"` are both
falsy**.

```mix
print(not true)
print(not 0)
print(not "")             -- empty string -> falsy -> not -> true
print(not "0")            -- the string "0" is ALSO falsy
print(not "x")            -- non-empty -> truthy -> false
print(not nil)
print(not [])             -- empty list falsy
print(not [1])
```
```text
false
true
true
true
false
true
true
false
```

## Nil-coalesce: `??`

`a ?? b` is **nil-coalesce, not falsy-coalesce** — it short-circuits on **nil
only**: it returns `a` unless `a` is `nil`, in which case it returns `b`. Unlike
`or`, a falsy-but-non-nil left (`0`, `""`, `false`) is kept. Use it to supply a
default for a possibly-unset value; use `? :` for a real condition.

```mix
$x = nil
print($x ?? "default")    -- nil -> right side
print(0 ?? "default")     -- 0 is not nil -> kept
print("" ?? "default")    -- "" is not nil -> kept
print(false ?? "x")       -- false is not nil -> kept
```
```text
default
0

false
```

Contrast with `or`, which *would* replace `0`, `""`, and `false` because they
are falsy.

**Precedence trap with `..`:** concat (prec 5) binds *tighter* than `??`
(prec 4), so `"x = " .. $v ?? "d"` parses as `("x = " .. $v) ?? "d"` — the
concat result is never nil, so the default **never fires** (you get `x = nil`).
Parenthesise the coalesce inside a concat:

```mix
$v = nil
print("x = " .. $v ?? "d")    -- ("x = " .. $v) ?? "d" -> default dead
print("x = " .. ($v ?? "d"))  -- what you meant
```
```text
x = nil
x = d
```

The other direction reads naturally: `??` binds tighter than comparison, so
`$a ?? 0 < 5` is `($a ?? 0) < 5`.

## Ternary: `cond ? a : b`

The C-style conditional expression (since 0.20.0). It evaluates `cond`, then
**only the taken branch** — `a` if `cond` is truthy, else `b`. It is the
**loosest-binding** operator (looser than `or`) and **right-associative**, so a
chain is an else-if ladder: `a ? b : c ? d : e` is `a ? b : (c ? d : e)`.

```mix
$n = 2
print($n > 0 ? "pos" : "neg")              -- basic
print($n == 1 ? "one" : $n == 2 ? "two" : "many")  -- right-assoc chain
```
```text
pos
two
```

**Prefer `? :` over the `cond and a or b` idiom.** Because Mix is broadly falsy
(`""`, `0`, `false`, `nil`, `[]`), `cond and a or b` silently returns `b` whenever
the *middle* operand `a` is falsy — `$ok and false or "x"` wrongly yields `"x"`,
while `$ok ? false : "x"` correctly yields `false`.

It is single-line at statement top level (a newline before `?` ends the statement
first — use an [`if` expression](control-flow.md) for a multi-line conditional
value), but like any expression it may span newlines inside `(...)`/`[...]`/`{...}`.

A ternary works as a bare statement too, including variable-led (`$c ? "y" : "n"`),
but both branches must be **expressions** — `print` is a *statement* keyword, so
`$c ? print("y") : print("n")` is a parse error. For side-effecting branches use
an [`if` statement](control-flow.md).

## Quick reference

```text
+  -  *  /  %  **      arithmetic  (f64; coerces numeric strings/bools; / or % by 0 errors)
..                     string concat (always stringifies both sides)  <-- use for text
+                      string-concat FALLBACK only when not both numeric (avoid for text)
== !=                  value equality (Number<->String coerces; List/Map always !=)
<  >  <=  >=           ordering: numeric-first, else lexicographic strings, else error
eq ne                  textual equality on the printed Mix string forms
and or                 short-circuit; RETURN the deciding operand (not a bool)
not  !                 logical negation -> always a real Bool
??                     nil-coalesce: left unless left is nil (keeps falsy non-nil)
cond ? a : b           ternary: cond truthy -> a else b (loosest, right-assoc, short-circuit)
-x                     numeric negate;  not x / !x  logical not
```

No operator chains comparisons (`a < b < c` is `(a<b) < c`), and there is no
string-`*`-repeat, list-`+`-merge, or `++`/`--`. Reach for the
[builtins](builtins.md) (`repeat`, `min`, `max`) instead. For a conditional value
use the ternary `cond ? a : b` above or an [`if` expression](control-flow.md).

## See also

- [numbers](numbers.md) — f64 model, integer-clean printing, radix literals
- [math](math.md) — numeric builtins (`sqrt`, `pow`, `round`, IEEE-754 `inf`/`NaN`)
- [strings](strings.md) — `'raw'` vs `"${interp}"`, codepoint vs byte ops
- [control-flow](control-flow.md) — `if` / `while` / `for each`, where truthiness is used
- [errors](errors.md) — `try`/`catch` for division-by-zero and coercion errors
- [variables](variables.md) — the `$` sigil and scope
- [lists](collections.md) — why `List == List` is false; element comparison
- The source: [`evaluator.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/evaluator.rs) (`eval_binop`, `num_cmp`) and [`parser.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/parser.rs) (`parse_binary_rhs` precedence table)
- `mix help` for the topic list; `mix what NAME` for one-line builtin help
