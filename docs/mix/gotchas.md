# gotchas — what you will guess wrong on your first try

Mix has almost no presence in any model's training data, so a new session
extrapolates from bash, Python and JavaScript — and several of those guesses
are **silently wrong**: they run, produce a value, and mean something else.
This page is the list of them.

Read it before writing Mix. It is the one page that pays for itself in a
single session. `mix man syntax` is the mental model; this is the errata.

Every row below is executed by `cargo test -p cosmix-mix --test man_gotchas`,
which runs the **probe** column through `mix -c` and compares the **prints**
column. A wrong row fails the build, so the page cannot rot into confident
fiction.

## The table

A `prints` cell starting with `!` means the probe **fails**, and that text
must appear in the error.

| you will guess | Mix is | probe | prints |
|---|---|---|---|
| a bare name reads the variable | a bare word is a **string literal** — `$` is not optional, and omitting it is silent | `$x = 5; print(x)` | `x` |
| `"hi $name"` interpolates | only `${...}` interpolates; a bare `$name` in a string is **literal text** | `$n = "world"; print("hi $n")` | `hi $n` |
| — so use braces | `${name}` — and it is the **same rule everywhere** a string is built, heredocs included | `$n = "world"; print("hi ${n}")` | `hi world` |
| `elseif` | `elif` | `if 1 == 2 then print("a") elif 1 == 1 then print("b") end` | `b` |
| `str(x)` | `to_string(x)` | `print(to_string(5))` | `5` |
| `print(a, b)` | `print` is a **statement**, not a function, so `(a, b)` is read as an expression and fails | `print("a", "b")` | `!expected RParen` |
| — so how do I print two things | `print a, b` — no parens, joined by a space; `print(x)` only works because `(x)` is parenthesised | `print "a", "b"` | `a b` |
| `run()` returns a status | `run()` **raises** on a non-zero exit | `print(run("false"))` | `!failed (rc=1)` |
| — so how do I get the code | `run_rc()` returns `{rc, stdout, stderr, …}` | `print(run_rc("false").rc)` | `1` |
| `read_file()` returns nil when missing | it **raises**; test with `is_file()` first | `print(read_file("/no/such/file"))` | `!No such file or directory` |
| `push` mutates any list | `push` writes through the **slot**; on a nested list it is a silent **no-op** | `$m = {a: [1]}; push($m.a, 2); print($m)` | `{a: [1]}` |
| — so how | read it out, push, store it back | `$m = {a: [1]}; $i = $m.a; push($i, 2); $m["a"] = $i; print($m)` | `{a: [1, 2]}` |
| a declared `fn` name is a value | a **lambda** is a real value, but a declared `fn`'s bare name is the **string** `"f"` | `fn f($x) return $x end; $g = f; print($g)` | `f` |
| — so how do I pass one | assign a lambda: `$f = fn($x) = …`, which is a `function` and works in `map`/`filter` | `$f = fn($x) = $x + 1; print(map([1, 2], $f))` | `[2, 3]` |
| a `fn` can write an outer variable | assignment inside `fn` binds a **local**; return the value instead | `$n = 1; fn bump() $n = 99 end; bump(); print($n)` | `1` |
| `catch $e` gives an error object | `$e` is the **message string**; use `catch $msg, $err` for `$err.code` | `try raise("E_X", "boom") catch $m, $e print($e.code) end` | `E_X` |
| `raise("boom")` | `raise(CODE, MESSAGE)` — one argument is an arity error | `try raise("E_X", "boom") catch $m print($m) end` | `boom` |
| `json_decode()` | `json_parse()` (and `json_encode()` the other way) | `print(json_parse("{\"a\":1}").a)` | `1` |
| `==` compares two maps or lists | it **raises** rather than answer a useless `false` | `print([1, 2] == [1, 2])` | `!would always answer false` |
| — so how do I compare them | `deep_eq(a, b)` | `print(deep_eq([1, 2], [1, 2]))` | `true` |
| `re_replace(pattern, s, ...)` | **subject first**: `re_replace(s, pattern, replacement)` | `print(re_replace("a1b", "[0-9]", "#"))` | `a#b` |
| `sort_by` takes a direction | ascending only, with a lambda; `reverse()` for descending | `print(sort_by([3, 1, 2], fn($x) = $x))` | `[1, 2, 3]` |
| `'~/x'` expands | a **single**-quoted string is raw — no `~`, no `${...}` | `print('~/x')` | `~/x` |
| — and `"~/x"`? | a double-quoted one expands `~` to `$HOME` | `print(starts_with("~/x", "/"))` | `true` |
| `send svc-name verb` needs quoting | it does not — a tight-hyphenated bare target is read whole, like the quoted and `$var` forms | `send comp-nested ping timeout=1; print(type($result))` | `string` |
| — but `send a - b verb`? | a **space** makes it subtraction again, and a bareword is a string, so that is a type error | `send a - b ping timeout=1` | `!as number` |
| `replace()` tells you it missed | it returns the input unchanged, **silently** — check the result, or use `mix edit` from a prompt | `print(replace("abc", "zz", "!"))` | `abc` |
| `replace()` replaces the first | it replaces **all** of them | `print(replace("a a a", "a", "b"))` | `b b b` |
| `mix -c 'print(x)'` needs escaping gymnastics | it does not; a probe is one call and the binary is the oracle | `print(mix_version() != "")` | `true` |

## Three rules that are not a syntax trap

These cost more than any row above, and no probe can catch them.

1. **Never `sed -i`, never Python.** A one-line edit is `mix edit FILE OLD NEW`
   — exact match, refuses an absent or ambiguous needle (see
   [the mix command](cli.md)). A script is a `.mix` file. If Mix genuinely
   cannot do something, that is a **reportable gap in Mix**, not a licence to
   reach for another language.
2. **Never `ssh host 'bash -c "…"'`.** Use
   `ssh_mix(host, source, {bindings: {…}})` — it ships the source over ssh
   stdin into `mix -`, bypassing **all** shell quoting, and `bindings` passes
   values in as `$name` assignments instead of string-interpolating them.
   Quoting a remote loop by hand fails in ways that look remote and are local.
   See [remote execution](remote.md).
3. **Probe, do not extrapolate.** `mix -c '<code>'` answers in under a second
   and the binary outranks every document, including this one. `mix builtins
   NAME` gives the exact signature, `mix what NAME` the one-liner, `mix lint
   FILE` the call-contract check before you run anything non-trivial.

## See also

- [syntax & the classifier](syntax.md) — the mental model these are errata to.
- [the mix command](cli.md) — `mix edit`, `mix lint`, `mix what`, `mix man`.
- [errors & exit handling](errors.md) — the full `try`/`catch`/`raise` model.
- [collections](collections.md) — why `push` writes through a slot.
