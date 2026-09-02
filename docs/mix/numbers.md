# Numbers

Mix has **one numeric type: a 64-bit IEEE-754 double (`f64`)**. There is no separate integer type — `5`, `5.0`, `1e6`, `0xFF`, and `0o755` are all the same `Number` value under the hood. What makes Mix *feel* like it has integers is the printer: a number that is exactly representable as an `i64` is written without a decimal point (`5`, not `5.0`). This page covers that f64 model, integer-clean formatting, the `0o`/`0x`/`0b` radix literals (and why a bare leading zero is a lexer error), scientific-notation literals, strict numeric-string coercion, `NaN`/`inf` behaviour, the ordering rules, and how whole numbers cross serialization boundaries as integers.

For the numeric *functions* (`round`, `floor`, `sqrt`, `pow`, `abs`, …) see [math](math.md). For the comparison and arithmetic *operators* see [operators](operators.md).

> One-line model: **every number is an f64; it prints as an integer when it round-trips through `i64`; `0o`/`0x`/`0b` and `1e6` are sugar for the same f64; a bare leading zero is rejected.**

## One type: f64

```mix
print(2 + 2)        -- 4
print(10 / 2)       -- 5      (whole result, prints clean)
print(7 / 2)        -- 3.5    (fractional, prints with a point)
print(10 / 4)       -- 2.5
print(2.5)          -- 2.5
print(1.0)          -- 1      (1.0 round-trips through i64 → printed as 1)
print(-5)           -- -5
print(17 % 5)       -- 2      (modulo)
```
```text
4
5
3.5
2.5
2.5
1
-5
2
```

Because the type is f64, integer division is *not* a separate operation — `/` is always float division and `10 / 2` happens to land on a whole number. There is no `//` floor-division operator; use [`floor`](math.md) if you want it.

### f64 caveats apply

Mix inherits every IEEE-754 wrinkle. Fractions are binary, so the classic rounding error is visible:

```mix
print(0.1 + 0.2)    -- not exactly 0.3
```
```text
0.30000000000000004
```

Integers stay exact only up to **2^53** (`9_007_199_254_740_992`). Past that, even whole numbers lose precision:

```mix
print(9007199254740992)   -- 2^53, exact
print(9007199254740993)   -- 2^53 + 1, rounds back down
```
```text
9007199254740992
9007199254740992
```

That 2^53 boundary is why [`stat`](builtins.md) returns inode/device numbers as **strings**, not numbers — a u64 inode above 2^53 would lose its low bits as an f64.

Division **and modulo** by zero are runtime errors (not `inf`/`NaN`), but a math function can still produce a non-finite value:

```mix
print(1 / 0)        -- runtime error
```
```text
Runtime error at line 1: division by zero
```
```mix
print(5 % 0)        -- runtime error too (0.21+; it used to return NaN silently)
```
```text
Runtime error at line 1: modulo by zero
```
```mix
print(sqrt(-1))     -- NaN (from math, not an error)
print(ln(0))        -- -inf
print(exp(1000))    -- inf (overflow)
```
```text
NaN
-inf
inf
```

Once a non-finite value exists it **propagates** through arithmetic per IEEE-754 (`exp(1000) + 1` is `inf`, `1 / exp(1000)` is `0`) and prints as `NaN` / `inf` / `-inf`. Comparisons involving a `NaN` *value* follow IEEE-754 too — they are `false`, not errors:

```mix
print(sqrt(-1) == sqrt(-1))   -- false (NaN is never equal to anything)
print(sqrt(-1) < 5)           -- false (any ordering against NaN is false)
```
```text
false
false
```

## Integer-clean printing

When Mix renders a number — via `print`, `${...}` interpolation, `..` concatenation, or inside a list/map — it uses an **i64 round-trip gate**: if `(n as i64) as f64 == n`, it prints the i64 form (no decimal point); otherwise it prints the full f64 (`value.rs::write_mix`). The gate is the exact i64 cast, not merely "is it integral", so a value too big for i64 (like `1e20`) still prints in f64 form instead of silently saturating. (One pinhole: exactly 2^63 passes the gate through the saturating cast and prints as `9223372036854775807` — one below the true f64 value. Anything past 2^53 is approximate territory anyway.)

