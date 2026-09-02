# Invoking Mix

`mix` is one binary with several entry modes: an interactive shell (REPL), a
script runner, a `-c` one-liner, a stdin reader, a syntax checker, a supervised
Bus daemon, and a family of one-shot meta commands (`mix help`, `mix man`,
`mix stats`, …). The same binary is also Mark's mesh **login shell**, so
`ssh host '<mix source>'` is evaluated by Mix, not bash. This page covers how to
start Mix in each mode and the rules that decide whether a `-c` / login-shell
line runs as Mix code or dispatches a shell command.

Verified against **mix 0.61.0** — the binary is the oracle. Argument parsing
lives in [`cosmix-mix/src/main.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-mix/src/main.rs);
the shell-first classifier is in `cosmix-mix/src/shell.rs`.

```text
$ mix --version
mix 0.21.2
```

## Modes at a glance

| Form | What it does |
|---|---|
| `mix` | Interactive REPL (loads `~/.mixrc`) |
| `mix script.mix [args…]` | Run a file from the OS shell; args via `args()` / `$1`, `$2`, … |
| `mix script.mix [args…]` at the REPL | Run the file in a clean child process; the REPL survives |
| `mix -c '<code>'` | Run one line of Mix **or** dispatch a shell command (shell-first classifier) |
| `mix -i -c '<code>'` | As `-c`, but load `~/.mixrc` first (aliases + PATH), like `bash -ci` |
| `mix - [args…]` | Read the whole script from **stdin** (clean byte channel) |
| `mix --check <script>` | Parse without executing — a syntax check |
| `mix lint [flags] FILE...` | Semantic analysis: undefined names, arity, must-use, stable machine-readable diagnostics (0.29.0 — see [lint](lint.md)) |
| `mix --no-prelude …` | Skip loading the standard prelude (applies to any run mode) |
| `mix --no-traceback …` | Uncaught errors print the legacy single line instead of a traceback (0.29.0 — see [errors](errors.md)) |
| `mix --strict-arity …` | Strict call arity: wrong-arity user-function/builtin calls raise catchable `ARITY_MISMATCH` instead of the compatible missing→nil / extra-ignored binding (0.29.0 — see [functions](functions.md)) |
| `mix --serve <script> [--name <svc>]` | Run the script as a supervised Bus daemon citizen — see [serve](serve.md) |
| `mix --version` / `-V` | Print version |
| `mix --help` / `-h` | Usage summary |
| `mix stats [sub…]` | Window-labelled usage reports and static authorship coverage — see [stats](stats.md) |
| `mix help` / `mix what <name>` / `mix man [topic]` | Builtin/keyword reference and this manual — see [the `mix` meta-command](cli.md) |

`mix help`, `mix what`, `mix man`, `mix status`, `mix build`, … are the same
meta commands the REPL's bareword `mix` dispatcher accepts, exposed as one-shot
CLI commands: `mix man invocation` prints this page; bare `mix man` prints
[the manual index](README.md). In both contexts, meta names are checked
**before** the script-file fallback, so a script named `build`, `status`, or
`test` in the CWD is shadowed — prefix the path with `./`, for example
`mix ./build`. Full list on [the cli page](cli.md). An unknown option
(`mix --zz`) errors and exits 1; an unknown REPL meta token names the token,
notes that no such file exists, then shows the meta-command overview.

Bare `mix` with **no arguments** starts the REPL — including when stdin is a
pipe. The piped lines are then read and executed one at a time through the
shell-first classifier (with `~/.mixrc` loaded), so a pipe into bare `mix`
**does** run code — it works, but it is not the intended path. For a script use
`mix <file>` or `mix -`: a whole-program parse over a clean byte channel, no
per-line classification, no `~/.mixrc`.

All modes that evaluate Mix record best-effort usage by default and are tagged
as `interactive`, `script`, `c`, `stdin`, or `serve`. Set `MIX_STATS=off` (also
`0` or `false`) to skip collection and all stats I/O. See [usage
statistics](stats.md).

## Running a script file

The first non-flag argument is the script path; everything after it is passed to
the script.

```mix
-- greet.mix
$all = args()
print("argc=" .. length($all))
print("first=" .. $1)
for each $a in $all
  print("arg: " .. $a)
end
```

```text
$ mix greet.mix alpha beta
argc=2
first=alpha
arg: alpha
arg: beta
```

Two ways to read arguments, both populated the same way:

- **`args()`** returns the list of script arguments (not counting the script path, and not counting any flag given to `mix` itself — `mix --strict-arity greet.mix alpha` passes one argument, not two). See [builtins](builtins.md).
- **Positional `$1`, `$2`, …** are the same arguments by index; `$0` is the script path / filename. These are the one place a bare sigil number is allowed to be unbound without raising — an out-of-range positional reads as `nil`.

`include` inside a script resolves **relative to the running file's directory**
(not the CWD), because the runner records the script path (it is also `$0`, and
error diagnostics attribute to it). The every-time, CWD-relative loader is
`source`. See [modules](functions.md).

The script's exit code is `0` on success, `1` on an uncaught error (the error
message goes to stderr); a `Ctrl-C` interrupt is a clean exit.

A command line ending in an odd run of backslashes may continue onto the next
physical line. Mix splices it before classification, so the same command behaves
identically here, under `-c`, and in the REPL. Even trailing runs are literal;
backslashes inside Mix strings and heredoc bodies are untouched. See
[syntax](syntax.md#explicit-line-continuation--at-end-of-line).

At the REPL or login-shell prompt, `mix FILE [args…]` launches the same script
mode as the OS-shell form in a child process. It therefore gets a **clean
scope**, `$0` is `FILE`, and the remaining words populate `args()` and `$1`,
`$2`, …; when it finishes, the existing REPL resumes. Use `source FILE` when
the file should instead execute in and modify the **current session scope**.
Meta names keep precedence over files: if `status` exists in the CWD,
`mix status` still shows status and `mix ./status` runs the file.

A script that registers `on <cmd> … end` handlers does **not** exit after its
last statement — it stays alive pumping events for those handlers. For the
supervised daemon variant (broker registration, reconnect, journald logging),
use [`--serve`](serve.md).

## `mix -c` — one-liners and the shell-first classifier

`mix -c '<code>'` runs a single line. **It is shell-first**: the same classifier
the REPL uses decides whether the line is Mix code or an external command. This
is what makes Mix usable as a login shell — `mix -c 'hostname'` dispatches a
command, `mix -c 'print(1 + 1)'` evaluates Mix.

```text
$ mix -c 'print(1 + 1)'
2
$ mix -c 'echo hello from shell'
hello from shell
```

### How the classifier decides (shell.rs `classify_input`)

A line starting with `#` or `--` is a comment (empty input). Otherwise the first
word (after skipping `KEY=VALUE` env prefixes and expanding any alias) routes
the line:

1. **Starts with `$`** → Mix code (e.g. `$x = 1`), with a shell-chain fallback.
2. **A Mix statement-keyword head** (`print`, `if`, `for`, `send`, `function`, `try`, …) → Mix, also with the shell-chain fallback. `true` / `false` / `nil` are deliberately **not** keyword heads (since 0.21.0): a bare `true` or `false` line routes to `/usr/bin/{true,false}` so `false` exits **1** like every shell, and `nil` (no external binary) falls through to the Mix parse. Inside Mix source the lexer still owns all three literals (`$x = true`).
3. **A shell builtin head** (`cd`, `pushd`, `popd`, `history`, `exit`, `which`, `type`, `jobs`, `mix`, …) → external path.
4. **A tight-hyphenated command-shaped head** (`cosmix-comp`, `systemd-nspawn`) → external command before Mix can parse it as subtraction, whether or not it is on `PATH`. The rule is statement-head-only and requires an ASCII letter/`_` start with only ASCII letters/digits/`_`/`-`; spaced `a - b`, sigil/numeric subtraction, and expression-position `print(a-b)` stay Mix. Scope does not affect the decision: bare `a` is the string `"a"`, not `$a`, so `a-b` remains a command when `$a` is live.
5. **First word found on `PATH` or is a path** (`./x`, `/bin/ls`) → external command (design principle: shell-first). Routing is head-first — `&&` / `||` / `;` never force the shell path by themselves: a Mix-keyword head with a chain stays Mix (Mix has native statement chaining), an external head parses the chain on the shell path.
6. **Otherwise** → parse as Mix (function call, expression). If that fails definitively, a genuinely command-like head (`gti status`) reports the familiar "command not found" and exits 127; a Mix-shaped head — a lexer failure (`0755`), an operator, a glued call/index opener (`print(…`) — surfaces the **real lex/parse error** and exits 1. An unknown command-like semicolon list (`zqxfoo; zqxbar`) stays on the shell path even though each bare word could parse as a discarded Mix string; this preserves the pre-0.31 shell result instead of silently succeeding. A known preserved wart is that a non-keyword alphabetic head such as `not $x; print(1)` is also shell-routed and exits 127; use a Mix keyword, sigil, or parenthesized call to make the intent explicit.

Two consequences worth internalising:

```text
$ mix -c 'false' ; echo $?    # routes to /usr/bin/false — exits 1, like every shell
1
$ mix -c 'true' ; echo $?
0
$ mix -c 'print(false)'       # the boolean literal, inside Mix source
false
```

A dispatched command's exit status becomes Mix's exit code
(`mix -c '/bin/sh -c "exit 7"'` exits 7). `&&` / `||` / `;` chaining and pipes
work on the external path, and a plain foreground `cd` works inside chains
(`cd /tmp && pwd`) — `-c` also has bare `cd` (→ `$HOME`), `cd -`, and
`~`-expansion. See [the shell page](shell-mode.md) for the full dispatch
grammar (redirects, brace expansion, `$(…)` substitution).

A definitive Mix lex/parse error exits non-zero with the real diagnostic — it is
**not** silently retried as a command:

```text
$ mix -c 'print(0755)'
Lexer error at line 1:7: ambiguous leading-zero number '0755' — use a 0o (octal) / 0x (hex) / 0b (binary) prefix, or drop the leading zero(s) for decimal
$ echo $?
1
```

### The bare-name footgun

Every Mix variable needs a `$` sigil. A bare `name = value` line is read as a
shell command, not an assignment:

```text
$ mix -c 'x = 1'
mix: x: No such file or directory (os error 2)
$ echo $?
127
$ mix -c '$x = 1
print($x)'          # the $ sigil makes it Mix
1
```

See [variables](variables.md) for the sigil rules.

### Multi-statement `-c`

Mix statements may be separated by `;` or real newlines. Semicolons are the
compact one-line form:

```text
$ mix -c '$x = 5; $y = 7; print($x * $y)'
35
```

Literal newlines remain useful for a readable multi-statement body:

```text
$ mix -c '$x = 5
$y = 7
print($x * $y)'
35
```

### Positional args under `-c`

Everything after the code string is a positional `$1`, `$2`, … — even a word
starting with `-` (so flags like `--no-prelude` must come **before** `-c`).
`$0` is unset (`nil`) under `-c`.

```text
$ mix -c 'print("got " .. $1 .. " and " .. $2)' one two
got one and two
```

A bare `mix` inside a `-c` line is refused (it would otherwise launch a nested
REPL):

```text
$ mix -c 'mix'
mix: -c: bare 'mix' would start an interactive REPL — ignored
```

## `mix -i` — load `~/.mixrc` first

`-i` makes a `-c` run interactive-flavoured (≈ `bash -ci`): it sources
`~/.mixrc` first, so your aliases and the toolkit's `PATH` are in scope before
the line is classified and run. The combined spellings `-ci` and `-ic` mean the
same thing.

```text
$ mix -i -c '<line>'     # aliases expanded, ~/.mixrc PATH applied
$ mix -ci '<line>'       # same
```

Without `-i`, a plain `mix -c` has **no aliases** and does not read `~/.mixrc`
(≈ `bash -c`). `~/.mixrc` also auto-loads for the plain interactive REPL
(`mix` with no args). It does **not** load for a non-interactive `ssh host
'<mix>'` — that path is closest to `mix -c`.

## `mix -` — read a script from stdin

`mix -` reads the entire program from stdin as a clean byte channel — no shell
re-quoting, nothing written to disk. This is the residue-free way to ship a
script to a remote host (it is also the transport under the `ssh_mix` builtin —
see [remote](remote.md)).

```text
$ echo 'print("from stdin: " .. (2 * 21))' | mix -
from stdin: 42
$ echo 'print("name=" .. $1)' | mix - worldarg
name=worldarg
```

Args after `-` are the script's positionals, exactly as with a file. `$0` is set
to `"-"`. Unlike a pipe into bare `mix` (which the REPL consumes line by line
through the classifier), `mix -` parses the whole program up front.

## Running as a login shell and over SSH

When `/opt/cosmix/bin/mix` is **root's login shell** on a host, `ssh host
'<cmd>'` is evaluated by Mix, not bash. The universal shell `-c` contract still
holds because the login path runs through the same shell-first classifier:

- **`ssh host hostname`** — bare command, dispatched natively (no Mix needed).
- **`ssh host 'mix status'`** — `mix` self-resolves via `current_exe`, no PATH needed for this case.
- **`ssh host '<mix source>'`** — send Mix source to get Mix behaviour, e.g. `ssh host 'print(run("hostname"))'`.

```text
$ ssh node1 'print("hostname is: " .. run("hostname"))'   # node1 runs mix as its login shell
hostname is: node1
```

(Locally, `echo '<mix source>' | mix -` runs the same script against *this* host.)

From a Mix script, drive a remote mix through the ssh builtins instead of raw
`ssh`: `ssh_run($host, $cmd)` for a one-liner, and `ssh_mix($host, $source)` —
which ships whole Mix source over ssh stdin into `/opt/cosmix/bin/mix -`,
bypassing every shell-quoting layer — for anything with quoting. The
[`ssh_mix` + heredoc headline idiom](remote.md#headline-idiom-ssh_mix--heredoc)
shows the interpolation and bindings rules.

Two PATH gotchas to remember on the remote side:

- Inside `run` / `run_rc` (which spawn `/bin/sh` with a **minimal PATH**), call binaries by **full path** — `/opt/cosmix/bin/mix`, not bare `mix`. See [running commands](system.md).
- A non-interactive `ssh host '<mix>'` has **no aliases** (it's the `mix -c` path, not `mix -i`).

For Mix-as-a-shell language facts (`&&`/`||`/`;` chaining, pipes, redirects,
brace expansion, `$(…)` command substitution on a shell-dispatch line), see
[the shell page](shell-mode.md).

## `--no-prelude`

Mix loads a small standard prelude (Mix-source helpers) before running your
code; `--no-prelude` skips it. It applies to any run mode (`-c`, file, `-`,
`--serve`). Builtins are compiled in and are unaffected — only prelude-defined
helpers disappear. Give the flag **before** the code string / script path
(after them it would be read as a script argument); `--serve` also accepts it
after the script path.

```text
$ mix --no-prelude -c 'print("" .. range(1, 3))'   # range is a builtin, still works
[1, 2, 3]
```

The prelude source is `cosmix-lib-mix/std/prelude.mix`, compiled into the binary.
A user override at `<base>/prelude.mix` is honoured if it exists, where `<base>`
is `$COSMIX_SRC` when that variable is set, otherwise `$COSMIX`. (Setting
`$COSMIX_SRC` *replaces* the base — `$COSMIX/prelude.mix` is then not consulted.)
Otherwise the built-in prelude is used.

## `--check` — syntax check without running

`--check` lexes and parses a script and prints `<file>: OK`, or the first
parse/lex error and exits 1. Nothing is executed.

```text
$ mix --check good.mix
good.mix: OK
$ echo $?
0
$ mix --check bad.mix
Parse error at line 3:1: expected End, got Eof
$ echo $?
1
```

Use it in CI or a pre-commit hook to catch unterminated blocks (missing `end`),
unclosed strings, and lexer errors before deploy.

## `--serve` — run a script as a supervised Bus daemon

`mix --serve <script> [--name <svc>]` runs the script as a long-lived,
**supervised** Bus citizen instead of a one-shot script. It connects to the local
broker (`cosmix-noded`), registers, and runs an unconditional event pump that
dispatches your `on <cmd> … end` handlers until SIGTERM/Ctrl-C or a `QUIT`.

```text
$ mix --serve worker.mix              # Bus service name = script stem ("worker")
$ mix --serve worker.mix --name probe # explicit service name
```

Name derivation (SPEC 18 §3.1): `--name` wins; otherwise the default is the
script's file **stem**. A leading `cosmix-` is stripped from either source
(`/usr/local/lib/cosmix/statecache.mix` → `statecache`, and `--name cosmix-foo`
canonicalises to `foo`). An anonymous serve with no derivable name is a launch
error — never a nameless citizen. Serve mode takes **no positional script args**
(a daemon has no argv); only `--name` and `--no-prelude` may follow the script
path, in any order.

It differs from a plain run in four ways: a reconnecting supervised transport (a
broker bounce is a transient drop, not a death); an **unconditional** event pump
(a plain run pumps only when the script registered handlers); an exhausted
initial connect budget that fails fast (exit non-zero) under systemd; and a
runtime-reserved verb surface — `HELP` / `INFO` / `QUIT` plus
`<svc>.props.{get,list,describe}` are answered by the runtime pre-dispatch, so
an `on` handler can never shadow them. Logging goes to **journald** (stderr
fallback off-systemd), tagged with `service = <svc>`. The full contract is on
[the serve page](serve.md); see [Bus messaging](bus.md) for `on … end` handlers
and the broker model.

## Resource note

Scripts run by the `mix` binary on the main thread get a raised recursion cap
(128 vs the library default of 16). A runaway still returns a clean error rather
than a native stack overflow. The binary does **not** sandbox its own operator —
time/collection caps stay unset for binary-run scripts.

## See also

- [variables](variables.md) — the `$` sigil and scope rules
- [strings](strings.md) — `'raw'` vs `"${…}"`, `..` concat
- [functions](functions.md) — `args()`, positionals, `include`/`source`
- [the shell page](shell-mode.md) — chaining, pipes, redirects, brace/`$(…)` expansion
- [running commands](system.md) — `run` / `run_rc` and the minimal PATH
- [remote execution](remote.md) — `ssh_run` / `ssh_must` / `ssh_mix`
- [serve](serve.md) — the full `--serve` citizen contract
- [the `mix` meta-command](cli.md) — `mix help`, `mix man`, `mix stats`, …
- [Bus messaging](bus.md) — `send` / `on … end` and `--serve` citizens
- [builtins index](builtins.md) — `args`, `range`, `run`, …
- [the manual index](README.md)
- `mix help` — full categorised builtin reference; `mix what <name>` — per-name lookup
- [Mix repo](https://github.com/markc/cosmix) · [ARexx background](https://en.wikipedia.org/wiki/ARexx)
