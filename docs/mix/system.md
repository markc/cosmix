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
run_parallel(jobs [, {max, timeout}]) -> LIST of process_result maps in input order; runs many run_argv jobs concurrently with a bounded pool; one job's failure is DATA, never a raise (v0.82.0)
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
- `grace`: seconds, default **0**, fractional ok — what happens AT the
  deadline. `0` SIGKILLs the child's process group at once (the historic hard
  kill). A positive grace sends SIGTERM to the group, then waits until the
  **whole group** is gone or `grace` runs out, then SIGKILLs whatever is
  left. The grace covers the group, not only the child. In
  `sh -c "pg_dump app | gzip > f"`, `sh` may die at once, but `pg_dump` and
  `gzip` still get the full grace to finish. A descendant that ignores SIGTERM
  is killed at the grace deadline, never left running. The same escalation
  applies when the child has already exited but a descendant still holds a
  captured stream open at the deadline. The call can therefore take up to
  `timeout + grace`. The result still reports `timed_out: true`, and `signal`
  says how the child ended: `15` if it obeyed SIGTERM, `9` if it had to be
  killed. `grace` with `timeout: 0` raises
  `OPTION_INVALID`, because without a deadline it would do nothing.

  ```mix
  $r = run_argv(["pg_dump", "app"], {timeout: 600, grace: 10,
                                     stdout: {file: "/srv/app.sql"}})
  if $r.timed_out then
    eprint("dump cut off at the deadline (signal " .. $r.signal .. ")")
  end
  ```
- `stdin`: `nil` or `{null: true}` closes stdin; string/bytes/buffer supplies
  those bytes; `{file: path}` opens a local file for the child to read.
  `{inherit: true}` hands the child mix's own stdin, but **only when that
  stdin is not a terminal**, such as a pipe or a redirected file:

  ```mix
  -- producer | mix filter.mix
  $r = run_argv(["sort", "-u"], {stdin: {inherit: true}})
  ```

  When mix's stdin is a terminal, `{inherit: true}` raises `STDIN_TERMINAL`
  before anything spawns. run_argv puts its child in a new process group, so it
  is not the terminal's foreground group, and a terminal read would stop it
  with `SIGTTIN`. The call would then hang until its deadline. That is why
  there is no string form either: `stdin: "inherit"` is ordinary stdin data,
  the seven bytes `inherit`. Use `run_stream` when a child must own the
  terminal and its stdin. The same rule applies to stage 0 of `run_pipeline`
  and to `run_parallel` jobs. At most one `run_parallel` job may inherit
  stdin. Inheriting stdin makes sense for a script run as `mix script.mix`
  or `mix -`, where stdin is data. A bare REPL fed through a pipe reads its
  own input lines from that same stdin, so an inheriting child there would
  compete with the REPL for the lines of the script. Two or more raise `OPTION_INVALID` before any job runs, because
  concurrent readers of one pipe would race for its bytes. With
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
`error_code`, `error`, `stages`, `status`, `failed_stage`, `summary`.

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
`accepted_signal`, `status`, `broken_pipe`. Stage stderr is untrimmed.
`accepted_signal` records the SIGPIPE policy below; it is false for ordinary
exits and rejected signals.

#### Why each stage ended — `status`, `failed_stage`, `summary`

Gates branch on these fields, never on stderr or `summary` text. A stage's
`status` is the first of these that applies:

| stage `status` | meaning |
|---|---|
| `ok` | exited 0 |
| `timeout` / `interrupted` | still running when the deadline / Ctrl-C made Mix signal its group — the death is Mix's, not the stage's |
| `broken_pipe` | killed by SIGPIPE: its reader closed (also set when `allow_signal` accepted it — `.ok` carries the acceptance, `status` the fact) |
| `signal` | killed by any other signal (`.signal` names it) |
| `exit_nonzero` | exited with a non-zero code (`.exit_code`) |
| `setup_error` | started, then killed because a later stage could not be set up |

`broken_pipe` (bool) is true whenever SIGPIPE killed the stage. SIGPIPE is the
only evidence of a closed reader that a stage cannot forge: a program that
ignores SIGPIPE and exits non-zero on `EPIPE` reports `exit_nonzero`.