Output is always plain decimal — the printer never uses exponent form, even for values you *wrote* in scientific notation: `print(1e20)` gives `100000000000000000000`, `print(1e-10)` gives `0.0000000001`.

```mix
print(4.0 / 2.0)          -- 2     (whole → integer form)
print(3.0 * 3.0)          -- 9
$n = 5
print("count is ${n}")    -- interpolation: clean
print("id-" .. $n)        -- concat: clean
$f = 2.5
print("val ${f}")         -- fractional stays fractional
```
```text
2
9
count is 5
id-5
val 2.5
```

Every render path shares this gate, so a number looks the same whether you `print` it, splice it into a string, or nest it in a structure.

## Radix literals — `0o` / `0x` / `0b`

For file modes and bitmasks, write the value in its natural base. All three prefixes are pure sugar that lex to the same f64 (`lexer.rs::lex_radix_number`):

| Prefix | Base | Example | Value |
|---|---|---|---|
| `0o` / `0O` | octal | `0o755` | 493 |
| `0x` / `0X` | hex | `0xFF` | 255 |
| `0b` / `0B` | binary | `0b101` | 5 |

```mix
print(0o755)            -- 493
print(0xFF)             -- 255
print(0b101)            -- 5
print(0o755 == 493)     -- true  (it IS the f64 493)
```
```text
493
255
5
true
```

`_` digit separators are allowed anywhere in the body for readability — in decimals too:

```mix
print(1_000_000)        -- 1000000
print(0xFF_FF)          -- 65535
print(0b1010_1010)      -- 170
```
```text
1000000
65535
170
```

Radix literals are checked at lex time. A digit out of range, an empty body, or a value past 2^53 is a clear lexer error rather than a silently-split or silently-rounded token:

```mix
print(0o78)             -- 8 is not an octal digit
```
```text
Lexer error at line 1:7: '8' is not a valid octal digit in a 0o literal
```
```mix
print(0xG)              -- no hex digit
print(0x)               -- empty body
print(0x20000000000001) -- past 2^53
```
```text
Lexer error at line 1:7: 'G' is not a valid hex digit in a 0x literal
Lexer error at line 1:7: 0x literal has no digits
Lexer error at line 1:7: 0x20000000000001 exceeds the exact-integer range (2^53); Mix numbers are f64
```

The token boundary is precise: only an *alphanumeric* follower is a bad digit. An operator, `.`, space, or `)` is a legitimate end of the literal — `0xFF+1`, `0o7 .. "x"`, and `0b101)` all lex fine.

Use these for [native `chmod`/`write_new`](builtins.md), where the mode is the f64 *value*: `chmod($path, 0o755)`, `write_new($path, $body, 0o600)`.

### No bitwise operators

Mix has **no** `&` / `|` / `^` / `<<` / `>>` operators and no bitwise builtins. `^` is not a power operator either — the power operator is [`**`](operators.md) (or the [`pow`](math.md) builtin). A bare `&` is a lexer error nudging you toward `&&`:

```mix
print(2 ^ 10)           -- '^' is not an operator
```
```text
Lexer error at line 1:9: unexpected character '^'
```
```mix
print(2 ** 10)          -- ** is the power operator (real powf)
print(pow(2, 10))       -- same thing as a builtin
```
```text
1024
1024
```

## The bare-leading-zero trap

A decimal integer **must not** start with a redundant `0`. Rust's f64 parser would silently read `0755` as `755`, which is a footgun for a substrate whose daily work is octal file modes. Mix rejects it at lex time and tells you the fix (`lexer.rs::lex_number`):

```mix
print(0755)
```
```text
Lexer error at line 1:7: ambiguous leading-zero number '0755' — use a 0o (octal) / 0x (hex) / 0b (binary) prefix, or drop the leading zero(s) for decimal
```

This is a **Mix-expression** hazard, not a shell one. When `0755` is a *string* (an argument in a shell-out), nothing is lexed as a number, so `run("install -m 0755 a b")` is fine. The error only fires when `0755` would be a Mix *number* literal.

Plain `0` and genuine fractions are unaffected — the rule looks only at the integer part:

