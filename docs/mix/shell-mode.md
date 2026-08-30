# shell-mode — shell-dispatch mode

Mix is both a scripting language **and** a login shell. When you hand it a line
that looks like a command rather than Mix code — `git status`, `ls -la | wc -l`,
`make && ./run` — it dispatches that line to the system the way a shell does:
`&&` / `||` / `;` chaining, `|` pipes, brace expansion, `$(...)` command
substitution, redirects. This is **shell-dispatch mode**, the shell half of the
`mix -c` / login-shell contract.

The whole point: a node that runs `/opt/cosmix/bin/mix` as root's login shell
must still honour the universal `sh -c` contract — `ssh host 'hostname'` and
`ssh host 'systemctl restart foo'` have to *just work*, while `print(1 + 1)` and
every other Mix statement still evaluate as [Mix](overview.md). One classifier
decides per line which world you are in.

Since 0.31, `;` also separates **Mix statements** on a line classified as Mix
(`$x = 1; print($x)`). Classification is still whole-line: shell-dispatch gives
the glyph the command-list semantics documented here; executable Mix gives it
the hard statement-boundary semantics documented in [syntax](syntax.md).
It never switches languages midway. In particular, Mix permits `&&`/`||`
continuation across a physical newline, but `;` always ends that chain.

> **One sentence to carry:** shell-dispatch handles **one command, a pipeline,
> or a `&&`/`||`/`;` chain** with bash-style brace expansion, `$(...)`, and
> redirects — it is *not* a full shell scripting language (no `for`/`while`/`if`
> shell constructs, no functions — those are [Mix](control-flow.md)).

