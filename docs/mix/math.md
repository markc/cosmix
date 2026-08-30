# math — numeric builtins

The `math` builtin category (27 functions, added in mix 0.19.0) — pure numeric
functions over Mix's f64 numbers. List them live with `mix builtins math`;
one-line help for any single name with `mix what NAME`.

## Common semantics

All math builtins share the same rules:

- **Argument coercion** — like the rest of Mix, a numeric **string** or a **bool** is accepted and coerced (`sqrt("16")` → `4`, `abs(true)` → `1`). A genuinely non-numeric argument **raises** a runtime error (`sqrt("hello")` → error, `abs(nil)` → error).
- **Numeric-string coercion is strict** (0.21.0): a non-finite *spelling* is not a numeric string — `abs("inf")`, `abs("nan")`, and `abs("1e999")` all raise. Non-finite number *values* still flow through (next rule).
- **NaN / infinity propagate, they do not error.** Out-of-domain results follow IEEE-754: `sqrt(-1)` → `NaN`, `ln(0)` → `-inf`, `log10(-5)` → `NaN`. Mix prints these as `NaN`, `inf`, `-inf`.
- **There are no `NaN` / `inf` literals.** A bareword `NaN` or `inf` in source is a *string*, not a number (and per the strict rule above it won't coerce). Produce non-finites through arithmetic — `sqrt(-1)` for NaN, `ln(0)` for `-inf` — or the overflowing literal `1e999`, which is the f64 `inf`.
- **Whole results print integer-clean.** `round(2.5)` prints `3`, not `3.0` (Mix formats a whole f64 without a trailing `.0` — see [numbers](numbers.md)).
- **Extra arguments are ignored** by the math family (minimum-arity): `sqrt(9, "x")` → `3`, `pi(1)` → `3.141592653589793`. (This is no longer universal across builtins — since 0.21.0 `run`/`run_rc`/`http_*` reject surplus arguments loudly.)

## Rounding

```
round(x)         round to the nearest integer, half away from zero
round(x, n)      round to n decimal places
floor(x[, n])    round down  toward -inf
ceil(x[, n])     round up    toward +inf
trunc(x[, n])    truncate toward zero (drop the fraction)
```

`round` breaks ties away from zero: `round(2.5)` → `3`, `round(-2.5)` → `-3`.

The optional second argument is the number of **decimal places**. A **negative**
`n` rounds to the tens / hundreds / … place:

```
round(3.14159, 2)   -> 3.14
floor(3.999, 2)     -> 3.99
ceil(3.001, 2)      -> 3.01
trunc(3.789, 1)     -> 3.7
round(1234, -2)     -> 1200
round(1250, -2)     -> 1300
```

Details of the places argument:

- A **fractional** `n` is truncated toward zero: `round(3.14159, 2.9)` rounds to 2 places → `3.14`. Internally `n` is clamped to ±308 (the largest finite power of ten).
- A **non-finite** `n` is rejected: `round(1.9, sqrt(-1))` → error (`round() decimal-places argument must be finite, got NaN`).
- A **non-finite `x`** has no fraction to round and is returned unchanged: `round(sqrt(-1))` → `NaN`, `round(ln(0), 2)` → `-inf`.
- Coarse rounding stays correct across the whole f64 range: `round(5e19, -20)` → `100000000000000000000` (1e20 — whole numbers print full-width; Mix never prints e-notation). Scientific-notation *literals* like `5e19` are valid input since 0.20.5 — see [numbers](numbers.md).

## Sign & magnitude

```
abs(x)     absolute value
sign(x)    -1, 0, or 1   (±0 -> 0; NaN -> NaN)
```

```
abs(-7)    -> 7
sign(-3)   -> -1
sign(0)    -> 0
```

## Bitwise (v0.46.0)

```
band(a, b)   bitwise AND
bor(a, b)    bitwise OR
bxor(a, b)   bitwise exclusive-OR
bnot(x)      one's complement, 64-bit two's complement  (bnot(0) -> -1)
bshl(x, n)   shift left by n bits    (n in 0..63)
bshr(x, n)   arithmetic shift right  (sign bit replicated)
```

```
band(0o755, 0o111)  -> 73     -- an execute bit is set
band(0o644, 0o111)  -> 0      -- none is
bxor(6, 3)          -> 5
bshr(-8, 1)         -> -4     -- arithmetic, not logical
```

The everyday use is permission bits, where asking about *some* bits is the only
correct question — a filesystem may add a setgid or group bit of its own, so an
equality test against a whole mode reports a chmod that landed as one that did
not:

```mix
if band(stat($p)["perm"], 0o111) != 0 then
  print("something can execute it — ask access($p, \"x\") whether YOU can")
end
```

Mix numbers are `f64`, so these operate on **exact integers within ±2^53** —
the range an `f64` carries without loss. A fraction, an infinity, a NaN, a
magnitude outside that range, or a shift count outside `0..63` **raises**. So
does a `bshl` whose result would leave the range, *including* one that would
leave 64 bits entirely (`bshl(0x10000000000000, 12)` raises rather than
answering `0` — v0.46.1; 0.46.0 wrapped first and then range-checked the
wreckage). Nothing here truncates, wraps, or rounds silently: an operation on
bits the caller cannot see the number of is not an answer worth returning.

## Powers, roots & exponential

```
sqrt(x)            square root          (negative -> NaN)
cbrt(x)            cube root            (defined for negatives: cbrt(-27) -> -3)
pow(base, exp)     base raised to exp   (pow(2, 10) -> 1024)
exp(x)             e raised to x
```

## Logarithms

```
ln(x)              natural log, base e  (ln(0) -> -inf, ln(neg) -> NaN)
log10(x)           base-10 log          (log10(1000) -> 3)
log2(x)            base-2 log           (log2(8) -> 3)
log(x, base)       log of x in an arbitrary base  (log(81, 3) -> 4)
```

## Bounds

```
min(a, b, ...)     smallest of the arguments
min(list)          smallest element of a single list argument
max(a, b, ...)     largest of the arguments
max(list)          largest element of a single list argument
clamp(x, lo, hi)   constrain x to [lo, hi]
```

`min` / `max` are **variadic or take a single list** — `max(3, 1, 2)` and
`max([3, 1, 2])` both give `3` (a single scalar is fine too: `max(7)` → `7`).
They mirror the `<` / `>` [operator ordering](operators.md):

- **Numeric** when *every* argument coerces to a number — including numeric strings, so `min("5", "10")` → `5` (numeric comparison, and the result is normalized to a number). A stray `NaN` is **skipped** like `f64::min`/`max`: `max(1, sqrt(-1), 5)` → `5`; an all-`NaN` set stays `NaN`.
- **Lexicographic by codepoint** when every argument is a string and they don't all coerce: `max("apple", "banana")` → `"banana"`, `max("apple", "5")` → `"apple"` (`a` is codepoint 97, `5` is 53). The winning string is returned unchanged.
- A genuinely mixed set errors: `min(1, "abc")` → `min() needs all-numeric or all-string arguments (cannot compare a mix)`.
- An empty list errors: `max([])` → `max() of an empty list`.

`clamp` returns `lo` if `x < lo`, `hi` if `x > hi`, else `x`. It **errors** if
`lo > hi` (`clamp() lower bound 10 exceeds upper bound 0`) and if either bound
is `NaN` (a NaN bound would silently defeat the range check — `NaN > x` is
always false). `±inf` bounds are fine — `clamp($x, 0, 1e999)` ≡ `max($x, 0)`.
A `NaN` value of `x` is returned unchanged.

```
min(3, 1, 2)        -> 1
max([5, 2, 9])      -> 9
clamp(42, 0, 10)    -> 10
clamp(-3, 0, 10)    -> 0
```

## Distance

```
hypot(x, y)    sqrt(x*x + y*y), computed without intermediate overflow
```

```
hypot(3, 4)    -> 5
```

## Trigonometry (radians)

```
sin(x)  cos(x)  tan(x)
asin(x)  acos(x)  atan(x)      asin/acos domain is [-1, 1], else NaN
atan2(y, x)                    angle of the point (x, y), in radians
```

```
sin(0)            -> 0
cos(0)            -> 1
atan2(1, 1)       -> 0.7853981633974483   (pi/4)
```

## Constants

```
pi()    the constant pi  (3.141592653589793)
e()     Euler's number e (2.718281828459045)
```

These are zero-argument functions — call them with parentheses. Combine with the
trig functions, e.g. degrees → radians is `deg * pi() / 180`.

`e()` coexists cleanly with scientific-notation literals (`1e6`, `2e-3`,
added in 0.20.5): the lexer only consumes an `e`/`E` when a digit follows, so
`2 * e()` is never eaten as the start of `2e…` — it evaluates to
`5.43656365691809`.

## Randomness

```
random()          a float in [0.0, 1.0)
random(min, max)  an integer in [min, max] INCLUSIVE  (e.g. random(1, 6) = a d6)
```

`random()` (added in 0.23.0) has two forms and, unlike the minimum-arity math
family above, is **strict about arity** — it accepts exactly 0 or 2 arguments and
rejects 1 or 3+ loudly (`random() expects 0 args (float [0,1)) or 2 args (int min,
max)`). The two-argument form requires **integer** bounds (a fractional bound like
`random(1.5, 3)` errors), `min <= max` (`random(6, 1)` errors), and both endpoints
are reachable (the range is inclusive of `max`). Bounds must be within `±(2^53 − 1)`
— Mix numbers are f64, so an integer past that ceiling isn't exactly
representable and is rejected rather than returned lossily. 2^53 itself is
refused since 0.59.0: it is also what 2^53 + 1 rounds to, so accepting it
would accept an aliased bound the caller never wrote.

```
random()          -> 0.6009269054001612
random(1, 6)      -> 3            (1..=6, each equally likely)
random(5, 5)      -> 5            (a single-point range is fine)
random(-3, -1)    -> -2           (negative ranges work)
```

`random` is drawn from the **thread-local RNG** (auto-seeded from OS entropy) — fast
enough for tight loops, but **non-deterministic and NOT cryptographically strong**.
For anything security-sensitive (passwords, tokens) use
[`random_password`](system.md) or `uuid`, which draw from a cryptographically
secure RNG (`random_password` uses `OsRng`; `uuid` uses `getrandom`).

## Notes

`min`, `max`, `abs` and `clamp` replaced the old `prelude.mix` shims of the same
names — **a builtin shadows any same-named Mix function**, so defining your own
`function abs($x)` has no effect on `abs(-7)`. They preserve the value/ordering
the shims selected, with two refinements: a numeric result is **normalized to a
number** (a numeric-string or bool argument no longer round-trips with its
original type — `abs("5")` → `5`, not `"5"`), and `clamp` errors on an inverted
`lo > hi` range or a `NaN` bound instead of returning a meaningless value.

Every math builtin is capability class **`Pure`** — no host authority, always
allowed even in a sandboxed embedding (see [capabilities](capabilities.md)).

Division and modulo are *operators*, not math builtins: `5 / 0` and `5 % 0`
both raise a runtime error (modulo-by-zero became loud in 0.20.5) — see
[operators](operators.md).

## See also

- [numbers](numbers.md) — the f64 model, integer-clean printing, radix and scientific-notation literals, numeric strings and `to_number`
- [operators](operators.md) — arithmetic, `%`, and the `<` / `>` ordering rule that `min`/`max` mirror
- [builtins](builtins.md) — the full categorized builtin index
- [capabilities](capabilities.md) — the `Pure` class every math builtin carries
- `mix builtins math` lists every math builtin with its one-line description; `mix what NAME` gives one-line help for a single name; `mix help` is the full categorized reference
