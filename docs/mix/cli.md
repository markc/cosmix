# cli — the `mix` meta-command

Inside the Mix REPL (and from the OS shell as `mix <subcommand>`) the bareword
**`mix`** is its own dispatcher: a built-in suite for introspecting the live
session, reading the language reference, building/testing the interpreter,
probing the Bus mesh, and driving AI-assisted workflows. It is *not* a Mix
builtin function — it is a command the shell intercepts before evaluation, so
you write `mix vars`, never `mix("vars")`.

Two execution contexts, one command surface:

- **From the OS shell** — `mix status`, `mix builtins math`, `mix what round`. The binary keeps an explicit allowlist of meta names and checks it *before* treating the argument as a script filename — so a script named `build` in the CWD is shadowed (run it as `mix ./build`). The session it reports on is the one that binary just started (so `mix vars` from a cold shell shows only the prelude functions), which makes the build/reference/mesh families useful while the *introspection* family is most meaningful inside a running REPL. `mix stats` also works here as a one-shot: every report names its window, and `mix stats coverage DIR` uses the real parser for static authorship coverage. See [usage statistics](stats.md).
- **Inside the REPL** — the same plus the **stateful** subcommands that need live REPL state: `time`, `trace`, `history`, `reload`, `diagnose` (and `stats` against the live in-session counters). Those live in the REPL loop, not `meta::dispatch`, because they need readline history, the trace flag, or the usage-stats handle. From the OS shell these names are *not* on the allowlist — `mix trace on` there is read as a script filename (`Error reading 'trace': No such file or directory`). After checking all meta names, the REPL also treats an existing first token as a script path: `mix FILE [args…]` runs it in a clean child process and returns to the same prompt. This is deliberately different from `source FILE`, which executes in and can modify the current session scope.

Bare `mix` prints the status overview **inside the REPL**; from the OS shell
it starts the REPL instead (use `mix status` there). Meta names win over CWD
files in both contexts, so `mix status` remains the status command even when a
file named `status` exists; use `mix ./status` to run that file. An unknown
non-file token inside the REPL prints
`mix: unknown meta-command '<token>' (and no such file)` on stderr and then the
subcommand overview (the `mix meta-commands:` listing); from the OS shell an
unknown name is tried as a script filename.

```mix
mix status
```

```text
mix 0.21.2
  pid:       50960
  uptime:    ?
  memory:    5776 kB
  variables: 0
  aliases:   0
  functions: 5
  scope:     1 frame(s)
  trace:     off
```

## Introspection — what is in the session

These read the live [scope](variables.md): variables, [aliases](shell-mode.md),
user [functions](functions.md), and the runtime config.

```
mix status      version + pid, uptime, RSS, var/alias/func counts, scope depth, trace
mix vars        every variable as `$name = value` (values over 60 chars truncated)
mix aliases     every alias as `name = expansion`
mix functions   every user function as `name(param, param)`
mix all         vars + aliases + functions under --- section headers ---
mix type NAME   classify NAME: builtin / keyword / alias / function / variable / PATH command
mix config      version, $HOME, prelude path, ~/.mixrc path, OS id, arch, pid
```

`mix type` resolves in the same order the evaluator does — builtin → keyword →
alias → user function → variable → `$PATH` command → not found:

```mix
mix type round
mix type ls
mix type bogusname
```

```text
round is a builtin function
ls is a builtin function
bogusname: not found
```

`mix functions` and `mix all` from a cold shell show the [prelude](builtins.md)
shims that ship in every session:

```mix
mix functions
```

```text
avg(list)
chars(s)
lines(s)
read_lines(path)
sum(list)
```

`mix config` is the quickest way to confirm which interpreter and rc file are in
play:

```mix
mix config
```

```text
version:  mix 0.21.2
home:     /home/user
prelude:  /home/user/.cosmix/crates/cosmix-lib-mix/std/prelude.mix
rc file:  /home/user/.mixrc
os:       linux
arch:     x86_64
pid:      50955
```

> `mix status` shows `uptime: ?` when run from the OS shell — the process has no
> recorded start instant for a one-shot invocation; inside a long-lived REPL it
> reports real uptime.

## Reference — learn the language

The reference family is self-documenting and verified against the binary, so it
never drifts from training data. Start broad with `mix help`, narrow with
`mix builtins CATEGORY`, pinpoint with `mix what NAME`, then read the full
prose with `mix man TOPIC`.

