# errors — error handling & exit control

Mix has one error channel and three ways to use it: **catch** an error and recover
(`try … catch $e … end`), **raise** one deliberately (`die`), or **stop the whole
process** (`exit`, or the uncatchable `panic`). `exit` first unwinds active
`finally` cleanup; `panic` aborts past it. Command failures are split two ways
on purpose — `run` *raises* (catch it or it kills the script), while `run_rc`
*returns a status map* you branch on. The parser and evaluator have resource caps
(nesting, recursion) that turn a runaway script into a **clean, catchable error**
instead of a native crash.

Mental model: when you `catch $e`, `$e` is the **message string** — unchanged since
the beginning. Since 0.29.0 every runtime error ALSO carries a structured payload
(a stable `code`, optional `details` map, and a call traceback); bind it with the
optional second catch variable — `catch $msg, $err` — and raise your own with
`raise(code, message[, details])`. Since 0.30.0 a `finally` clause runs on every
exit path. There is no exception hierarchy.

## The two kinds of failure

| | Raised by | Catchable by `try`? | Exit code if uncaught |
|---|---|---|---|
| **`die` error** | `die "msg"`, `run(...)` on non-zero exit or timeout | **yes** | 1 |
| **runtime error** | unbound `$var`, bad builtin arg, recursion cap, division/modulo by zero, `ssh_must(...)` on failure | **yes** | 1 |
| **lexer / parse error** | bad syntax (a misplaced `;` inside an expression, missing operand, nesting cap) | **no** — fails before any code runs | 1 |
| **`panic(...)`** | the `panic` builtin only | **no** — aborts the process (see [§ panic](#panic--the-uncatchable-abort)) | 101 (Rust panic abort) |
| **`exit(code)`** | the `exit` builtin | **no** — it *is* the exit | `code` |

The dividing line: `try` catches the two errors that happen **while your code is
running** (`die` and runtime errors). A **syntax** error is detected at parse time —
before the first statement executes — so no `try` can wrap it. `exit` and `panic`
are not errors at all and cannot land in `catch`: `exit` is an interpreter control
signal that runs `finally` while unwinding to the process boundary; `panic` aborts.

## Structured errors, codes & tracebacks (0.29.0)

Every catchable error carries a structured payload. `catch $msg, $err` binds it —
`$msg` stays the plain message string, `$err` is a map:

```mix
try
  raise("VALIDATION_REQUIRED", "node is required", {field: "node"})
catch $msg, $err
  print($err.code)             -- VALIDATION_REQUIRED
  print($err.message)          -- node is required
  print($err.details.field)    -- node
  print(length($err.frames))   -- call traceback, outermost first
end
```

- `$err.code` — a stable `UPPER_SNAKE` identifier. Legacy errors surface as `RUNTIME_ERROR` (generic runtime failures) and `USER_DIE` (`die`, plus `run` non-zero/timeout today). Emitted today: `NAME_UNDEFINED`, `FUNCTION_UNDEFINED`, `CAPABILITY_DENIED`, `TYPE_MISMATCH`/`OPTION_INVALID` (argument validation), `VALUE_OUT_OF_RANGE` (a typed value outside a backend's representable domain), `TOML_UNREPRESENTABLE` (`toml_encode`, with `{path,type}` details), the `PROCESS_*` family (`run_argv`/`run_argv_must`), the `PIPELINE_*` family (`run_pipeline`/`run_pipeline_must`), and the `VALIDATION_*` family (`validate` and friends). The `SSH_*`/`HTTP_*` families are reserved and roll out as those builtins migrate (0.29–0.30); until then ssh/http failures surface as `RUNTIME_ERROR`/`USER_DIE`. Codes are stable identifiers, never reused; scripts may define their own.
- `$err.message` — the same string `$msg` binds.
- `$err.details` — an operation-specific map (`nil` when the error carries none).
- `$err.cause` — a nested error map, or `nil` (used by wrapping layers).
- `$err.frames` — the call traceback, a list of `{kind, function, file, line, column}` maps, outermost-to-innermost with the failure site last. `kind` is `"script"` or `"builtin"`. Frames cover the standard call path; the numeric fast paths contribute no frames, and `column` is always `nil` today.

`PROCESS_STDIO` means `run_argv` could not set up a requested
`stdin`/`stdout`/`stderr` destination before spawning the child — opening or
creating a route file, or creating the pipe/descriptor a `stderr: "stdout"`
merge needs. `run_argv` encodes it in
the returned process_result (`ok: false`, `exit_code: nil`); `run_argv_must`
raises the same code with that result under `$err.details.result`. The child is
not spawned after this setup failure. No route is truncated until all routes
have opened; truncation itself is not transactional, so a later truncation
failure can leave an earlier route truncated. A deadline expiring while an
input FIFO waits for a writer is also `PROCESS_STDIO`; Mix wakes and reaps the
open worker before returning, so repeated failures do not leak blocked threads.

Capture abandonment is a deadline/interrupt cleanup outcome, not a
`PROCESS_IO` error. The returned captured prefix is marked truncated, and an
abandoned `stream: true` reader stops teeing before the call returns; an
already-blocked parent-stream write can delay that return. With `timeout: 0`,
normal completion waits for capture EOF and for any stdin-data writer, rather
than abandoning either worker. Late descendant output is returned untruncated;
a descendant retaining stdin without reading can make the explicit no-deadline
call wait indefinitely.

The pipeline family is distinct because a pipeline can fail in a middle stage
while its final stage exits 0. The codes split by which builtin can produce
them, exactly as `PROCESS_*` does.

**Returned in the pipeline_result** (`ok: false`, `error_code` set) by
`run_pipeline`, and raised with that same result under `$err.details.result` by
`run_pipeline_must` — these are setup/lifecycle failures, never ordinary
command failure:

- `PIPELINE_STDIO` — a requested stdin/stdout/stderr file or pipe could not be
  opened or created. Every stage's routes and every pipeline/capture pipe are
  prepared before any stage is spawned, so this code means no stage ran. No
  route is truncated until the full set has opened, but the later truncation
  pass is not transactional.
- `PIPELINE_SPAWN` — a stage could not be spawned. If earlier stages had
  already started, `.stages` contains their reaped outcomes; captured stderr
  abandoned during that cleanup is empty and marked truncated.
- `PIPELINE_IO` — writing pipeline stdin or draining captured output failed.
- `PIPELINE_INTERNAL` — process polling, waiting, or a pipeline worker failed.

**Raised only by `run_pipeline_must`.** `run_pipeline` reports these outcomes as
data — `ok`, `exit_code`, `signal`, `timed_out`, `interrupted`,
`stdout_truncated`/`stderr_truncated` — and leaves `error_code` nil:

- `PIPELINE_EXIT_NONZERO` — at least one stage exited non-zero.
- `PIPELINE_SIGNAL` — a stage was signal-killed and was not an accepted
  non-final SIGPIPE.
- `PIPELINE_TIMEOUT` — the whole-pipeline deadline expired.
- `PIPELINE_INTERRUPTED` — Ctrl-C interrupted the pipeline.
- `PIPELINE_OUTPUT_LIMIT` — a captured stream exceeded `max_output`.

`raise(code, message[, details])` raises a catchable structured error. The code
must match `[A-Z][A-Z0-9]*(_[A-Z0-9]+)*` and `details` must be a map when given.

An **uncaught** error that crossed a function or builtin boundary prints a
traceback to stderr (exit 1, as before):

```text
Traceback (most recent call last):
  at <main> (/path/job.mix:7)
  at provision (/path/job.mix:2)
  at connect (/path/job.mix:5)
  at <builtin:raise> (/path/job.mix:5)
SSH_TIMEOUT: remote operation exceeded 60 seconds
```

`mix --no-traceback script.mix` restores the legacy single-line rendering
(`Runtime error at file:line: message`); errors that never crossed a function
boundary render single-line either way, so shallow scripts and stderr scrapers
are unaffected. Deep stacks elide the middle frames in the *rendering* only —
`$err.frames` always holds the full list.

One field-name note: `$err.frames[0].function` works — `function` is accepted in
field position since 0.29.0 (`fn` reads the same field; map-literal and
strict-data keys still require quoting `"function"`).

## try … catch — recover from a failure

```mix
try
  die("boom: something failed")
catch $e
  print("caught: " .. $e)
end
print("after")
```
```text
caught: boom: something failed
after
```

`catch $e` binds the **message string** to `$e`. The block closes with `end` (no
`do`; statements use newline or `;` separators). A `catch` clause is
**mandatory**: a bare `try … end` is a parse error.

```mix
try
  print("hi")
end
```
```text
Parse error at line 3:1: unexpected token End
```

### It also catches runtime errors

Not just `die` — any runtime error (an unbound variable, a bad builtin argument)
is caught by the same `catch`:

```mix
try
  print($nope)
catch $e
  print("caught: " .. $e)
end
```
```text
caught: undefined variable '$nope'
```

```mix
try
  sqrt("hello")
catch $e
  print("caught: " .. $e)
end
```
```text
caught: sqrt() expects a number, got string
```

### `$e` is a string, not an object

There is no `$e.message` / `$e.code` — `$e` *is* the message. Use it with `..`
concat, `contains()`, `split()`, or any [string](strings.md) builtin:

```mix
try
  die("disk full: /var")
catch $e
  if contains($e, "disk full") then
    print("handling disk-full path")
  end
end
```
```text
handling disk-full path
```

This holds even when `die` is given a non-string: the argument is **stringified**
at the raise. `die(42)` catches as the *string* `"42"` (`length($e)` is 2), and
`die({code: 7})` catches as the map's repr text — `$e["code"]` fails with *cannot
index string with string*. If you want structured error data, put it *in* the
message yourself (e.g. a JSON string) and `json_parse($e)` in the catch — Mix
won't do it for you.

### `catch $e` is a function-local binder

Inside a function, `catch $e` binds `$e` **locally** — it shadows, and cannot
clobber, a same-named caller/global variable (the same rule as loop vars and
parameters — see [functions](functions.md)):

```mix
$e = "outer"
function risky()
  try
    die("inside")
  catch $e
    print("local e: " .. $e)
  end
end
risky()
print("outer e still: " .. $e)
```
```text
local e: inside
outer e still: outer
```

### `finally` — cleanup that always runs (0.30.0)

A `finally` clause runs on **every** exit path of the `try`: normal completion, a
caught error, and a *propagating* error, `return`, `break`, or `continue`. It is
the right place for cleanup that must happen whether or not the work succeeded
(remove a temp file, unmount, release a lock). `catch` is optional when `finally`
is present — a `try … finally … end` runs the finally body, then lets the error
propagate. At least one of `catch`/`finally` must be present.

```mix
function provision($tmp)
  try
    do_work($tmp)          -- may die
    return "ok"
  finally
    remove($tmp)           -- runs on success, on die, and on the return
  end
end
```

`finally` runs after any `catch` body, on `return`/`break`/`continue` out of the
`try`, on an uncaught error, and while `exit()` unwinds towards the process
boundary. Nested cleanup runs innermost-to-outermost. Only `panic()`, SIGKILL, or
another process-level abort bypasses it.

Control replacement follows one rule: an outcome produced by `finally` overrides
the outcome that entered it. A later/inner `exit(9)` therefore replaces a pending
`exit(2)`. If `finally` raises while an exit is pending, the cleanup error wins and
is catchable by an outer `try`; the exit is not stored as `$err.cause` because an
exit request is control flow, not an error payload. When both outcomes are errors,
the displaced error remains available as `cause` (readable via
`catch $msg, $err` → `$err.cause`), so a cleanup failure never silently hides the
error it replaced.

### Nested try and re-raising

A `catch` body can `die` again; the next enclosing `try` catches it. There is no
`rethrow` keyword — just `die($e)` (or a new message):

```mix
try
  try
    die("inner")
  catch $e
    print("inner caught: " .. $e)
    die("rethrown: " .. $e)
  end
catch $outer
  print("outer caught: " .. $outer)
end
```
```text
inner caught: inner
outer caught: rethrown: inner
```

> **`return` / `break` / `continue` / `exit` are not errors.** They unwind through
> a `try` without being caught. A `return` inside a `try` body returns from the
> enclosing function; `exit()` continues through every enclosing `finally` and is
> consumed only by the process boundary.

## die — raise a catchable error

`die` is a statement that takes an expression (parentheses optional). Uncaught, it
prints the message to **stderr** and exits **1**:

```mix
print("one")
die("stop here")
print("two")
```
```text
one
stop here
exit=1
```

Both call styles work and are equivalent:

```mix
die("with parens")
die "no parens"
```

The message is any expression — build it with `..`:

```mix
$code = 42
die("exit code " .. $code)
```

Use `die` for **must-succeed** steps where you want fail-fast rather than a status
check — and wrap the section in `try` if a caller should recover:

```mix
try
  run("install -m 0755 src dst")   -- run() dies on non-zero exit
  print("installed")
catch $e
  print("install failed: " .. $e)
end
```

## run vs run_rc — the two command-failure styles

This is the most important practical distinction. See [running commands](system.md) for
the full surface; here is the error angle.

**`run(cmd)`** returns stdout as a string and **raises a catchable `die`** on a
non-zero exit — fail-fast:

```mix
try
  run("false")
catch $e
  print("run died: " .. $e)
end
```
```text
run died: run: 'false' failed (rc=1)
```

When the failed command wrote to stderr, `run`'s message appends a stderr tail:
`run: 'ls /no/such/path' failed (rc=2): ls: cannot access '/no/such/path': No
such file or directory` — so the caught `$e` usually already says *why*.

**`run_rc(cmd)`** **never raises** — it returns a map
`{rc, stdout, stderr, timed_out, interrupted}` you branch on. The exit code is
*data*, not `$?` soup:

```mix
$r = run_rc("false")
print("rc=" .. $r.rc)
if $r.rc != 0 then
  print("command failed, continuing")
end
```
```text
rc=1
command failed, continuing
```

`run_rc` also captures stderr, so you can log *why* it failed without aborting:

```mix
$r = run_rc("ls /no/such/path")
print("rc=" .. $r.rc)
print("stderr=" .. trim($r.stderr))
```
```text
rc=2
stderr=ls: cannot access '/no/such/path': No such file or directory
```

### Timeouts, Ctrl-C, and signals

Both `run` and `run_rc` take an optional trailing `{timeout: seconds}` opts map.
The default is `0` = **no deadline** (the historic contract for long builds); a
bounded call is killed at the deadline (own process group, SIGKILL path — it can
never wedge the shell). Each failure mode follows the split above — `run` raises,
`run_rc` reports:

| Event | `run` | `run_rc` |
|---|---|---|
| deadline hit | dies (catchable): `run: 'sleep 5' timed out after 1s` | returns `rc: -1, timed_out: true` |
| Ctrl-C during the child | raises `run: interrupted` | returns `rc: -2, interrupted: true` |
| child killed by a signal | dies: `run: '…' failed (rc=143)` | `rc` = shell-convention **128+sig** (SIGTERM → 143) |

```mix
$r = run_rc("sleep 30", {timeout: 5})
if $r.timed_out then
  print("gave up after 5s")
end
```

Two things to know about the sentinels: the **negative** `rc` values (`-1`
timeout, `-2` interrupt) can never collide with a real child exit (`0..255`,
including `128+sig`); and an **interrupt is a stop request, not a recoverable
error** — after Ctrl-C the interpreter winds the script down and the process
exits cleanly (0), so don't build recovery logic on catching `run: interrupted`
or branching on `.interrupted` (later statements may not run).

The opts map is validated loudly (and catchably): an unknown key raises
`run_rc: unknown opt "bogus" (supported: timeout)`, and a surplus argument raises
`run() expects at most 2 argument(s), got 3` — a misplaced opts map errors instead
of running unbounded.

**Choosing:** loop over many commands and keep going → `run_rc` + `.rc` branch. One
step that *must* succeed before the next → `run` inside a `try` (or let it abort).

```mix
$cmds = ["true", "false", "true"]
for each $c in $cmds
  $r = run_rc($c)
  if $r.rc == 0 then
    print($c .. " ok")
  else
    print($c .. " FAILED rc=" .. $r.rc)
  end
end
```
```text
true ok
false FAILED rc=1
true ok
```

The same split exists for SSH: **`ssh_run`** returns a map (with `.ok`,
`.exit_code`, `.timed_out`, `.interrupted`) and never raises; **`ssh_must`**
returns the stdout string and raises a catchable Mix error on any non-success —
non-zero exit, timeout, interrupt, or signal — with the host, disposition, and
exit code in the message (`ssh_must: <what> on <host> (exit_code=N): <stderr>`).
See [remote execution](remote.md) and `mix what ssh_run` / `mix what ssh_must`.

## exit — stop the process with a code

`exit(code)` requests process termination with that status. `exit()` defaults to
0. It is **not** an error and **not** catchable: execution immediately unwinds
through every active `finally` block, innermost first, then the script, `-c`, stdin,
REPL, or serve entrypoint terminates with the requested code. Ordinary statements
after the call never run:

```mix
print("before exit")
exit(3)
print("never")
```
```text
before exit
exit=3
```

`exit` is for *deliberate* control-flow termination (a CLI tool reporting a status
to its caller). For a *failure* you want a script-level recovery point to see, use
`die` instead — `die` is catchable, `exit` is not.

The same rule applies inside `catch`: `exit()` leaves the catch body, runs that
try's `finally` if present, then keeps unwinding. An `exit()` called by a `finally`
replaces an earlier pending exit; a cleanup error raised by `finally` overrides a
pending exit, as described above.

## panic — the uncatchable abort

```mix
panic("hard abort")   -- aborts via a Rust panic; NOT caught by try
```

`panic` exists for one narrow purpose: it is the hard-abort primitive that the
**Bus `--serve` handler boundary**
(SPEC 18 §3.4 — formal spec in the operator control repo) is designed to *isolate*. In
`mix --serve`, a panic raised inside an `on … end` handler is caught by the
per-handler boundary, its payload sanitized, and the supervisor keeps running — that
survival is the acceptance gate the boundary proves. **Outside a handler it aborts
the process like any Rust panic, and `try` does not catch it** — uncaught, it
prints a raw `thread 'main' panicked at …` trace to stderr (not a Mix error line)
and exits **101**, the Rust panic status. You almost never want `panic` — reach
for `die` (catchable) for ordinary failures. See [serve mode](serve.md).

## Resource caps — runaways become clean errors

The point of the caps is that a pathological script (or a hostile one in an embedded
daemon) returns a **clean error** instead of taking down the process with a native
stack overflow or OOM.

### Recursion depth

The `mix` binary caps function-call recursion at **128** (the library default for
embedders is a more conservative 16). Past it you get a catchable runtime error, not
a SIGSEGV:

```mix
function f($n)
  return f($n + 1)
end
try
  f(0)
catch $e
  print("caught: " .. $e)
end
```
```text
caught: recursion depth exceeded (limit 128)
```

Because it is a *runtime* error, you can catch it — but the right fix is almost
always to rewrite unbounded recursion as a loop or a [HOF](functions.md)
(`reduce`/`map`).

### Parser nesting depth

The parser caps expression/statement nesting at **200**. Over-deep nesting
(`((((…`, deeply nested `if`/maps) is a **parse error** — it happens before any code
runs, so it is *not* catchable:

```mix
print((((( ... 201 levels ... )))))
```
```text
Parse error at line 1:202: nesting too deep (limit 200)
```

Other bounded builtins follow the same philosophy — `repeat`/`lpad`/`rpad` reject a
result over 256 MiB, `range` over 10M elements, `http_*` a body over 64 MiB — each a
normal runtime error rather than an OOM. `substr` clamps an out-of-range
length/offset rather than panicking. The `http_*` builtins also carry a **30-second
total-request deadline by default** (override with a trailing `{timeout: N}` opts
map, `{timeout: 0}` disables) — a stalled server becomes a catchable error, not a
wedged script. See [HTTP](http.md).

## Syntax errors are not catchable

A lexer or parse error is reported before the first statement executes, so wrapping
it in `try` does nothing — the whole source fails to compile:

```mix
try
  $x =
catch $e
  print("caught: " .. $e)
end
```
```text
Parse error at line 2:7: unexpected token Newline
```

This is about **your** source. Bad syntax in a data file someone else wrote is
an ordinary runtime condition, and `load_data` raises it as a catchable
`RuntimeError` naming the path — see [data](data.md). (Before 0.36.1 it leaked
the parse error, so a config typo aborted past the `catch` with exit 1.)

Semicolons are valid executable-Mix statement separators, so compact error
handling is legal too:

```text
$ mix -c 'print("a"); print("b")'
a
b
```

A semicolon is not universally legal punctuation: it must follow a complete
statement at a real statement boundary. `print(1; 2)`, `print((1; 2))`, and
`print(1) &&; print(2)` are parse errors, and nothing runs. See the authoritative
[statement-separator contract](syntax.md).

Classification remains whole-line: `print(1); echo hi` is a Mix parse error,
not a request to execute half Mix and half shell. On a shell-dispatch line,
`echo a; echo b` keeps the shell command-list meaning. See [overview](overview.md).

## Error message anatomy

Uncaught, each error kind prints a recognisable prefix and exits 1:

| Error | Printed form |
|---|---|
| `die` | the message verbatim, e.g. `fatal` |
| runtime | `Runtime error at line N: <msg>` |
| parse | `Parse error at line N:C: <msg>` |
| lexer | `Lexer error at line N:C: <msg>` |

```text
$ mix -c 'die("fatal")'         ; echo $?    →  fatal               1
$ mix -c 'print($x)'            ; echo $?    →  Runtime error …     1
$ mix -c '$x ='                 ; echo $?    →  Parse error …       1
```

When the error comes from a script **file** (rather than `mix -c`), the runtime
form names it: `Runtime error at /path/script.mix:1: undefined variable '$nope'`.

When caught, `$e` holds only the human-readable `<msg>` part (no `Runtime error at
line N:` prefix) — as seen in the examples above (`undefined variable '$nope'`).

## Idioms

**Validate, then fail fast.**
```mix
$path = args()[0]
if is_empty($path) then
  die("usage: tool <path>")
end
```

**Try a recoverable step, fall back.**
```mix
$cfg = nil
try
  $cfg = json_parse(read_file("/etc/app/config.json"))
catch $e
  print("no config (" .. $e .. "), using defaults")
  $cfg = { port: 8080 }
end
```

**Branch on command status instead of catching.**
```mix
if run_rc("systemctl is-active some-service").rc == 0 then
  print("service up")
else
  print("service down")
end
```

**Bound a command that might hang, branch on the outcome.**
```mix
$r = run_rc("curl -s https://example.com/health", {timeout: 10})
if $r.timed_out then
  print("health check hung")
else if $r.rc != 0 then
  print("health check failed rc=" .. $r.rc)
end
```

**Aggregate failures across a batch without aborting.**
```mix
$failed = []
for each $host in ["node1", "node2"]
  $r = ssh_run($host, "uptime")
  if not $r.ok then
    push($failed, $host)
  end
end
if not is_empty($failed) then
  die("hosts unreachable: " .. join($failed, ", "))
end
```

## Gotchas

- **`catch $e` gives a string, not an object** — no `.message`/`.code`. Even `die(42)` / `die($map)` stringify at the raise. Use string ops on `$e`.
- **`try` requires `catch` and/or `finally`** — bare `try … end` is invalid.
- **`run` raises, `run_rc` does not.** Mixing them up is the usual command-handling bug: a `run(...)` whose failure you forgot to wrap aborts the whole script.
- **`run_rc`'s timeout/interrupt sentinels are negative** — `rc: -1` (timed out) and `rc: -2` (interrupted) can never collide with a real child exit; a signal-killed child is a real exit, `128+sig`.
- **Bus `send`/`emit` failures never raise.** A failed `send` sets a **negative `$rc`** (`-1` transport, `-2` timeout, `-3` no broker) and the statement returns nil; a peer error is `$rc >= 10`; `emit` is silently fire-and-forget. Delivery status is data, not an exception — see [Bus messaging](bus.md).
- **Syntax errors can't be caught** — they fail at parse time. Only `die` and runtime errors reach `catch`.
- **`exit` and `panic` are not catchable.** `exit` unwinds `finally` and then ends with your code; `panic` aborts past `finally` with the Rust panic status 101 (only meaningful behind the `--serve` handler boundary).
- **Newline or `;` separates statements** — comments still own the rest of their physical line, so a `;` inside `--`/`#` does not resume execution.
- **`return`/`break`/`continue` propagate through `try`** uncaught — they are control flow, not errors.

## See also

- [running commands](system.md) — `run` / `run_rc` / `run_stream`, the structured-return model
- [remote execution](remote.md) — `ssh_run` / `ssh_must` / `ssh_mix`, the same raise-vs-report split over SSH
- [functions](functions.md) — `return`, recursion, function-local binders (incl. `catch $e`)
- [strings](strings.md) — operating on the caught `$e` message
- [Bus messaging](bus.md) — the `$rc` band contract: send failures are data, never exceptions
- [serve mode](serve.md) — the `--serve` per-handler boundary that isolates a `panic`
- [overview](overview.md) — Mix vs shell, the `mix -c` classifier, statement separation
- [builtins index](builtins.md) — every builtin by category
- `mix what die` · `mix what exit` · `mix what panic` · `mix what run` · `mix what run_rc` — one-line live help
- [Mix on GitHub](https://github.com/markc/cosmix)
