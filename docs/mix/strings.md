# Strings

Mix has two string literal forms, `..` for concatenation, and a deliberately
**codepoint-based** core with explicit byte and grapheme twins. This page covers
the lexer rules (what interpolates, what stays literal, the escapes) and the
string-category [builtins](builtins.md). Every example below was run on **Mix
0.21.2**; the output shown is real.

> Mental model: `"double"` interpolates `${...}` and expands a leading `~`;
> `'single'` is fully raw; `..` joins; `length`/`pos`/`substr` count **Unicode
> codepoints**, with `byte_*` and `grapheme_*` twins when you need the other units.

## The two literal forms

```mix
$name = "world"
print("hello ${name}")          -- ${...} interpolates
print("hello $name")            -- bare $name is LITERAL (unlike bash!)
print('raw ${name} $(date)')    -- single-quote: nothing interpolates
```
```text
hello world
hello $name
raw ${name} $(date)
```

The split mirrors bash's quoting, with one trap that bites everyone:

| Form | Interpolates `${x}` | Bare `$x` | `$(...)` | leading `~` | Escapes |
|---|---|---|---|---|---|
| `"double"` | yes (scope → env) | **literal text** | **literal text** | expands to `$HOME` | `\n \t \r \e \" \\ \$ \~ \u{…}` |
| `'single'` | no | literal | literal | literal | only `\'` and `\\` |

**Only `${...}` interpolates** inside double quotes. A bare `$name` is the literal
three-or-more characters `$name` — the opposite of bash, and the single most common
error an agent makes here. When unsure, prefer `..` concat (below) over
interpolation.