```
mix help              full categorized builtin reference + keyword list + subcommand map
mix keywords          every reserved word, one per line
mix builtins [CAT]    list builtins (derived signature + description); with a category, only that one
mix builtins --json   machine-readable contract table (metadata_schema 1 — see builtins.md)
mix builtins --data   same table as strict-data Mix source
mix builtins --names  bare name-per-line list
mix what NAME         one-line description of a builtin OR a keyword
mix man [TOPIC]       read a markdown manual page (no arg = the index)
mix syntax            shortcut for `mix man variables`
mix operators         shortcut for `mix man operators`
mix diff bash         bash → Mix translation cheatsheet
mix tutorial          guided walkthrough of the basics
mix examples          copy-paste-runnable snippets by category
```

`mix builtins` takes one of ten categories — `string type math list map io
system format json hof` — and prints each derived signature with its registered
description (signatures are generated from the structured contracts, 0.29.0):

```mix
mix builtins math
```

```text
math builtins:
  round(x[, n]) -> number          Round to nearest integer, half away from zero; round(x, n) to n decimal places (n<0 rounds to tens/hundreds) (v0.19.0)
  floor(x[, n]) -> number          Round down toward -inf; floor(x, n) to n decimal places (v0.19.0)
  ceil(x[, n]) -> number           Round up toward +inf; ceil(x, n) to n decimal places (v0.19.0)
  ...
  pi() -> number                   The constant π (v0.19.0)
  e() -> number                    Euler's number e (v0.19.0)
```

A bad category lists the valid set instead of failing silently:

```text
mix builtins: unknown category 'maths'
Categories: string, type, math, list, map, io, system, format, json, hof
```

All CLI/meta output is BrokenPipe-tolerant. If a downstream reader closes early,
Mix treats writes to that closed stream as normal and exits normally; it never
prints a Rust `Broken pipe` panic or exits `101`. For example, this is safe even though
`head` consumes only the start of the JSON report:

```text
mix builtins --json | head
```

The pipeline status remains the last command's status under ordinary shell
rules, so a successful `head` makes the pipeline successful.

`mix what` answers "what does this single name do?" for both builtins and
keywords from the same lookup:

```mix
mix what round
mix what send
mix what map
```

```text
round: Round to nearest integer, half away from zero; round(x, n) to n decimal places (n<0 rounds to tens/hundreds) (v0.19.0)
send: Send message to Bus port: send PORT "msg"
map: Return new list of transform(item) results (v0.2.0)
```

`mix keywords` is the canonical reserved-word list — the names you cannot use
as function or variable *names* (`function step(...)` is an error). Since 0.21
a keyword IS accepted anywhere it is unambiguously a name — bare map keys
(`{label: 1, to: "x"}`), field access (`$m.to`), `send` kwargs — with the one
exception of `fn`, which lexes identically to `function` and still needs
quoting as a key. See [keywords](keywords.md).

```mix
mix keywords
```

```text
Mix reserved words:

  if
  else
  end
  for
  in
  while
  loop
  function
  fn
  return
  select
  ...
```

### `mix man` resolves online-first, with a local fallback