The pipeline's `status` is `setup_error` (with `error_code`), else
`interrupted`, else `timeout`, else `ok` when `.ok` is true, else the status of
`failed_stage`. `failed_stage` is the **rightmost** stage whose `ok` is false —
`set -o pipefail`'s rule — or `nil`. Rightmost because a downstream stage that
exits early leaves its writer with a broken pipe: in `yes | sh -c 'exit 3'` the
cause is stage 1 (`exit_nonzero`) and stage 0's `broken_pipe` is the symptom.
For `PIPELINE_SPAWN` it is the stage that could not start; other setup errors
name no stage.

`summary` is a one-line human rendering with no durations, so it is stable
across runs:

```
ok (2 stages)
ok (2 stages; stage[0] yes broken pipe accepted)
stage[1] sh exited 3
stage[0] sh killed by signal 15
stage[0] yes: broken pipe (its reader closed)
timed out (deadline 300 ms); killed stage[1] sleep
```

A captured stream that truncated appends ` [output truncated]`.

```mix
$r = run_pipeline([["producer"], ["filter"], ["consumer"]], {timeout: 60})
if $r.status == "timeout" then
  eprint("slow: " .. $r.summary)
elif $r.status != "ok" then
  $s = $r.stages[$r.failed_stage]
  eprint("stage " .. $s.index .. " " .. $s.status .. ": " .. $s.stderr)
end
```

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
`$err.details.result`. A stage failure is raised from that result's own
`status` and `failed_stage`, so the raise and the result always agree. A
`signal` or `broken_pipe` status gives `PIPELINE_SIGNAL`, anything else gives
`PIPELINE_EXIT_NONZERO`, and the message names `failed_stage`. For
`yes | sh -c 'exit 3'` that is `PIPELINE_EXIT_NONZERO` for stage 1, not
stage 0's broken pipe.

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
timeout SIGKILLs the group immediately (`run_argv` can opt into SIGTERM first
with `grace`); a Ctrl-C sends SIGTERM, waits a 2-second grace, then SIGKILLs.
Every SIGTERM path waits for the whole group to empty, bounded by its grace,
then SIGKILLs the group. A descendant that honours SIGTERM can finish its
cleanup; one that ignores it is killed at the deadline, not orphaned. On Linux
the child is reaped only after that, so its zombie keeps the group id
reserved and every signal reaches the right group. "The group has emptied"
is read from `/proc`, and any record Mix cannot read counts as still alive.
A member hidden from `/proc` entirely, by another pid namespace or
`hidepid=2` for another user's processes, cannot be counted. There the grace
may end early, but the SIGKILL still reaches the whole group. An interrupt that lands on the same poll as the deadline
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

### Repository version gate

The repository's `src/tools/version_flag_gate.mix` reports every skipped
binary as `SKIP <bin> (<crate>): <reason>`; its summary skip count matches
those lines, including binaries absent because required features were not built.

### run_parallel — process-level fan-out (v0.82.0)

`run_parallel(jobs [, {max, timeout}])` runs many `run_argv` jobs at once through
a bounded worker pool and returns a **list of `process_result` maps in input
order** — each map is exactly what the same `run_argv` call would return, so
existing result-handling code ports unchanged. One job's ordinary failure
(nonzero exit, timeout, a spawn error) is **data in its map, never a raise**.

Ctrl-C is checked before taking each job and again before starting it. Jobs
that have not started return `ok: false, interrupted: true` in their original
input positions without spawning a process; running jobs use the normal
process interruption and cleanup path.

```mix
-- Fan a health check across a fleet; collect every result.
$hosts = ["alpha", "beta", "gamma"]
$jobs = map($hosts, fn($h) = ["ssh", $h, "uptime"])
$results = run_parallel($jobs, {max: 8, timeout: 10})
for each $i in range(0, len($hosts) - 1)
  $r = $results[$i]
  print($hosts[$i] .. ": " .. ($r.ok ? trim($r.stdout) : "DOWN (" .. $r.exit_code .. ")"))
end
```

A job is either an **argv list** (`["ssh", $h, "uptime"]`) or a **`{argv, …}`
map** carrying the same per-job options `run_argv` accepts (`stdin`, `cwd`,
`env`, `clear_env`, `stdout`, `stderr`, `max_output`, `timeout`). `max` bounds
concurrency (default 8, **hard-capped at 256** live workers — a larger `max`
still runs every job, just no more than 256 at once); a top-level `timeout`
(seconds) overrides every job's own. A job may **not** disable its deadline
(`timeout: 0` is refused): one never-exiting job would park a worker forever and,
because the call waits for all workers, hostage the whole batch. A parse error in
**any** job fails the whole call before a single process spawns, exactly as
`run_argv` validates before spawning.

