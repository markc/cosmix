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

## First, "substrate"

**The substrate is Cosmix itself** — the agent-operable computing environment,
not any one app on it: the message Bus (the broker `cosmix-noded`), the services
running on it, and the WireGuard mesh that joins nodes into one fabric. The word
is chosen over "OS" or "desktop" on purpose — it is meant to be *legible,
modifiable, and reconstructible* by an AI agent, the ground the system is built
on rather than a feature bolted onto one. **Citizens (below) are the live
services the substrate is made of.**

## What "citizen" means in Cosmix

A **citizen** is a process that is a first-class, named member of the Cosmix
substrate — not a throwaway script but a resident participant on the Bus. A plain
`mix script.mix` run is anonymous and one-shot: nothing can address it and it
exits when the script ends. Turning it into a citizen (`mix --serve`) gives it a
name, a lifetime, and a seat at the table — it becomes something the rest of the
system, and the whole mesh, can talk to. Concretely, a citizen has:

- **Identity** — a registered Bus service name others address it by (`send
  <name> …`). There is no anonymous citizen.
- **Addressability** — any process, agent, or mesh node can reach it by that
  name, from this machine or across the WireGuard mesh.
- **Residency + supervision** — it is long-lived and stays registered. A broker
  (`cosmix-noded`) restart is a transient drop it reconnects and re-registers
  through, not a death.
- **Participation** — it receives verbs (`on`), `reply`s, `send`s, `emit`s, and
  `subscribe`s to topics: a full member of the message fabric, not just a caller.
- **A public surface it did not write** — the runtime injects `HELP`, `INFO`,
  `QUIT`, and `<svc>.props.{get,list,describe}`, so every citizen is
  introspectable and controllable by the same verbs.
- **Trust** — a citizen is a *full-capability, trusted* process (see
  [capabilities](capabilities.md)), not a sandbox for untrusted code.
- **Provenance** — its binary (`cosmix-mix`) and version show in `noded.list`, so
  a fleet of agents can see exactly what is running and where.

In short: a citizen is the Cosmix substrate's unit of **live, addressable,
agent-operable service**. The rest of this page is how you make one and how it
behaves.

