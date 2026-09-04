# Regular expressions

Mix ships five regex builtins — `re_match`, `re_find`, `re_replace`,
`re_split`, `grep_lines` — built on the Rust
[`regex`](https://docs.rs/regex) crate, all taking their **subject first**
like every literal-string builtin. The `mix` binary compiles with the
`regex` feature **on**, so they're always available; a library build
without that feature won't have the four `re_*` (and `grep_lines` falls
back to a literal substring test per line). The plain string ops `replace`
/ `split` / `contains` are **always present** (not feature-gated) and treat
their needle **literally** — `split("a1b22c", "[0-9]+")` does not split —
so reach for them first when the needle is a fixed substring.

> **History.** From 0.2.0 to 0.72.0 the family was pattern-first
> (`regex_match(pattern, s)`, plus `grep`), which silently inverted at
> swapped call sites — both arguments are strings, so a swapped call is a
> plausible "no", never an error; it overwrote two journals with regex
> text on one occasion. The subject-first `re_*` names landed in 0.63.0,
> every caller in the fleet was migrated, and the five legacy names were
> **deleted in release B (0.73.0)** once the fleet-wide inventory read
> zero. A surviving legacy call now fails loudly: `MIX-E1102` at lint
> time, `undefined function` at runtime.

The flavor is **RE2-style**: linear-time matching, full Unicode support, but
**no backreferences and no look-around** (a deliberate trade for guaranteed
linear time — see [Limitations](#limitations)). Patterns and replacements are
ordinary [strings](strings.md), so the quoting rules there apply directly and
bite hard — read [Quoting traps](#quoting-traps) before you write a `$`-bearing
pattern.

> One-line mental model: **a regex is just a string argument**; matches come
> back as structured maps (`{match, start, end, groups}`), not as magic `$~`
> globals.

```mix
print(re_match("abc123", "[0-9]+"))
print(re_find("a1 b22 c333", "[0-9]+"))
print(re_replace("hello world", "[aeiou]", "_"))
print(re_split("a, b ,c,  d", "\\s*,\\s*"))
```
```text
true
[{match: 1, start: 1, end: 2}, {match: 22, start: 4, end: 6}, {match: 333, start: 8, end: 11}]
h_ll_ w_rld
[a, b, c, d]
```

## The family

| Call | Returns | Notes |
|---|---|---|
| `re_match(s, pattern)` | **bool** | true if `pattern` matches *anywhere* in `s` (not anchored) |
| `re_find(s, pattern)` | **list of maps** | every non-overlapping match, `{match, start, end[, groups]}` — **codepoint** offsets, so `start`/`end` compose with `substr`/`slice`/`index_of`; `[]` when none |
| `re_replace(s, pattern, repl)` | **string** | replaces **all** matches; `$1`/`${name}` backrefs in `repl` |
| `re_split(s, pattern)` | **list of strings** | splits on each match; keeps empty leading/trailing parts |
| `grep_lines(text, pattern)` | **list of strings** | the lines of `text` matching `pattern`; `[]` when none |

All five **compile the pattern fresh on every call** and **raise a catchable
runtime error** on an invalid pattern (see [Errors](#errors-and-validation)).
There is no precompile/cache step — for a hot loop, hoist the work, not the
pattern (the crate has no Mix-visible compiled handle yet).

Arity is checked as a **minimum**: a missing argument raises, but surplus
arguments are **silently ignored** (`re_match("abc", "b", "extra")` →
`true`). Non-string arguments are coerced with the standard string coercion —
`re_match("x420", 42)` matches the pattern `42` → `true`.

### re_match — does it match?

`re_match` is a predicate. The pattern is **unanchored** — it matches if it
occurs anywhere — so add `^` / `$` yourself to anchor.

```mix
print(re_match("abc123", "[0-9]+"))   -- substring match -> true
print(re_match("abc123", "^[0-9]+$")) -- fully anchored  -> false
print(re_match("abcd", "^abc$"))      -- anchored        -> false
print(re_match("HELLO", "(?i)hello")) -- inline flag     -> true
```
```text
true
false
false
true
```

Use it straight in an `if` ([control flow](control-flow.md)):

```mix
$email = "joe@example.com"
if re_match($email, "^[^@]+@[^@]+\\.[^@]+$") then
  print("looks like an address")
else
  print("rejected")
end
```
```text
looks like an address
```

### re_find — every match, with offsets and captures

`re_find` returns a **list of maps**, one per non-overlapping match, left to
right. Each map always has `match`, `start`, `end`; if the pattern has capture
groups it also gets a `groups` list (group 1.. — group 0 is `match`).

```mix
print(re_find("a1 b22 c333", "[0-9]+"))
```
```text
[{match: 1, start: 1, end: 2}, {match: 22, start: 4, end: 6}, {match: 333, start: 8, end: 11}]
```

With capture groups, `groups` carries each `(...)` in order:

```mix
print(re_find("joe@acme jane@corp", "(\\w+)@(\\w+)"))
```
```text
[{match: joe@acme, start: 0, end: 8, groups: [joe, acme]}, {match: jane@corp, start: 9, end: 18, groups: [jane, corp]}]
```

An optional group that didn't participate comes back as `nil` in `groups`
(verified — the second match has no `px`):

```mix
$r = re_find("10px 20", "(\\d+)(px)?")
print($r)
```
```text
[{match: 10px, start: 0, end: 4, groups: [10, px]}, {match: 20, start: 5, end: 7, groups: [20, nil]}]
```

Field access is the usual map syntax — `["match"]` or `.start`
([collections](collections.md)):

```mix
$m = re_find("id=42", "[0-9]+")
print($m[0]["match"])
print($m[0].start)
```
```text
42
3
```

> **`start`/`end` are CODEPOINT indices** — the same unit `substr`, `slice`
> and `index_of` count in, so `substr($s, $m[0].start, 1)` is always the
> first matched character, non-ASCII included. (The deleted legacy
> `regex_find` answered in raw UTF-8 *byte* offsets, which silently broke
> that composition on any input with an `é` in it.)

```mix
print(re_find("café über", "über"))
-- "café " is 5 CODEPOINTS (é is one), so "über" starts at 5
```
```text
[{match: über, start: 5, end: 9}]
```

No match → an **empty list**, never `nil`. Test with `is_empty` or `length`:

```mix
$r = re_find("abc", "z+")
print($r)
if is_empty($r) then
  print("no match")
end
```
```text
[]
no match
```

### re_replace — substitute all matches

`re_replace(s, pattern, replacement)` replaces **every** match (it's
`replace_all` under the hood) and returns the new string. The replacement
supports the crate's backreference syntax: **`$1` … `$N`** for numbered groups
and **`${name}`** for named groups.

```mix
print(re_replace("hello world", "[aeiou]", "_"))
print(re_replace("joe@acme", "(\\w+)@(\\w+)", "$2.$1"))
print(re_replace("The theory", "(?i)the", "X"))
```
```text
h_ll_ w_rld
acme.joe
X Xory
```

Note the last one: `(?i)the` matched both the standalone `The` **and** the
`the` inside `theory` — `re_replace` is unanchored and global, so use `\b`
word boundaries when you mean whole words.

**Named groups** use `(?P<name>...)` — or the shorter `(?<name>...)` — in the
pattern and `${name}` in the replacement. But `${name}` is exactly what a Mix
double-quoted string interpolates, so **single-quote the replacement** (or it's
eaten before the regex sees it — see [Quoting traps](#quoting-traps)):

```mix
print(re_replace('joe@acme', '(?P<user>\w+)@(?P<host>\w+)', '${host}/${user}'))
```
```text
acme/joe
```

To insert a **literal `$`**, double it: `$$`.

Two more replacement-parser rules (both verified):

- **Group names parse greedily.** `$1x` reads as one reference to the (nonexistent) group *named* `1x` — `re_replace('10', '(\d+)', '$1x')` returns `""`, not `10x`. Brace the number whenever a word character follows: `'${1}x'` → `10x`. (`'$2.$1'` works only because `.` isn't a word char.)
- **An unknown group substitutes the empty string**, never an error — a typoed backref silently deletes the match.

### re_split — split on a pattern

`re_split(s, pattern)` splits `s` at each match and returns the pieces
as a list. Empty leading/trailing/adjacent pieces are **kept** (unlike a naive
trim), and a pattern that never matches yields the whole string as one element.

```mix
print(re_split("a, b ,c,  d", "\\s*,\\s*"))  -- comma + flexible spaces
print(re_split("ax12by34cz", "[0-9]+"))      -- split on digit runs
print(re_split(",a,,b,", ","))               -- empties preserved
print(re_split("no-commas", ","))            -- no match -> whole string
print(re_split("abc", ""))                   -- empty match at every boundary
```
```text
[a, b, c, d]
[ax, by, cz]
[, a, , b, ]
[no-commas]
[, a, b, c, ]
```

### grep_lines — regex line filter (always present)

`grep_lines(text, pattern)` returns the **lines** of `text` that match
`pattern`, as a list of strings (`[]` when none). Unlike the `re_*` four it
is **not** feature-gated — but when the `regex` feature is on (always, in
the `mix` binary) the pattern is a full regex with the same syntax and the
same invalid-pattern error; a library build without the feature falls back
to a literal substring test per line.

```mix
$t = "alpha\nbeta\ngamma\nbravo"
print(grep_lines($t, "^b"))
```
```text
[beta, bravo]
```

## Syntax flavor

The patterns are Rust-`regex` syntax — see the
[full grammar](https://docs.rs/regex/latest/regex/#syntax). The common pieces:

| Construct | Meaning |
|---|---|
| `.` `*` `+` `?` `{n}` `{n,m}` | any-char, repetition (greedy; add `?` for lazy: `+?`) |
| `[...]` `[^...]` | character class / negated class |
| `\d \w \s` (+ `\D \W \S`) | digit / word / whitespace classes (**Unicode-aware**) |
| `\b \B` | word / non-word boundary |
| `^` `$` | start / end (of text; of line with the `m` flag) |
| `(...)` | capturing group |
| `(?:...)` | non-capturing group |
| `(?P<name>...)` / `(?<name>...)` | named capturing group (both spellings work) |
| `(?i)` `(?m)` `(?s)` `(?x)` | inline flags: ignore-case, multiline, dot-all, verbose |
| `a\|b` | alternation |

Inline flag groups also scope: `(?i:abc)` is case-insensitive only inside.

`\d`/`\w` are **Unicode by default**, so accented letters count as word chars
(offsets in codepoints — `é` is one):

```mix
print(re_find("café über", "\\w+"))
```
```text
[{match: café, start: 0, end: 4}, {match: über, start: 5, end: 9}]
```

Matching is codepoint-exact with **no Unicode normalization**: precomposed `é`
(U+00E9) does not match decomposed `e` + combining acute —
`re_match("e\u{0301}", "é")` → `false` — so normalize first when inputs mix
forms (same rule as the plain byte-exact string ops, [strings](strings.md)).
Case-insensitive `(?i)` *does* full Unicode case-folding: `(?i)é` matches `É`.

Multiline `^`/`$` (the `(?m)` flag) anchor per line:

```mix
print(re_find("alpha\nbeta\ngamma", '(?m)^\w+'))
```
```text
[{match: alpha, start: 0, end: 5}, {match: beta, start: 6, end: 10}, {match: gamma, start: 11, end: 16}]
```

## Quoting traps

This is where most regex bugs in Mix come from. A pattern is a string, and Mix
string rules ([strings](strings.md)) run **before** the regex crate sees a
single byte.

**1. Backslashes.** In a `"double-quoted"` Mix string you escape each backslash:
`"\\d"` is the two characters `\d` that the regex wants. In a `'single-quoted'`
string a backslash is **literal**, so `'\d'` is already `\d` — single quotes are
usually the cleaner choice for a regex.

```mix
print(re_match("x42", "\\d+"))   -- double quotes: \\ -> \
print(re_match("x42", '\d+'))    -- single quotes: \ literal, same result
```
```text
true
true
```

**2. `${...}` in double quotes interpolates.** A double-quoted `"${host}"` is a
scope/env lookup that resolves to `nil` if unset — *before* the regex engine
runs. This silently corrupts a named-group replacement:

```mix
-- WRONG: "${host}" interpolates to nil at string-build time
print(re_replace('joe@acme', '(?P<u>\w+)@(?P<h>\w+)', "${h}/${u}"))
-- RIGHT: single quotes keep ${h}/${u} literal for the regex crate
print(re_replace('joe@acme', '(?P<u>\w+)@(?P<h>\w+)', '${h}/${u}'))
```
```text
nil/nil
acme/joe
```

A **bare** `$1` (no braces) is *literal* in a double-quoted Mix string, so
numbered backrefs happen to survive double quotes — but `${name}` does not.
**Rule of thumb: single-quote regex patterns and replacements** and you sidestep
both traps. (`$(...)` is also literal in a Mix string — it does not run a
command here.)

## Errors and validation

An invalid pattern, or an unsupported construct, raises a **catchable** runtime
error and exits non-zero from `-c`:

```mix
print(re_match("x", "[unterminated"))
```
```text
Runtime error at line 1: invalid regex '[unterminated': regex parse error:
    [unterminated
    ^
error: unclosed character class
```

**Long or multi-line patterns get a truncated diagnostic** (0.63.0). A
pattern over 80 characters — almost always a **swapped call** that passed
the *subject* as the pattern — is echoed truncated with only the regex
engine's final `error:` line, plus a hint naming the usual cause, instead
of printing the whole document back:

```text
Runtime error at line 2: invalid regex 'line of roster text line of …' (8020 chars, truncated): error: repetition quantifier expects a valid decimal
  (in the re_* family the SUBJECT comes first and the PATTERN second — a
  swapped argument order is the usual cause of a huge pattern: see mix man regex)
```

A swapped call where both compile is *silent* (`re_match($pattern, $text)`
is just a "no") — but subject-first matches every other string builtin, so
the muscle memory that writes `replace($s, …)` writes `re_replace($s, …)`
correctly too. That consistency is what retired the legacy family's trap.

Wrap user-supplied patterns in `try`/`catch` so a bad pattern doesn't abort the
script ([errors](errors.md)):

```mix
try
  print(re_match($text, $user_pattern))
catch $e
  print("bad pattern: " .. ("" .. $e))
end
```

Wrong arity is a runtime error too:

```mix
print(re_match("a"))
```
```text
Runtime error at line 1: re_match() expects at least 2 argument(s), got 1
```

## Limitations

The Rust `regex` crate guarantees linear-time matching by **not** supporting
features that require backtracking. Both raise a compile error, not a silent
mismatch:

```mix
print(re_match("aa", "(a)\\1"))         -- backreference
print(re_match("foobar", "foo(?=bar)")) -- lookahead
```
```text
Runtime error at line 1: invalid regex '(a)\1': regex parse error:
    (a)\1
       ^^
error: backreferences are not supported
```
```text
Runtime error at line 1: invalid regex 'foo(?=bar)': regex parse error:
    foo(?=bar)
       ^^^
error: look-around, including look-ahead and look-behind, is not supported
```

If you reach for a backreference, usually a non-regex approach works:
`contains`/`split`/`replace`/`starts_with` for fixed substrings (byte-exact, no
pattern compile), or extract groups with `re_find` then compare in Mix code.

There is also no compiled-pattern value type — every call recompiles. For most
scripting that's fine; for a tight inner loop, prefer one `re_find` over the
whole input to a per-element `re_match`.

## Recipes

**Extract numbers and sum them** — `re_find` + the [HOF](hof.md) builtins
`map`/`reduce`:

```mix
$ms = re_find('a1 b22 c333', '\d+')
$nums = map($ms, fn($m) = to_number($m["match"]))
print($nums)
print(reduce($nums, 0, fn($a, $b) = $a + $b))
```
```text
[1, 22, 333]
356
```

**Parse `key=value` lines** with named groups:

```mix
$ms = re_find('host=node1 port=8443 tls=on', '(?P<k>\w+)=(?P<v>\S+)')
$out = map($ms, fn($m) = $m["groups"][0] .. " -> " .. $m["groups"][1])
print(join($out, "\n"))
```
```text
host -> node1
port -> 8443
tls -> on
```

**Normalize whitespace** — collapse any run of spaces/tabs/newlines to one space:

```mix
print(re_replace("  too    much\twhitespace ", '\s+', " "))
```
```text
 too much whitespace 
```

**Redact something that looks like a secret** before logging:

```mix
print(re_replace("user=joe token=abc123 ok", '(?i)(token|password)=\S+', "$1=***"))
```
```text
user=joe token=*** ok
```

## See also

- [strings](strings.md) — quoting (`'raw'` vs `"${...}"`), codepoint vs byte ops; read this *first* for regex patterns
- [collections](collections.md) — the `{match, start, end, groups}` maps and `groups` lists `re_find` returns
- [functions](functions.md) / [hof](hof.md) — lambdas and `map`/`filter`/`reduce` for processing match lists
- [control-flow](control-flow.md), [errors](errors.md) — `if`/`try`/`catch` around match predicates and bad patterns
- [builtins](builtins.md) — full builtin index; [the manual index](README.md)
- Rust [`regex` crate syntax reference](https://docs.rs/regex/latest/regex/#syntax) — the authoritative pattern grammar (RE2-style)
- [RE2 design notes](https://github.com/google/re2/wiki/WhyRE2) — why linear-time matching means no backreferences/look-around
- `mix help` for the builtin overview; `mix what re_match` (etc.) for the one-line signatures