```mix
print(0)        -- 0
print(0.5)      -- 0.5
print(0.05)     -- 0.05
```
```text
0
0.5
0.05
```

## Scientific-notation literals

The lexer accepts an `[eE][+-]?digits` exponent on a decimal literal (0.20.5+; earlier versions parse-errored on `1e3`). It is sugar for the same f64 — `_` separators work in the exponent too:

```mix
print(1e6)          -- 1000000
print(1.5e3)        -- 1500
print(2e-3)         -- 0.002
print(1E3)          -- 1000
print(1e+2)         -- 100
print(1e1_0)        -- exponent 10
```
```text
1000000
1500
0.002
1000
100
10000000000
```

Two deliberate edges:

- The `e`/`E` is only consumed when a digit (after an optional sign) actually follows, so the [`e()`](math.md) Euler-constant builtin is never eaten — `2 * e()` is `5.43656…`, and a bare `2e` lexes as `2` followed by the token `e`.
- The exponent is checked *after* the leading-zero rule, which sees only the mantissa: `0e5` is a valid `0`, but `07e2` is still the ambiguous-leading-zero lexer error (see above).

Radix literals take no exponent — `0x1e3` is just hex digits (`483`).

## Coercion and numeric strings

`to_number(s)` converts a value to an f64, or returns `nil` on failure (`value.rs::to_number`):

```mix
print(to_number("42"))     -- 42
print(to_number("3.14"))   -- 3.14
print(to_number("1e3"))    -- 1000 (exponent strings parse)
print(to_number("  7 "))   -- 7    (whitespace trimmed)
print(to_number(true))     -- 1    (bools coerce: true→1, false→0)
print(to_number("abc"))    -- nil  (not a number)
print(to_number("0xFF"))   -- nil  (radix prefixes are SOURCE syntax, not parseable at runtime)
```
```text
42
3.14
1000
7
1
nil
nil
```