`$(...)` and `$((...))` are **literal text** inside a Mix string — the old
build-time command-substitution footgun was removed. To splice a command's output,
use [`run`/`run_rc`](system.md) plus `..`. (Two exceptions still substitute: a
[heredoc](#heredocs) body, and the standalone `$(cmd)` expression — neither is a
plain string literal.)

Know the two contexts. The literal rule above is about **Mix string literals**. In
a `run`/`run_rc`/`ssh_run` *command string*, a literal `$(...)` passes through to
the target shell — so it substitutes in the `/bin/sh` that actually runs the
command: local for `run`/`run_rc`, the **remote** shell for `ssh_run`:

```mix
print(run("echo $(echo passthru)"))   -- /bin/sh does the substitution
```
```text
passthru
```

And on a *shell-dispatch* line (`mix -c`'s shell branch, the login shell), `$(...)`
**is** command substitution — see [shell mode](shell-mode.md).

### Interpolation walks scope, then env

`${X}` looks up the name in the local scope first, then the process environment:

```mix
print("val=${MYVAR}")
```
```text
# run as:  MYVAR=fromenv mix -c '...'
val=fromenv
```

A name unbound in **both** scope and env is a **runtime error** — the same as
reading a bare `$X`, so a typo is caught loudly rather than silently splicing
text:

```mix
print("[${NOPE_NOT_SET}]")
```
```text
Runtime error: undefined variable '$NOPE_NOT_SET' in interpolation (use ${NOPE_NOT_SET ?? default} for a fallback)
```

A variable whose **value** is nil is bound, so it still renders the literal
`nil` (`$x = nil` → `${x}` → `nil`); likewise a missing map key in a dotted path
(`${a.b}` → `nil`), matching `$a.b`.

### Defaults — `${x ?? default}` and `${x ?: default}`

Supply a fallback with the same coalescing operators Mix uses in expressions.
The default is any Mix expression, evaluated **only** when the fallback fires:

```mix
print("[${NOPE ?? "none"}]")     -- unbound → the default (no error)
$x = nil
print("[${x ?? $home}]")          -- nil value → default may be a variable…
print("[${x ?? upper("hi")}]")    -- …or any expression
```
```text
[none]
[/home/you]
[HI]
```

- `??` fires on **nil only** (an unbound name or a nil value). `${e ?? "x"}`
  where `$e` is `""` keeps the empty string — `??` does not treat it as missing.
- `?:` fires on any **falsy** value (`nil`, `""`, `0`, `false`, `[]`) — use it
  when you want bash's `${x:-…}` "empty also falls back" behaviour.
- Limitation: a default cannot contain a literal `}` (the scanner ends at the
  first `}`) — bind it to a variable first.

For an explicit, unambiguous environment read use `env("X")` (see
[builtins](builtins.md)); for scope-only data prefer `..` concat.

## Concatenation is `..`

Join with `..` — never `+` (that's numeric addition, see [numbers](numbers.md)) and
never `.` (that's map/field access). Non-string operands are stringified:

```mix
$x = 42
print("answer is " .. $x .. "!")
```
```text
answer is 42!
```

`..` is the most robust way to build strings — it sidesteps the bare-`$name`
interpolation trap entirely:

```mix
$user = "ada"
print("hi " .. $user)           -- always works
print("hi $user")               -- literal "hi $user" — usually NOT what you meant
```
```text
hi ada
hi $user
```

## Escapes and `\u{XXXX}`

Double-quoted strings honour `\n \t \r`, `\e` (ESC, `\x1b`), `\" \\ \$ \~`, and the
braced unicode escape `\u{XXXX}` (1–6 hex digits):

```mix
print("tab\there")
print("price: \$5")             -- \$ for a literal dollar
print("\u{2764} love")          -- U+2764 HEAVY BLACK HEART
print("strip BOM:[\u{FEFF}]")   -- zero-width no-break space
```
```text
tab	here
price: $5
❤ love
strip BOM:[﻿]
```

`\u{…}` is **braced-only**: a bare `\u` with no `{` stays literal, so a Windows path
`"C:\users"` or an embedded JSON `\uXXXX` is unchanged. Single-quoted strings keep
`\u{…}` literal too. A surrogate or out-of-range codepoint is a loud lex error
(`\u{D800} is not a valid unicode codepoint`), never a silent pass-through.

Any **unrecognised** escape keeps the backslash literally: `"\d"` is the two
characters `\d`, not an error. Strip a BOM with the real codepoint:
`replace($s, "\u{FEFF}", "")`.

## Leading `~` expansion

A `~` at the **very start** of a double-quoted string expands to `$HOME` at
runtime, and only when the `~` is followed by `/` or ends the string — i.e.
exactly `"~"` and `"~/..."`. Mid-string `~` is always literal (so DNS-zone tokens
like `"~example.com"` survive), and `'~/...'` in single quotes does not expand:

```mix
print("~/.config")
```
```text
/home/user/.config
```

Use `"\~"` for a literal leading tilde. `~user` is not supported — `"~root/x"`
stays the literal text `~root/x` (no expansion, no error); only the running
user's home expands.

**Trap — tilde strings as *text*, not paths.** The expansion is lexical, so it
fires on any double-quoted literal, including one you meant as a search needle.
`replace($s, "~/.gh/x", "~/new/x")` looks for the literal `/home/you/.gh/x` and
silently matches nothing against file text that says `~/.gh/x` — no error, no
change. The same applies to `contains`, `starts_with`, `split`, `index`, and to
`"~" .. "/x"` (the bare `"~"` already expanded). For literal-tilde text use single
quotes, and `grep` the result after a rewrite:

```mix
$s = "see ~/.gh/x"
print(replace($s, "~/.gh/x", "Z"))     -- needle became /home/you/.gh/x: no match
print(replace($s, '~/.gh/x', "Z"))     -- single quotes: literal needle
```
```text
see ~/.gh/x
see Z
```

## Heredocs

`<<TAG ... TAG` is a multi-line literal whose body **does** interpolate `${...}`
(scope → env, like double quotes) and, in this context, `$(...)` command
substitution:

```mix
$who = "team"
$body = <<END
Dear ${who},
welcome.
END
print($body)
```
```text
Dear team,
welcome.
```

The rules, precisely:

- The tag is ASCII letters/digits/`_`. The closing tag must sit alone on its line (surrounding whitespace is allowed); the newline before it is stripped.
- A bare `$name` in the body stays **literal**, exactly as in double quotes — only `${...}` interpolates.
- `$(cmd)` runs via `/bin/sh` and splices its stdout — the one string context where Mix itself substitutes a command.
- Escapes `\n \t \r \e \\ \$` work (`\$` suppresses interpolation: `\${not_a_var}`); **`\u{…}` is NOT processed** in a heredoc body — it stays literal. Any other `\x` stays literal too.

## Codepoint, byte, grapheme — pick the right unit

This is the design decision to internalise. The core string ops — `length`/`len`,
`pos`, `lastpos`, the string overload of `index_of` — count **Unicode codepoints**,
so they compose with `substr`/`reverse`/`$s[i]` (also codepoint-based). When you
need raw bytes or user-perceived characters, reach for the explicit twin family.
(The pre-0.8.0 byte value of `length` lives on as `byte_length`.)

```mix
print(length("café"))           -- codepoints: c a f é
print(byte_length("café"))      -- raw UTF-8 bytes (é is 2)
print(grapheme_count("café"))   -- user-perceived chars
print(display_width("日本語"))   -- terminal cells (CJK = 2 each)
print(pos("é", "café"))         -- 1-based codepoint position
```
```text
4
5
4
6
4
```

`length`/`len` on a **list or map** is the element count, not a string length.

The three boundaries, in one table:

| Want | Builtins | Unit |
|---|---|---|
| codepoints (default) | `length`/`len`, `pos`, `lastpos`, `index_of`, `substr`, `reverse`, `left`, `right` | Unicode scalar values |
| raw bytes | `byte_length`, `byte_pos`, `byte_lastpos`, `byte_index_of` | UTF-8 bytes |
| user-perceived chars | `grapheme_count`, `grapheme_substr`, `grapheme_reverse` | grapheme clusters (UAX #29) |
| terminal columns | `display_width`, `lpad_w`, `rpad_w`, `word_wrap_w` | display cells (UAX #11) |

All four rows are operations on **text**. Operations on a raw `bytes`/`buffer`
*value* are a different family, `bytes_*` — see
[io](io.md#naming-byte_-vs-bytes_). The two are easy to confuse and are not
interchangeable: `byte_length($some_bytes)` stringifies its argument first and
so measures the `<bytes:N>` placeholder, not the bytes.

A plain `substr`/`reverse` is codepoint-based and **splits** an emoji ZWJ sequence or
combining cluster; the `grapheme_*` ops keep it whole:

```mix
print(reverse("a👍🏽b"))          -- codepoint reverse splits the skin-tone modifier
print(grapheme_reverse("a👍🏽b")) -- grapheme reverse keeps it whole
```
```text
b🏽👍a
b👍🏽a
```

A decomposed `e` + combining acute is 2 codepoints but 1 grapheme and 1 display cell:

```mix
$d = "e\u{0301}"                -- e + U+0301 COMBINING ACUTE
print(length($d))
print(grapheme_count($d))
print(display_width($d))
```
```text
2
1
1
```

The spread gets dramatic with ZWJ sequences — a 👨‍👩‍👧 family emoji is **1 grapheme,
5 codepoints, 18 bytes**:

```mix
$f = "👨‍👩‍👧"
print(grapheme_count($f))
print(length($f))
print(byte_length($f))
```
```text
1
5
18
```

### Indexing bases differ — a footgun

- `substr`, array indexing, list `index_of`, `slice` are **0-based**. (`left`/`right` take a *count*, not an index — `left("hello", 2)` → `"he"` — so they have no base.)
- `pos` and `lastpos` are **1-based**, returning **0 when not found**.
- An empty needle gives `pos == 1`.

```mix
print(substr("hello world", 0, 5))   -- 0-based, length 5
print(substr("hello world", 6))      -- no len → to the end
print(pos("world", "hello world"))   -- 1-based
print(pos("xyz", "hello world"))     -- not found → 0
print(lastpos("o", "hello world"))   -- 1-based last
```
```text
hello
world
7
0
8
```

`substr` clamps a start past the end to empty and never panics on an oversized
length. The byte twins keep their codepoint counterparts' base conventions:
`byte_pos`/`byte_lastpos` are 1-based with `0` = not found (like `pos`/`lastpos`),
while `byte_index_of` follows the `index_of` convention (0-based, `-1` when not
found). Watch the arg order: `index_of`/`byte_index_of` take
`(haystack, needle)`, `pos`/`byte_pos` take `(needle, haystack)`.

## Case, search, edit

```mix
print(upper("hello"))
print(lower("HELLO"))
print(contains("hello", "ell"))
print(starts_with("hello", "he"))
print(ends_with("hello", "lo"))
print(replace("aaa", "a", "b"))       -- replaces ALL occurrences
```
```text
HELLO
hello
true
true
true
bbb
```

`contains` also works on a list (membership, not substring):

```mix
print(contains([1,2,3], 2))
```
```text
true
```

> **Match ops are byte-exact** — `contains`/`starts_with`/`ends_with`/`split`/`join`/
> `replace`/`template` do **no** Unicode normalization and **no** case-folding. A
> precomposed `é` (U+00E9) will not match a decomposed `e`+`◌́`. Normalize first if
> you need NFC-insensitive matching.

`upper`/`lower` do full Unicode case mapping (`upper("café")` → `"CAFÉ"`); the
byte-exact rule is about *matching*, not casing. For word extraction there are the
ARexx-flavoured `words($s)` (count whitespace-delimited words) and `word($s, n)`
(Nth word, 1-based); for pattern matching see [regex](regex.md).

## Ordering and equality

The comparison operators `<` `>` `<=` `>=` pick their mode from **both** operands:

1. If both coerce to numbers — real numbers, or numeric strings like `"5"` — the comparison is **numeric**. So `"5" < "10"` is `true` (not a bash-style string surprise).
2. Otherwise, if both are strings, the comparison is **lexicographic by codepoint**: `"apple" < "banana"`, `"Z" < "a"` (uppercase sorts before lowercase), and the classic range test `$ch >= "a" and $ch <= "z"` all work. `"5" < "abc"` is a string-vs-string compare (lexicographic → `true`).
3. A number against a **non-numeric** string is a runtime error, not a silent coercion.

```mix
print("5" < "10")     -- both numeric strings → numeric compare
print("apple" < "banana")
print("Z" < "a")      -- codepoint order: U+005A < U+0061
```
```text
true
true
true
```

```mix
print(5 < "abc")
```
```text
Runtime error at line 1: cannot compare 'abc' as number
```

`==`/`!=` never error: a number and a numeric string compare **equal by value**
(`"5" == 5` is `true`), but two strings compare as text (`"5" == "05"` is
`false`, `"abc" == 5` is `false`). Full operator detail in
[operators](operators.md).

## Split, join, slice

`split` defaults to a space delimiter; `join` to a space separator:

```mix
$parts = split("a,b,c", ",")
print(join($parts, " | "))
print(length(split("one two three")))   -- default delim = space
```
```text
a | b | c
3
```

For substring-by-position use `substr`/`grapheme_substr`; for a sublist use `slice`
(0-based, half-open `[start, end)`, see [builtins](builtins.md)):

```mix
print(join(slice([10,20,30,40], 1, 3), ","))
```
```text
20,30
```

## The delimiter family — `before`/`after`/`split_once` (0.63.0)

The helpers a script reaches for on line 3, replacing every
`substr($s, pos(..) ± n)` computation. All subject-first. **Contract:**
absent delimiter → `nil` (never `""` and never the whole string — `""` is
a *real* result, the delimiter at the edge); empty delimiter raises.

```mix
print(after("key=value", "="))          -- value
print(before("key=value", "="))         -- key
print(after_last("/a/b/base.txt", "/")) -- base.txt   (basename)
print(after_last("base.txt", "."))      -- txt        (extension)
$kv = split_once("k=v=w", "=")          -- [head, tail] at the FIRST =
print($kv[0] .. " / " .. $kv[1])        -- k / v=w    (rsplit_once: LAST)
print(between("a <b> c", "<", ">"))     -- b
print(after("no-equals", "=") or "dflt")-- dflt  (nil-coalesce a default)
```

`before_last` mirrors `before` at the last occurrence. Absence is `nil`,
so `if after($s, "=") == nil` is the "not found" test — an empty capture
and a missing delimiter never blur.

**Prefix/suffix forms** return the subject **unchanged** when there is
nothing to strip ("nothing to do" is an answer, not a failure):
`strip_prefix(s, p)`, `strip_suffix(s, x)`, `replace_first(s, old, new)`
(first occurrence only; empty `old` mirrors `replace()`), and
`count_of(s, needle)` (non-overlapping; 0 for an empty needle).

**Lines, fields, chars** (0.63.0 — `lines`/`chars` were prelude
functions, now native): `lines(s)` splits on `\n`, strips one trailing
`\r` per line (CRLF-safe) and drops exactly one trailing empty element
(the final newline) — `lines("a\nb\n")` is `["a", "b"]`, `lines("")` is
`[]`, `"a\n\n"` keeps its real empty line. `fields(s)` splits on
whitespace *runs* like awk (no empties; `word(s, n)` is the 1-based
single-field form). `chars(s)` yields codepoints as 1-char strings.
`last_index_of(s, v)` is the 0-based codepoint twin of `lastpos()` (args
reversed, -1 when absent — same condition warning as `index_of`).

## Trim, pad, repeat, reverse

`trim`/`strip` take an optional **charset** — a *set* of codepoints to
strip (PHP-style), honoured since 0.63.0 (before that the second argument
was silently ignored — the call "worked" and did nothing). One-sided:
`ltrim`/`rtrim`, same optional charset:

```mix
print(trim("xxhelloxx", "x"))     -- hello
print(ltrim("zyxabczyx", "xyz"))  -- abczyx
print(rtrim("zyxabczyx", "xyz"))  -- zyxabc
```

```mix
print("[" .. trim("  hi  ") .. "]")    -- strip is an alias for trim
print("[" .. lpad("7", 4) .. "]")      -- right-align in width 4
print("[" .. rpad("7", 4) .. "]")      -- left-align in width 4
print(repeat("ab", 3))
print(reverse("hello"))
```
```text
[hi]
[   7]
[7   ]
ababab
olleh
```

`lpad`/`rpad` pad by **codepoint count**, which misaligns a column of CJK/emoji (a
wide glyph counts as 1 there but renders as 2 cells). The `%Ns` width of
[`fmt`/`printf`](builtins.md) pads by codepoints too (`fmt("%6s", "日本")` adds
four spaces, not two). Use `lpad_w`/`rpad_w` to pad by **display cells**:

```mix
print("[" .. rpad("日本", 6) .. "]")    -- codepoint pad: 2 chars → +4 spaces
print("[" .. rpad_w("日本", 6) .. "]")  -- cell pad: width 4 → +2 spaces
```
```text
[日本    ]
[日本  ]
```

### Padding with something other than a space

All four padders take an **optional third argument: the fill character** (v0.54.0).
It defaults to a space, so existing two-argument calls are unchanged.

```mix
print(lpad("7", 4, "0"))        -- zero-pad a number-as-text
print(rpad("Name", 10, "."))    -- dot leader
print(lpad_w("日本", 6, "."))   -- fill by display cells
```
```text
0007
Name......
..日本
```

The fill must be **exactly one character**, and for the `_w` variants exactly one
**display cell** — a wide fill would overshoot the target by a cell per pad
character, which defeats the point of padding by width. Both raise a runtime
error naming the offending value rather than silently mis-aligning:

```text
lpad: fill must be exactly one character, got "ab" (2 chars)
lpad_w: fill must be exactly 1 display cell wide, got '漢' (2 cells)
```

Padding is **saturating** — content already at or beyond the width is returned
unchanged, never truncated.

To zero-pad a **number**, prefer `fmt`'s `%0Nd`, which keeps the sign to the left
of the zeros (`fmt("%05d", -42)` → `-0042`). Reach for `lpad(s, n, "0")` when the
value is already a string, since `%0Ns` is undefined for strings in C and is
ignored here.

`repeat`/`lpad`/`rpad` cap a result at 256 MiB and raise a normal runtime error
rather than OOM on a hostile count/width. Since 0.59.0 the cap bounds the
**bytes the padding would build** — a multibyte fill counts at its UTF-8 width
and the original string is included in the sum — so the error fires before the
allocation it names, not after a 4-byte fill has built four times the
advertised limit. (For a width within the cap, a string already at or past that width passes
through unchanged, whatever its size — the cap meters what padding
constructs, not what you already had. A width beyond the cap errors before
anything is examined.)

Every numeric position/count/width argument in this page uses Mix numeric
coercion: a number or numeric string works (`left("abcdef", "2")` is `ab`, as
is `left("abcdef", 2)`). A supplied value that cannot be parsed as a number now
raises `TYPE_MISMATCH`, naming the builtin, 1-based argument position, value,
and type. It never silently becomes zero or the omitted default. This applies
to `left`, `right`, `substr`, `grapheme_substr`, `repeat`, `lpad`, `rpad`,
`lpad_w`, `rpad_w`, `word`, `word_wrap`, and `word_wrap_w`:

```mix
print(substr("abcdef", "2x"))
```

```text
TYPE_MISMATCH: substr(): argument 2 must be a number, got "2x" (string)
```

## Templates and wrapping

`template` substitutes single-brace `{key}` placeholders from a map (distinct from
`${...}` interpolation, and from the `%s`/`%d` of [`fmt`/`printf`](builtins.md)):

```mix
$m = { name: "Ada", role: "dev" }
print(template("{name} is a {role}", $m))
```
```text
Ada is a dev
```

`template` is **single-pass**: a substituted value is emitted verbatim and never
rescanned, so untrusted data containing `{other_key}` cannot inject a second
substitution (`template("{a}", {a: "{b}", b: "X"})` → `{b}`), and the output does
not depend on map iteration order. A placeholder with no matching key stays
literal (`template("{missing}", {})` → `{missing}`).

`word_wrap` greedily wraps to a codepoint budget (`word_wrap_w` to a display-cell
budget):

```mix
print(word_wrap("the quick brown fox jumps", 10))
```
```text
the quick
brown fox
jumps
```

## Quoting for shells and SQL

When a string must cross into a shell command or a SQL literal, escape it natively
rather than hand-rolling quotes:

```mix
print(shell_quote("it's a test"))     -- safe single-quote wrap for POSIX sh
print(sql_quote("O'Brien"))           -- doubles ' (MySQL/MariaDB-safe)
```
```text
'it'\''s a test'
O''Brien
```

`sql_quote` doubles `'` **and escapes `\`** (`sql_quote("a\\b")` → `a\\b`) and
strips NUL bytes — MySQL/MariaDB-safe. It is also safe for SQLite, where a
literal backslash arrives doubled; for exact bytes use `sqlexec` parameter binds
instead of quoting (see [system](system.md)) — binds are typed (nil → NULL,
whole number → INTEGER, string → TEXT, bytes → BLOB), so the value never passes
through a quoted literal at all.

`sanitize` makes untrusted bytes safe for a one-line diagnostic — collapsing line
breaks to spaces and replacing C0/C1 controls and Trojan-Source bidi/zero-width
characters with `?`:

```mix
print(sanitize("line1\nline2\ttab"))
```
```text
line1 line2?tab
```

For HTML output use `html_escape`; for Bus/SSE framing see [Bus messaging](bus.md)
and the [Datastar](https://data-star.dev) `ds_*` builtins. To embed a value as
re-parseable Mix source, use `data_encode` (see [builtins](builtins.md)).

## Mail headers — `rfc2047_decode` / `rfc2047_encode` (0.67.0)

A mail header may only carry ASCII, so anything else travels as RFC 2047
**encoded-words**: `=?charset?B-or-Q?data?=`. A `Subject:` read raw comes back
as `=?utf-8?B?Q2Fmw6k=?=`, which is exactly the unreadable output a report
exists to prevent.

```
rfc2047_decode(header)             -> plain string
rfc2047_encode(text[, {encoding}]) -> header value ("B" default, or "Q")
```

```mix
print(rfc2047_decode("=?utf-8?B?SGVsbG8gV29ybGQ=?="))
print(rfc2047_decode("=?ISO-8859-1?Q?caf=E9?="))
print(rfc2047_decode("Re: =?utf-8?B?dGVzdA==?= (fwd)"))
print(rfc2047_encode("plain ascii subject"))
print(rfc2047_encode("café"))
```
```text
Hello World
café
Re: test (fwd)
plain ascii subject
=?UTF-8?B?Y2Fmw6k=?=
```

**Decoding.** The charset token is honoured — `utf-8`, `us-ascii` and
`iso-8859-1` (with the usual aliases, and RFC 2231's `*language` suffix
stripped) decode properly; **any other charset falls back to UTF-8 with U+FFFD
substitution**, so a `koi8-r` header is legible-ish rather than exact. Adjacent
encoded-words are joined at the **byte** level and the whitespace between them
is dropped, per RFC 2047 §6.2 — which is not cosmetic: a long non-ASCII subject
is folded by splitting it into several words, and a multi-byte character
straddling that split only survives if the bytes are joined before decoding.
Anything that is not a well-formed encoded-word is passed through **literally**,
never dropped; a visible `=?utf-8?X?…?=` beats a silently lost subject line.

**Encoding.** Plain ASCII is returned **unchanged** (§5 permits an encoded-word
only where one is needed), unless it contains a literal `=?`, which would
otherwise be misread by the receiving decoder. Output is always UTF-8 — the
right answer for modern mail, and not a choice worth offering. `B` (base64) is
the default; `{encoding: "Q"}` is more readable when the text is mostly ASCII.
Each emitted word stays within §2's 75-character limit and splits on
**character** boundaries, because an encoded-word must be independently
decodable — a 4-byte emoji may never straddle two words.

This pair was promoted from a deployed nospam report script, with **three**
changes the original could not justify:

1. it decoded every charset as UTF-8-lossy — right for the one mailbox it
   served, wrong in general;
2. it joined adjacent words as decoded *strings* rather than bytes, so a
   character split across the fold became two replacement characters;
3. on a malformed word it either emitted the raw payload with the wrapper
   stripped (presenting undecoded bytes as though they had been decoded) or
   abandoned the whole scan, leaving every later word in the header encoded.

Everything the original got right is unchanged, including the false-terminator
handling that is the reason this was promoted rather than rewritten. All three
changes are pinned by tests.

## Gotchas recap

- `"double"` interpolates **only** `${...}`; a bare `$name` is literal text.
- `'single'` is fully raw — nothing interpolates, `~` does not expand.
- `$(...)` is literal inside a Mix string (use `run`/`run_rc` + `..` to splice output) — but it *does* substitute in a heredoc body, as a standalone expression, and it passes through to the target shell in a `run`/`ssh_run` command string.
- Concatenate with `..`, never `+` or `.`.
- `length`/`pos`/`substr`/`reverse` are **codepoints**; use `byte_*` for bytes, `grapheme_*` for emoji/combining, `display_width`/`*_w` for terminal columns. `length`/`len` on a list/map is the element count.
- `pos`/`lastpos`/`byte_pos`/`byte_lastpos` are **1-based, 0 = not found**; `substr`/arrays/`index_of`/`byte_index_of`/`slice` are **0-based** (`index_of` returns `-1` when not found).
- `replace` replaces **all** occurrences; match ops are byte-exact (no case-fold, no normalization).
- `<`/`>` compare **numerically** when both sides coerce to numbers (`"5" < "10"` is `true`), **lexicographically by codepoint** when both are strings; a number vs a non-numeric string is a runtime error.
- `lpad`/`rpad`/`%Ns` pad by codepoints — `lpad_w`/`rpad_w` for cell-exact columns.
- `${X}` unbound in scope **and** env is a runtime error (like a bare `$X`); a nil *value* still renders `nil`. Supply a fallback with `${X ?? default}` (nil-only) or `${X ?: default}` (any falsy).

## See also

- [numbers](numbers.md) — the f64 numeric type, `+`/`%`/`**`, radix literals
- [operators](operators.md) — `..` concat, `==`/`<` ordering, `??` nil-coalesce
- [regex](regex.md) — `regex_match`/`regex_find`/`regex_replace`/`regex_split`
- [syntax](syntax.md) — the lexer, comments, and the shell/Mix classifier
- [functions](functions.md) — lambdas and HOFs for transforming string lists
- [builtins index](builtins.md) — the full string/byte/grapheme/`_w` builtin set
- [running commands](system.md) — `run`/`run_rc` for splicing command output
- [shell mode](shell-mode.md) — where `$(...)` *is* command substitution
- [Bus messaging](bus.md) — `send`/`emit`, `ds_*` SSE framing, `html_escape`
- `mix help` — list all builtins · `mix what NAME` — describe one (e.g. `mix what grapheme_substr`)