> **Most examples here need a live broker** (`cosmix-noded`, from the
> [cos repo](https://github.com/markc/cosmix)). Where one is running, the output
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
on statecache.get desc "Read the cached value back to the caller"
  reply($value)                 -- rc defaults to 0
end

on config.current               -- a topic delivery: update state, no reply
  $value = $event.body
end
```

### Doc-strings — a citizen self-describes at the handler site

`on <cmd> desc "what this verb does" [async]` (mix ≥ 0.87.1; `desc` and
`async` compose in either order) attaches a **doc-string** to the handler.
The runtime surfaces it verbatim as that verb's `description` in the `HELP`
reply, so GUIs, discovery tools, and agents all read the text the author
wrote where the handler lives — no separately-maintained manifest. Handlers
without one show the generic `Author-defined handler`.

`desc` is a contextual marker (like `async`), consumed only when a
**non-empty static** string literal follows it — an empty or whitespace-only
`desc ""` is refused with a parse error rather than rendering a blank HELP
description. The doc is static metadata that never evaluates:
in double quotes only `${…}` interpolates, so a bare `$var` inside a doc
stays literal text, and `desc "${var}"` is a parse error (as that adjacent
pair always was). A body statement that merely *uses* the name `desc` (on
its own line) is untouched. With multiple handlers registered on one
command, the first documented one wins. An older mix **rejects** a
doc-annotated script at parse time — loudly, not with a silent misparse — so
deploy the runtime before the scripts.

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
| `HELP` | L0 | `[{name, description, args}]` — reserved verbs first, then the author's commands (sorted, deduped; `description` is the handler's doc-string when one was written) |
| `INFO` | L0 | the `{name, version, description}` triple |
| `QUIT` | L0 | replies `rc:0`, then triggers the §3.5 graceful shutdown |
| `RELOAD` | L0 | hot-reload (mix ≥ 0.88.0): re-parses the script; `rc:0 {reloading: true}` = accepted and parsed (the swap then commits, or reverts if the new init fails at runtime — poll `lifecycle.generation` to confirm), or `rc:10 {error}` and the citizen is untouched — see below |
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

## Hot-reload — `RELOAD` (load-beside-swap)

`send <svc> RELOAD` (mix ≥ 0.88.0) asks a citizen to re-read its own script
and swap to it **without ever leaving the Bus** — the broker connection and
service registration survive; only the evaluator (handlers + globals) is
replaced. The contract is load-beside-swap, in Quickshell's sense:

1. The runtime **re-reads and parses** the script first. A parse failure
   answers `rc:10 {error}` and the running citizen is completely untouched —
   a broken edit is a refused reload, never a dead service.
2. On a clean parse it answers `rc:0 {reloading: true}`, then builds a
   **fresh evaluator beside the running one** (same wiring: prelude, limits,
   reserved-verb surface) and executes the new top-level.
3. New top-level succeeds → the new evaluator takes over the pump; the old
   one is drained (in-flight async handlers get the shutdown discipline) and
   dropped. It **fails at runtime** → the failure is logged loudly and the
   OLD evaluator resumes, its state intact.

This is what makes it safe for an agent to edit a live citizen: the worst a
bad edit can do is a logged revert. Things to know:

- **State does not carry over.** The new top-level re-initializes its
  globals. State that must survive a reload belongs in the substrate
  (props, statecache) — that is the persistent-state model, not process
  memory.
- **A request in flight across the swap gets a terminal reply, not
  silence.** When the old generation is retired its pending requests are
  answered with a shutdown-flavoured reply (the connection is live, so the
  caller is never left hanging to its own timeout). SIGTERM/Ctrl-C during a
  reload's init still shuts the citizen down cleanly, and a new init body
  that fails after admitting traffic has its spawned handlers cancelled
  before the old generation resumes.
- **The script path is resolved to an absolute path at startup**, so a
  citizen whose init `chdir`s still reloads its own file.
- **`rc:0` means "accepted and parsed", not "this exact revision is now
  serving"** — a runtime failure in the new init reverts, having already
  acknowledged. **`INFO`/`HELP` cannot confirm a swap** (the version is the
  mix version, and a body-only handler edit changes neither). Poll
  **`<svc>.props.get lifecycle.generation`**: it starts at 0 and advances by
  exactly one per committed swap, with `lifecycle.script_loaded_at` stamping
  when the live generation loaded. `lifecycle.uptime_s`/`started_at` keep
  reporting the PROCESS, so a reloaded citizen is not mistaken for a
  crash+restart.
- **The not-yet-committed generation serves live traffic during its init.**
  A top-level `sleep()` dispatches events, so a new init body that runs
  before it commits (or before it fails) can already answer requests — a
  revert un-registers its handlers and cancels its async tasks, but it
  cannot un-answer a request the rejected code already replied to, nor undo
  its substrate side effects (props writes, `subscribe`, file writes,
  spawned externals). "Resumes with state intact" is a guarantee about the
  evaluator, not the substrate: keep a reloadable citizen's init idempotent,
  and do irreversible work in a handler, not at top level.
- The `rc:0` reply races the swap by design: a follow-up sent immediately
  queues at the broker and is answered by whichever evaluator holds the
  pump. To observe which generation went live, poll `lifecycle.generation`
  (above) — not `HELP`/`INFO`, which cannot tell a swap from a revert.
- Topic subscriptions made by the old top-level persist on the shared
  connection; a re-subscribing new top-level may duplicate delivery
  (v1 limitation — avoid reloading citizens that `subscribe`, or make
  subscription idempotent on the handler side).
- Like every reserved verb, an author `on RELOAD` handler is unreachable
  and filtered from `HELP`.

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

## Citizens as adapters — bridging the Bus to anything

A citizen's `on` handlers don't have to *implement* a service — they can
**translate**. An adapter citizen speaks the Bus on one side and some foreign
control surface on the other, and its whole job is to map between them. This is
the AmigaOS ARexx model in full: ARexx let a script receive a message on an
application's port and turn it into whatever that application understood; a Mix
citizen receives a Bus verb and turns it into a D-Bus call, a shell command, an
HTTP request, or a `send` to another node.

The shape is tiny. A KDE Plasma workspace adapter, in full:

```mix
-- desktop-workspace.mix — run: mix --serve desktop-workspace.mix --name desktop
fn kwin(m)
  -- qdbus6 is an external tool, not a builtin — shelling out is correct.
  return trim(run("qdbus6 org.kde.KWin /KWin org.kde.KWin." .. m))
end

-- Multi-segment verbs must be QUOTED in `on` (a bare handler verb takes one dot).
on "desktop.workspace.next"
  kwin("nextDesktop")
  reply(json_encode({ok: true, desktop: kwin("currentDesktop")}))
end

on "desktop.workspace.prev"
  kwin("previousDesktop")
  reply(json_encode({ok: true, desktop: kwin("currentDesktop")}))
end
```

Now `send desktop desktop.workspace.next` from **any** process on the node
switches the desktop. The Bus verb is the request; `run("qdbus6 …")` is the
translation. Why this is more than a convenience:

- **The verb is the stable interface; the adapter is swappable.** Everything
  upstream — a keybinding, an agent, another citizen — only knows the verb
  `desktop.workspace.next`. *How* it is carried out is hidden behind the adapter.
  Swap the host desktop for a compositor that answers the verb natively and the
  adapter simply retires — nothing upstream changes. The vocabulary is portable;
  the adapter absorbs the host difference.
- **It bridges to any host.** The same shape wraps GNOME's D-Bus, a media player,
  `notify-send`, a REST endpoint, `systemctl`, or the tools on another mesh node.
  The Bus presents one clean verb vocabulary; a small Mix adapter per host
  translates it into that host's native language.
- **The whole glue layer is editable Mix, not compiled daemons.** Because an
  adapter is a `--serve` script, you write one in minutes, `mix --check` it
  offline, and reload it without rebuilding or restarting anything else. Reserve
  compiled code for the performance- or kernel-adjacent core; make the adapters,
  launchers, and per-key actions Mix.

Two idioms complete the pattern:

- **Launch things** with `spawn(cmd)` (detached, via `/bin/sh -c`, returns a PID)
  or `run_argv(argv)` (foreground, an argv list). An app launcher is barely a
  file: `on "launch.editor" spawn("my-editor") end`. Run such a citizen in the
  **user's** session — GUI programs need the caller's `WAYLAND_DISPLAY` /
  `DBUS_SESSION_BUS_ADDRESS`, so never launch them from a root daemon; a
  root daemon should `send` the verb and let a session citizen do the launching.
- **Fan out** with topics: a citizen can `subscribe` to an event topic and
  translate each delivery, so one publisher drives many adapters without knowing
  any of them — the ARexx broadcast, mesh-wide.

The result: the Bus becomes a universal remote for the machine — and for the
mesh — with the translation layer written in the same small language you script
everything else in. A verb in; a real-world effect out; the bridge is a handful
of lines of Mix.

## See also

- [Bus messaging](bus.md) — `send` / `emit` / `address` / `on` / `reply`, topic pub/sub, `$result`/`$rc`, the no-broker state machine, `async`/Class C
- [invocation & CLI](invocation.md) — every `mix` entry mode, including the `--serve` flag summary and `--check`
- [capabilities & embedding](capabilities.md) — the trust model; a citizen is a *trusted, full-capability* process, not a sandbox for untrusted code
- [errors](errors.md) — `try … catch`, `die`, and how runaways become clean errors inside a handler
- [functions](functions.md) — handler bodies, lambdas, and the pass-in / return / reassign idiom for threading state
- The [cos repo](https://github.com/markc/cosmix) — `cosmix-noded`, the Bus broker
- The [mix repo](https://github.com/markc/cosmix) — [AGENTS.md](https://github.com/markc/cosmix/blob/main/AGENTS.md) is the agent orientation sheet; this manual is the language reference
- `mix help` · `mix what send` · `mix what emit` · `mix what address` (`mix what` covers the Bus keywords; `on`/`reply` are handler forms it does not index)