`to_number` parses a *decimal* string (sign, fraction, exponent). `"abc"` and `"0xFF"` both fail (the f64 parser doesn't accept a `0x` prefix), so guard for `nil` if the input is untrusted.

**String coercion is strict about non-finites** (0.21+). Rust's f64 parser happens to accept the words `"inf"` / `"infinity"` / `"nan"` (any case), but those are not Mix numeric strings — and an overflowing spelling like `"1e999"` is unrepresentable:

```mix
print(to_number("inf"))    -- nil
print(to_number("nan"))    -- nil
print(to_number("1e999"))  -- nil (overflows f64)
print(is_number("inf"))    -- false (is_number agrees with to_number)
print(is_number("1e3"))    -- true
```
```text
nil
nil
nil
false
true
```

Only *string* coercion is strict. A non-finite number **value** passes through untouched — `to_number(sqrt(-1))` is `NaN`, and `is_number(sqrt(-1))` is `true` (it *is* a number; math keeps propagating IEEE-754 non-finites).

Arithmetic operators auto-coerce a numeric *string* operand, so light shell-style input just works:

```mix
print("5" + 3)             -- 8   (string "5" coerces to 5)
```
```text
8
```

`to_number` itself returns `nil` on failure because it is the explicit probe.
Builtins and numeric language forms that require a number do not turn that
failure into `0`: they accept the same numeric strings and bools, but a supplied
unparseable value raises `TYPE_MISMATCH`. Since 0.55.0 this is consistent across
string positions/counts, `range`, numeric `for`, date/format helpers, `sleep`,
`exit(code)`, and arithmetic `/`/`%` operands.

Equality treats a number and its numeric-string form as equal, and `5 == 5.0` is true because they are the same f64:

```mix
print("5" == 5)            -- true
print(5 == 5.0)            -- true
```
```text
true
true
```

## Ordering rules

The comparison operators `< > <= >=` follow a two-step rule (see [operators](operators.md) for the full table):

1. **If *both* operands coerce to numbers** (numbers, or numeric strings like `"5"`), compare **numerically**.
2. Else, **if both are strings**, compare **lexicographically by codepoint**.
3. Otherwise — a string-vs-number mix where the string isn't numeric — raise an error.

```mix
print("5" < "10")     -- true  (both numeric → 5 < 10, NOT string order)
print(5 < 10)         -- true
print("apple" < "banana")  -- true  (both non-numeric strings → lexicographic)
print("5" < "abc")    -- true  ("5" < "abc" lexicographically; not both numeric)
print(5 >= 5)         -- true
print(3 != 4)         -- true
```
```text
true
true
true
true
true
true
```

The footgun bash brings is the *opposite* of Mix here: `"5" < "10"` is **numeric** (true), because both strings coerce. You only fall back to string ordering when at least one side won't coerce to a number. A genuine type clash errors rather than guessing:

```mix
print(5 < "abc")
```
```text
Runtime error at line 1: cannot compare 'abc' as number
```

## Modulo sign

`%` follows the f64 remainder (the result takes the sign of the **dividend**, like C/Rust `%`, *not* Python's floored modulo):

```mix
print(-7 % 3)     -- -1   (sign of -7)
print(7 % -3)     -- 1    (sign of 7)
print(5.5 % 2)    -- 1.5  (works on non-integers too)
```
```text
-1
1
1.5
```

A zero divisor is a runtime error (`modulo by zero`), matching `/` — see the caveats section above.

## Serialization: whole numbers travel as integers

Because the one type is f64, every serialization boundary decides whether a whole value leaves as an *integer* or a *float*. Mix consistently picks **integer for whole numbers**, so a peer field typed `usize`/`i64` accepts it:

- **[`json_encode`](data.md)** — a finite whole number in `[-2^63, 2^63)` encodes as a JSON integer; anything else (fractional, or ≥ 2^63) encodes as a JSON float (`json.rs::mix_to_json`). The upper bound is exclusive because `i64::MAX as f64` rounds up to exactly 2^63 — an f64 of 2^63 encodes as the real `9.223372036854776e+18` instead of silently saturating.
- **[`sqlexec`](builtins.md) binds** — the same rule, typed: a whole finite number binds as SQLite `INTEGER`, fractional/non-finite as `REAL` (nil→`NULL`, bool→`0`/`1`, string→`TEXT`, bytes→`BLOB`).
- **[Bus `send` args](bus.md)** — a whole number within ±2^53 serializes as a JSON integer (`limit=2` reaches the peer as `2`, not `2.0`); fractional values stay floats.

```mix
print(json_encode(2))            -- 2 (integer, not 2.0)
print(json_encode(2.5))          -- 2.5
print(json_encode([1, 2.5, 3]))  -- [1,2.5,3]
```
```text
2
2.5
[1,2.5,3]
```

### Non-finite numbers refuse to serialize

`print` happily shows `NaN`/`inf`, but the serializers are **loud** about a non-finite number anywhere in the value — there is no JSON or strict-data representation for it:

- **`json_encode`** raises a catchable error (0.21+; it used to encode `0` silently): `json_encode: non-finite number NaN has no JSON representation (JSON has no NaN/Infinity)`.
- **[`data_encode`](data.md)** and the strict-data writers refuse likewise (`value.rs::write_mix_data`): `data_encode: Data serialize error: non-finite number NaN has no strict-data representation`.

Keep generated config values finite, or `try`/`catch` the encode ([errors](errors.md)).

## See also

- [math](math.md) — `round` / `floor` / `ceil` / `trunc` (each takes an optional decimal-places arg; negative rounds to tens/hundreds), `abs`, `sqrt`, `pow`, `min`/`max`/`clamp`, trig, `pi()`/`e()` — all propagate `NaN`/`inf` instead of erroring
- [operators](operators.md) — arithmetic incl. `**` power, the full comparison/ordering table, `and`/`or`/`not`
- [strings](strings.md) — `..` concatenation, `${...}` interpolation, codepoint counting
- [builtins index](builtins.md) — `to_number`, `chmod` / `write_new` (mode is the f64 value), `stat` (ino/dev as strings), `sqlexec` typed binds
- [data](data.md) — `json_encode` / `data_encode` serialization (both reject non-finite numbers)
- [bus](bus.md) — `send` arg serialization (whole numbers as JSON integers)
- The repo: <https://github.com/markc/cosmix>
- `mix help` for the topic list; `mix what to_number` (or `round`, `pow`, …) for a one-line builtin reference
