# system — processes, environment & system primitives

The `system` builtin category — run external commands, inspect and mutate the
process/OS environment, and produce the small security/identity primitives
(quoting, hashing, UUIDs, passwords) that glue scripting to the outside world.
List them live with `mix builtins system`; one-line help for any single name
with `mix what NAME`.

Mix is an [Bus-native shell](bus.md): for **mesh** work prefer `send` / `emit`
over shelling out, and for **remote** work prefer [`ssh_run` / `ssh_must` /
`ssh_mix`](remote.md) over hand-built `ssh` command strings. In particular,
use the [`ssh_mix` + heredoc headline idiom](remote.md#headline-idiom-ssh_mix--heredoc)
for multi-line remote Mix or anything with nested quotes. This page covers
the *local* surface — launching processes and reading the machine. The category
listing also shows families documented on their own pages: the `ssh_*` builtins
([remote](remote.md)), `http_*` ([http](http.md)), the date/time helpers
([datetime](datetime.md)), `dns_lookup` ([builtins index](builtins.md)), and
`panic` ([errors](errors.md)).

## The structured-return win

The headline difference from bash: a command's result is **structured data**, not
`$?`-soup you re-parse. The local runners have explicit contracts:

```
run("cmd" [, {timeout: s}])        -> stdout STRING (trailing whitespace stripped); RAISES a catchable die on non-zero exit; /bin/sh
run_rc("cmd" [, {timeout: s}])     -> map {rc, stdout, stderr, timed_out, interrupted}; NEVER raises — branch on .rc; /bin/sh
run_argv(argv [, opts])            -> full process_result map; argv direct (NO shell), structured stdio routing, captured streams bounded, optional live tee; NEVER raises on command failure (v0.29.0; stream v0.47.0)
run_argv_must(argv [, opts])       -> stdout STRING (untrimmed); RAISES PROCESS_* structured errors with the result map in $err.details.result (v0.29.0)
run_pipeline(stages [, opts])      -> distinct pipeline_result map with per-stage outcomes; argv direct (NO shell); NEVER raises on ordinary pipeline failure
run_pipeline_must(stages [, opts]) -> final stdout STRING (untrimmed); RAISES PIPELINE_* with the pipeline_result in $err.details.result
run_stream(argv [, {env, clear_env, cwd}]) -> exit-code NUMBER; inherited stdio (live stream, no /bin/sh); opts v0.51.0
```

Pick by intent: **`run_argv` is the default for operational code** — injection-inert
argv, captured output, a real deadline, and one consistent result shape;
`run_argv_must` for a must-succeed step in argv form; `run`/`run_rc` when you
genuinely want `/bin/sh` semantics (pipes, globs) in the command string;
`run_stream` when the child must own the terminal (live progress, an interactive
prompt); `run_argv(..., {stream: true})` when output must be both visible live and
captured for the result. The capturing runners give the child `/dev/null`-style
stdin by default — a child that prompts gets instant EOF, so an interactive
command needs `run_stream` (or `run_argv`'s `stdin:` option for pre-supplied
input). Tee mode does not give the child a terminal or inherited stdin.

### run_argv — structured argv execution (v0.29.0)

`run_argv(argv, [opts])` runs an argv **list** directly (no shell anywhere),
routes stdin/stdout/stderr explicitly, enforces a deadline, and returns one
consistent map:

```mix
$r = run_argv(["pct", "start", "" .. $vmid], {timeout: 60})
if not $r.ok then
  eprint("start failed (exit " .. $r.exit_code .. "): " .. $r.stderr)
end
```

The result map always contains, in order: `ok` (true only for exit 0 with no
timeout/interrupt/signal/spawn error), `exit_code` (number, or `nil` when the
child didn't exit normally), `stdout`, `stderr` (lossy UTF-8, **untrimmed**),
`timed_out`, `interrupted`, `signal` (number or `nil`), `duration_ms`,
`stdout_truncated`, `stderr_truncated`, `utf8_lossy`, `error_code`, `error`
(both `nil` unless process setup/lifecycle itself failed — `PROCESS_STDIO` /
`PROCESS_SPAWN` / `PROCESS_IO` / `PROCESS_INTERNAL`; an ordinary non-zero exit is NOT an error
here and never raises).

Options (unknown keys are a hard `OPTION_INVALID` error):

- `timeout`: seconds, default **30**, `0` disables, fractional ok. The clock
  starts before file routes are opened, so it bounds setup as well as the child
  and capture drains. With `timeout: 0`, Mix waits for every captured stream to
  reach EOF; it does not abandon a reader merely because the direct child has
  exited. Note `run`/`run_rc` default to no deadline.
- `stdin`: `nil` or `{null: true}` closes stdin; string/bytes/buffer supplies
  those bytes; `{file: path}` opens a local file for the child to read. There is
  deliberately no `stdin: "inherit"` route: run_argv puts its child in a new
  process group, so it is not the terminal's foreground group and a terminal
  read can receive `SIGTTIN`. The string `"inherit"` is ordinary stdin data.
  Use `run_stream` when a child must own the terminal and inherited stdin. With
  `timeout: 0`, Mix also waits for a stdin-data writer to finish after the
  direct child exits. A descendant which retains the read end without consuming
  the data can therefore make the call wait indefinitely; that is the explicit
  no-deadline contract, and the writer is not detached as a leaked thread.
- `stdout`: `"capture"` (default), `"inherit"`, `"null"`, or
  `{file: path, append?: bool, mode?: number}`.
- `stderr`: `"capture"` (default), `"inherit"`, `"null"`, `"stdout"`
  (the `2>&1` merge), or the same file map.
- `cwd`, `env` (map overlaid on the inherited environment; keys
  `[A-Za-z_][A-Za-z0-9_]*`, values string/number/bool), and `clear_env` (bool —
  start from an empty environment) retain their existing meanings.
- `max_output`: bytes **per captured stream**, default 8 MiB, `0` disables.
  Excess is drained and discarded — the child is never blocked or killed by the
  cap, and the captured stream's truncation flag is set. The same flag is true
  when a deadline forces Mix to abandon a capture before EOF; in that case the
  returned bytes are the partial prefix received before abandonment.
- `stream`: bool, default `false`; when true, captured stdout chunks are also
  written to the parent's stdout and captured stderr chunks to the parent's
  stderr as they arrive, with a flush after each chunk. Streaming continues
  after `max_output` truncates capture. `stream: true` with
  `stdout: "inherit"` is rejected as `OPTION_INVALID`: the child already owns
  that stream, so teeing it is meaningless.

File output defaults to mode `0o600`, `append: false`, and therefore truncates an
existing file. `mode` is the creation mode (subject to the process umask). All
local file routes are opened **before** the child is spawned. An open/create
failure returns the normal failure-shaped process_result with `ok: false`,
`exit_code: nil`, and `error_code: "PROCESS_STDIO"`; it never raises from
`run_argv`, and the child is not run.

On Unix, an output FIFO is opened non-blocking during setup and restored to
blocking mode for the child. With no reader its open fails immediately with
`ENXIO`, reported as `PROCESS_STDIO` (not `timed_out`). An input FIFO retains
normal wait-for-writer open semantics, but that wait is performed by a bounded
worker. Mix anchors the FIFO inode with a descriptor before starting the worker;
reaching the call deadline wakes that same inode and reaps the worker even if
the pathname was concurrently renamed, unlinked, or replaced, then returns
`PROCESS_STDIO` with no child spawned. The wake open is non-blocking. Repeated
no-writer deadlines therefore do not accumulate blocked FIFO-open threads.
Pipeline routes use the same rules and return `PIPELINE_STDIO`.

Every route is opened first and a non-append route is truncated only once the
whole set has opened, so a bad `stderr` path cannot leave the `stdout` path's
existing file emptied on the way to reporting `PROCESS_STDIO`. (The guarantee
is "nothing is truncated until everything has opened", not a transaction: if the
truncation of one route then fails — an immutable file opens but will not
shorten — an already-truncated earlier route stays truncated.) A route pointing
at a symlink is followed, as `>` would; a route pointing at a non-regular target
(`/dev/null`, a character device) is written to, never truncated.

A non-captured stdout/stderr route always leaves that result field as `""` and
its `*_truncated` flag false. `max_output` does not apply to inherited, null, or
file-routed streams; Mix never silently caps them. With `stderr: "stdout"`, both
child streams go to the selected stdout destination, the combined bytes count
against stdout's cap when stdout is captured, and result `.stderr` stays `""`
with `stderr_truncated: false`.

With `stream: true`, a non-consuming parent stdout or stderr can block its drain
worker and delay return past `timeout`; the child is still killed at the
deadline. The tee-enabled check and parent-stream write are one critical
section. Before abandoning a reader Mix disables teeing under that same lock,
so an in-flight write finishes before return and later chunks from an escaped
descendant are suppressed. The captured prefix is returned with its truncation
flag set.

Kill discipline matches the whole family: the child runs in its own process
group; a timeout SIGKILLs the group immediately; Ctrl-C sends SIGTERM, waits up
to 2s, then SIGKILLs.

`run_argv_must(argv, [opts])` accepts the same options and routing rules, and
returns `$r.stdout` unchanged when `ok` and
neither stream was truncated; otherwise it raises a structured error
(`PROCESS_EXIT_NONZERO`, `PROCESS_TIMEOUT`, `PROCESS_SIGNAL`,
`PROCESS_INTERRUPTED`, `PROCESS_OUTPUT_LIMIT`, or the setup/lifecycle code,
including `PROCESS_STDIO`)
carrying the complete result map in `$err.details.result` — catch with
`catch $msg, $err` (see [errors](errors.md)).

### run_pipeline — structured shell-free pipelines

`run_pipeline(stages, [opts])` runs one or more argv stages directly and connects
stage `i` stdout to stage `i + 1` stdin. It does not invoke `/bin/sh`, parse a
command string, or reinterpret argv characters:

```mix
$r = run_pipeline([
  ["sha256sum", "/srv/image.raw"],
  ["cut", "-d ", "-f1"]
], {timeout: 60})
if $r.ok then
  print(trim($r.stdout))
else
  eprint("pipeline failed: " .. $r.stderr)
end
```

A stage is either an argv list or a map:

```mix
[
  {argv: ["producer"], cwd: "/srv/input", env: {MODE: "raw"}, stdin: {file: "/srv/request"}},
  {argv: ["filter"], clear_env: true, env: {LANG: "C"}, stderr: "inherit"},
  {argv: ["consumer"], stdout: {file: "/srv/result", mode: 0o600}}
]
```

Every stage map requires `argv`. All stages accept `cwd`, `env`, `clear_env`, and
`stderr`. Only the first accepts `stdin`; only the last accepts `stdout` (a
one-stage pipeline is both first and last). These values use the exact
[`run_argv`](#run_argv--structured-argv-execution-v0290) grammar and validation:
stdin data/null/file, stdout capture/inherit/null/file, and stderr
capture/inherit/null/stdout/file. A misplaced route, unknown key, or invalid
option raises `OPTION_INVALID` before spawning; malformed stages/argv raise
`TYPE_MISMATCH`. An empty pipeline is invalid. A one-stage pipeline has the same
execution and familiar result fields as `run_argv`, plus its one-element
`.stages` list.

The distinct `pipeline_result` map always contains, in order:
`ok`, `exit_code`, `stdout`, `stderr`, `timed_out`, `interrupted`, `signal`,
`duration_ms`, `stdout_truncated`, `stderr_truncated`, `utf8_lossy`,
`error_code`, `error`, `stages`.

- `exit_code` and `signal` describe the **last** stage. They do not alone decide
  overall success: a middle-stage failure makes `.ok` false even when the last
  stage exits 0.
- `stdout` is the last stage's captured stdout, untrimmed. It is `""` when the
  last stage routes stdout elsewhere.
- `stderr` concatenates captured stage stderr in stage order. Routed/merged
  stderr is absent from this aggregate.
- `stderr_truncated` and `utf8_lossy` are aggregate flags. `stdout_truncated`
  applies to final stdout. A truncation flag also records a capture abandoned
  at the deadline before EOF, not only bytes discarded by `max_output`.
- `error_code` / `error` are normally `nil`. Setup/lifecycle failures use
  `PIPELINE_STDIO`, `PIPELINE_SPAWN`, `PIPELINE_IO`, or `PIPELINE_INTERNAL` and
  are returned as `ok: false`; ordinary non-zero exits and signals remain data.
  A later-stage `PIPELINE_SPAWN` is the unavoidable partial-run case:
  `.stages` contains only stages already started and reaped. Their captured
  stderr is abandoned during emergency cleanup and marked truncated.

Each `.stages[i]` map contains, in order: `index`, `argv`, `ok`, `exit_code`,
`signal`, `duration_ms`, `stderr`, `stderr_truncated`, `utf8_lossy`,
`accepted_signal`. Stage stderr is untrimmed. `accepted_signal` records the
SIGPIPE policy below; it is false for ordinary exits and rejected signals.

Pipeline options (unknown keys raise `OPTION_INVALID`):

- `timeout`: one wall-clock deadline in seconds for the **whole pipeline**,
  starting before route and pipe setup; default 30, `0` disables, fractional
  values are accepted. A route open that reaches the deadline returns
  `PIPELINE_STDIO` before any stage runs. Process enforcement polls
  at 50 ms, exactly as `run_argv` does, so a pipeline that finishes inside the
  first poll of an unreachably short deadline is reported as a normal
  completion, not a timeout. A deadline bounds a wedge; it is not a stopwatch.
- `max_output`: bytes per captured stream, default 8 MiB, `0` disables. It caps
  final stdout and each captured stage stderr independently; excess is drained
  and flagged, not allowed to block a child.
- `allow_signal`: bool, **default `false`**. By default every stage must exit
  normally with code 0, so any signal death — SIGPIPE included — makes `.ok`
  false. Set `allow_signal: true` to accept a **non-final** stage killed by
  SIGPIPE when every downstream stage succeeded, which is what the ordinary
  early-reader idiom needs:

  ```mix
  $r = run_pipeline([["yes"], ["head", "-1"]], {allow_signal: true})
  ```

  The default is `false` because **the signal is all Mix can see**. A stage that
  kills itself with SIGPIPE for a fatal reason of its own is indistinguishable
  from one whose reader simply closed early, so accepting it by default reports
  this as success:

  ```mix
  run_pipeline([["yes"], ["sh", "-c", "printf fatal >&2; kill -PIPE $$"], ["true"]])
  ```

  The middle stage announced a fatal condition and killed itself, every
  downstream stage exited 0, and `.ok` would have been `true`. Reporting that as
  success is the silent-wrong-answer class this surface exists to remove, so the
  honest answer is the default and the ergonomic one is opt-in. This matches
  `set -o pipefail`, which likewise reports 141 for `yes | head -1`.

  Per-stage `.accepted_signal` records where an acceptance was applied.

Every stage's file routes and every inter-stage/data/capture pipe are prepared
before **any** stage is spawned. Non-append routes are truncated only after the
full set has opened. A later open or pipe-creation failure therefore returns
`PIPELINE_STDIO` with no stage run; the truncation pass itself is not a
transaction if one truncation succeeds and a later one fails.

Every stage leads its own process group. The pipeline owns one deadline: expiry
SIGKILLs every stage group, including descendants, and sets `timed_out: true`.
Ctrl-C sends SIGTERM to every group, allows the same two-second grace as
`run_argv`, then SIGKILLs survivors. A descendant that deliberately escapes its
stage group can retain a captured descriptor; Mix waits only a short bounded
drain window, then returns the captured prefix with the relevant truncation
flag true rather than waiting indefinitely for EOF. When `timeout: 0` disables
the deadline, capture instead waits for EOF and is never abandoned merely
because every direct stage has exited. A supplied stdin-data writer is likewise
joined: a descendant which holds its read end without consuming can make an
explicit no-deadline pipeline wait indefinitely, but cannot leave a detached
native writer thread behind.

`run_pipeline_must(stages, [opts])` accepts the same forms. It returns final
stdout unchanged only when the aggregate `.ok` is true and no captured stream
was truncated. Otherwise it raises `PIPELINE_EXIT_NONZERO`, `PIPELINE_TIMEOUT`,
`PIPELINE_SIGNAL`, `PIPELINE_INTERRUPTED`, `PIPELINE_OUTPUT_LIMIT`, or the
setup/lifecycle code above. The complete pipeline_result is always available as
`$err.details.result`.

### run — stdout string, fail-fast

`run(cmd)` runs `cmd` through `/bin/sh -c`, returns its stdout as a string with
**trailing** whitespace stripped (leading whitespace is preserved), and on a
non-zero exit (or signal kill) **raises a catchable `die`** carrying the
command excerpt, the exit status, and a tail of stderr.

```mix
$out = run("echo hello world")
print($out)
print("len: " .. length($out))
```
```text
hello world
len: 11
```

The die on failure — wrap a must-succeed step in [`try`/`catch`](errors.md):

```mix
try
  run("false")
catch $e
  print("caught: " .. ("" .. $e))
end
```
```text
caught: run: 'false' failed (rc=1)
```

The die message includes a stderr tail when there is one (`run: 'cmd' failed
(rc=2): <stderr tail>`, last 200 characters). A signal-killed child reports the
shell convention `rc = 128 + signo` — SIGTERM shows as `failed (rc=143)`, not a
separate `signal=N` form. The excerpt and tail are run through
[`sanitize`](#sanitize) so a hostile command/stderr can't smuggle control
characters into your logs.

### run_rc — the {rc, stdout, stderr, timed_out, interrupted} map

`run_rc(cmd)` runs the same way but **never raises** — it returns a map so you
branch on the exit code yourself. `stdout` and `stderr` are both stripped of
trailing whitespace. The `timed_out` / `interrupted` bools are always present
(`false` on a normal exit) — see [Timeouts](#timeouts) below.

```mix
$r = run_rc("ls /nonexistent-xyz")
print("rc=" .. $r.rc)
print("stderr=" .. $r.stderr)
```
```text
rc=2
stderr=ls: cannot access '/nonexistent-xyz': No such file or directory
```

The idiomatic exit-code branch — the structured-return payoff:

```mix
$r = run_rc("systemctl is-active some-service")
if $r.rc != 0 then
  print("not running")
else
  print("active: " .. $r.stdout)
end
```

`rc` is the real exit code as a number. A signal-killed child reports
`128 + signo` (SIGTERM → `143`); `-1` means the `timeout` deadline fired and
`-2` means Ctrl-C interrupted the wait (see below) — never confuse those with a
child's own exit code. Trailing newlines are stripped, so a one-line tool's
`stdout` is clean to compare or concatenate:

```mix
$r = run_rc("printf 'a\nb\n\n'")
print("[" .. $r.stdout .. "]")
```
```text
[a
b]
```

### Timeouts

`run` and `run_rc` take an optional second argument — an opts map with a single
`timeout` key, in **whole seconds** (every ssh/run timeout in Mix is seconds; the
Bus `send … timeout=2000` form is the only millisecond surface). The default is
`0` = **no deadline** — the historic contract, so a long build never gets cut
off by surprise. With a deadline set, a hung child can never wedge the shell:

```mix
$r = run_rc("sleep 5", {timeout: 1})
print("rc=" .. $r.rc .. " timed_out=" .. ("" .. $r.timed_out))
```
```text
rc=-1 timed_out=true
```

```mix
try
  run("sleep 5", {timeout: 1})
catch $e
  print("caught: " .. ("" .. $e))
end
```
```text
caught: run: 'sleep 5' timed out after 1s
```

The full status contract:

| Outcome | `run` | `run_rc` |
|---|---|---|
| normal exit | stdout string / die on non-zero | `rc` = exit code |
| deadline fired | dies (catchable): `timed out after Ns` | `rc: -1, timed_out: true` |
| Ctrl-C during the wait | raises `run: interrupted` | `rc: -2, interrupted: true` |
| child signal-killed | dies: `failed (rc=128+sig)` | `rc` = `128 + signo` (SIGTERM → 143) |

Kill mechanics (the same machinery as [`ssh_run`](remote.md)): the child is
spawned in its **own process group**, so the kill reaches every descendant — an
`ssh` helper or forked worker can't keep the pipes open past the deadline. A
timeout SIGKILLs the group immediately; a Ctrl-C sends SIGTERM, waits a 2-second
grace, then SIGKILLs. An interrupt that lands on the same poll as the deadline
wins the tie — it's reported as the cause.

The opts map is validated **loudly** — a mistake can't silently leave a call
unbounded:

```text
run_rc("echo hi", {bogus: 1})       -> Runtime error: run_rc: unknown opt "bogus" (supported: timeout)
run_rc("echo", {timeout: 1.5})      -> Runtime error: run_rc: timeout must be a non-negative integer, got 1.5
run_rc("echo", "notamap")           -> Runtime error: run_rc: opts must be a map like {timeout: 30}, got string
run_rc("echo", {timeout: 1}, "x")   -> Runtime error: run_rc() expects at most 2 argument(s), got 3
```

`run_stream` takes **no** timeout opt; it blocks until the child exits, which is
the point of handing it the terminal. Since v0.51.0 it *has* an options map, so
`{timeout: 5}` is no longer a silently-ignored surplus argument — it raises
`OPTION_INVALID` naming `run_argv` as the runner that honours a deadline (as do
`stdin`, `stdout`, `stderr`, `max_output` and `stream`). `spawn` doesn't need one — it returns
immediately and you `kill()` the PID yourself. The `http_*` builtins have their
own deadline (default 30 s) — see [http](http.md).

### run_stream — live stdio, argv list, no shell

`run_stream(argv_list)` runs an **argv list directly** — no `/bin/sh`, so no
word-splitting, globbing, quoting, or operator interpretation. Each list element
is one argument verbatim, which makes user values **injection-inert by
construction**. It inherits the parent's stdin/stdout/stderr (output streams live
as it happens) and returns the **exit code** as a number.

```mix
$code = run_stream(["echo", "streamed", "live"])
print("exit code: " .. $code)
```
```text
streamed live
exit code: 0
```

Use it for a foreground one-shot where the child should own the terminal — live
progress (a build, a long copy) or an interactive prompt (a password, an `apt`
confirmation) — *provided the command allocates a pty itself*, e.g.
`run_stream(["ssh", "-t", $host, $cmd])`. A signal-killed child reports
`128 + signo` (the shell convention).

It is strict about its argument: a non-list, an empty list, or a non-string
element all raise rather than silently stringifying:

```mix
try
  run_stream("ls")
catch $e
  print("caught: " .. ("" .. $e))
end
```
```text
caught: run_stream: argument must be a list of strings, got string
```

Because it blocks the evaluator until the child exits, keep it out of a hot
`on … async` [event handler](bus.md) — there it stalls dispatch like `run`/`run_rc`
would, and there's no terminal for interactivity anyway.

#### run_stream options — env, clear_env, cwd (v0.51.0)

The optional second argument is `{env, clear_env, cwd}`, with **exactly** the
semantics [`run_argv`](#run_argv--structured-argv-execution-v0290) gives those
three keys — same name validation (`[A-Za-z_][A-Za-z0-9_]*`), same NUL
rejection, same `OPTION_INVALID` code, all checked *before* the child is
spawned. The order is clear-then-layer, so `{clear_env: true, env: {…}}` means
"exactly these":

```mix
$code = run_stream(["npm", "install"], {
  cwd: "/srv/app",
  env: {NODE_ENV: "production", CI: "1"}
})
```

Values reach the child through `execve`, not a shell — spaces, globs and a
literal `$HOME` in a value survive verbatim.

Use it instead of prefixing the argv with coreutils `env`. That old workaround
still runs, but it puts every value in the child's `ps` argv, where any user on
the box can read it — the option does not, which makes it the route for a token
or password handed to an interactive child. The `export` statement keeps a
value out of argv as well — it is the mechanism behind [`ssh_run`](remote.md)'s
default `mix` env transport and `ssh_mix` — but it mutates this process's
environment: the variable stays set for the rest of the run, and every child
spawned after it inherits the value unless that child clears or overrides it.
The option is scoped to the single call.

⚠️ On a mix older than 0.51.0 the map is **not** an error — default compatible
arity silently ignores a surplus argument, so the child runs with the inherited
environment, the inherited cwd, and no sign that anything was dropped. That is
a fail-*open* version boundary. A caller that may meet a mixed-version fleet
should install the new binary everywhere before the caller, run under
`--strict-arity` (where the old contract's arity raises `ARITY_MISMATCH`), or
probe the option behaviourally: spawn `["/bin/sh", "-c", "test x$SENTINEL =
xVALUE"]` with the sentinel in `env` and check the exit code before trusting
the real run. Prove the sentinel is *absent* from the parent first — an
inherited value passes the probe on the very binary the probe exists to catch.

The four `run_argv`-only keys are refused **by name** rather than ignored:
`timeout` (this runner blocks until the child exits), `stdin` (it inherits the
parent's), and `max_output` / `stream` (it captures nothing). Each error names
`run_argv` as the runner that does honour the key.

⚠️ A bare `argv[0]` is resolved against the **child's** `PATH`, not the
parent's — so `env: {PATH: "/nonexistent"}` makes `run_stream(["sh", …])` fail
to spawn, even though `sh` is on yours. (`clear_env: true` alone leaves no
`PATH` at all, and the C library's default path still finds `/bin/sh` on Linux —
but that is the platform's fallback, not a guarantee.) Pass an absolute
`argv[0]` whenever you touch `PATH` or clear the environment; see
[the minimal-PATH rule](#-the-minimal-path-rule). Pass one under `cwd` too — a
*relative* `argv[0]` is resolved in a directory the OS deliberately leaves
platform-specific, so `run_stream(["./build.sh"], {cwd: "/srv/app"})` is not
portable; spell it `/srv/app/build.sh`.

### Which runner?

| Need | Use |
|---|---|
| stdout string, abort on failure | `run` |
| inspect a non-zero exit code as data | `run_rc` |
| live output / interactive child / inject-safe argv | `run_stream` |
| run Mix source on another node | [`ssh_mix` + heredoc](remote.md#headline-idiom-ssh_mix--heredoc) |
| run a shell snippet on another node | [`ssh_run` / `ssh_must`](remote.md) |
| talk to a local/mesh Bus broker | [`send` / `emit`](bus.md) |

## What the command string sees

`run`/`run_rc` hand the whole command string to `/bin/sh -c`, so POSIX-sh
syntax inside it just works — and three constructs behave differently there
than on a [shell-dispatch line](shell-mode.md):

- **Subshell `( … )` / brace-group `{ …; }` grouping**: shell dispatch (the REPL / `mix -c` shell branch) does **not** support POSIX command grouping — a bare `(echo a; echo b) | sort` at the prompt fails because a *leading* `(` classifies the line as Mix, where `echo a` is not a valid parenthesized expression (and ordinary expression parentheses do not permit `;` either); a mid-line group gets word-split. The fix is not a `.sh` file — hand the whole pipeline to `/bin/sh` as one Mix line:

  ```mix
  $r = run_rc("(echo b; echo a) | sort")   -- {rc, stdout, stderr}; group runs in /bin/sh
  print($r.stdout)
  print(run("(cd /tmp; pwd) | tr a-z A-Z"))
  ```
  ```text
  a
  b
  /TMP
  ```

  On a mix-login-shell node a remote `(…; …)` hits the *remote* classifier the
  same way. Ship multi-line Mix without nested escaping via
  [`ssh_mix` + heredoc](remote.md#headline-idiom-ssh_mix--heredoc).

- **`$(...)` command substitution**: literal in a double-quoted Mix [string](strings.md), but it **passes through** in a `run`/`run_rc` command string — it substitutes in the `/bin/sh` that actually runs the command (local here; the *remote* shell for `ssh_run`).

- **Brace expansion `{a,b}` / `{1..5}`**: a shell-dispatch feature, **not** a `/bin/sh` one — braces in a `run`/`run_rc` command string pass through to POSIX sh, which does no brace expansion. Use a dispatch line or a Mix loop when you need it.

## ⚠️ The minimal-PATH rule

`run`, `run_rc`, and `spawn` go through `/bin/sh` with whatever PATH the parent
process had — non-interactively that is **minimal**, and `~/.mixrc` aliases are
*not* loaded (those only apply to interactive `mix -i`). The safe habit:
**call binaries by full path** inside scripts.

```mix
-- fragile: bare name may not be on /bin/sh's PATH non-interactively
run("mix --version")

-- robust: absolute path always resolves
print(run("/opt/cosmix/bin/mix --version"))
```
```text
mix 0.21.2
```

`run_stream` takes the program name as `argv[0]` and resolves it against PATH the
same way — full-path that too if you can't guarantee the environment. The rule
of thumb: **inside scripts, always full-path** (`/opt/cosmix/bin/...`). The one
context where a bare `mix` *does* resolve is as the ssh command itself
(`ssh host 'mix status'`) — the login-shell mix self-resolves via its own
executable path, no PATH needed ([remote](remote.md)).

## Background processes — spawn, kill, process_alive

`spawn(cmd[, stdout_path[, stderr_path]])` starts a background process via
`/bin/sh -c` with stdin from `/dev/null` and returns its **PID** as a number. It
does not wait. Stdio routing by arity:

- 1 arg — both stdout and stderr → `/dev/null`
- 2 args — stdout → file (truncated), stderr → `/dev/null`
- 3 args — stdout → file1, stderr → file2; pass the **same path** for both to merge them into one combined log (like bash `&>file`)

```mix
$p = spawn("echo logged-line", "/tmp/spawn.log")
sleep(0.2)
print(run("cat /tmp/spawn.log"))
```
```text
logged-line
```

**All three arguments are strings, and none of them is coerced** (strict since
v0.52.0). This is the loud-validation rule the opts maps have always had,
reaching the one runner that had missed it — and `spawn` needs it most, because
it is the only runner with nowhere to put a failure. It returns a **PID, not a
result map**, and it does not wait, so a child that dies on the very first line
is indistinguishable from one that worked. Until 0.52.0 a non-string was
stringified into its display form and handed to `sh` anyway:

```text
spawn(["touch", $p])   -- 0.51.0: returns a healthy PID; sh dies with
                       --   `sh: line 1: [touch,: command not found`
                       --   and nothing anywhere reports it
                       -- 0.52.0: raises TYPE_MISMATCH
```

There is **no argv form of `spawn`** — it is `sh -c` by definition. Build the
command string with [`shell_quote`](#shell_quote), or use `run_argv` /
`run_argv_must` when you want an argv list and can accept a foreground child:

```text
spawn(["touch", "/tmp/x"])     -> TYPE_MISMATCH: spawn: cmd must be a shell command
                                  string, got list — spawn runs `sh -c` and has no
                                  argv form; build the string with shell_quote(), or
                                  use run_argv/run_argv_must for an argv list in the
                                  foreground
spawn("true", ["a"])           -> TYPE_MISMATCH: spawn: stdout_path must be a string,
                                  got list (no coercion — encode explicitly)
spawn(7)                       -> TYPE_MISMATCH: spawn: cmd must be a string, got
                                  number (no coercion — encode explicitly)
```

A NUL byte in any of the three raises too — as `TYPE_MISMATCH`, at argument
validation. It was never *accepted*: `std`'s `Command::spawn` and `File::create`
both reject an interior NUL, so 0.51.0 raised `spawn failed: nul byte found in
provided data`. What changes is when and as what. The check now runs before any
stdio file is opened, so a NUL in `stderr_path` can no longer truncate the
`stdout_path` file on its way to failing, and the error arrives in the same
`TYPE_MISMATCH` shape as every other bad argument instead of as a late
`RUNTIME_ERROR` from std.

`kill(pid[, signal])` sends `signal` (default `15` = SIGTERM) and returns a bool
(`true` if the syscall succeeded). **Both arguments are whole numbers and
neither is coerced** (strict since v0.52.0) — the same rule as `spawn`, and for
a sharper reason:

```text
kill(false)          -- 0.51.0: to_number(false) is 0, and kill(0, sig) signals
                     --   EVERY process in the caller's own process group — the
                     --   script and its siblings — while returning true
                     -- 0.52.0: raises TYPE_MISMATCH
kill($p, "SIGKILL")  -- 0.51.0: the signal silently fell back to SIGTERM, so the
                     --   caller believed SIGKILL had been sent
                     -- 0.52.0: raises TYPE_MISMATCH
kill($p, 9.5)        -- 0.51.0: truncated to 9; 0.52.0: raises (a typo, not a request)
```

A pid arriving as `false` from a failed lookup is exactly how that first line
happens in practice. Signal *names* are not accepted — pass the number (`9` for
SIGKILL); `kill(-$pgid, sig)` still addresses a process group deliberately.

`process_alive(pid)` is a liveness probe
(signal-0 test) returning a bool; it first does a non-blocking `waitpid(WNOHANG)`
to reap a zombie child of *this* process before checking — so a `spawn`ed child
that has already exited reports `false`, not a stale "alive" from a `<defunct>`
slot.

```mix
$p = spawn("sleep 30")
print("alive: " .. ("" .. process_alive($p)))
print("killed: " .. ("" .. kill($p)))
```
```text
alive: true
killed: true
```

A signal to a non-existent PID is harmless and reports the failure honestly:

```mix
print("" .. kill(999999))
print("" .. process_alive(999999))
```
```text
false
false
```

`kill(pid, 9)` sends SIGKILL; pass any signal number you need.

## Environment, identity & the working directory

```
env("NAME")     environment variable value ("" if unset — never raises)
env("NAME", d)  value, or default d when NAME is unset OR empty
args()          list of script arguments
pid()           this process's PID (number)
uid()           this process's EFFECTIVE user id (number)
gid()           this process's EFFECTIVE group id (number)
groups()        every group id this process is in (sorted list of numbers)
hostname()      the system hostname (from /etc/hostname)
cwd()           current working directory
chdir(path)     change the working directory (raises on failure)
platform()      {os, arch} map
which("cmd")    the PATH entry joined with cmd if EXECUTABLE, else nil
exit([code])    unwind finally, then terminate with status code (default 0)
sleep(secs)     suspend for secs seconds (fractional ok; async-aware)
```

`env` reads an environment variable, returning `""` (not nil, no raise) when
unset — distinct from the string-interpolation `${NAME}` form, which walks
scope → env → literal-nil. Use `env(...)` when you want an explicit env read.

A **second argument is a default**: `env("NAME", d)` returns `d` when `NAME` is
unset *or* set-but-empty (shell `${VAR:-default}` semantics) — the common "env
or fallback" need in config code, where the plain `""`-for-unset return would
otherwise defeat a `??` (nil-coalesce) default. The default is returned
**verbatim** (any value), so `env("PORT", 8080)` yields the number `8080`. The
one-arg form is unchanged.

```mix
print(env("FOO"))
print("missing=[" .. env("NOPE_XYZ") .. "]")
print(env("NOPE_XYZ", "fallback"))
print(env("PORT", 8080))
```
```text
bar
missing=[]
fallback
8080
```
*(run as `FOO=bar mix script.mix`)*

`args()` is the script's positional arguments — exactly the same list `$1`,
`$2`, … are indexed from, so the two can never disagree. It is what the runner
parsed, not a slice off the process command line: flags to `mix` itself are not
in it, wherever they appear, and under `-c` the program text is not in it
either. An embedded interpreter that was never given script arguments returns
`[]`. Reach for [`getopt`](builtins.md) when you want flag parsing.

```mix
-- script.mix:
print("" .. args())
```
```text
[alpha, beta]
```
*(run as `mix script.mix alpha beta`)*

`uid()` / `gid()` are the **effective** ids (`geteuid`/`getegid`) — normally the
identity filesystem access is checked against, so they are what a `stat()` map's
`uid`/`gid` should be compared with when a script is deciding whether a path is
its own. Strictly, Linux checks the *filesystem* ids, `fsuid`/`fsgid`; they track
the effective ids unless a process deliberately changes them with `setfsuid(2)`,
which a Mix script has no way to do — so for Mix code the two are the same
answer, and for a Mix interpreter embedded in something that does call it, they
are not.

```mix
$st = stat("/etc/hostname", {follow_symlinks: false})
print("mine=" .. ("" .. ($st.uid == uid())))
print("root-owned=" .. ("" .. ($st.uid == 0)))
```
```text
mine=false
root-owned=true
```
*(as a non-root user)*

Without these the only way to learn your own uid was to create a file and stat
it — and that probe is a liability, not a measurement: the write follows
symlinks, so another user who wins the race gets a file of their choosing
truncated under your identity, and the uid that comes back is *theirs*. Ask the
kernel instead. Under `sudo` these report the target identity, not the invoking
one; there is deliberately no real-uid form, because the effective id — with the
`fsuid` caveat above — is the one a file access is checked against, and the real
id is not.

`groups()` is the **whole** group membership — the `getgroups(2)` supplementary
set plus the effective gid, sorted, with no duplicates. (POSIX permits
`getgroups` to leave the effective gid out; this always includes it, so a caller
never has to add it back. The kernel may also report the same gid twice; the
duplicate is removed here, so the result is a set in the ordinary sense.)

`getgroups(2)` is a measure-then-fill pair, so the set can change underneath it.
Mix cannot call `setgroups`, which means this needs another thread in an
embedder — which is exactly the case that matters, since the daemons embed this
interpreter. A set that *shrinks* is truncated to what was returned; a set that
*grows* makes the fill call fail, and `groups()` re-measures and retries rather
than reporting "cannot tell", because the answer is available and only the first
measurement was stale. The retry is bounded at four attempts: a group set that
will not hold still for four reads raises instead of returning a guess.

It exists because `gid()` alone cannot answer the question scripts actually ask
of it. Unix picks **one** permission class and does not fall through: owner if
the uid matches, else group if you are in the file's group, else other. So for a
file owned by someone else, `gid()` tells you only whether its group is your
*effective* one — and a file grouped under any of your other groups gets the
group bits applied while a `gid()`-only check reaches for the other bits and
gets the answer wrong in either direction. Mode `0701` in a group you belong to
is **not** executable by you, whatever the other bit says.

```mix
$st = stat("/usr/bin/passwd", {follow_symlinks: false})
$mine = false
for each $g in groups()
  if $g == $st.gid then
    $mine = true
  end
end
print("in its group=" .. ("" .. $mine))
print("groups=" .. ("" .. len(groups())))
```
```text
in its group=false
groups=3
```

`platform()` returns a **map**, not a bare string — `os` (`linux`, `macos`,
`windows`, …) and `arch` (`x86_64`, `aarch64`, …):

```mix
$p = platform()
print("os=" .. $p.os)
print("arch=" .. $p.arch)
```
```text
os=linux
arch=x86_64
```

`which` searches PATH for an executable, returning the matching path or `nil`
(absolute whenever PATH's entries are, which is the normal case — but it is the
PATH entry joined with `cmd`, so a relative or empty entry yields a relative
answer):

```mix
print(which("sh"))
print("missing: " .. ("" .. which("definitely-not-a-real-binary-xyz")))
```
```text
/usr/bin/sh
missing: nil
```

**It answers "can I run this?", not "does this exist?"** (since v0.52.0). A PATH
entry comes back only when it is a regular file *and* the kernel says this
process may execute it — asked with the same ACL-aware `faccessat2(2)` check
[`access`](io.md#kernel-permission-checks--access) uses, never from `stat().perm` arithmetic. Before 0.52.0 the
test was `is_file()`, so a non-executable file on PATH was reported as a
command and the caller's very next `run_argv` failed to spawn it — a probe
whose job is to prevent that failure causing it instead.

Both halves are load-bearing: `X_OK` is true for a *searchable directory*, so a
PATH entry holding a directory named `git` would come back as the git binary if
the regular-file test were dropped. And `cmd` is a string, not coerced —
`which(["git"])` raises `TYPE_MISMATCH` rather than searching for a file named
`[git]`.

The consequence worth knowing: on a PATH you do not control, `which` can now
return `nil` where it used to return a path. That is the honest answer — the
old one was a spawn failure deferred by one line.

`cwd` / `chdir` read and set the working directory; `chdir` raises a catchable
error if the path doesn't exist:

```mix
chdir("/tmp")
print(cwd())
```
```text
/tmp
```

`sleep` suspends for the given seconds (fractional allowed). Under the Bus
[`--serve`](bus.md) runtime it is async-aware — a sleep with registered handlers
yields to the dispatch loop rather than blocking the OS thread:

```mix
print("before")
sleep(0.2)
print("after")
```
```text
before
after
```

The duration may be a number or numeric string (`sleep("0.2")`). A supplied
value that cannot be parsed as a number raises `TYPE_MISMATCH`; it no longer
silently becomes a zero-second sleep.

`exit([code])` stops ordinary execution, unwinds every active `finally` block
innermost-first, then terminates with the given status (default `0`):

```mix
print("exiting")
exit(3)
```
```text
exiting
```
*(process exit status is `3`)*

The optional status may be a number or numeric string. `exit()` still defaults
to status `0`; a supplied non-numeric value raises `TYPE_MISMATCH` instead of
silently exiting successfully.

## Safe interpolation — shell_quote, sql_quote, sanitize

When you *must* compose a string for an external shell or SQL statement, quote
the untrusted parts. These are the inert-by-construction helpers — but prefer
`run_stream`'s argv list (no shell at all) or a parameterised query whenever you
can.

### shell_quote

`shell_quote(s)` single-quote-wraps `s` for a POSIX shell (equivalent to PHP
`escapeshellarg`): the result is inert under shell parsing, with internal `'`
rendered as `'\''` (close, escape, reopen).

```mix
print(shell_quote("it's a file"))
```
```text
'it'\''s a file'
```

```mix
$name = "rm -rf /; echo pwned"
print(run("echo " .. shell_quote($name)))
```
```text
rm -rf /; echo pwned
```

### sql_quote

`sql_quote(s)` escapes a string for interpolation **inside** a SQL string literal:
it doubles every `'` *and* escapes `\` → `\\` (safe under MySQL/MariaDB's default
sql_mode, the documented target — quote-doubling alone is injectable there), and
strips NUL bytes. It does **not** add the outer quotes — you compose those.

```mix
print(sql_quote("O'Brien"))
```
```text
O''Brien
```

It stays safe for SQLite/Postgres standard mode (where `\` is literal — the
trade-off is a literal backslash arrives doubled). For exact-byte SQLite
literals, use a real binding (`sqlexec()` placeholders), not string composition.

### sanitize

`sanitize(s)` makes untrusted bytes safe for **one-line diagnostics**: line
breaks (including U+2028/U+2029) collapse to spaces, and C0/C1 controls plus
Trojan-Source-class characters (bidi overrides, zero-width spoofing, BOM) become
`?`. Printable Unicode is kept intact. This is what `run`'s die message uses on
the command excerpt and stderr tail.

```mix
$s = "line1\nline2\ttab"
print(sanitize($s))
```
```text
line1 line2?tab
```

Reach for it before logging anything that came from a network peer, a filename, a
header, or another process's output.

## Identity & crypto primitives

```
uuid()                 random UUID v4 string
random_password([len]) alphanumeric password (default 16, no O/o, class-diverse)
hash_sha256(s[, {raw:true}])  SHA-256 digest of a string/bytes/buffer value
hash_blake3(s[, {raw:true}])  BLAKE3 digest
hash_md5(s[, {raw:true}])     MD5 digest    -- BROKEN hash, legacy interop only
hash_sha1(s[, {raw:true}])    SHA-1 digest  -- BROKEN hash, legacy interop only
hmac_sha256(key, msg[, {raw:true}])  HMAC-SHA256 (RFC 2104) — webhook signatures
constant_time_eq(a, b) timing-safe equality — compare MACs/secrets with this, not ==
hash_file(p[, algo][, {raw:true}])   streaming digest of a FILE
                       algo: "sha256" (default) "blake3" "md5" "sha1"
                       (all of the above: lowercase hex, or bytes with raw:true)
base64_encode(v)       base64 of a string or bytes buffer
base64_decode(s)       decode base64 -> a bytes buffer
```

Every `hash_*` call takes the same trailing options map, and the only option is
**`{raw: true}`** (v0.66.0): return the digest as raw `bytes` instead of
lowercase hex. `hash_sha256($x, {raw:true})` is 32 bytes, `hash_md5` 16,
`hash_sha1` 20, `hash_blake3` 32, and `hmac_sha256` 32 — which lets a MAC be
compared with `constant_time_eq` without a hex round trip:

```mix
$mac = hmac_sha256($secret, $payload, {raw: true})
if constant_time_eq($mac, base64_decode($header)) then
  print("signature ok")
end
```

Before 0.66.0 a second argument was **silently ignored**:
`hash_sha256("abc", {raw:true})` returned the hex string and said nothing. That
is fixed, and so is the class of bug behind it — an unknown option key or a
non-map option now raises `OPTION_INVALID` rather than being discarded.

`raw` is a **strict** boolean, unlike `bytes_to_string`'s `lossy`, which accepts
any truthy value. The difference is deliberate: `raw` selects the *return type*,
so `{raw: "false"}` — a string that a config file or an argument might easily
produce — would hand back bytes and fail somewhere far from the cause.
`{raw: "false"}` raises; only `true` and `false` are accepted.

### ⚠ MD5 and SHA-1 are broken — legacy interop only

`hash_md5` and `hash_sha1` exist to talk to formats and tools that already chose
those algorithms: `Content-MD5` headers, mail dedup keys, git object ids, older
ETags and vendor APIs, and checksums you need to compare against an existing
`md5sum`/`sha1sum` output. Mix computes them so a script does not have to fork
coreutils, not because they are fit for anything new.

**They must never carry a security decision.** MD5 has had practical collisions
since 2004 and SHA-1 since 2017 (SHAttered), so neither can establish that two
inputs are the same, that a document is unmodified by an adversary, or that a
signature is valid. For anything of that kind use `hash_sha256` or `hash_blake3`,
and for authentication `hmac_sha256` with `constant_time_eq`. Note that Mix
classifies both as capability-`Pure` — that means "touches no host authority",
which is not the same as "safe"; the sandbox has no opinion about your choice of
hash.

The digests are the standard ones and interoperate exactly:

```
$ printf 'The quick brown fox jumps over the lazy dog' > /tmp/fox
$ mix -c 'print(hash_md5(read_file("/tmp/fox")))'
9e107d9d372bb6826bd81d3542a419d6
$ md5sum /tmp/fox
9e107d9d372bb6826bd81d3542a419d6  /tmp/fox
```

`hash_file` streams with a fixed 64 KiB working set whatever the file's size, so
it is the right call for anything large — `hash_sha256(read_file_bytes($p))`
holds the whole file in memory to compute the same answer.

`uuid()` is a fresh random v4 each call:

```mix
print(uuid())
```
```text
0a5fcc5b-ad60-4e8e-9e61-b52f14a067e7
```

`random_password([len])` generates an alphanumeric password from OS entropy
(`OsRng`), defaulting to length 16. It **guarantees** at least one upper, one
lower, and one digit, and excludes the confusable `O`/`o`. `len` must be an
integer in `3..=1024`; out of range raises.

```mix
print(random_password())
print(length(random_password()))
print(random_password(8))
```
```text
qALPecswZfH3YkP0
16
ZDv4WBFl
```

`hash_sha256` / `hash_blake3` return the lowercase hex digest of the input
(string or bytes buffer):

```mix
print(hash_sha256("hello"))
print(hash_blake3("hello"))
```
```text
2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f
```

`hmac_sha256(key, msg)` is the keyed twin (RFC 2104): the hex HMAC-SHA256 of
`msg` under `key` (both accept string/bytes/buffer). Its everyday job is
verifying webhook signatures — e.g. Stripe's `Stripe-Signature` `v1` value is
`hmac_sha256(endpoint_secret, timestamp .. "." .. payload)`:

```mix
print(hmac_sha256("Jefe", "what do ya want for nothing?"))
```
```text
5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843
```

Compare a computed MAC against a received signature with
`constant_time_eq(a, b)`, never `==` — plain equality short-circuits on the
first differing byte, a timing oracle. `constant_time_eq` scans the full
length unconditionally (a length mismatch returns false immediately; MAC
lengths are public):

```mix
print(constant_time_eq(hmac_sha256("Jefe", "payload"), hmac_sha256("Jefe", "payload")))
print(constant_time_eq("deadbeef", "deadbee5"))
```
```text
true
false
```

`hash_file(path[, algo])` hashes a **file** the same way, but reads it as a
64 KiB-chunked stream — so a multi-hundred-MB artifact (a release image, a
rootfs tarball) is digested with bounded memory instead of
`hash_sha256(read_file(path))` slurping the whole file into a string (which
also rejects non-UTF-8). `algo` defaults to `"sha256"`; `"blake3"`, `"md5"` and
`"sha1"` are the others (the last two added in v0.66.0 — read the warning
above before reaching for either). It takes the same trailing `{raw: true}`
as the in-memory family. The sha256 output is byte-identical to `hash_sha256` over the
same bytes, so it verifies against any `sha256sum`. Capability: **FsRead** (it
opens a path). This is the factory's "name every release artifact + its
digest, then sign the manifest" primitive.

```mix
print(hash_file("/srv/cosmix-factory/releases/2026-07-06-001/rootfs.tar.gz"))
print(hash_file("./image.raw", "blake3"))
```

`base64_encode` accepts a string **or** a `Value::Bytes` buffer (it encodes the
raw bytes, not a placeholder). `base64_decode` returns **raw bytes** — since
v0.64.0 those can be indexed, sliced, iterated and searched directly (see
[io](io.md#bytes-as-a-sequence-v0640)), or wrap in
`bytes_to_string` (strict UTF-8; pass `{lossy:true}` to tolerate non-UTF-8) to
read it back as text. The pair round-trips:

```mix
$enc = base64_encode("hello mix")
print($enc)
print(bytes_to_string(base64_decode($enc)))
```
```text
aGVsbG8gbWl4
hello mix
```

These (and `base64_*`, `uuid`) are behind the `crypto` feature, which the `mix`
binary always enables.

## Capability classes

Each system builtin carries a [capability class](capabilities.md) used by the
[`--serve`](bus.md) sandbox's `check_capability` gate: `env` / `pid` /
`hostname` / `cwd` / `platform` / `which` are **Env** (read-only inspection);
`run` / `run_rc` / `run_stream` / `spawn` / `kill` / `process_alive` / `chdir` /
`exit` (and `panic` — see [errors](errors.md)) are **Process** (they touch the
OS process table or filesystem CWD); `shell_quote` / `sql_quote` / `sanitize` /
`random_password` / `uuid` / hashes / `base64_*` are **Pure**. An embedding
daemon can deny the Process class to run untrusted Mix without it spawning
subprocesses.

## See also

- [strings](strings.md) — concat, interpolation, the byte/codepoint/grapheme split
- [errors](errors.md) — `try`/`catch`, the `die` raised by `run`, `panic`
- [Bus messaging](bus.md) — `send` / `emit` / `on … end`; prefer mesh IPC over shelling out
- [ssh](remote.md) — `ssh_run` / `ssh_must` / `ssh_mix` for remote commands
- [shell mode](shell-mode.md) — the dispatch layer: chains, pipes, redirects, brace expansion
- [capabilities](capabilities.md) — the `--serve` sandbox classes in full
- [builtins index](builtins.md) — `getopt`, `read_file`, `stat`, `chmod`, the full set

```
mix builtins system   list every system builtin with its one-line description
mix what NAME         one-line description of a single builtin (e.g. mix what run_rc)
mix help              the full categorized builtin reference
```