**This is PROCESS-level fan-out, not in-language concurrency.** The evaluator is
single-threaded by construction and Mix `Value`s are never shared across
threads: `run_parallel` parses every job to plain owned data first, and the
worker threads touch only process plumbing (argv, pipes, exit codes), marshalling
results back on the caller's thread. There is deliberately **no** `parallel(list,
fn)` that runs Mix functions concurrently — that would mean rebuilding the value
model, and it is not planned. A fleet sweep that walked N nodes serially becomes
one `run_parallel` of N ssh jobs; for Mix source, [`ssh_mix_many`](remote.md#ssh_mix_manyhosts-source-opts--map-host--result)
is the same pool with `ssh_mix`'s `bindings`, `decode` and result shape kept. A job's
`stream` flag is ignored (a parallel live tee would interleave into garbage).

### send_mail — report mail through the local MTA

`send_mail(msg [, {host, sendmail, timeout}])` is the last step of every nightly
report: render a plain-text message and hand it to the MTA. It replaces the
hand-written `.eml` file, the `run_argv(["/usr/sbin/sendmail", "-t", "-f", $from],
{stdin: {file: …}})` and the cleanup.

```mix
$r = send_mail({
  to: ["ops@example.com", "Audit <audit@example.com>"],
  from: "Reports <reports@example.com>",
  subject: "Weekly spam report",
  body: $report
})
if !$r.ok then
  eprint("mail failed: " .. $r.exit_code .. " " .. $r.stderr)
end
```

- **`msg`** needs `to` (a string or a list of strings), `from`, `subject` and
  `body`. `headers` is an optional map of extra headers. Any other key raises.
  There is no `cc` or `bcc` key: put `Cc` and `Bcc` in `headers`.
- **The message is built for you**: `From`, `To`, `Subject`, `Date` (UTC),
  `Message-ID`, `MIME-Version: 1.0` and `Content-Type: text/plain;
  charset=utf-8`. A CRLF body is normalised to LF.
- **Line limits are kept.** A long header (a long subject, forty recipients)
  is folded at whitespace so lines stay within 78 characters; a header with
  an unbreakable run over 998 characters raises. An ASCII body whose lines
  all fit in 998 bytes goes as `7bit`. A non-ASCII body, or one with a longer
  line (a JSON or CSV dump), goes as `quoted-printable`, so no MTA can split
  or mangle it.
- **A non-ASCII subject is RFC 2047-encoded** (as [`rfc2047_encode`](strings.md)
  does it). Every other header value must be ASCII, and a CR, LF or NUL in
  any header raises. There is no way to inject a second header through a
  value.
- **`headers` adds headers, or replaces a generated `Date`, `Message-ID` or
  `MIME-Version`.** `From`, `To` and `Subject` come only from `msg`, and
  `Content-Type` and `Content-Transfer-Encoding` only from `send_mail`, which
  encodes the body itself. Naming any of those five in `headers` raises. A
  label of your own would describe other bytes than the ones sent: a
  quoted-printable body labelled `8bit` reads as literal `=C3=BC`. The body is
  always `text/plain; charset=utf-8`.
- **Delivery is `sendmail -t -i -f <envelope>`** with the message on stdin,
  never a network SMTP client. `-t` takes the recipients from the headers, so
  a `Bcc` header works and is stripped by the MTA. `-i` keeps a line holding
  a lone `.` as body text.
- **`from` is exactly one mailbox**: `a@example.com` or
  `Display Name <a@example.com>`, where the display name is plain words or one
  `"quoted string"`. The envelope sender is that address. A comment, a second
  mailbox and the null sender `<>` raise rather than guess. An address
  without `@` (a local user such as `root`) is sent as it is, and the
  `Message-ID` domain then falls back to `localhost`.
- **Which sendmail**: the first `sendmail` on `PATH`, then `/usr/sbin/sendmail`,
  then `/usr/lib/sendmail`. The `sendmail` option names one explicitly.
- **`host` sends from another host** — the one with a working MTA — through
  [`ssh_exec`](remote.md). The message is still built locally; only the
  sendmail run is remote, with `/usr/sbin/sendmail` as the default there. The
  result carries `host` as `ssh_exec`'s does, and only then.
- **The result is `run_argv`'s `process_result` plus `message_id`**. `ok`
  means the MTA accepted the message for submission, not that it was
  delivered. An MTA that refuses the message, or a sendmail that is not
  there, is DATA: `ok: false` with `exit_code`, `stderr` and `error_code`.
- **Bad input raises before anything runs**: `OPTION_INVALID` for a bad
  `msg` field or option, `TYPE_MISMATCH` for a `msg` that is not a map or the
  wrong number of arguments, and with `host`, `ssh_exec`'s own validation
  errors.
- **`timeout`** is in seconds, as for `run_argv` (default 30).

Mail that a filter must recognise should use one fixed `from` for every
report, with the envelope matching it, which `send_mail` does by
construction.

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

`spawn` starts a background process and returns its **PID** as a number. It does
not wait, reap, or supervise — it owns nothing after it returns (that is
`run_argv`'s / a supervisor's job). It has **two forms**, chosen by the first
argument's type.

**Shell form — `spawn(cmd[, stdout_path[, stderr_path]])`**, a string command
run via `/bin/sh -c` with stdin from `/dev/null`. Stdio routing by arity:

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

All three shell-form arguments are strings and **none is coerced** (strict since
v0.52.0) — `spawn` needs loud validation most, because it is the only runner
with nowhere to put a failure: it returns a **PID, not a result map**, so a child
that dies on its first line looks exactly like one that worked. A non-string
raises `TYPE_MISMATCH` at argument validation, before any stdio file is opened
(so a NUL in `stderr_path` can no longer truncate the `stdout_path` file on the
way to failing).

**Argv form — `spawn(argv[, {detach, die_with_parent, cwd, env, clear_env, stdout, stderr}])`**
(v0.89.0), a **list** of strings run **directly, with no shell** — so no
word-splitting, glob expansion, or quoting surprises. This is the launcher /
daemon slot: the job that used to force `run("setsid app &")` through `sh`.

```mix
spawn(["bterm", "--profile", "work"])                 -- argv, no shell
spawn(["mydaemon"], {detach: true})                   -- new session (setsid),
                                                      --   survives the caller,
                                                      --   drops the terminal
