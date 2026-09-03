# Regular expressions

Mix ships four regex builtins — `regex_match`, `regex_find`, `regex_replace`,
`regex_split` — built on the Rust [`regex`](https://docs.rs/regex) crate. The
`mix` binary compiles with the `regex` feature **on**, so they're always
available; a library build without that feature won't have them. The plain
string ops `replace` / `split` / `contains` are **always present** (not
feature-gated) and treat their needle **literally** — `split("a1b22c",
"[0-9]+")` does not split — so reach for them first when the needle is a fixed
substring. `grep(pattern, text)` is also always present and regex-powered
whenever the feature is on (see [grep](#grep--regex-line-filter-always-present)).

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
print(regex_match("[0-9]+", "abc123"))
print(regex_find("[0-9]+", "a1 b22 c333"))
print(regex_replace("[aeiou]", "hello world", "_"))
print(regex_split("\\s*,\\s*", "a, b ,c,  d"))
```
```text
true
[{match: 1, start: 1, end: 2}, {match: 22, start: 4, end: 6}, {match: 333, start: 8, end: 11}]
h_ll_ w_rld
[a, b, c, d]
```

## The four builtins

| Call | Returns | Notes |
|---|---|---|
| `regex_match(pattern, text)` | **bool** | true if `pattern` matches *anywhere* in `text` (not anchored) |
| `regex_find(pattern, text)` | **list of maps** | every non-overlapping match; `[]` when none |
| `regex_replace(pattern, text, repl)` | **string** | replaces **all** matches; `$1`/`${name}` backrefs in `repl` |
| `regex_split(pattern, text)` | **list of strings** | splits on each match; keeps empty leading/trailing parts |

All four **compile the pattern fresh on every call** and **raise a catchable
runtime error** on an invalid pattern (see [Errors](#errors-and-validation)).
There is no precompile/cache step — for a hot loop, hoist the work, not the
pattern (the crate has no Mix-visible compiled handle yet).

Arity is checked as a **minimum**: a missing argument raises, but surplus
arguments are **silently ignored** (`regex_match("b", "abc", "extra")` →
`true`). Non-string arguments are coerced with the standard string coercion —
`regex_match(42, "x420")` matches the pattern `42` → `true`.

### regex_match — does it match?

`regex_match` is a predicate. The pattern is **unanchored** — it matches if it
occurs anywhere — so add `^` / `$` yourself to anchor.

```mix
print(regex_match("[0-9]+", "abc123"))   -- substring match -> true
print(regex_match("^[0-9]+$", "abc123")) -- fully anchored  -> false
print(regex_match("^abc$", "abcd"))      -- anchored        -> false
print(regex_match("(?i)hello", "HELLO")) -- inline flag     -> true
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
if regex_match("^[^@]+@[^@]+\\.[^@]+$", $email) then
  print("looks like an address")
else
  print("rejected")
end
```
```text
looks like an address
```

### regex_find — every match, with offsets and captures

`regex_find` returns a **list of maps**, one per non-overlapping match, left to
right. Each map always has `match`, `start`, `end`; if the pattern has capture
groups it also gets a `groups` list (group 1.. — group 0 is `match`).

```mix
print(regex_find("[0-9]+", "a1 b22 c333"))
```
```text
[{match: 1, start: 1, end: 2}, {match: 22, start: 4, end: 6}, {match: 333, start: 8, end: 11}]
```

With capture groups, `groups` carries each `(...)` in order:

```mix
print(regex_find("(\\w+)@(\\w+)", "joe@acme jane@corp"))
```
```text
[{match: joe@acme, start: 0, end: 8, groups: [joe, acme]}, {match: jane@corp, start: 9, end: 18, groups: [jane, corp]}]
```

An optional group that didn't participate comes back as `nil` in `groups`
(verified — the second match has no `px`):

```mix
$r = regex_find("(\\d+)(px)?", "10px 20")
print($r)
```
```text
[{match: 10px, start: 0, end: 4, groups: [10, px]}, {match: 20, start: 5, end: 7, groups: [20, nil]}]
```

Field access is the usual map syntax — `["match"]` or `.start`
([collections](collections.md)):

```mix
$m = regex_find("[0-9]+", "id=42")
print($m[0]["match"])
print($m[0].start)
```
```text
42
3
```

> **`start`/`end` are BYTE offsets**, not codepoint indices. They're raw byte
> positions into the UTF-8 text — they line up with `byte_*` string ops, not
> with `substr`/`length` (which count codepoints, see [strings](strings.md)).
> Watch this with non-ASCII:

```mix
print(regex_find("über", "café über"))
-- "café" is 5 BYTES (é = 2), so the space is byte 5 and "über" starts at byte 6
```
```text
[{match: über, start: 6, end: 11}]
```

No match → an **empty list**, never `nil`. Test with `is_empty` or `length`:

```mix
$r = regex_find("z+", "abc")
print($r)
if is_empty($r) then
  print("no match")
end
```
```text
[]
no match
```

### regex_replace — substitute all matches

`regex_replace(pattern, text, replacement)` replaces **every** match (it's
`replace_all` under the hood) and returns the new string. The replacement
supports the crate's backreference syntax: **`$1` … `$N`** for numbered groups
and **`${name}`** for named groups.

```mix
print(regex_replace("[aeiou]", "hello world", "_"))
print(regex_replace("(\\w+)@(\\w+)", "joe@acme", "$2.$1"))
print(regex_replace("(?i)the", "The theory", "X"))
```
```text
h_ll_ w_rld
acme.joe
X Xory
```

Note the last one: `(?i)the` matched both the standalone `The` **and** the
`the` inside `theory` — `regex_replace` is unanchored and global, so use `\b`
word boundaries when you mean whole words.

**Named groups** use `(?P<name>...)` — or the shorter `(?<name>...)` — in the
pattern and `${name}` in the replacement. But `${name}` is exactly what a Mix
double-quoted string interpolates, so **single-quote the replacement** (or it's
eaten before the regex sees it — see [Quoting traps](#quoting-traps)):

```mix
print(regex_replace('(?P<user>\w+)@(?P<host>\w+)', 'joe@acme', '${host}/${user}'))
```
```text
acme/joe
```

To insert a **literal `$`**, double it: `$$`.

Two more replacement-parser rules (both verified):

- **Group names parse greedily.** `$1x` reads as one reference to the (nonexistent) group *named* `1x` — `regex_replace('(\d+)', '10', '$1x')` returns `""`, not `10x`. Brace the number whenever a word character follows: `'${1}x'` → `10x`. (`'$2.$1'` works only because `.` isn't a word char.)
- **An unknown group substitutes the empty string**, never an error — a typoed backref silently deletes the match.

### regex_split — split on a pattern

`regex_split(pattern, text)` splits `text` at each match and returns the pieces
as a list. Empty leading/trailing/adjacent pieces are **kept** (unlike a naive
trim), and a pattern that never matches yields the whole string as one element.

```mix
print(regex_split("\\s*,\\s*", "a, b ,c,  d"))  -- comma + flexible spaces
print(regex_split("[0-9]+", "ax12by34cz"))      -- split on digit runs
print(regex_split(",", ",a,,b,"))               -- empties preserved
print(regex_split(",", "no-commas"))            -- no match -> whole string
print(regex_split("", "abc"))                   -- empty match at every boundary
```
```text
[a, b, c, d]
[ax, by, cz]
[, a, , b, ]
[no-commas]
[, a, b, c, ]
```

### grep — regex line filter (always present)

`grep(pattern, text)` returns the **lines** of `text` that match `pattern`, as
a list of strings (`[]` when none). Unlike the `regex_*` four it is **not**
feature-gated — but when the `regex` feature is on (always, in the `mix`
binary) the pattern is a full regex with the same syntax and the same
invalid-pattern error; a library build without the feature falls back to a
literal substring test per line.

```mix
$t = "alpha\nbeta\ngamma\nbravo"
print(grep("^b", $t))
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

`\d`/`\w` are **Unicode by default**, so accented letters count as word chars:

```mix
print(regex_find("\\w+", "café über"))
```
```text
[{match: café, start: 0, end: 5}, {match: über, start: 6, end: 11}]
```

Matching is codepoint-exact with **no Unicode normalization**: precomposed `é`
(U+00E9) does not match decomposed `e` + combining acute —
`regex_match("é", "e\u{0301}")` → `false` — so normalize first when inputs mix
forms (same rule as the plain byte-exact string ops, [strings](strings.md)).
Case-insensitive `(?i)` *does* full Unicode case-folding: `(?i)é` matches `É`.

Multiline `^`/`$` (the `(?m)` flag) anchor per line:

```mix
print(regex_find('(?m)^\w+', "alpha\nbeta\ngamma"))
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
print(regex_match("\\d+", "x42"))   -- double quotes: \\ -> \
print(regex_match('\d+', "x42"))    -- single quotes: \ literal, same result
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
print(regex_replace('(?P<u>\w+)@(?P<h>\w+)', 'joe@acme', "${h}/${u}"))
-- RIGHT: single quotes keep ${h}/${u} literal for the regex crate
print(regex_replace('(?P<u>\w+)@(?P<h>\w+)', 'joe@acme', '${h}/${u}'))
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
print(regex_match("[unterminated", "x"))
```
```text
Runtime error at line 1: invalid regex '[unterminated': regex parse error:
    [unterminated
    ^
error: unclosed character class
```

**Long or multi-line patterns get a truncated diagnostic** (0.63.0). A
pattern over 80 characters — almost always a **swapped call** that passed
the *subject* as argument 1 — is echoed truncated with only the regex
engine's final `error:` line, plus a hint naming the usual cause, instead
of printing the whole document back:

```text
Runtime error at line 2: invalid regex 'line of roster text line of …' (8020 chars, truncated): error: repetition quantifier expects a valid decimal
  (argument 1 is the PATTERN — the subject string comes after it; a swapped
  argument order is the usual cause of a huge pattern: see mix man regex)
```

Remember the order: **pattern first, subject second** — a swapped call
where both compile is *silent* (`regex_match($text, $pattern)` is just a
"no"), which is why the argument order is worth pinning in muscle memory
now and why subject-first `re_*` names are planned.

Wrap user-supplied patterns in `try`/`catch` so a bad pattern doesn't abort the
script ([errors](errors.md)):

```mix
try
  print(regex_match($user_pattern, $text))
catch $e
  print("bad pattern: " .. ("" .. $e))
end
```

Wrong arity is a runtime error too:

```mix
print(regex_match("a"))
```
```text
Runtime error at line 1: regex_match() expects at least 2 argument(s), got 1
```

## Limitations

The Rust `regex` crate guarantees linear-time matching by **not** supporting
features that require backtracking. Both raise a compile error, not a silent
mismatch:

```mix
print(regex_match("(a)\\1", "aa"))      -- backreference
print(regex_match("foo(?=bar)", "foobar")) -- lookahead
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
pattern compile), or extract groups with `regex_find` then compare in Mix code.

There is also no compiled-pattern value type — every call recompiles. For most
scripting that's fine; for a tight inner loop, prefer one `regex_find` over the
whole input to a per-element `regex_match`.

## Recipes

**Extract numbers and sum them** — `regex_find` + the [HOF](hof.md) builtins
`map`/`reduce`:

```mix
$ms = regex_find('\d+', 'a1 b22 c333')
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
$ms = regex_find('(?P<k>\w+)=(?P<v>\S+)', 'host=node1 port=8443 tls=on')
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
print(regex_replace('\s+', "  too    much\twhitespace ", " "))
```
```text
 too much whitespace 
```

**Redact something that looks like a secret** before logging:

```mix
print(regex_replace('(?i)(token|password)=\S+', "user=joe token=abc123 ok", "$1=***"))
```
```text
user=joe token=*** ok
```

## See also

- [strings](strings.md) — quoting (`'raw'` vs `"${...}"`), codepoint vs byte ops; read this *first* for regex patterns
- [collections](collections.md) — the `{match, start, end, groups}` maps and `groups` lists `regex_find` returns
- [functions](functions.md) / [hof](hof.md) — lambdas and `map`/`filter`/`reduce` for processing match lists
- [control-flow](control-flow.md), [errors](errors.md) — `if`/`try`/`catch` around match predicates and bad patterns
- [builtins](builtins.md) — full builtin index; [the manual index](README.md)
- Rust [`regex` crate syntax reference](https://docs.rs/regex/latest/regex/#syntax) — the authoritative pattern grammar (RE2-style)
- [RE2 design notes](https://github.com/google/re2/wiki/WhyRE2) — why linear-time matching means no backreferences/look-around
- `mix help` for the builtin overview; `mix what regex_match` (etc.) for the one-line signatures — but note `mix what regex_find`'s one-liner ("first regex match, or nil") is **stale**: it actually returns a list of *all* matches, `[]` when none

