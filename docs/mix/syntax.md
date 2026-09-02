# syntax — lexical structure & the shell/Mix classifier

How Mix reads source: what a token is, how statements end, and the one rule that
trips up everyone coming from bash or Python — **when a line is run as Mix code
versus dispatched to the shell**. Verified against **mix 0.57.0**.

> Mental model: Mix is a Bus-native shell where every variable carries a `$`
> sigil, concatenation is `..`, and statements are separated by a **newline or
> `;`**. At a `mix -c` one-liner or the interactive REPL, a *shell-first
> classifier* decides per input whether to evaluate it as Mix or hand it to the
> shell. That single decision explains why `$x = 1` works but `x = 1` runs a
> command called `x`.

## The five things to get right first

1. **Every variable has a `$` sigil** — `$x = 1`, never `x = 1`. A bare `x = 1` is read as a shell command (run the program `x` with args `=` and `1`).
2. **Concatenation is `..`** — `"a" .. $x`, never `.` (that's field access). `+` happens to also join strings today, but `..` is the canonical form.
3. **Statements end at a newline or `;`.** Newlines are the readable script style; `;` is the compact `mix -c` / generated-source form.
4. **`--` and `#` start a line comment.** No block comments (`/* */`).
5. **Blocks close with `end`.** No `do` keyword.

```mix
$x = 1
print($x)
```
```text
1
```

```mix
x = 1
```
```text
mix: x: No such file or directory (os error 2)
```

The second snippet isn't a Mix error at all — the classifier saw a bareword head
(`x`) and dispatched the line to the shell, which tried to exec a program named
`x`. See [the classifier](#the-shellmix-classifier) below.

## Comments

Two line-comment forms, identical behaviour: `--` (ARexx/Lua/Haskell flavour) and
`#` (shell flavour). Both run to the end of the line. There are **no block
comments** — `/* ... */` is not recognised.

Comments own the rest of their **physical line**. A semicolon inside a comment
does not resume execution: `print(1); -- note; print(2)` prints only `1`.

```mix
-- a full-line comment
$y = 2  -- a trailing comment
# also a comment, shell-style
print($y)
```
```text
2
```

Non-ASCII inside a comment is fine for the lexer, but keep comments ASCII as a
house convention.

> **`mix -c` gotcha — a leading comment line swallows the whole input.** The
> classifier trims the input and, if it *starts* with `#` or `--`, treats the
> entire `-c` argument as an empty (comment-only) line — it does **not** strip
> the first line and run the rest. So this prints nothing:
>
```text
$ mix -c '# header
print(42)'
(no output)
```
>
> Put the comment *after* the first real statement, or run from a file / stdin
> (`mix script.mix`, `mix -`), where the input is parsed as source rather than
> classified as one `-c` command. A leading comment in a script is normal.

## Statement separators — newline and `;`

This section is the authoritative `;` contract for **executable Mix**. A
semicolon is a real lexer token and the parser accepts it as a statement
separator only where a complete statement may end. It is not text substitution
for a newline and it is not an expression operator.

Use a physical newline for normal script layout and `;` when several complete
statements genuinely need to share a line — most often in `mix -c`, the REPL,
generated source, or remote Mix source:

```mix
$a = 1; $b = 2; print($a + $b)
```
```text
3
```

### Where `;` is legal

| Context | Legal? | Example / rule |
|---|---:|---|
| between top-level statements | yes | `$a = 1; print($a)` |
| after a block header or clause | yes | `if true then; print(1); else; print(0); end` |
| between statements in a function, loop, `try`, handler, or `address` body | yes | `function f(); print(1); return 2; end` |
| leading, trailing, or repeated at a statement boundary | yes, no-op | `; print(1);;` |
| inside strings, heredoc bodies, or comments | literal text | `print("a;b")`; a comment still owns its physical line |
| inside an ordinary expression container | no | calls, parentheses, lists, and maps use one expression or `,`, not `;` |
| in strict-data (`data_parse`, `load_data`, config files) | no | use newline or `,`; see [data](data.md) |

Semicolons remain visible inside `()` / `[]` / `{}`, so a nested construct with
a real statement body — an expression-position `if` or function literal — may
use them:

```mix
print(if true then; $x = 1; $x + 1; else; 0; end)  -- valid: prints 2
print((function($n); return $n + 1; end)(4))       -- valid: prints 5

print(1; 2)             -- parse error: call arguments need `,`
print((1; 2))           -- parse error: parentheses hold one expression
print([1; 2])           -- parse error: list items need `,`
print({a: 1; b: 2})     -- parse error: map entries need `,`
```

The diagnostic for the rejected forms says that `;` separates Mix statements
only. In a script/stdin source, or with a Mix-keyword head such as `print` under
`-c`/the REPL, parsing finishes before evaluation, so nothing before the error
runs. A malformed **`$`-led** `-c`/REPL input can instead trigger the historical
whole-input shell fallback; use the keyword-led forms above when checking a
diagnostic. The classifier section later on this page explains that fallback.

### Precedence and hard-boundary rule

As a statement separator, `;` binds looser than Mix statement chains and ends
the complete construct to its left:

```mix
print("a") && print("b"); print("c")
-- parsed as: (print("a") && print("b")); print("c")

print("x") | tr x y; print("z") -- `;` also ends the raw external pipeline tail
```

There is no general “pipeline before `&&`/`||`” rule in executable Mix. After a
Mix `|`, everything through newline, `;`, or end-of-input is the raw external
command tail; an `&&`/`||` there belongs to that external shell. In the other
direction, a Mix statement-chain operand may itself start a pipeline:

```mix
print("a") && print("b") | cat; print("c")
-- Mix chain first; the right operand's external tail is `cat`; `;` then starts `c`
```

Unlike a physical newline, a semicolon is a **hard** chain boundary. Mix permits
an existing `&&` / `||` chain to continue across a newline on either side of the
operator, but never across `;`:

```mix
print("a")
&& print("b")            -- valid newline continuation

print("a"); && print("b") -- parse error
print("a") &&; print("b") -- parse error
```

The lexer also suppresses newlines — but deliberately retains semicolon tokens —
inside ordinary grouping, as described below.

### Mix versus shell meaning

The separator does **not** mix the two languages. A line is classified once and
the whole line must be valid Mix or valid shell. Use `sh`, `run_argv`, or
`run_rc` for an external operation inside a Mix sequence; `print(1); echo hi`
is a Mix parse error, not “run one Mix statement then one shell command”. On an
external-command line, `;` keeps its shell command-list meaning:

```text
$ mix -c 'echo one; echo two'
one
two
```

The distinction matters for empty pieces: leading/trailing/repeated semicolons
are harmless only after the input has been classified as Mix. Shell-dispatch
accepts a single trailing `;`, but rejects a repeated/interior empty piece such
as `echo a;; echo b`. A leading `; echo x` is tried as Mix first and surfaces a
Mix parse error rather than a shell empty-piece error. See [shell mode](shell-mode.md)
for command-list classification, unknown-command preservation, and the
whole-file-first `source` caveat.

### Newlines are suppressed inside `(` `[` `{`

While the lexer is inside an open paren, bracket, or brace, newlines are
swallowed — so list/map literals and call arguments wrap freely across lines
without a continuation marker:

```mix
$nums = [
  1,
  2,
  3
]
print(length($nums))
```

```text
3
```

### Explicit line continuation: `\` at end of line

Outside a string or heredoc body, end a physical line with a backslash to
continue onto the next physical line. This works for Mix expressions and for
shell-dispatch commands in script files, `mix -c`, and the REPL:

```mix
$total = 1 + \
2 + \
3
print($total)
```
```text
6
```

```text
$ mix -c 'echo one \
two'
one two
```

The splice happens **before shell/Mix classification**, so the classifier sees
one complete logical line. This matters for operator-shaped command arguments:
`ls -d \` followed by `/tmp` stays an `ls` command instead of becoming the Mix
arithmetic expression `ls - d / tmp`.

Trailing backslashes are counted. Odd means continuation: the final backslash
and physical newline are removed, while preceding even pairs remain for the
normal parser. Thus `\` continues, `\\` is a literal backslash, and `\\\` is
one literal backslash plus continuation.

A continuation marker at EOF is typed incomplete input. The REPL retains its
accumulator; a file or `-c` invocation exits non-zero. Backslashes inside Mix
single/double-quoted strings and heredoc bodies are not continuation markers,
so Windows-style strings such as `"C:\\path"` and command text passed through
`run` or `sh` keep their existing shell-owned escaping.

### Concat continuation: trailing `..`

The concat operator also continues its expression when `..` ends a physical
line. Blank lines and comment-only lines may appear before the right operand:

```mix
$message = "a long prefix: " .. -- explain the next fragment

  "middle" ..
  " and suffix"
print($message)
```

This is deliberately **trailing-only**. A `..` at the start of a line does not
reattach to the previous statement, and `../path` keeps its existing relative
path meaning in shell commands and in `source` / `include`. A semicolon remains
a hard statement boundary. Use trailing `\` for general continuation after any
other operator; no other binary operator gains this newline rule.

## Blocks close with `end` — no `do`

Every compound form (`if`, `for`, `while`, `function`/`fn`, `on`, `try`) is closed
by `end`. There is **no `do` keyword**; writing one breaks an inline body. (A few
legacy terminators — `next` for loops, `done` for `while`/`loop`/`on` — still
parse with a deprecation warning; `if` and `function`/`fn` accept **only** `end`.
Prefer `end` everywhere.) Full grammar of each block lives in
[control flow](control-flow.md) and [functions](functions.md).

```mix
$n = 3
if $n > 0 then
  print("positive")
end
```
```text
positive
```

## Tokens the lexer produces

### Numbers — f64, with radix literals

Mix has a single numeric type (f64). Decimal, plus `0x` hex / `0o` octal / `0b`
binary integer literals (sugar for the f64 value — handy for file modes and
bitmasks). Underscores are allowed as digit separators. Scientific notation is
accepted: `[eE][+-]?digits` — `1e6`, `1.5e3`, `2e-3`, `1E3`. The `e`/`E` is only
consumed when a digit follows, so the Euler builtin in `2 * e()` is never eaten.

```mix
print(0o755)
print(0xFF)
print(0b101)
print(1e6)
print(2e-3)
```
```text
493
255
5
1000000
0.002
```

A **bare leading-zero integer is a lex error** — `0755` is rejected so a file
mode can't silently become decimal `755`. The error is surfaced, not masked:

```mix
0755
```
```text
Lexer error at line 1:1: ambiguous leading-zero number '0755' — use a 0o (octal) / 0x (hex) / 0b (binary) prefix, or drop the leading zero(s) for decimal
```

Plain `0`, and genuine fractions like `0.5`, are unaffected, and `0e5` is a
valid `0` — but `07e2` still errors (the leading-zero check runs before the
exponent). This is a *Mix number-literal* hazard only: inside a shell-out the
arg is a string, so `run("install -m 0755 a b")` is fine. Full numeric
semantics live in [numbers](numbers.md).

### Strings — `'raw'` vs `"interpolated"`

Two forms, bash-style. Full coverage in [strings](strings.md); the lexical facts:

- `'single'` is **fully raw** — no interpolation, no `~` expansion (only `\'` and `\\` escapes).
- `"double"` interpolates **`${name}`** (scope → env), expands a **leading `~`** to `$HOME`, and honours `\n \t \r \e \\ \" \$ \~` and `\u{XXXX}` escapes.
- In double quotes a **bare `$name` is literal text** (the opposite of bash), and **`$(...)` is literal** — the build-time-substitution footgun was removed.

```mix
$name = "Ada"
print("hi $name")
print("hi ${name}")
print('hi ${name}')
print("~/code")               -- a LEADING ~ expands to $HOME
print("home is ~/code")       -- a mid-string ~ stays literal
```
```text
hi $name
hi Ada
hi ${name}
/home/user/code
home is ~/code
```

### Variables — the `$` sigil

A variable token is `$` followed by `[A-Za-z0-9_]+`. Reading an **unbound** sigil
raises a runtime error (positional `$1`/`$2` script args are exempt):

```mix
print($undefined)
```
```text
Runtime error at line 1: undefined variable '$undefined'
```

### Operators & delimiters

```text
arithmetic   +  -  *  /  %  **            (** is power: 2 ** 10 -> 1024)
concat       ..                            ("a" .. $x)
compare      ==  !=  <  <=  >  >=  eq  ne  (eq/ne are keyword string-equality ops)
logical      and  or  not                  (keywords, not && / ! in expressions)
nil-coalesce ??                            ($x ?? "default")
ternary      ?  :                          ($n > 0 ? "pos" : "neg")
field/index  .  []                         ($m.key  /  $m["key"])
grouping     (  )   [  ]   {  }
statement    ;                              (hard statement boundary; see above)
misc         :  ,  =  ~                     (: in maps, = assignment, ~ path tilde)
```

`??` returns its right side when the left is `nil`:

```mix
$x = nil
print($x ?? "default")
```
```text
default
```

`**` is power; `..` is concat (not a range). A lone `?` is the ternary
`cond ? a : b` token (see [operators](operators.md) for precedence and the
if-expression form). A lone `&` is a lex error *with a hint*:

```mix
print(1 & 2)
```
```text
Lexer error at line 1:9: unexpected '&', did you mean '&&'?
```

### Heredocs — `<<TAG`

`<<TAG` opens a heredoc; the body runs to a line containing only `TAG`. Unlike a
double-quoted string, a heredoc body **does** interpolate `${var}` *and* run
`$(command)` substitution. See [strings](strings.md) for the full treatment.

### Keywords — reserved as identifiers, usable as names

Identifiers matching a keyword become that keyword token. (`mix keywords` prints
a curated subset; the complete lexer set — which also includes `then`/`each`/
`to`/`step`/`next`/`done`/`with`/`on`/`include`/`label`/`eq`/`ne` — is below.)
The full set: `if then else end for each in to step next while done loop break
continue function fn return select when otherwise and or not true false nil
parse with send address emit on try catch die export alias print eprint source
include label sh eq ne`. See [keywords](keywords.md) for what each one does.

Keywords **cannot** be function names or bare user identifiers:

```mix
function step($x) return $x end
```
```text
Parse error at line 1:10: expected identifier, got Step
```

But since 0.21 a keyword **is accepted anywhere it is unambiguously a NAME** —
the old "quote every reserved-word key" footgun is retired:

- **Bare map keys**: `{ label: 1, to: "x", on: true, parse: 2 }` parses.
- **Field access & assignment**: `$m.to`, `$r.label`, `$cfg.to = 1`. An assignment target may chain accessors freely since 0.33.0 — `$cfg.server.host = "x"`, `$m[$u]["k"] = 1`, `$l[0][1] = 9` (see `mix man collections`).
- **Strict-data `.conf.mix` keys**: a key named `to`/`in`/`on`/`label` needs no quoting.
- **`send`/`emit`/`address` kwargs**: `send maild mailbox.move to=$dest` parses (keyword + `=` is a named arg).
- **`parse … with` delimiter words**: `parse $s with $a to $b` uses the literal word `to` as the delimiter — EXCEPT the block/branch terminator set (`end` `else` `catch` `when` `otherwise` `done` `next`), which still ends an inline `parse` statement first.
- **Sigil variables were never restricted**: `$to`, `$label`, even `$fn` are ordinary variables — the lexer reads the name after `$` as a variable token, never a keyword.

```mix
$m = { label: 1, to: "x" }
print($m.to)
$m.label = 9
print($m.label)
```
```text
x
9
```

**The one exception is `fn`** — it lexes to the *same token* as `function`, so
the original spelling is unrecoverable and a bare `fn` key/field still errors.
Quote the key and use index access:

```mix
$m = { fn: 1 }
```
```text
Parse error at line 1:8: expected identifier, got Function
```

```mix
$m = { "fn": 1 }
print($m["fn"])
```
```text
1
```

### Parser nesting cap — depth 200

The parser caps recursive nesting (parens, deep `if` chains, nested maps/lists)
at **depth 200**, matching the evaluator's recursion cap — a pathological
`((((…` input gets a clean parse error, never a stack-overflow abort:

```text
$ mix -c 'print((((( … 205 deep … )))))'
Parse error at line 1:202: nesting too deep (limit 200)
```

## The shell/Mix classifier

At a `mix -c '<line>'` one-liner, the interactive REPL, and a mix login shell, Mix
follows the universal shell `-c` contract: it is **shell-first**. Each input is
classified once into one of: **Mix code**, **external command**, **empty**,
**incomplete**, or **parse-error**. The decision rests almost entirely on the
**first word** (after alias expansion and after skipping any leading `KEY=VALUE`
env prefixes).

The order of checks (from `shell.rs`):

1. **Empty / comment** — blank, or trimmed input starting with `#` or `--` → *empty*, runs nothing.
2. **`$`-led line** → **Mix** (with a shell-chain fallback). A line beginning with `$` is your variable work.
3. **First word is a Mix keyword** (`if for while loop function fn return select print eprint die try parse export alias break continue send address emit source sh label`) → **Mix**. Note `true`/`false`/`nil` are deliberately **not** in this list: as a line *head* they are shell commands, so a bare `false` at the prompt runs `/usr/bin/false` and exits **1** like every shell. The lexer still owns the literals inside Mix source (`$x = true`, `if $a == nil`); `nil` has no external binary, so it falls through to the Mix parse anyway.
4. **First word is a REPL shell builtin** (`cd pushd popd history exit which type unalias jobs fg bg mix`) → **external command**. (A plain foreground `cd` also works inside `&&`/`||`/`;` chains — `cd /tmp && pwd` is intercepted in-process; see [shell-mode](shell-mode.md).)
5. **First word is a tight-hyphenated command shape** (`cosmix-comp`, `systemd-nspawn`, `weston-simple-dmabuf-egl`) → **external command**, whether or not it is on `$PATH`. This applies only to the statement head: it must start with an ASCII letter or `_`, contain a `-` with no surrounding whitespace, end with an ASCII letter/digit/`_`, and otherwise contain only ASCII letters/digits/`_`/`-`.
6. **First word is found on `$PATH` or is a path** (`/`, `./`, `~/…`) → **external command** (the shell-first principle). If the head is a real program but the command line is malformed (e.g. a bad redirect), the shell tokenizer's error is surfaced.
7. **Otherwise** → try to parse as **Mix**. If that parse succeeds it's Mix code; if it fails, a tie-break decides whether to surface the real Mix lex/parse error or report "command not found".

So the `$` sigil isn't decoration — it is what routes assignment to the Mix
evaluator instead of the shell:

```mix
$greeting = "hi there"
print($greeting)
```
```text
hi there
```

A parenthesized call is unambiguously Mix:

```mix
print(1 + 1)
```
```text
2
```

A bareword head that lives on `$PATH` is a shell command:

```text
$ mix -c 'echo hello from shell'
hello from shell
```

A tight hyphen keeps a command name whole before the Mix parser can read it as
subtraction. PATH membership is not required, so a typo still names the complete
command and exits 127:

```text
$ mix -c 'cosmix-comp --nested'       # runs one command named cosmix-comp
$ mix -c 'alpha-no-such-command'
mix: alpha-no-such-command: No such file or directory (os error 2)
```

The rule is deliberately statement-head-only. Whitespace or an expression
marker makes subtraction explicit, so all of these stay Mix:

```mix
a - b
$a - $b
1 - 2
$x-1
$a-$b
print(alpha-beta)       -- hyphen is inside a call, not the line head
```

Scope never changes this decision. Bare `alpha` is the string `"alpha"`, not a
reference to `$alpha`, so `alpha-beta` is a command even when `$alpha` is live.
Working variable subtraction is sigil-led (`$alpha-$beta`) and stays Mix.
Earlier exact head rules still win, so `mix not-a-command` remains the `mix`
meta-command with one hyphenated argument, not a command named
`not-a-command`.

A `KEY=VALUE` prefix is skipped to find the real head — this is an *external
command* with an env var set for it:

```text
$ mix -c 'FOO=bar printenv FOO'
bar
```

### The tie-break: real Mix error vs "command not found"

After the tight-hyphen check, when the head is **not** on `$PATH` and the Mix
parse **fails**, Mix inspects the head's shape (only the first word — a later
character can't flip the verdict):

- A **bareword / path-like** head (`gti status`, `./build`, `foo:bar`) is a command target → reports the familiar command-not-found:

```text
$ mix -c 'gti status'
mix: gti: No such file or directory (os error 2)
```

- A **Mix-expression** head (a number, an operator, a `$`, or a call/index opener like `print(`) was meant as Mix → surfaces the real lex/parse error instead of masking it as a missing binary:

```text
$ mix -c 'print(0755)'
Lexer error at line 1:7: ambiguous leading-zero number '0755' — use a 0o (octal) / 0x (hex) / 0b (binary) prefix, or drop the leading zero(s) for decimal
```

This is why `print(0755)` tells you about the bad number rather than "No such
file `print(0755)`".

### Shell chaining vs Mix chaining

`&&` / `||` / `;` on an **external-command** line are shell operators (command
lists) — `true`/`false` heads are external commands, so `true && echo yes` and
`false || echo fallback` are plain shell chains. On a **Mix-keyword or `$`-led**
line, Mix's own grammar owns `&&` / `||` and `;`, so a valid Mix
chain stays Mix. A keyword/`$` head that *fails* Mix parse **and** is a genuine
multi-command shell list normally falls back to the shell (e.g.
`send oops ; echo hi` runs as a two-command shell list); definitive language
errors such as an assignment-led chain do not. Mix-keyword heads never
reach the command-not-found tie-break — they route through the Mix-then-shell
path, so a keyword-led line that fails both surfaces its Mix error
(`print(1); echo hi` reports a Mix parse error, not "command not found").

In a **Mix-classified** chain, an assignment cannot be any operand of `&&` or
`||`, whether first, middle or last. Since these are statement-chain operators,
accepting `$ok = $a.ok && $b.ok` would assign only `$a.ok`, while
`print("gate") && $ok = false || print("fallback")` can conceal the falsy
assignment behind a successful statement chain. Mix rejects every such shape as
`MIX-E1002`, and the line's own operands never run. Use `and` / `or` inside the
assigned expression for logical values, or split the assignment and command
chain into two statements if shell-style conditional execution was intended.
This applies to variable, field, index and nested-path assignments, to the
value-binding `export x = v`, `alias n = c` and terse `function f() = expr`
forms, and to all of them inside nested statement bodies. The forms that bind
no `=` expression stay legal operands: `alias` in its query and list forms, and
the block-bodied `function f() … end`.

Once a line is rejected it stays rejected: the error never falls back to shell
execution under `mix -c`, `source`, or `~/.mixrc` loading.

The qualifier "Mix-classified" is load-bearing. Two conditions must **both**
hold before a line carrying an assignment keeps shell semantics:

1. the classifier routes the line to the shell — either straight off the head
   (a path, a name on `PATH`, a shell builtin), or through one of the
   fallbacks that reconsider a line whose Mix parse failed and which is
   structurally a shell command list (a shell redirect; an env prefix in
   front of a keyword head like `export`); and
2. the Mix parse never reaches the assignment, so the typed
   assignment-chain error is never constructed.

Where both hold, the line behaves as `bash` does with it: `$x = false` is
three shell words whose `$x` expands, the failed `=` command is reported on
stderr, and `||` fires. That covers a path-shaped head (`/usr/bin/true && $x =
false || cmd` fails the Mix parse at `unexpected token Slash`), a shell
redirect (`zqxfoo > /dev/null; $x = false || cmd`), and an env prefix
(`FOO=bar export x = false || cmd`). Adding a redirect or an env prefix to an
otherwise-rejected line therefore turns it from a Mix error into a shell
command list — a consequence of where the parse gives out, not a guarantee
that assignments are honoured there.

Neither condition is sufficient alone. **Failing to parse as Mix does not by
itself send a line to the shell**: `print(1); echo hi` and `$x = (((` are both
parse errors that stay Mix errors, because a call head and a `$` head are
never routed to the shell. And a head being a shell command is not enough
either — `true && $x = false || cmd` has a head that is both a `PATH` binary
and a valid Mix literal, so the parse reaches the assignment and condition 2
fails. Once the parser has typed this error no head rule may reclassify the
line; the same holds for a tight-hyphenated head (`zqx-foo && …`) and for
`cd`. Those head rules still decide every line that does *not* carry a
rejected assignment, so `cosmix-comp --nested` and `true && cmd` remain shell
as before. In a mixed sourced file, earlier shell lines execute normally
before a later line's rejection aborts the `source`.

One asymmetry predates this rule and is unchanged by it: the tight-hyphen head
discriminator exists only in the `mix -c`/REPL classifier. A `source`d file
reads `cosmix-comp --nested` as the subtraction `"cosmix" - "comp"` (the
option becomes a comment) and fails with `cannot use 'cosmix' as number` —
whether the file parses whole or falls back to per-line handling. Give such a
head its full path (`/usr/bin/cosmix-comp --nested`). Quoting it does **not**
work: `"cosmix-comp" --nested` is a discarded string followed by a comment, so
the command silently never runs.

```mix
$ok = $a.ok and $b.ok       -- logical value: valid
$value = primary() ?? fallback() -- nil fallback: valid
$ok = $a.ok && $b.ok        -- parse error: assignment cannot be chained
print("gate") && $ok = false -- same parse error; nothing prints
```

```text
$ mix -c 'true && echo yes'
yes
$ mix -c 'false || echo fallback'
fallback
```

The full set of shell-dispatch syntax — pipes, redirects, `$(...)` command
substitution, brace expansion — is documented in [shell-mode](shell-mode.md).
Those apply only on a line the classifier routed to the shell; inside Mix *source*
they are inert (`$(...)` is literal text, `{a,b}` is a map/string) — except
inside a heredoc body, which does interpolate `$(...)`.

### Running scripts vs one-liners

- `mix script.mix` / `mix -` (stdin) — normally a whole-file Mix **script** (`echo hi` remains a Mix parse error). An explicitly continued command line is the narrow exception: its physical lines are assembled, then the complete logical line enters the strict classifier fallback.
- `mix -c '<line>'` — a one-liner: the **whole argument** is classified as one unit (hence the leading-comment gotcha above). Use `;` or real newlines for multiple Mix statements.
- `mix -i -c '<line>'` — interactive flavour: loads `~/.mixrc` (aliases + PATH) before classifying, so an aliased head resolves.

See [invocation](invocation.md) for the full launch matrix.

## See also

- [strings](strings.md) — `'raw'` vs `"interp"`, `${...}`, `~`, `\u{}`, heredocs
- [shell-mode](shell-mode.md) — shell-dispatch syntax: pipes, redirects, `$(...)`, brace expansion, `&&`/`||`/`;`
- [keywords](keywords.md) — what each reserved word does
- [numbers](numbers.md) — f64 semantics, radix and scientific literals
- [variables](variables.md) — sigils, scope, the unbound-read rule
- [control flow](control-flow.md) · [functions](functions.md) — the `end`-closed block forms
- [operators](operators.md) — precedence, ternary `? :`, comparison/ordering, `??`, `..`
- [invocation](invocation.md) — `mix file` / `-c` / `-` / `-i`
- [Bus messaging](bus.md) — the `send` / `emit` / `address` / `on` keywords
- [builtins index](builtins.md)

```text
mix keywords          list the reserved words
mix help              the full categorized builtin reference
mix what NAME         one-line description of a builtin or keyword
```

The source of truth is the lexer and parser in the public
[mix repo](https://github.com/markc/cosmix/tree/main/src/crates/cosmix-lib-mix/src).