spawn(["worker"], {cwd: "/srv/app", env: {ROLE: "bg"},
                   stdout: {file: "/var/log/worker.log", append: true}})
```

- `detach: true` → the child is put in a **new session** (`setsid`): it has no
  controlling terminal and its own session, so a terminal hangup or the caller
  exiting does not take it down — the daemon shape, stronger than the shell
  form's `&`. (Session separation, not immortality: a service-cgroup teardown
  or an explicit signal still reaches it.) Since v0.92.0 it is also
  **double-forked**: the child is reparented to init (or the nearest
  subreaper), which reaps it — so a long-lived caller such as a `mix --serve`
  launcher citizen is never left holding zombies, and "owns nothing after it
  returns" is literally true. The returned PID is the real (grandchild)
  process, and it still leads its own session. Because init reaps it, the pid
  is FREE as soon as the child exits and the kernel may hand it to an unrelated
  process: a later `kill($pid)` or `process_alive($pid)` can hit a stranger.
  A live pid only says SOME process holds that number, and a pidfile alone
  says no more — a stale one names whoever holds that pid now. To stop a
  detached daemon later, check a pidfile together with the process start time
  (field 22 of `/proc/<pid>/stat`, recorded when the pid was written), or ask a
  Bus verb the child itself answers, rather than trusting a bare pid. Default `false` (a plain child
  in the caller's session, which stays the caller's to reap).
- `die_with_parent: true` (Linux) → the opposite slot. The child **ends with
  this mix process**, for a helper that must not outlive the script or
  `--serve` citizen that started it:

  ```mix
  $pid = spawn(["goose", "serve", "--port", "7070"], {die_with_parent: true})
  ```

  The child leads its own process group. Two mechanisms end it:
  - **Graceful exit.** This covers the script ending, `exit()`, a `--serve`
    citizen's QUIT or SIGTERM drain, and a REPL restart. Mix sends SIGTERM to
    the child's whole process group, waits up to 2 s for the group to empty,
    then SIGKILLs whatever is left. The child gets its chance to clean up, and its own
    children go too.
  - **`--serve` RELOAD.** The old generation's owned children are ended the
    same way *before* the new script's init runs. An init that starts its
    helper again therefore never races a leftover one for the same port. If
    the reload then reverts, because the new init failed, the old script
    resumes without those children. Anything the failed init had already
    spawned with `die_with_parent` is ended too, so a failed reload leaks no
    helpers.
  - **Hangup of the interactive shell.** When the terminal goes away, the
    job-control shutdown sweeps owned children the same graceful way before
    it exits.
  - **Crash, or a signal mix does not handle.** If mix is SIGKILLed, panics
    or is OOM-killed, the kernel SIGKILLs the child (`PR_SET_PDEATHSIG`). The
    same happens when a plain `mix script.mix` or a non-interactive mix
    receives SIGTERM, SIGHUP or SIGQUIT, because those still end mix at once
    by default. That path reaches the child only, not its descendants, and
    gives it no chance to clean up. A `--serve` citizen is different: its
    SIGTERM is a graceful drain, so the sweep runs. A script that must clean
    up its helper's own children on SIGTERM should run as a `--serve`
    citizen or be stopped with Ctrl-C, which ends the evaluation normally and
    so reaches the sweep.

  `detach` together with `die_with_parent` raises `OPTION_INVALID`, because
  they contradict each other. Mix itself is the only thing that reaps an
  owned child. `process_alive` answers for it without freeing its pid, so
  the group id cannot be recycled while it is registered. A finished child
  whose group has emptied is reaped at the next `process_alive` or `spawn`.
  One whose descendants are still running is kept as a zombie until the
  sweep ends them. PDEATHSIG is keyed to the thread that called
  `spawn`, and the ownership registry is process-wide. So the option works
  only on a thread whose host owns it. The `mix` binary evaluates on one
  thread that lives until exit and sweeps before it ends. Elsewhere, for
  example webd, cosmix-mcp or cosmix-claud evaluating on pooled `spawn_blocking`
  threads, `die_with_parent` raises `OPTION_INVALID`. An embedder that does
  own a long-lived evaluation thread opts in by calling
  `builtins::owned_spawns::enable()` on it. That call returns `false`, and
  changes nothing, if another thread enabled first; only the first thread is
  ever the host. The embedder must also call `sweep()` on that
  same thread before it exits. Off Linux the option raises
  `OPTION_INVALID`. Default `false`: a plain spawn child is untouched by mix's
  exit.
- `cwd` / `env` / `clear_env` behave exactly as in [`run_argv`](#run_argv)
  (clear-then-layer: `{clear_env: true, env: {…}}` starts from empty).
- `stdout` / `stderr` reuse `run_argv`'s routing, minus capture: `"null"`
  (default), `"inherit"`, or a `{file, append?, mode?}` map — and
  `stderr: "stdout"` to merge. **`"capture"` is refused**: capturing means
  waiting, which is `run_argv`'s job. A file-open failure means the child is
  **not** spawned.
- argv must be a non-empty list of strings, none coerced; a non-string element
  or an empty list raises `TYPE_MISMATCH`.

```text
spawn(["true"], {stdout: "capture"})  -> OPTION_INVALID: stdout cannot be "capture"
spawn(["echo", 42])                   -> TYPE_MISMATCH: argv[1] must be a string
spawn(7)                              -> TYPE_MISMATCH: cmd must be a string
```

A NUL byte in any string argument is rejected at validation, before any stdio
file is opened. The error *code* follows where the argument is validated: the
shell form's positional command and path args, and the argv form's argv
elements, are `TYPE_MISMATCH`; the argv form's option values — `cwd`, `env`,
and a file route's `{file: …}` path — are `OPTION_INVALID`. `std` would reject a
NUL at spawn anyway, but catching it early keeps a late failure from truncating
a good log on the way down.

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
has_builtin(n)  does THIS mix have the named builtin? -> bool (v0.78.0)
mix_version()   {major, minor, patch, string} — the runtime version (v0.78.0)
script_version() the entry script's provenance map, nil outside a script (v0.95.0)
exit([code])    unwind finally, then terminate with status code (default 0)
sleep(secs)     suspend for secs seconds (fractional ok; async-aware)
```