`mix man TOPIC` prints the markdown page `TOPIC.md`. The **canonical source is the
online documentation service** at [`cosmix.dev/mix`](https://cosmix.dev/mix) — the
same pages this site renders — because no tool can assume *where* (or whether) a
local checkout lives. Resolution order in the default `auto` mode:

1. **Fresh disk cache** — a copy fetched within the last 24 h short-circuits
   everything (no network, no latency on the common path).
2. **Online fetch** from `cosmix.dev/mix` (2 s wall-clock budget, HTTPS-only, no
   downgrade redirects); a good page is cached under your XDG cache dir.
3. **Local checkout**, tried in this order — an *explicit* signal beats the
   default: `$COSMIX_SRC/mix` (when `COSMIX_SRC` is set) → `$COSMIX/docs/mix`
   (editing the repo's own pages) → `$COSMIX/mix` (a default checkout).
4. **Stale cache**, if the network is down and no checkout is present.
5. Otherwise a *not-found* message naming the paths searched and how to force
   local mode.

No argument reads the index (`README.md`); an unknown topic lists the available
ones (the list excludes `README` itself):

```mix
mix man math        -- prints the math page (online, cached, or local)
mix man             -- prints the index
mix man nope        -- "no manual page for 'nope'" + available topics
```

**Environment controls:**

- `COSMIX_MAN_SOURCE=local` — skip the network entirely; resolve only from a
  local checkout (then stale cache). Use it when editing pages and wanting your
  edits to win immediately, or when deliberately offline. Default is `auto`.
- `COSMIX_MAN_URL=<base>` — override the online base URL (default
  `https://cosmix.dev/mix`). Points the fetch + cache at an alternate host.
- `COSMIX_SRC=<dir>` — adds `<dir>/mix` as the highest-priority local checkout.

`mix syntax` and `mix operators` are thin aliases — `mix man variables` and
`mix man operators` respectively. To add a topic, drop `TOPIC.md` into
`docs/_man/`; it publishes to `cosmix.dev/mix` on the next Pages build and is
reachable as `mix man TOPIC` with no code change.

### `mix diff bash`

A side-by-side translation table for anyone arriving from bash — it captures the
sharp edges directly (`$` sigils everywhere, `..` for concat, `end`/`done`/`next`
for block close, `$rc` instead of `$?`). `mix diff sh` is accepted as an alias;
any other language names the available set (`available: bash`):

```mix
mix diff bash
```

```text
Bash to Mix Translation Cheatsheet

Bash                               Mix
──────────────────────────────     ──────────────────────────────
VAR="value"                        $var = "value"
echo "$VAR"                        print $var
if [ -f file ]; then               if is_file("file") then
[[ -z $a ]]                        if $a == "" then
for i in 1 2 3; do                 for each $i in [1, 2, 3]
...
```

## Build — rebuild the interpreter from itself

The build family wraps `cargo` against the Mix source tree (`$COSMIX_SRC`,
default `$COSMIX`) so the REPL can rebuild and re-exec into a fresh binary —
the self-reconstruction surface. Point `$COSMIX_SRC` at the directory holding
the workspace `Cargo.toml` (in a clone of the public repo that is `mix/src`).

```
mix build       cargo build --release, then install target/release/mix → $COSMIX_BIN/mix
mix clean       cargo clean (removes the target/ tree)
mix update      git pull, then build + install
mix test        cargo test, streaming output
mix self check  syntax-check ~/.mixrc and std/prelude.mix without executing them
mix check FILE  syntax-check one .mix file (lex + parse, no execution)
```

`mix build` removes the destination first (a running ELF can't be written in
place — `ETXTBSY`), copies the new binary in, and — when run from the REPL —
saves history then `exec()`s into it, so the new interpreter replaces the old
process live. As a one-shot from the OS shell it installs and simply exits
(there is no REPL to restart). `mix update` is `git pull` then the same build.

`mix check` is a fast parse-only gate — perfect for a pre-commit hook or
validating a script before shipping it:

```mix
mix check /tmp/ok.mix      -- a well-formed file
mix check /tmp/bad.mix     -- an unterminated function header
```

```text
OK: /tmp/ok.mix
/tmp/bad.mix: Parse error at line 2:1: expected variable, got Eof
```

`mix self check` validates your two startup files. `~/.mixrc` is treated as a
hybrid Mix-plus-shell file (it tolerates bareword shell lines), while
`std/prelude.mix` must be strict, whole-file-valid Mix:

```mix
mix self check
```

```text
~/.mixrc: OK
std/prelude.mix: OK
```

(A `.mixrc` that mixes bareword shell lines with Mix reports
`OK (mixed shell+Mix)` — the per-line classifier validated every line.)

### `mix watch` — the edit-test loop

`mix watch PATTERN COMMAND` is a polling file-watcher: every 250 ms it scans
the current directory for files matching PATTERN and runs COMMAND (via
`sh -c`) when one changes, with a 500 ms debounce. Globs are simple — `*.ext`,
or `**/*.ext` to recurse (hidden directories and `target/` are always
skipped). Ctrl-C stops it.

```mix
mix watch '*.mix' 'cargo test'
```

## Diagnostics — measure and trace

These need live REPL state (readline history, the trace flag, the stats
handle), so they are handled in the REPL loop rather than `meta::dispatch`.
Apart from `mix stats` — which also runs one-shot from the OS shell against
the on-disk data — they are **not** available as `mix …` from the OS shell.

```
mix trace [on|off]    per-statement tracing to stderr (no arg = report state) — see below
mix history [PAT]     show readline history, optionally filtered to lines containing PAT
mix reload            re-execute ~/.mixrc in the current session
mix diagnose [on|off] auto-send REPL errors to Claude for diagnosis (needs the claude CLI)
mix stats [SUB]       usage tracking — see below
```

### `time` — the timing modifier

`time` is a shell-dispatch **modifier**, not a meta-command: it wraps whatever
follows — an external command, a pipeline, a bareword function, or Mix code —
and reports elapsed time on stderr. Unlike the rest of this section it is NOT
REPL-only; it works under `-c` and over ssh too. Full semantics:
[shell-mode](shell-mode.md).

```mix
time sum([1, 2, 3, 4, 5])
```

```text
15
Elapsed: 0.012ms
```

`mix time EXPR` is the same modifier under its older spelling. Before 0.32.0 it
was a REPL-only meta-command that could time Mix code but not a command, and a
bare `time cmd` died with `time: No such file or directory` — bash's `time` is a
keyword, so there is no `time` binary for the classifier to find.

### `mix trace` — the statement tracer

`mix trace on` prints one line **to stderr** for every statement executed, in
the form `trace <file>:<line> <kind>` — in the REPL the file shows as
`<repl>`:

```mix
mix trace on
$x = 41
print($x + 1)
```

```text
Trace: on
trace <repl>:1 Assignment
trace <repl>:1 Print
42
```

Shell-dispatch lines are traced too, as `trace <repl> shell: <cmd>` —
external binaries, pipelines, `cd`, and whole `&&`/`||`/`;` chains (a chain
traces as one line, after alias expansion). The `mix …` meta-commands
themselves are excluded as REPL machinery, as is the prompt's own render.

The tracer is self-contained: no `RUST_LOG` needed, and `RUST_LOG` does
**not** enable it (a `tracing` subscriber exists only under `--serve`) — the
stderr line is the real channel. It is REPL-only: it cannot be armed under
`--serve` or from a non-interactive `ssh host '<mix>'`. Current state shows in
`mix status` (`trace: on|off`) and `mix context` (`"trace": true|false`);
`mix trace` with no argument reports it.

### `mix stats` — usage tracking

`mix stats` aggregates which builtins, functions, aliases, commands, keywords,
meta-commands, and error kinds you actually use — including a `never` report
of builtins/keywords you have never invoked. Data lives under
`$XDG_STATE_HOME/mix/` (default `~/.local/state/mix/`) as weekly JSON files,
mirrored into a `mix.db` SQLite database when the `sqlite3` CLI is on `$PATH`
(that database backs `trend`/`since`/`query`). `MIX_STATS=0` disables the
subsystem. Run `mix stats` for the top-20 overview, or a subcommand:

```
mix stats              top 20 most-used across all categories
mix stats builtins     per-builtin usage counts          mix stats never      never-used builtins/keywords
mix stats functions    user-function counts              mix stats sessions   session history
mix stats aliases      alias expansion counts            mix stats raw        current stats as JSON
mix stats commands     external-command counts           mix stats reset      reset current counters
mix stats keywords     keyword counts                    mix stats clear NAME remove NAME from every category
mix stats meta         meta-command counts               mix stats all        aggregate every weekly file
mix stats errors       error-kind counts                 mix stats week W     one week (e.g. 2026-W27)
mix stats trend NAME / since DATE / query SQL            SQLite history
```

## Ecosystem — probe the Bus mesh

These query the local [noded](bus.md) broker for its registered Bus service
roster (the mesh is TCP noded — `ws://<wg-ip>:4200/ws` — resolved the same way
the [`send`](bus.md) keyword resolves a target). All three fail fast with a 5 s
timeout so a stale broker address can't hang a diagnostic.

```
mix mesh          mesh status + the full list of services the local noded knows
mix ports         the registered Bus ports/services with a count
mix ping SERVICE  is SERVICE reachable? (`noded` itself is always the local broker)
```

```mix
mix mesh
```

```text
Bus mesh active via local noded (ws://192.0.2.10:4200/ws)
  dnsd
  indexd
  log
  webd
4 service(s) registered
```

```mix
mix ping webd
mix ping noded
mix ping nope
```

```text
webd: reachable (registered with noded)
noded: reachable (local broker)
nope: not found
```

When no broker is running the same commands degrade to a clean one-line message
(`No mesh — local noded not reachable (...)`) rather than an error.

There is also a small **orchestration** group: `mix deploy SVC`
(`systemctl --user restart cosmix-SVC`), `mix health [SVC]` (probe Bus unix
sockets in `/run/bus`, falling back to `/tmp/bus`), and `mix logs SVC [-f]`
(`journalctl --user -u cosmix-SVC`, last 50 lines; `-f`/`--follow` to stream).
These assume a systemd-user-managed Cosmix install.

## AI-powered — agent-driven workflows

The AI family shells out to the Claude Code CLI (`claude -p "<prompt>"`); each
constructs a task prompt pointed at the Mix source tree. If `claude` is not on
`$PATH` they print an install hint and stop — they never alter anything on their
own.

```
mix fix DESC      read source, write a fix + test, run cargo test
mix extend DESC   implement a feature (tests first), run cargo test
mix review        git log / git diff HEAD~1, then a code review
mix explain NAME  explain how a builtin works from builtins.rs + evaluator.rs
mix evolve        pick the highest-value item from MIX_TODO.md and implement it
mix dogfood       write practical scripts, report awkward syntax / gaps
mix fuzz          generate random Mix to fuzz the parser/evaluator, report crashes
mix teach         build a tutorial .mix from the newest features
```

A related triad — `mix ask QUESTION` (one-shot Claude query with session context
injected), `mix chat` (interactive Claude with a Mix-aware system prompt), and
`mix context` / `mix snapshot` (export vars + functions + aliases + runtime info
as JSON) — feeds the live session into an agent. `mix context` is the
machine-readable companion to `mix all`:

```mix
mix context
```

```text
{
  "aliases": { ... },
  "current_line": 42,
  "cwd": "/home/user",
  "extensions": [ ... ],
  "functions": [ ... ],
  "pid": 50955,
  "scope_depth": 1,
  "trace": false,
  "variables": { ... }
}
```

Finally, `mix claude-start` / `mix claude-stop` / `mix claude-status` manage
the `cosmix-claude` Bus port daemon (socket
`/run/user/<uid>/cosmix/ports/claude.sock`): `start` spawns the binary
(searched in `$COSMIX_SRC/target/{release,debug}/`, then `$PATH`), `stop`
removes the socket, `status` probes it and flags a stale socket.

## Notes

- `mix` is intercepted by the shell, so it never sees `$`-sigil arguments — write `mix what round`, not `mix what $name`.
- The introspection family (`vars`/`aliases`/`functions`/`all`/`context`) is most useful **inside a REPL**, where the session has accumulated state; from a one-shot OS-shell invocation it reports only the freshly-loaded prelude.
- The diagnostics family (`time`/`trace`/`history`/`reload`/`diagnose`) is REPL-only — those names are not on the OS-shell allowlist, so `mix trace on` there is read as a script filename. The one exception is `mix stats`, which has a dedicated one-shot OS-shell path against the same on-disk data.
- Paths follow the Cosmix layout: source at `$COSMIX_SRC` (default `$COSMIX`), installed binaries at `$COSMIX_BIN` (default `~/.local/bin`, or `/usr/local/bin` when running as root).

## See also

- [the manual index](README.md) — every page `mix man TOPIC` can read
- [variables](variables.md) — the `$`-sigil scope `mix vars` reports on
- [functions](functions.md) — what `mix functions` lists
- [shell-mode](shell-mode.md) — the alias/classifier layer behind `mix aliases` and `mix reload`
- [invocation](invocation.md) — the `mix -c` / `mix -` / `--serve` entry points around this command
- [keywords](keywords.md) · [operators](operators.md) · [math](math.md) — topics readable via `mix man TOPIC`
- [Bus messaging](bus.md) — the `send`/`emit`/`address` mesh `mix mesh`/`ports`/`ping` probe
- [builtins index](builtins.md) — the full builtin catalogue behind `mix builtins`
- The public repo: [github.com/markc/mix](https://github.com/markc/mix)

```
mix help          full categorized reference + the complete subcommand map
mix what NAME     one-line description of any builtin or keyword
mix man TOPIC     read a full manual page (mix man with no arg = the index)
```