The two-context split is the thing to internalise: `$(...)` and `{a,b}` are
**LIVE** on a shell-dispatch line but **LITERAL** inside a Mix string. See
[The two-context split](#the-two-context-split) below.

## Where shell-dispatch happens

Three entry points run the same classifier (`shell::classify_input`) and the
same executor (`exec.rs`):

```
mix -c '<line>'         one-shot:  classify <line>, dispatch or evaluate
mix -i -c '<line>'      as -c, but load ~/.mixrc first (aliases + PATH)
<the REPL prompt>        every line you type interactively
ssh host '<line>'        if mix is root's login shell there, host runs it as -c
```

An ordinary `.mix` script remains whole-file Mix. The narrow exception is an
explicitly continued command line: its physical lines are spliced first, then
the complete logical line is classified through the same shell-first path. This
makes `echo one \` followed by `two` agree in a file, `-c`, and the REPL without
turning every bareword in every script into a command.

A login-shell node makes the last one transparent — `ssh host 'uptime'` is
evaluated by mix, dispatches `uptime` to the system, and returns its output and
exit code. Send Mix *source* when you want Mix behaviour; send a bare command
when you want shell dispatch.

> Two *other* paths run commands but are **not normally** this classifier: the
> [`run()` / `run_rc()`](system.md) builtins always spawn `/bin/sh -c`
> (POSIX sh — no brace expansion there), and a `.mix` script file is parsed
> whole as Mix unless the explicit-command-continuation exception above applies.
> Backslashes inside a `run`/`sh` argument string stay untouched and belong to
> that real shell.

`source` first tries to parse the entire file as Mix, then falls back to
classifying it line by line when that parse fails. An explicitly continued
logical line classified as shell also forces this fallback, even if the
half-joined text could have parsed as Mix arithmetic. Consequently, a file
containing argument-less bareword command lists such as `ls; pwd` can parse as
discarded Mix string expressions and do nothing; add command arguments to force
the shell fallback, or write explicit Mix calls in a pure-Mix file. This is a
known compatibility edge of whole-file-first sourcing.

## The classifier — shell or Mix?

Each line is classified **shell-first** by its first word:

| First word is… | Goes to |
|---|---|
| a [Mix keyword](keywords.md) head (`print`, `if`, `for`, `send`, `try`, …) | Mix (with a shell-chain fallback) |
| a `$`-led line (`$x = 1`) | Mix (with a shell-chain fallback) |
| a shell builtin (`cd`, `exit`, `which`, `mix`, …) | shell dispatch |
| a **defined [function](functions.md)** invoked bareword (`sc restart nginx`) | Mix — a command-style call `sc("restart", "nginx")` |
| a tight-hyphenated command head (`cosmix-comp`, `systemd-nspawn`) | shell dispatch, even when missing from `$PATH`; a missing command reports its complete name |
| found on `$PATH`, or an absolute/relative path | shell dispatch |
| none of the above, but parses as a Mix expression | Mix |
| none of the above and a command-like head | shell dispatch (so you get the familiar `No such file or directory`, exit 127) |

The exact keyword-head set routed to Mix: `if` `for` `while` `loop` `function`
`fn` `return` `select` `print` `eprint` `die` `try` `parse` `export` `alias`
`break` `continue` `send` `address` `emit` `source` `sh` `label`. Note what is
**not** in it: `true`, `false` and `nil` (see the next section).

### Bareword function dispatch — bash parity for your toolkit

A line whose head is a **currently-defined Mix function** dispatches as a
command-style call, so a ported bash toolkit works with no parens:

```text
$ sc restart nginx          -- runs sc("restart", "nginx")
$ health                    -- zero-arg call, no parens
$ newpw 20                  -- newpw("20")
```

The function check sits **after** Mix keywords and shell builtins but **before**
the `$PATH` probe, so a defined function **shadows a same-named binary** — matching
bash's `alias → keyword → builtin → function → PATH` order. Only a *simple*
command qualifies: plain word arguments, quotes (`'…'` / `"…"`) group and are
stripped, and there is **no variable or glob expansion** — bareword args are
**literal** (use the paren form `sc("restart", $svc)` when you need
interpolation). Anything with a pipe, redirect, `&&`/`||`/`;` chain, `$(…)`
substitution, or a `KEY=val` env prefix falls through to normal shell dispatch,
so a function in a pipeline is still the shell's job. Inside a `.mix` **script**
(the whole-file parse path) there is no shell classifier — call functions the
Mix way, `sc("restart", "nginx")`.

A function is "defined" only when it is in scope at classify time: interactive
`mix -i` (which loads `~/.mixrc` and the `~/.rc` toolkit) and `mix -i -c '…'`
see them; a bare `mix -c '…'` (no rc) does not, so there the same line stays a
shell command.

A line the classifier hands to Mix that then fails to lex/parse reports the
**real** lexer/parser error and exits 1 — `mix -c 'print(0755)'` prints the
leading-zero lex error, not "No such file". A genuine shell typo (`gti status`)
still gets `mix: gti: No such file or directory` and exit 127, and real
commands keep dispatching.

The tight-hyphen rule applies only to a command-shaped **statement head**: an
ASCII letter/`_` start, at least one unspaced `-`, an ASCII letter/digit/`_` end,
and no other characters. It does not reach into `print(a-b)`, assignments, or
parenthesised expressions; `a - b`, `$a-$b`, `$x-1`, and `1-2` are subtraction.
Scope does not affect the result: bare `a` is the string `"a"`, not `$a`, so
head `a-b` is a command even when `$a` is live. Exact earlier heads retain
precedence, so `mix not-a-command` is still a `mix` meta-command invocation.

```mix
print(1 + 1)        -- Mix: prints 2
```

```text
2
```

```mix
echo hi             -- shell dispatch: echo is on PATH
```

```text
hi
```

### Gotcha: a bare `true` / `false` runs the shell binary

Inside a Mix expression, `true`, `false`, and `nil` are literals (`$x = false`,
`if $a == nil`, `[true, false]`). But as a bare **line head** `true` and `false`
route to shell dispatch and run the `/usr/bin/true` / `/usr/bin/false` binaries —
so `false` exits **1** like in every other shell, honouring the universal shell
idiom:

```text
$ mix -c 'false'; echo "rc=$?"
rc=1
$ mix -c 'true'; echo "rc=$?"
rc=0
```

This is deliberate: forcing a bare `false` to the Mix literal made it exit `0`,
which broke `cmd || false`-style scripts. `nil` has no external binary, so a bare
`nil` still falls through to the Mix parse. In a `&&`/`||`/`;` chain they behave
like the shell too:

```text
$ mix -c 'true && echo chained'
chained
$ mix -c 'false || echo fallback'
fallback
```

## Chaining — `&&`, `||`, `;` (shell-dispatch lines)

bash semantics, evaluated left to right:

```text
$ mix -c 'echo one ; echo two'
one
two
$ mix -c 'mkdir -p /tmp/d && echo made'
made
$ mix -c 'test -f /nope || echo missing'
missing
```

- `;` — run unconditionally (sequence).
- `&&` — run only if the previous command **succeeded** (exit 0).
- `||` — run only if the previous command **failed** (non-zero).

The chain's exit code is the **last command actually executed**. A skipped
command carries the prior success/failure forward, exactly like bash.

A short-circuited branch is **never expanded or run** — a `$(...)` inside it has
no side effect:

```text
$ mix -c 'false && /bin/touch /tmp/pwn'
$ test -f /tmp/pwn && echo EXISTS || echo absent
absent
```

A single trailing `;` is allowed (`echo a ;`). A **leading or interior** empty
piece (`&& echo x`, `echo a ;; echo b`) is an error. (Note: a `&&`-led line is
tried as Mix first, so `mix -c '&& echo x'` surfaces a Mix parse error, not the
shell empty-piece error — start a chain with a real command.)

That empty-piece rule is shell-specific. On a line already classified as Mix,
leading/trailing/repeated semicolons are harmless empty statements:
`; print(1);;` prints `1`.

## Pipes — `|`

```text
$ mix -c 'printf "b\na\nc\n" | sort'
a
b
c
$ mix -c 'ls /etc | wc -l > /tmp/count.txt'
```

Each pipe segment is its own program; stdout of one feeds stdin of the next.
The pipeline's exit code is its **last** segment's. Pipes bind tighter than the
chain operators, so `a | b && c | d` is `(a | b) && (c | d)`.

## Background — trailing `&`

A `&` at the very **end** of a line backgrounds the whole pipeline and returns
immediately (exit 0):

```text
$ mix -c '/bin/sleep 5 &'; echo "rc=$?"
rc=0
```

A `&` that is *not* at the end is **not** a job separator (Mix has no mid-line
`&` job control like bash). To background one command and continue, split with
`;`:

```text
$ mix -c '/bin/sleep 5 & ; echo launched'
launched
```

In a one-shot `mix -c`, a backgrounded child is re-parented to init when the
process exits. In the long-lived REPL it goes into the job table (`jobs`,
`fg`, `bg`).

## Brace expansion

bash-5.3 brace expansion, expanded **before** variable/`$(...)` resolution
(like bash). Alternation `{a,b,c}` and sequences `{x..y[..step]}`:

```text
$ mix -c 'echo file.{txt,md,rs}'
file.txt file.md file.rs
$ mix -c 'echo {1..5}'
1 2 3 4 5
$ mix -c 'echo {a..e}'
a b c d e
$ mix -c 'echo {1..10..2}'
1 3 5 7 9
$ mix -c 'echo {a,b{1,2}}'
a b1 b2
```

**Sign-aware zero padding** — an explicit leading zero on an endpoint pads every
element to the widest endpoint's width:

```text
$ mix -c 'echo {01..03}'
01 02 03
$ mix -c 'echo {-03..3..2}'
-03 -01 001 003
```

Other bash-5.3 rules that hold: nesting (`{a,b{1,2}}`), reverse ranges
(`{5..1}`), the increment's absolute value is used and `0` means `1`. A group
that is **not** a valid brace expression stays **literal**:

```text
$ mix -c 'echo {a}'
{a}
$ mix -c 'echo a{}b'
a{}b
```

So `find . -exec {} \;` is safe — the `{}` is invalid as a group and passes
through untouched.

**Hardening (never a partial expansion):** a word expanding past ~10 000
results, or groups nested/chained past 64 deep, is left **completely
unexpanded** rather than partially expanded. The injection invariant holds —
braces are structural only in **raw literal text**: a quoted/escaped `{`, a
`${X}`, and anything a variable's *value* contains never expand.

## `$(...)` command substitution

Runs the inner command via `/bin/sh -c`, splices its stdout into the line with
trailing newlines stripped:

```text
$ mix -c 'echo today is $(date +%A)'
today is Monday
$ mix -c 'echo got-$(echo X)'
got-X
```

**Unlike bash, the result is ONE word** — it is *not* re-split on whitespace.
This is a deliberate safety difference:

```text
$ mix -c 'echo [$(echo a b c)]'
[a b c]
```

(In bash, `echo [$(echo a b c)]` would re-split into three words. Mix keeps the
captured output intact as a single argument — closer to a quoted `"$(...)"`.)

Things that **stay literal** on a shell-dispatch line:

```text
$ mix -c 'echo $((1+2))'          # arithmetic $(( )) is NOT command substitution
$((1+2))
$ mix -c "echo '\$(echo X)'"      # single-quoted is fully literal
$(echo X)
```

An inner `|` / `;` / `&&` belongs to the **substitution**, not the outer line:
`echo $(ls | wc -l)` runs `ls | wc -l` inside the `$(...)` and splices the
count. A lazy detail: a `$(...)` is run **only** when its chain piece is
selected for execution (see the short-circuit example above).

## Redirects

In parse order, like bash:

```
>  file     1> file    redirect stdout (truncate)
>> file     1>> file   redirect stdout (append)
<  file                redirect stdin
2> file                redirect stderr (truncate)
2>> file               redirect stderr (append)
2>&1                   fd 2 := fd 1   (stderr to wherever stdout points)
1>&2  / >&2            fd 1 := fd 2   (stdout to stderr; >&2 is the common idiom)
>&1                    1>&1 — stdout onto itself, a no-op
&> file     &>> file   both streams to file  (≡ >file 2>&1 / >>file 2>&1)
```

```text
$ mix -c 'echo hi > /tmp/out.txt'; cat /tmp/out.txt
hi
$ mix -c 'echo l1 > /tmp/log'; mix -c 'echo l2 >> /tmp/log'; cat /tmp/log
l1
l2
$ mix -c 'ls /nope 2>&1' | head -1
ls: cannot access '/nope': No such file or directory
$ mix -c 'ls /nope &> /tmp/both.txt'; cat /tmp/both.txt
ls: cannot access '/nope': No such file or directory
```

Ordering matters exactly like a shell: `>f 2>&1` sends both streams to `f`,
while the unusual `2>&1 >f` keeps stderr on the original stdout.

Mix dups **only fds 1 and 2**. An unsupported fd target (`>&3`, `>&-`, `3>&1`)
is a clear **error**, never a silent junk file:

```text
$ mix -c 'echo hi >&3'
unsupported fd in redirect '1>&3' — Mix dups only fds 1 and 2
```

A bare `&` is a word-breaking metacharacter only immediately before `>`
(`cmd arg&>out` redirects, no stray `arg&`); a lone `&` mid-word is literal, and
a trailing `&` is background.

## `mix` meta-commands under plumbing (interactive REPL)

A bare `mix <sub>` line at the REPL is a **meta-command**: it runs in-process
so it can read this shell's live state (`mix vars`, `mix history`, `mix trace
on`, live `mix stats` counters). In-process output prints straight to the
terminal, so plumbing on the line — a pipe tail, any redirect, a background
`&`, or an env prefix — cannot be honoured by that path.

Since 0.61.1 a plumbed `mix …` line is therefore **not intercepted**:

- **Stateless subcommands** (`stats`, `builtins`, `man`, `keywords`, `help`,
  `version`, `config`, `what`, …) run as a normal external pipeline, so
  `mix stats never | wc -l`, `mix stats never >> log`, and
  `mix builtins --json | jq .` all work. Live stats are flushed to disk first,
  so the external `mix stats` child reads current data including this session.
- **Live-state subcommands** (`history`, `vars`, `aliases`, `functions`,
  `all`, `type`, `status`, `context`, `snapshot`, `ask`, `chat`, `trace`,
  `diagnose`, `reload`, `build`, `update`, and bare `mix`) are **refused
  loudly** with exit status 2 — an external child would silently answer from
  different state. `mix stats reset` / `mix stats clear` are refused under
  plumbing for the same reason: they mutate the persisted store under an
  invariant that spans this shell's live counters.

```text
alpha ~ mix history | wc -l
mix history: runs inside the interactive shell — pipes, redirects and & are not supported; run it bare
```

Before 0.61.1 the plumbing was silently dropped: `mix stats never | wc -l`
printed the report and never ran `wc`; `mix stats never > x` created nothing.

All of the above applies to a **single-pipeline** line. A `;`/`&&`/`||`
**chain** is a different intake with no meta-command interception (shell
builtins like `cd` still run in-process there): `mix vars; echo hi` answers
`vars` from a fresh external child (empty), with no refusal and no stats
flush. Don't put live-state meta-commands — or the mutating
`mix stats reset` / `mix stats clear` — in chains.

## Variables and the injection invariant

On a shell-dispatch line, `$NAME` / `${NAME}` resolve from the Mix scope, then
the environment:

```text
$ X=hello mix -c '/bin/echo "got: $X"'
got: hello
```

The structural decisions — where the operators, redirects, `name=`
assignments, fd prefixes, and brace groups are — are fixed from the **literal
line text** in a pass that never sees a value. So a variable's value can
**never** inject syntax: it is always a single data word, never a `;`, a `&&`,
a redirect, or an extra argument.

```text
$ X='a; echo PWNED' mix -c '/bin/echo "[$X]"'
[a; echo PWNED]
```

The `;` inside `$X` is data, not a command separator. The same holds for
`$(...)` results — captured stdout is spliced as data and never re-parsed for
operators.

A leading `name=value` (literal key only) sets an env var for that command,
bash-style:

```text
$ mix -c 'FOO=bar /bin/sh -c "echo \$FOO"'
bar
```

## In-process builtins under `-c`

A handful of builtins are handled **in-process** rather than spawned. `cd` is
intercepted as a lone command **and** in a single-segment foreground
`&&`/`||`/`;` chain, so it changes the directory instead of failing to spawn a
nonexistent `cd` binary:

```text
$ mix -c 'cd /tmp && pwd'; echo "rc=$?"      # cd in a chain: in-process
/tmp
rc=0
$ mix -c 'cd /nope && pwd'; echo "rc=$?"     # failing target: rc=1, not 127
cd: /nope: No such file or directory (os error 2)
rc=1
```

The narrow exceptions still spawn (and fail) so a redirect or env prefix is never
silently dropped — a **piped**, **backgrounded**, **redirected**, or
**env-prefixed** `cd`:

```text
$ mix -c 'cd /tmp | cat'; echo "rc=$?"       # piped cd: spawns, fails
mix: cd: No such file or directory (os error 2)
rc=127
```

`exit` is intercepted only as a **lone** command; in a chain it spawns like any
external program and fails. Note the shell-layer `exit N` parses N as a plain
i32 (an unparseable N — including a fractional one — silently exits `0`, and
the OS truncates a status above 255), while the *language* builtin `exit(n)`
validates `0..=255` and raises `VALUE_OUT_OF_RANGE` outside it (since 0.59.0)
— a known language/shell-layer divergence, queued for its own fix. The bare `mix` head is self-resolved to the running
binary (so `mix --version` re-enters this executable regardless of PATH), and a
bare `mix` with no subcommand under `-c` is refused (it would start a nested
REPL). The REPL-only builtins (`jobs`, `fg`, `bg`, `pushd`, `popd`, `history`,
`unalias`) have no meaning under `-c`.

```text
$ mix -c 'mix --version'
mix 0.21.2
```

## The two-context split

The single most important rule. The **same characters** mean different things
depending on whether they are on a shell-dispatch line or inside a **Mix
string**:

| Construct | On a shell-dispatch line | Inside a Mix `"string"` |
|---|---|---|
| `$(cmd)` | **runs** `cmd`, splices stdout (one word) | **literal text** `$(cmd)` |
| `{a,b}` / `{1..5}` | **brace-expands** | **literal text** (a `{...}` in Mix source is a map literal or literal text) |
| `$((1+2))` | literal (arithmetic, unsupported) | literal text |
| `${NAME}` | env/scope interpolation | scope→env interpolation |
| bare `$name` | env/scope interpolation | **literal text** (the opposite of bash) |

```text
$ mix -c 'print("{1..3}")'        # Mix string: braces are literal
{1..3}
$ mix -c 'print("$(echo hi)")'    # Mix string: $(...) is literal
$(echo hi)
$ mix -c 'echo {1..3}'            # shell line: braces expand
1 2 3
```

The trap: you reach for `$(...)` or `{a,b}` inside a Mix double-quoted string
and nothing happens — they are inert data there. To splice a command's output
**in Mix code**, use [`run()` / `run_rc()`](system.md) and `..`
concat:

```mix
$count = trim(run("ls /etc | wc -l"))
print("etc has " .. $count .. " entries")
```

Two narrow exceptions where `$(...)` *does* substitute in Mix source: a
**heredoc** body, and the standalone `$(cmd)` **expression** (see
[strings](strings.md)). And in a `run()` / `ssh_run()` command **string**, a
`$(...)` passes through to the *target* shell that runs the command (local for
`run`, remote for `ssh_run`). For multi-line remote Mix or nested quotes, skip
that extra parse with the
[`ssh_mix` + heredoc idiom](remote.md#headline-idiom-ssh_mix--heredoc).

## Timing — the `time` modifier

`time` is a **modifier**, not a command — exactly as in bash, where `time` is a
shell keyword and not a program. Mix resolves it before dispatch, so it wraps
whatever the rest of the line turns out to be: an external command, a pipeline,
a chain, a bareword function, or Mix code.

```text
$ time shwho example.org
...
Elapsed: 412.181ms

$ time sum([1, 2, 3, 4, 5])
15
Elapsed: 0.012ms

$ time ls | wc -l
18
Elapsed: 1.251ms
```

The elapsed line goes to **stderr**, so a timed command's stdout stays clean for
a pipe or a redirect. The timed command's exit code is passed through unchanged.
Sub-second runs report milliseconds; longer ones promote to `1.500s` / `1m5.000s`.

`time` is resolved **before alias expansion**, so the wrapped head still expands
— `time ll` times `ls -l`. A bare `time` with nothing to run reports usage and
exits 2.

Being resolved before dispatch, the modifier **shadows every other `time`**: a
`time` binary on PATH (GNU `/usr/bin/time` — reach it by full path, as Mix has no
`command` builtin), an `alias time = …`, and a user-defined Mix `function time`.
Bash's keyword shadows a PATH `time` the same way. Rename any Mix function you
want to keep callable bareword; a paren call is unaffected.

`time()` — with parens — is the unrelated [`time()` builtin](datetime.md)
returning a Unix timestamp, not the modifier: only a bare `time` HEAD followed by
something to run is stripped. `timeout 5 cmd` is likewise untouched (the head must
be exactly `time`).

`mix time EXPR` is the same modifier under its older spelling, kept working.

The modifier lives in the shell-dispatch layer, so it is available at the prompt,
under `-c`, and over ssh (`ssh host 'time shwho example.org'`) — but NOT in a
`.mix` script (parsed as Mix, not dispatched line-by-line) and NOT in a sourced
line or `~/.mixrc`. To time a region of a script, subtract two
[`time()`](datetime.md) reads, or use the `duration_ms` field that
[`run_argv()`](system.md) already returns.

Two lines report no elapsed. A backgrounded command (`time sleep 5 &`) returns at
spawn, so there is no runtime to measure — it says `backgrounded (&) — not timed`
rather than print the spawn latency as if it were the duration. And an `exit()`
mid-line (or a `mix build` that re-execs) never returns, so nothing survives to
report; bash does print a timing for `time exit`, Mix does not.

`time;`, `time&` and `time|` have nothing to run and report usage.

## What shell-dispatch is NOT

Shell-dispatch handles a command, a pipeline, or a chain — **not** shell control
flow. There is no `for`/`while`/`if`/`case` shell syntax, no shell functions, no
`$?` (use the structured return of [`run_rc()`](system.md)). Those
constructs are [Mix](control-flow.md):

```text
$ mix -c 'for x in $(echo a b c); do echo "[$x]"; done'
mix: for: No such file or directory (os error 2)
mix: do: No such file or directory (os error 2)
mix: done: No such file or directory (os error 2)
```

Mix saw `for` as a keyword, tried to parse Mix, and `do` is not even a Mix
keyword — so it fell back to the shell path and tried to *run* `for` / `do` /
`done` as programs. Write it as a Mix loop instead:

```mix
for each $x in split(trim(run("echo a b c")), " ")
  print("[" .. $x .. "]")
end
```

```text
[a]
[b]
[c]
```

When in doubt: a PATH/bare command head → shell dispatch; a `$`/Mix-keyword/call
head → Mix. Operators later on the line—including `;`—do not override the
head-first classifier.

## Aliases (`-i` only)

`~/.mixrc` aliases and PATH load for **interactive** logins (`mix -i`,
`mix -i -c`, the REPL) — **not** a plain `mix -c`. So a non-interactive
`ssh host '<line>'` runs with no aliases. Aliases are expanded on the first word
before classification, so an alias expanding to a chain (`u = "sudo apt update
&& sudo apt upgrade"`) lands on the shell path correctly.

## Terminal output mode (interactive REPL)

The interactive REPL owns the terminal's output post-processing: before every
prompt it re-asserts `OPOST | ONLCR` on stdout, so a bare `\n` from an external
command always arrives as CR-NL and lines start at column 0. This repairs an
inherited raw tty (Mix started as a login shell on a terminal left with `ONLCR`
off) or a foreground child that exited without restoring it — the classic
"staircase", where each line steps rightward, can't persist across a prompt.

Consequence: a `stty -onlcr` you run at the Mix prompt does **not** stick — Mix
re-enables CR-NL output on the next prompt. Mix touches only those two output
bits; input flags, control characters, flow control and every other `stty`
setting are left alone. Non-interactive modes (`mix -c`, scripts, `mix --serve`)
never touch the tty.

## See also

```
mix help              the full categorized builtin reference
mix what run          one-line help for the run() builtin
mix keywords          every Mix keyword (so you know what the classifier reserves)
```

- [overview](overview.md) — the language at a glance
- [running-commands](system.md) — `run` / `run_rc` (via `/bin/sh`, structured returns), `run_argv` / `run_stream` (argv lists, no shell)
- [strings](strings.md) — `'raw'` vs `"${...}"`, heredocs, the literal-`$(...)` rule
- [keywords](keywords.md) — Mix keywords the classifier routes to Mix
- [control-flow](control-flow.md) — `if` / `for each` / `while` (the Mix replacements for shell control flow)
- [bus](bus.md) — `send` / `emit` / `on … end`, the Bus-native side of the language
- [The Mix repo](https://github.com/markc/mix) — [AGENTS.md](https://github.com/markc/mix/blob/main/AGENTS.md) is the agent orientation sheet on top of this manual