`has_builtin(name)` and `mix_version()` are the **portability pair** — for
a script that must run across a mixed-version fleet, or a compat shim.
`mix builtins NAME` exits 0 for *any* name and cannot tell you whether a
builtin exists, so `has_builtin` is the real yes/no:

```mix
if not has_builtin("ws_connect") then die("this needs mix >= 0.74") end
$v = mix_version()
if $v.major == 0 and $v.minor < 78 then die("needs mix >= 0.78.0") end
```

A feature-gated builtin missing from *this* build still reads `true` from
`has_builtin` (the binary knows the name — calling it raises "requires the
X feature"); that is the honest answer, distinct from a name the binary
has never heard of.

`script_version()` is the script-side twin of `mix_version()`: the running
entry script's `{name, version, sha, sha256, modified, mix: {version, sha,
dirty}}` — the facts `mix SCRIPT --version` prints, as data, so a script can
log its own provenance. `version` comes from the `-- version: X.Y.Z` header
(nil when absent); the map is nil in the REPL and under `-c`. See
[`--version` for scripts](invocation.md#--version-for-scripts).

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
password_hash(pw[, cost])   bcrypt "$2b$…" hash; cost 4-31, default 12 (v0.71.0)
password_verify(pw, hash)   check against a bcrypt hash -> bool (v0.71.0)
```

