# Serving as a Bus citizen

**`mix --serve service.mix` turns a Mix script into a supervised, mesh-registered
Bus daemon.** Write the `on` handlers; the runtime supplies the rest — broker
registration, reconnect with backoff, the standard `HELP`/`INFO`/`QUIT` verbs, a
lifecycle property tree, per-handler fault isolation, and graceful shutdown. A
script with **one** `on <cmd> … end` handler is a complete service.

This is the AmigaOS "application with an ARexx port" model made native: a
long-lived, addressable Mix process that *is* a service. The full normative
contract is **SPEC 18** (the Mix Citizen Runtime); this page is the operational
view — what the flag does, what the runtime injects, and how a citizen behaves.

> **Most examples here need a live broker** (`cosmix-noded`, from the
> [cos repo](https://github.com/markc/cos)). Where one is running, the output
> shown is **real**, captured from a live `statecache` citizen on a dev node.
> Examples that *start* a daemon (the `mix --serve …` invocations) are
> illustrative of the command shape — you can't paste them into a terminal and
> see output without a broker and a unit. The language facts (the `on`/`reply`
> model, `$result`/`$rc`, the reserved verbs, service-name derivation) are exact,
> and the no-broker and parse paths are verified standalone.

For the messaging primitives themselves — `send`, `emit`, `address`, `on`,
`reply`, topic pub/sub, `$result`/`$rc` — see [Bus messaging](bus.md). This page
assumes them and focuses on the **serve runtime**.

---

## What `mix --serve` does

```text
mix --serve <script.mix> [--name <svc>] [--no-prelude]
```

Compared with a plain `mix <script>` run, serve mode differs in three
load-bearing ways:

1. **It registers a service name** with the local broker and stays addressable under it. A plain script run has no name and nothing can `send` to it.
2. **It is supervised.** A `cosmix-noded` restart is a transient drop the citizen reconnects / re-registers / re-subscribes through — not a process death. A plain script's broker connection is one-shot.
3. **The event pump is unconditional and non-terminating.** A resident daemon, not a script with an optional event tail. It exits only on a fatal terminal (the supervised receiver going away for good), `SIGTERM`/Ctrl-C, or the `QUIT` universal.

Serve evaluation is recorded with mode `serve` and the script basename only.
For fleet services, set `MIX_STATS=off` to disable collection and all stats I/O;
see [usage statistics](stats.md).

The lifecycle on start:

```text
read + parse script   →  connect to broker  →  register <svc>
                                            →  run the script top-level ONCE (init)
                                            →  enter the event pump (resident)
```

The top-level body runs **exactly once** as initialization: open resources,
`subscribe` to topics, register `on` handlers. Then the runtime registers the
service name and pumps inbound Bus into your handlers until shutdown.

If the script fails to **lex or parse**, `--serve` logs the error and exits
non-zero before any broker contact — so a syntax check is cheap and offline:

```text
$ mix --check service.mix
service.mix: OK
```

---

## One handler is a whole service

There is no service scaffolding to write — no main loop, no registration call, no
manifest. A file with a single `on` handler, run under `--serve`, is a complete,
addressable citizen:

```mix
-- echo.mix  — run with:  mix --serve echo.mix
on echo.ping
  reply("pong")
end
```

```text
$ mix --serve echo.mix          # registers Bus service "echo"
```

From any other Mix process on the node:

```mix
send echo echo.ping
print("" .. $result)
```

```text
pong
```

That tiny script also answers `HELP`, `INFO`, `QUIT`, and
`echo.props.{get,list,describe}` — none of which appear in the file. The runtime
injects them (see [Reserved verbs](#reserved-verbs--what-the-runtime-injects)).

---

## Service-name derivation

The Bus service name is the `<svc>` the citizen registers under and the target
others `send` to. It comes from `--name`, falling back to the **script's file
stem**:

```text
mix --serve worker.mix                 # service name = "worker"  (stem)
mix --serve worker.mix --name probe    # service name = "probe"   (--name wins)
mix --serve /usr/local/lib/cosmix/statecache.mix   # → "statecache"
```

Two normalization rules apply (from `derive_serve_name` in `main.rs`):

- **A leading `cosmix-` is stripped** from either source. The system user is `cosmix-<svc>` but the Bus namespace uses the bare `<svc>` token, so a script (or `--name`) accidentally named after the POSIX user still yields the canonical Bus identity: `cosmix-statecache.mix` → `statecache`, `--name cosmix-foo` → `foo`. Surrounding whitespace is trimmed.
- **An anonymous citizen is rejected.** If no name can be derived — an empty path, a root path, a dotfile-only stem (`.foo`, whose `file_stem()` is the whole `.`-led name), or a `--name` that resolves empty or to a leading dot — `--serve` is a launch error and exits non-zero. There is no nameless citizen.

```text
$ mix --serve            # no script path
mix: --serve requires a script path
Usage: mix --serve <script> [--name <svc>]

$ mix --serve worker.mix junk      # serve mode takes no positional script args
mix: unexpected argument after --serve script: 'junk'
Usage: mix --serve <script> [--name <svc>]
```

Only `--name <svc>` and `--no-prelude` may follow the script path, in any order; a
daemon has no argv, so positional script args are a usage error.

---

## The handler model

Inbound Bus messages dispatch to matching `on <cmd> … end` handlers. The
`<cmd>` matches the **inner command/verb** the sender used — not the service
name or topic — so a request `send statecache statecache.get` fires
`on statecache.get`. The full grammar (the `$event` map, `reply(...)`,
`async`/Class C, `subscribe`) is in [Bus messaging](bus.md); the essentials for a
citizen:

```mix
on statecache.get               -- a request: read state back to the caller
  reply($value)                 -- rc defaults to 0
end

on config.current               -- a topic delivery: update state, no reply
  $value = $event.body
end
```

### `reply()` answers a request

`reply(body)` or `reply(rc, body)` answers the in-flight request. A non-zero `rc`
(`0..=255`) is an **application error** carried to the caller's `$rc`/`$result` —
distinct from a transport failure. `reply` is only valid inside a handler
servicing a request; calling it for a topic delivery (which has no caller) is a
hard error.

```mix
on do.validate
  if is_empty($event.body) then
    reply(10, "missing input")   -- caller sees $rc = 10
  else
    reply(0, "ok")
  end
end
```

### `quit()` self-terminates the citizen gracefully

`quit()` requests the **same graceful shutdown** as the `QUIT` universal (SPEC 18
§3.5) — deregister from the broker, then exit `0` — but self-initiated from
inside the script rather than driven by an inbound verb. It is the primitive for
a **one-shot / ephemeral citizen**: register, do a job that needs the event pump
(open a dialog, await a topic delivery, drive one interaction), then retire
itself. It takes no meaningful arguments and returns `nil` — like the `QUIT`
universal it has no exit-code channel (graceful serve shutdown is always exit
`0`), so any argument passed is accepted and ignored rather than an error. Use
`exit(n)` when the citizen must return an exact status: it immediately unwinds
the current handler/init body through active `finally` blocks, then enters the
same bounded deregister-and-drain shutdown path before the process exits with
`n`.

```mix
on dialogs.<handle>.state          -- a props.changed delivery
  if $event.args.new == "resolved" then
    $r = send interact "interact.dialog-result" handle=$h owner_token=$t
    -- …use $r…
    quit()                         -- job done: deregister + exit 0
  end
end
```

`quit()` does **not** abort the current handler: it sets a shutdown request that
the event pump observes at the top of its next loop turn, so statements after
`quit()` in the same handler still run to completion. For a synchronous (Class S)
handler or the init body the pump stops as soon as that code returns. An `async`
(Class C) handler runs as a spawned task while the pump is parked waiting for the
next message, so `quit()` there also **wakes the pump immediately** — it does not
wait for another inbound message to arrive. An idle citizen therefore stops
promptly on a Class C `quit()`, not only when the next event happens to come in.
In-flight handlers get the same bounded grace to finish as any graceful stop.
Prefer `quit()` for an ordinary clean citizen shutdown: it lets the current
handler finish its remaining statements and always exits `0`. Use `exit(n)` when
the current execution must stop immediately (apart from `finally`) or a specific
status must reach systemd. Exit requests from both synchronous and spawned async
handlers wake the event pump; the runtime then deregisters and drains before
returning the exact requested code.

### Outbound `send` from a handler uses `$result` / `$rc`

A handler can itself call other citizens. The reply lands in `$result`, the status
in `$rc`, exactly as in any Mix code:

```mix
on aggregate.report
  send dnsd dnsd.stats
  if $rc == 0 then
    reply($result)
  else
    reply(10, "upstream failed")
  end
end
```

If a handler makes a **slow / remote** downstream `send` and the citizen serves
concurrent callers, mark it `async` (Class C) so the dispatch yields at each
`send`/`reply`/`sleep_ms` and other callers interleave instead of
head-of-line-blocking:

```mix
on aggregate.report async
  send slow.upstream fetch.data        -- yields here; peers interleave
  reply($result)
end
```

Plain handlers are **Class S** (run-to-completion, one at a time) — the default,
and correct for fast handlers and sole-caller orchestration drivers. See
[Bus messaging → async handlers](bus.md#async-handlers-class-c) for the Class S vs
Class C rules and the **synchronous-cycle deadlock** prohibition (`async` does not
legalise an A→B→A cycle).

---

## Handler fault isolation (the per-request boundary)

A panic, a `die`, or any uncaught error inside one `on` handler **does not kill
the citizen**. The runtime catches it at the per-request boundary, logs the real
error, sends an error reply to the caller if the inbound message was a request
expecting one, and continues the pump. One malformed request must not deny
service to every other caller — this is a contract, not best-effort.

```mix
on risky.op
  $n = to_number($event.body)   -- a bad body raises here…
  reply("doubled: " .. ("" .. ($n * 2)))
end
-- …the citizen logs it, replies an error to THIS caller, keeps serving others.
```

**What the caller sees vs. what you debug with.** The caller's reply is a
**fixed** `rc=1`, `internal handler error` — the real error message is
deliberately **not** put on the wire (it can carry request data or
Trojan-Source bytes, and a peer in the mesh is not automatically trusted). The
real error — with the command, handler index, and the failing line — is
**logged instead**, and that's where you debug:

- **Interactive** (`mix --serve foo.mix` in a terminal): faults print straight
  to the terminal, e.g. `ERROR … Handler body errored … command=risky.op
  error=Runtime error at line 2: …`.
- **Under systemd** (the citizen has no terminal): the fault goes to journald —
  `journalctl -t cosmix-mix -f` (filter further on the `service = <svc>` field).

You can still handle errors yourself with [`try … catch`](errors.md) to send a
tailored reply; the runtime boundary is the backstop for anything you don't catch.

---

## Reserved verbs — what the runtime injects

Every serve citizen answers a fixed set of verbs the author does **not** write and
**cannot override** (SPEC 18 §7-Q4: *runtime wins*). They are intercepted
*pre-dispatch*, so an author `on HELP …` or `on <svc>.props.get …` handler is
unreachable — and is filtered out of `HELP` rather than advertised as a shadow
that never fires.

| Verb | Level | What it returns |
|---|---|---|
| `HELP` | L0 | `[{name, description, args}]` — reserved verbs first, then the author's commands (sorted, deduped) |
| `INFO` | L0 | the `{name, version, description}` triple |
| `QUIT` | L0 | replies `rc:0`, then triggers the §3.5 graceful shutdown |
| `<svc>.props.get` | L1 | a lifecycle property snapshot (root, or an optional `path=`) |
| `<svc>.props.list` | L1 | all defined property paths |
| `<svc>.props.describe` | L1 | the schema entry for a path |

This is the same Ch07 L0 + L1 daemon conformance a Rust daemon owes — a Mix
citizen is exactly as legible as a compiled one. The props surface reuses the
same `cosmix_props` encoder the Rust daemons use, so a citizen's `props.get`
output is byte-consistent with theirs.

**Live** (captured from a running `statecache` citizen):

```mix
send statecache HELP
print("" .. $result)
```

```text
[{args: [], description: List all commands this service accepts, name: HELP}, {args: [], description: Service identity and capabilities, name: INFO}, {args: [], description: Graceful shutdown: deregister, then exit 0 (SPEC 18 §3.5), name: QUIT}, {args: [path?], description: Property snapshot at an optional path (root if absent), name: statecache.props.get}, {args: [], description: All defined property paths, name: statecache.props.list}, {args: [path], description: Schema entry (type, mutability, sensitivity) for a path, name: statecache.props.describe}, {args: [], description: Author-defined handler, name: statecache.get}, {args: [], description: Author-defined handler, name: world.statecache.probe}]
```

```mix
send statecache INFO
print("" .. $result)
```

```text
{description: Mix supervised Bus citizen (SPEC 18 Phase 1 runtime), name: statecache, version: 0.21.2}
```

> `INFO.version` is the **`mix` runtime version** running the citizen, not a
> version of the script — a citizen's identity is "this mix build plus this
> script." Above it reads `0.18.1` (the deployed serve binary on that node), which
> need not match a newer `mix --version` you may have locally.

### The lifecycle property tree

The L1 `props.*` surface exposes a runtime-owned lifecycle tree. Five leaves,
queryable like any cosmix daemon's properties:

```mix
send statecache statecache.props.get
print("" .. $result)
```

```text
{lifecycle: {health: ok, mode: serving, props_level: L1, started_at: 2026-06-16T07:28:17.231260663+00:00, uptime_s: 10388}}
```

```mix
send statecache statecache.props.list
print("" .. $result)
```

```text
[lifecycle.started_at, lifecycle.uptime_s, lifecycle.mode, lifecycle.health, lifecycle.props_level]
```

A single leaf, by `path=`:

```mix
send statecache statecache.props.get path="lifecycle.uptime_s"
print("uptime_s = " .. ("" .. $result))
```

```text
uptime_s = 10388
```

`props.describe` returns the schema entry — note `uptime_s` is **transient** (it's
recomputed live from the monotonic clock on every `props.get`, never cached):

```mix
send statecache statecache.props.describe path="lifecycle.uptime_s"
print("" .. $result)
```

```text
{description: Seconds since process start., mutable: false, path: lifecycle.uptime_s, sensitive: false, transient: true, type: number}
```

| Leaf | Type | Meaning |
|---|---|---|
| `lifecycle.started_at` | string | RFC 3339 process start time |
| `lifecycle.uptime_s` | number | seconds since start (live, transient) |
| `lifecycle.mode` | string | operating mode — `serving` |
| `lifecycle.health` | string | coarse health — `ok` |
| `lifecycle.props_level` | string | conformance level — `L1` |

> `props.watch` (L2) and `props.set` / `props.delete` (SPEC 12) are **not**
> reserved — an author *may* implement them with ordinary `on` handlers, so they
> fall through to your code rather than being intercepted.

---

## Build provenance — discoverable in `noded.list`

A Mix citizen has no binary of its own; its provenance **is** the `mix` build that
runs it. At registration the runtime sends a `RegisterProvenance` body, so a
`noded.list` query reports which `mix` build runs the citizen — the version-
discovery contract for a fleet of agents asking "what runs where." The body is
built once at process start and re-sent on every reconnect, so `started_at` stays
the true process start.

```mix
send noded noded.list
for each $s in $result
  if $s["name"] == "statecache" then
    print("" .. $s)
  end
end
```

```text
{binary: cosmix-mix, build_time: 2026-06-15T03:06:09Z, git_dirty: false, git_sha: 2877eef19cf5, name: statecache, pid: 1352, registered_at: 2026-06-16T07:28:17Z, schema_version: 1, started_at: 2026-06-16T07:28:17Z, version: 0.21.2}
```

Note `binary: cosmix-mix` — the citizen is named `statecache`, but its provenance
points at the `mix` runtime that hosts it.

---

## Supervision, reconnect, and shutdown

Serve mode wraps the pump in a supervised client, so the citizen behaves like a
proper daemon across broker churn:

- **Reconnect with backoff.** On transport loss the runtime re-enters `connect → register → pump` with bounded exponential backoff + jitter, then **re-registers** the service name and **re-subscribes every topic** the script subscribed to (init-body *and* handler-body — the full set, replayed by the runtime, not the author). Broker-side registration and subscriptions do not survive a transport drop and are not silently assumed to.
- **No outbound queue while disconnected.** An outbound `send` while disconnected fails fast with a typed transport error — an outage is surfaced to the caller, never absorbed behind a buffer.
- **Initial-connect budget is fatal.** If the *first* connect+register budget is exhausted (no reachable broker at start), `--serve` exits non-zero so a misconfigured citizen fails fast under systemd rather than spinning silently.
- **Graceful shutdown.** `SIGTERM` (the systemd stop signal), Ctrl-C, the Ch02 `QUIT` universal, and the self-initiated `quit()` builtin all converge on **one** path: stop accepting new requests, let in-flight handlers a bounded grace to finish, **deregister** the service name (bounded so a wedged broker can't hang exit), then exit `0`. If the grace is exceeded or deregister fails, the process exits non-zero so systemd records an unclean stop and the broker registry doesn't retain a dead name. `QUIT` is **not** a no-op — it drives this exact sequence.

```mix
send statecache QUIT       -- replies rc:0, then the citizen deregisters and exits 0
```

Logs go to **journald** under systemd. When `mix --serve` runs **interactively**
(stderr is a terminal) they also go straight to that terminal, so a foreground
dev run shows startup, faults, and pump activity without a `journalctl` window;
they fall back to stderr too when there's no journal socket at all. Every
serve/supervisor line carries a structured `service = <svc>` field, so the
process name `cosmix-mix` never obscures which citizen logged. Tail a citizen's
logs with `journalctl` filtered on that field.

---

## Running under systemd

A resident citizen is a normal SPEC-10 daemon: a `sysusers.d` `u cosmix-<svc>`
entry in the citizen UID band and a unit whose `ExecStart` runs the script under
`--serve`. The shape is identical to `cosmix-{noded,maild,webd}.service`:

```ini
# /etc/systemd/system/cosmix-statecache.service  (illustrative)
[Unit]
Description=statecache Mix citizen
After=cosmix-noded.service

[Service]
ExecStart=/opt/cosmix/bin/mix --serve /usr/local/lib/cosmix/statecache.mix
User=cosmix-statecache
Group=cosmix-statecache
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The `mix` binary never `setuid`s itself — identity is assigned by systemd via
`User=`/`Group=`. Restart policy, ordering (`After=cosmix-noded.service`), and
resource limits are unit-file concerns; the serve runtime provides only the
in-process behaviour systemd can't (registration, reconnect, handler isolation,
graceful deregister).

---

## A complete state-holder citizen

The canonical Phase-1 reference shape: subscribe one topic, hold the last value as
in-process state, answer a query with it. This single file is a full citizen.

```mix
-- statecache.mix
--   mix --serve statecache.mix          (Bus service name "statecache")
$value = "(none)"

subscribe("config.current")              -- init: register topic interest

on config.current                        -- topic delivery updates state
  $value = $event.body
end

on statecache.get                        -- request reads it back
  reply($value)
end
```

It needs no `HELP`/`INFO`/`QUIT`/`props.*` handler — those are injected. Across a
broker bounce, the runtime re-registers `statecache` *and* re-subscribes
`config.current`, so a value published after the bounce is reflected in the next
`statecache.get`.

> A *transient* `mix statecache.mix` (no `--serve`) would run the init body, hit
> the end of the script with handlers registered, and pump events for that one
> connection — but it has no service name, no supervision, and dies when
> interrupted. Use `--serve` for anything resident.

---

## No broker present

The serve *entry point* needs a broker (the initial-connect budget is fatal), but
the underlying Bus forms degrade predictably on a bare host — relevant when you
develop a citizen script before a broker exists. See
[Bus messaging → no broker](bus.md#no-broker-graceful-degradation) for the full
state machine; in brief:

- **Never had a broker:** `send` returns `nil`, `emit` no-ops, `port_exists` is `false` — no error. The same binary becomes mesh-viable the instant a broker appears, with **no recompile**.
- **Had a broker, lost it:** `send`/`emit`/`port_exists` **raise** `mesh unavailable: …` — an outage is loud, never silently absorbed.
- **`subscribe`/`reply`** raise in both cases (a citizen must not believe it subscribed or replied when it didn't).

So you can `mix --check service.mix` and even dry-run the init logic offline; the
resident `--serve` daemon is what requires the broker to be up.

---

## Quick reference

| Thing | Value |
|---|---|
| Start a citizen | `mix --serve <script> [--name <svc>] [--no-prelude]` |
| Service name | `--name`, else the script file stem; leading `cosmix-` stripped |
| Anonymous serve | a launch error — no nameless citizen |
| Init | top-level body runs **once**, then the pump runs forever |
| Reserved (injected) | `HELP`, `INFO`, `QUIT`, `<svc>.props.{get,list,describe}` |
| Author can override reserved? | **No** — runtime wins (pre-dispatch intercept) |
| Handler fault | caught per-request, logged, error reply, pump continues |
| Slow downstream + concurrent callers | mark the handler `async` (Class C) |
| Sync cycle A→B→A | prohibited — deadlocks; break with `emit` + topic reply |
| Shutdown | `SIGTERM` / Ctrl-C / `QUIT` → deregister → exit 0 |
| Logs | journald, structured `service = <svc>` field (stderr fallback) |
| Provenance | `binary: cosmix-mix` + `mix` version, visible in `noded.list` |
| Normative spec | SPEC 18 (Mix Citizen Runtime) |

## See also

- [Bus messaging](bus.md) — `send` / `emit` / `address` / `on` / `reply`, topic pub/sub, `$result`/`$rc`, the no-broker state machine, `async`/Class C
- [invocation & CLI](invocation.md) — every `mix` entry mode, including the `--serve` flag summary and `--check`
- [capabilities & embedding](capabilities.md) — the trust model; a citizen is a *trusted, full-capability* process, not a sandbox for untrusted code
- [errors](errors.md) — `try … catch`, `die`, and how runaways become clean errors inside a handler
- [functions](functions.md) — handler bodies, lambdas, and the pass-in / return / reassign idiom for threading state
- The [cos repo](https://github.com/markc/cos) — `cosmix-noded`, the Bus broker
- The [mix repo](https://github.com/markc/mix) — [AGENTS.md](https://github.com/markc/mix/blob/main/AGENTS.md) is the agent orientation sheet; this manual is the language reference
- `mix help` · `mix what send` · `mix what emit` · `mix what address` (`mix what` covers the Bus keywords; `on`/`reply` are handler forms it does not index)