`password_hash`/`password_verify` exist for **account passwords written over
the Bus**: a raw `props.set` of `maild.accounts.password` stores the field
verbatim (only the `account add` CLI hashed it), so the bcrypt has to happen
client-side — which used to mean shelling out to PHP. Three deliberate
edges: input over **72 bytes raises** (bcrypt's own truncation limit,
surfaced instead of silently applied — hash a digest of longer secrets);
a **malformed hash raises** in `password_verify` rather than answering
`false` (a corrupt stored hash is a config fault, and "wrong password"
would misdirect the operator); and the general-purpose digests above are
**not** password hashes — `hash_sha256` has no work factor.

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

## UDP datagrams — `udp_send`, `udp_recv` (v0.71.0)

```
udp_send(host, port, payload)   -> bytes sent; payload string/bytes/buffer, verbatim
udp_recv(port[, opts])          -> {bytes, text, from_host, from_port} | nil on timeout
                                   opts: timeout (secs, default 30, 0 = forever),
                                         host (bind addr, default "0.0.0.0"),
                                         max  (read cap, default 65535)
```

One datagram in, one out — deliberately **not** a socket API: there is no
handle to hold, leak, or close. Before these, a unicast SSDP NOTIFY meant
`run_argv(["nc", "-u", …], {stdin: $pkt})`. `udp_send` resolves the host and
reports the byte count the kernel accepted (UDP being UDP, that is *sent*,
never *delivered*). `udp_recv` binds, waits for **one** datagram, and
returns `nil` on timeout — an ordinary answer for a datagram wait, not an
error. `text` is the payload decoded as UTF-8 or `nil`; `bytes` always
carries the truth. A datagram longer than `max` is truncated to it —
`recvfrom(2)`'s own contract — and the default `max` of 65535 can never
truncate, because no UDP payload exceeds it.

```mix
$notify = "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\r\n"
udp_send("192.168.1.20", 1900, $notify)     -- unicast SSDP to the TV

$got = udp_recv(9999, {timeout: 5})
if $got != nil then
  print("from " .. $got["from_host"] .. ": " .. ($got["text"] or "<binary>"))
end
```

## WebSocket client — `ws_connect`, `ws_send`, `ws_recv`, `ws_close` (v0.74.0)

```
ws_connect(url[, opts])   -> numeric handle; opts: insecure (skip TLS verify),
                             headers (map), timeout (secs, default 30, connect+handshake)
ws_send(h, payload)       -> nil; string -> text frame, bytes/buffer -> binary
ws_recv(h[, timeout])     -> string | bytes | nil on timeout (default 30, 0 = forever)
ws_close(h)               -> true when live, false when unknown (never raises for that)
```

A synchronous client with **recv-driven control flow** — the shape every
stateful request/response protocol needs: send `register`, wait for the
`registered` frame, then issue requests and parse each response (an
`alertId` echoed back in a `closeAlert`, a pairing prompt to wait out).
A blind `(cat msgs; sleep) | websocat` pipeline only handles
fire-and-forget; this is the difference that turned the LG SSAP TV driver
from a bash FIFO coprocess into a plain Mix loop. Home-Assistant-style
device control and Grafana live are the same shape.

The contracts worth knowing:

- **`insecure: true`** skips TLS certificate verification — for the
  self-signed device endpoint (`wss://tv:3001/`), never for services with
  real certificates.
- **`ws_recv` returns `nil` on timeout** — an ordinary answer; poll
  again. Ping/pong never surface (answered internally). A **peer close
  RAISES** (catchable) and retires the handle: "no more frames, ever"
  must not be confusable with "no frame yet".
- A closed/raised handle is **retired** — further calls say
  `unknown ws handle` rather than repeating the close error.
- Handles are process-global numbers (the `sqlopen` pattern); the kernel
  reaps the sockets at exit, `ws_close` is the tidy path. One user per
  handle: a handle is checked out of the registry for the duration of a
  call, so a concurrent call on the SAME handle answers `unknown ws
  handle` rather than queueing.
- `ws_send` writes under the connect timeout: a very large frame on a
  slow link can raise mid-flush with bytes still buffered — the next send
  on that handle continues the partial frame (no framing corruption).

```mix
$h = ws_connect("wss://192.0.2.20:3001/", {insecure: true})
ws_send($h, json_encode({type: "register", payload: $manifest}))
loop
  $frame = ws_recv($h, 5)
  if $frame == nil then continue end          -- quiet: keep waiting
  $msg = json_parse($frame)
  if $msg["type"] == "registered" then break end
end
ws_send($h, json_encode({type: "request", uri: "ssap://api/getServiceList"}))
print(ws_recv($h, 10))
ws_close($h)
```

## Raw TCP client — `tcp_connect`, `tcp_send`, `tcp_recv`, `tcp_recv_line`, `tcp_close` (v0.78.0)

```
tcp_connect(host, port[, {timeout, tls, insecure}]) -> handle
tcp_send(h, payload)                    -> bytes sent (string/bytes/buffer)
tcp_recv(h[, {timeout, max}])           -> bytes | nil on timeout
tcp_recv_line(h[, {timeout, max}])      -> string (LF+CR stripped) | nil
tcp_close(h)                            -> bool
```

The stream-socket primitive for a line-or-binary protocol that UDP, WS
and HTTP don't cover — an SMTP/IMAP probe, a redis `PING`, memcached
stats, a banner grab. `tls: true` wraps the connection (ring-pinned
rustls, webpki roots); `insecure: true` skips certificate verification
for a self-signed endpoint. Handles are process-global numbers like
`sqlopen`/`ws_*`, one user per handle, reaped by the kernel on drop.

`tcp_recv` returns whatever bytes are available (up to `max`, buffered
bytes first) — **bytes, not a string**, because a stream has no message
boundary; `nil` on timeout is an ordinary answer (poll again), and a peer
**close RAISES** and retires the handle. `tcp_recv_line` is the
line-protocol form: it buffers across reads and returns one line with its
`\n` (and a trailing `\r`) stripped, so a caller speaking SMTP or redis
never hand-rolls a `\r\n` scanner over accumulated bytes. `max` caps the
line length (refused past it, so a peer that never sends a newline can't
grow the buffer without bound).

```mix
$h = tcp_connect("mx.example.com", 25, {timeout: 10})
print(tcp_recv_line($h, {timeout: 10}))          -- 220 banner
tcp_send($h, "EHLO probe.local\r\n")
loop
  $line = tcp_recv_line($h, {timeout: 5})
  if $line == nil then break end
  print($line)
  if not starts_with($line, "250-") then break end -- 250<space> = last line
end
tcp_close($h)
```

Same inline-blocking caveat as the others (below): a pending `tcp_recv`
holds the evaluator's thread, so `{timeout: 0}` in a `--serve` citizen
can wedge the pump.

Both UDP builtins above, all four `ws_*`, and the five `tcp_*` are **Network**-class (below), like `http_*` and `dns_lookup` —
and like those, the wait happens inline on the evaluator's thread. In a
`--serve` citizen that means a pending `udp_recv` blocks the whole event pump
for its duration; **never** pass `{timeout: 0}` there — unlike an HTTP call,
where the wait rides an in-flight request, a datagram wait with no sender is
unbounded and nothing can interrupt it.

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
