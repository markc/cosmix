# bus — CosMix Agent Bus messaging (send / emit / address / on)

**Bus messaging is what makes Mix more than "a better shell."** `send`, `emit`,
`address`, and `on … end` are *language keywords*, not library calls — Mix talks
to a local message broker (and, over the mesh, to remote brokers) with no SDK,
no client object, no boilerplate. This is the ARexx lineage made native: every
service is a named, addressable port; a Mix one-liner can drive any of them.

> **Most examples on this page need a live broker** (`cosmix-noded`, from the
> [cos repo](https://github.com/markc/cosmix)). Where a broker is running the output
> shown is real, captured from `mix 0.21.2`. Where no broker is present the Bus
> forms **degrade gracefully** (see [No broker](#no-broker-graceful-degradation)) —
> that path is verified separately. Treat the networked examples as illustrative
> of the shape; the language facts (keywords, `$result`/`$rc`, RPC vs
> fire-and-forget, target syntax) are exact.

```mix
send noded noded.ping            -- RPC to the LOCAL broker
print("rc=" .. $rc)
print("" .. $result)
```
```text
rc=0
result={extensions: {core: 1.0, topic: 1.0}, pong: true}
```

---

## The model in one paragraph

A **broker** (`cosmix-noded`) runs on the node. Services register a **name**
(`noded`, `statecache`, `webd`, …). You **`send`** a service a **command** (a
dotted verb like `noded.ping`) with optional **key=value args**; the reply lands
in `$result` and the status code in `$rc`. That's an RPC. **`emit`** is the same
shape but fire-and-forget — no reply, no wait. **`address`** is a block of
implicit sends to one target. **`on … end`** is the other direction: it
*subscribes* a handler that fires when a matching message arrives — the basis of
a Mix [citizen](#serve-mode-mix-as-a-daemon) that *is* an addressable service.

The win over bash/python: there is no equivalent without a client library. In
Mix it is grammar.

---

## `send` — RPC (request/reply)

`send <target> <command> [key=value …]` dispatches a command and **waits for the
reply**. Two result variables are set as a side effect:

| Var | Meaning |
|---|---|
| `$result` | the reply value (a [map](collections.md), list, string, … — field-accessible) |
| `$rc` | numeric status in signed bands: `0` delivered+accepted · `1..9` delivered with a warning (still success) · `>= 10` peer application error (the exact peer rc is kept) · `-1` transport failure · `-2` per-send `timeout=` exceeded · `-3` Bus unavailable (no broker). All negatives are non-fatal. |

```mix
send noded noded.info
print("rc=" .. $rc)
print("" .. $result)
```
```text
rc=0
{node: node1, noded: {binary: cosmix-noded, name: noded, pid: 1342, version: 0.6.10, ...}, schema_version: 1, service_count: 8, uptime_s: 9445, wg_ip: 192.0.2.5}
```

`$result` is structured data — reach into it with `.field` access or `["key"]`
(see [collections](collections.md)):

```mix
send noded noded.ping
if $result.pong then
  print("pong is true")
end
print("core ext: " .. ("" .. $result.extensions.core))
```
```text
pong is true
core ext: 1.0
```

### `send` as an expression

`send` also returns the reply, so you can capture it directly (it still sets
`$result`/`$rc` too):

```mix
$r = send noded noded.ping
print("captured: " .. ("" .. $r))
```
```text
captured: {extensions: {core: 1.0, topic: 1.0}, pong: true}
```

### Args: scalars, `body=`, and numbers

For `send`, bare `key=value` args are serialized as a **JSON body** (RPC
framing) by default — noded/indexd/maild verbs read their args from it — with
two exceptions that select **header routing** (scalar args → Bus headers,
`body=` → the body channel):

- a `body=` key is present (the caller is speaking the headers+body shape), or
- the command is a SPEC-12 namespace-mode property call — `<svc>.props.<op>` (incl. multi-segment ops like `props.audit.watch`) with a `namespace=` arg — which reads args from Bus headers, so a kv-only `send webd webd.props.get namespace=x key=y` works without an explicit `body=""`. (SPEC-07 flat-path reads — `noded.props.get path=…` — carry no `namespace=` and stay JSON-body, as their servers expect. Both triggers apply on the broker path only; the local Unix-port path has no header mode.)

(`emit` header-routes **all** map args — RPC-style JSON body is a `send`/`call`
shape.) Whole-number args serialize as JSON **integers**,
not floats — so `limit=2` arrives as `2`, and a peer field typed `usize`/`i64`
accepts it; a fractional value (`2.5`) stays a float.

A `body=`-bearing `send` **awaits its reply** exactly like a positional or
scalar-header send — the handler's `reply(...)` lands in `$rc`/`$result`. (Only
`emit` is fire-and-forget; the arg shape never changes whether a reply is
collected.)

```mix
send noded noded.ping limit=2 note="hello"
print("rc=" .. $rc)
```
```text
rc=0
```

### Reading `$rc`: ok vs application error vs transport failure

A `$rc` of `>= 10` from a *reachable* broker is an **application** error (e.g.
unknown service, rejected command) — the peer's exact rc is preserved (a peer
`rc=42` stays `42`, never flattened to `10`) and it does **not** mean the mesh is
down. A `$rc` in `1..9` is a delivered-with-warning success:

```mix
send nonesuch some.cmd
print("rc=" .. $rc)
print("result=" .. ("" .. $result))
```
```text
rc=10
result=Service 'nonesuch' not found
```

A **negative** `$rc` is a *local*, non-fatal signal — the send never reached a
peer, so `$result` carries the reason string and the script continues: `-1`
transport failure (a broker was there but the send failed — lost or broken
connection), `-2` a per-send `timeout=` budget exceeded, `-3` Bus unavailable (no
broker was ever present, a bare host). See [timeout](#per-send-timeout) and
[no broker](#no-broker-graceful-degradation).

---

## `emit` — fire-and-forget

`emit <target> <command> [key=value …]` dispatches and returns immediately. No
reply is awaited and **`$rc`/`$result` are not set** by `emit` — it is a
statement that yields `nil`, used when you don't care about (or can't get) an
answer, e.g. publishing to a topic or notifying a service.

```mix
emit noded noded.ping note=hi
-- no $rc is written by emit; do not read it after an emit
print("emitted")
```
```text
emitted
```

Gotchas:

- `emit` is a **statement**, not an expression — `$x = emit …` is a parse error. (`send` *can* be an expression; `emit` cannot.)
- Because `emit` writes neither `$rc` nor `$result`, reading `$rc` right after an `emit` (with no prior `send`) raises *undefined variable `$rc`* — pre-init it or just don't.

---

## `address` — a block of implicit sends to one target

`address <target> … end` opens a block where **each line is an implicit send**
to that target. Drop the `send` keyword inside the block — writing it is a parse
error (the runtime catches the typo deliberately). Each line sets `$rc`/`$result`
in turn, so after the block they hold the **last** send's outcome.

```mix
address noded
  noded.ping
  noded.info
end
print("rc=" .. ("" .. $rc))
```
```text
rc=0
```

`address` is the ergonomic ARexx-style "talk to one port for a while" form. It is
purely a shorthand for repeated `send <same-target> …` lines.

---

## Targets: static dotted `.bus` vs dynamic built

The **command position** of `send`/`emit`/`address` accepts an expression, and so
does the **target**. Two shapes matter:

**1. A bare local service name** — `noded`, `statecache`, `webd`. The broker
routes it on the local node.

```mix
send statecache INFO
print("" .. $result)
```
```text
{description: Mix supervised Bus citizen (SPEC 18 Phase 1 runtime), name: statecache, version: 0.21.2}
```

**2. A static dotted `.bus` address** — `<service>.<node>.bus` addresses a
*remote* node's broker directly, mesh-routed, **no ssh and no string building**.
This literal-address path fires only for a **bare identifier immediately followed
by a dot** (e.g. `noded.node1.bus`); a single bareword, a `$var`, a `(paren expr)`,
a bareword call like `env("X")`, an index, and a `..` concat all stay ordinary
expressions.

```mix
-- illustrative (needs the named node to exist on the mesh):
send noded.node1.bus noded.info
print("" .. $result)
```

Self-addressing works — `send noded.<thisnode>.bus` resolves to the local broker.

**3. A dynamic target from a loop/var** — build the address (or the verb) with
`..` concat. This is the path when the node name isn't a literal:

```mix
$node = "node1"
$target = "noded." .. $node .. ".bus"
-- send $target noded.info   -- illustrative; routes to that node's broker

-- the verb is an expression too:
$verb = "noded" .. ".ping"
send noded $verb
print("rc=" .. ("" .. $rc) .. " result=" .. ("" .. $result))
```
```text
rc=0 result={extensions: {core: 1.0, topic: 1.0}, pong: true}
```

> Edge: if an inline `(expr)` in command position collides with the parser,
> pre-build the verb into a `$var` (as above) and pass the var.

---

## Per-send `timeout=`

`send … timeout=<seconds>` puts a wall-clock budget on the RPC (cooperative
cancellation; the pending-reply slot is freed on expiry). On timeout you get
`$rc = -2` (RC_TIMEOUT) — its **own** numeric band, distinct from `-1` transport
— with a `timeout: …` reason in `$result`. Non-fatal: the script continues.

```mix
-- against a 2s-slow downstream:
send slowsvc do.work timeout=0.5
-- $rc     == -2      (RC_TIMEOUT, numeric)
-- $result == "timeout: send to slowsvc exceeded 0.5s"
```

Rules (all raise a `RuntimeError` if violated): `timeout=` must be a **finite,
positive** number; **absence** means "no timeout" (there is no sentinel value);
`nil` is treated as absent (so `timeout=$t` is fine when `$t` may be unbound);
duplicate `timeout=` is last-wins. The `timeout=` slot is consumed locally and
does **not** appear in the args delivered to the peer.

---

## `port_exists(name)` — is a service registered?

A builtin (not a keyword) that asks the broker whether a named service is in its
registry. Useful as a guard before an RPC.

```mix
print("statecache: " .. ("" .. port_exists("statecache")))
print("ghost:      " .. ("" .. port_exists("ghostservice")))
```
```text
statecache: true
ghost:      false
```

> Note: the broker need not list *itself* in its service registry, so
> `port_exists("noded")` can return `false` even while `send noded noded.ping`
> succeeds — `port_exists` reflects the **registered-services list**, not broker
> reachability.

---

## `on … end` — receiving messages (handlers)

`on <command> [async] … end` registers a **handler** that fires when a matching
Bus message arrives. The handler body uses newline- or `;`-separated statements closed by
`end` — **no `do` keyword**. The `<command>` matches the *inner command* of the
inbound message (the verb the publisher sent), **not** the topic/target name —
check the publisher to know what to match.

```mix
on order.created
  print("got an order: " .. $event.body)
  reply("ack")
end
```

Legacy `done` still closes `on` (with a deprecation warning); prefer `end`.

### The `$event` map

Inside a handler, the inbound message is available as `$event`, a map with four
fields:

| Field | Type | Access |
|---|---|---|
| `$event.command` | string | the inbound verb |
| `$event.headers` | map | scalar headers, e.g. `$event.headers["topic"]` |
| `$event.body` | string | the raw message body |
| `$event.args` | parsed body \| nil | the body pre-parsed as JSON, or `nil` |

```mix
on topic.delivery
  $topic = $event.headers["topic"]
  print("delivery on " .. $topic .. ": " .. $event.body)
end
```

`$event.args` is a convenience view of the body: when the sender used the
JSON-body path — a **positional** `send svc cmd "a" "b"` (which arrives as
`{"_0":"a","_1":"b"}`) or a **map** arg — the body is already-parsed JSON, so
`$event.args["_0"]` / `$event.args.field` save you a `json_parse($event.body)`.
It is symmetric with the already-parsed `$event.headers` map; `$event.body`
stays the raw string regardless.

`$event.args` is `nil` when the body is empty or is **not** JSON — a raw
`body="hi"` text payload reads back as `nil`, so a handler tells "structured
args" from "raw text" and reaches for `$event.body` in the latter case:

```mix
on order.submit
  if $event.args != nil then
    reply("ok, item " .. ("" .. $event.args["_0"]))   -- send order.submit "widget"
  else
    reply(10, "expected structured args, got: " .. $event.body)
  end
end
```

### `reply(...)` — answer a request

From inside an `on` handler servicing a **request** (one expecting a reply), call
`reply(body)` or `reply(rc, body)`:

```mix
on statecache.get
  reply($current_value)         -- rc defaults to 0
end

on do.validate
  if not $event.body then
    reply(10, "missing input")  -- non-zero rc = application error to the caller
  else
    reply(0, "ok")
  end
end
```

`reply` rules: `rc` must be an integer `0..=255`; it may only be called from
inside an `on` handler; it errors loudly if the current event is *not* a request
(a topic delivery has no caller to answer). A dropped reply would block the
requester forever, so every `reply` failure path is a hard error by design.

### `async` handlers (Class C)

A trailing `async` on the handler header marks it **Class C** — the dispatch
*yields* at every `send`/`reply`/`sleep_ms` so concurrent invocations interleave
instead of head-of-line-blocking each other behind a slow downstream call. Plain
(non-`async`) handlers are **Class S**: run-to-completion, one at a time. Use
`async` only when a handler makes a slow/remote downstream `send` and the citizen
serves concurrent callers.

```mix
on aggregate.report async
  send slow.upstream fetch.data          -- yields here; peers can interleave
  reply($result)
end
```

`async` is a *contextual* modifier, not a reserved word — variables/keys named
`async` elsewhere are unaffected. **Synchronous request cycles (A→B→A) are
prohibited and deadlock** — `async` does not legalise them; break a cycle with
fire-and-forget `emit` + a topic reply instead.

### `subscribe` / `unsubscribe` — topic pub/sub

`subscribe("topic.name")` registers interest in a [topic](bus.md) so topic
deliveries reach a matching `on` handler; `unsubscribe("topic.name")` drops it.
Both are builtins that require a live broker — unlike `send`/`emit`, they
**raise** rather than no-op when no broker is reachable (a script must not believe
it subscribed when it didn't).

```mix
subscribe("metrics.cpu")
on metrics.cpu
  print("cpu sample: " .. $event.body)
end
-- … then enter serve mode / the event pump to actually receive them
```

---

## Serve mode — Mix as a daemon (citizen)

`mix --serve <script.mix> [--name <service>]` runs a script as a **first-class,
supervised Bus citizen**: the top-level body executes once as init (open
resources, `subscribe`, register `on` handlers), then the runtime registers the
service name and enters the event pump, dispatching inbound messages to your `on`
handlers. This is the AmigaOS "application with an ARexx port" model — a
long-lived, addressable Mix process that *is* a service. The full lifecycle
contract (registration, reconnect/backoff, handler fault isolation, graceful
`SIGTERM`/`QUIT` shutdown, health properties) is **SPEC 18**.

A live citizen answers the standard verbs like any Rust daemon:

```mix
send statecache HELP
print("" .. $result)
```
```text
[{args: [], description: List all commands this service accepts, name: HELP}, {args: [], description: Service identity and capabilities, name: INFO}, {args: [], description: Graceful shutdown: deregister, then exit 0 (SPEC 18 §3.5), name: QUIT}, {args: [path?], description: Property snapshot at an optional path, name: statecache.props.get}, ...]
```

A minimal state-holder citizen:

```mix
-- statecache.mix — run with: mix --serve statecache.mix
$value = "(none)"

subscribe("config.current")

on config.current               -- a topic delivery updates our state
  $value = $event.body
end

on statecache.get               -- a request reads it back
  reply($value)
end
```

> A *transient* `mix <script>` (not `--serve`) can `send`/`emit` and even
> `subscribe`, but it has no service name, no reconnect, and the process ends when
> the script does — fine for one-shot orchestration drivers, not for a resident
> service.

---

## Orchestration — citizen ↔ citizen

A Mix citizen can `send`/`address` other citizens, so multi-service orchestration
is native: **pipeline** (A→B→C), **scatter-gather** (fan a request to N workers,
merge replies), **supervisor/worker**, and **review-loop** (a proposer and a
critic iterating). A driver that is its own sole caller (a cron/oneshot/CLI
invocation, no registered name) issuing sequential `send`s is correct under the
cooperative loop. The hard rule: keep the synchronous-request graph **acyclic**;
break any cycle with `emit` + topic replies (see [the deadlock note](#async-handlers-class-c)).

```mix
-- scatter-gather sketch (illustrative)
$replies = []
for each $w in ["worker.a", "worker.b", "worker.c"]
  send $w do.task input=$payload
  push($replies, $result)
end
-- merge $replies …
```

---

## No broker — graceful degradation

The handler uses a **lazy probe** with a small state machine, so Bus forms behave
predictably on a host that may or may not have a broker:

- **Never had a broker (`NeverPresent`).** `send` returns `nil` with `$rc = -3` (RC_UNAVAILABLE — "Bus unavailable before delivery"), `emit` is silently dropped, `port_exists` returns `false` — **no raise**, and subsequent Bus forms don't re-probe (no per-call boot cost). The same binary becomes mesh-viable the moment a broker appears.
- **Had a broker, lost the connection (`Lost`).** `send` returns `nil` with `$rc = -1` (RC_TRANSPORT) and `emit` is silently dropped — both **non-fatal**, so a mesh script survives a broker blip instead of crashing. `port_exists` **raises** `mesh unavailable: … (call bus_reconnect() to retry the probe)`.
- **`subscribe`/`unsubscribe`/`reply`** raise in *both* states — these can't be faked; a script must know they didn't happen.
- **`bus_reconnect()`** resets the probe to `Unprobed` so the next Bus form re-dials — the recovery primitive after installing a broker or after a broker bounce.

This is deliberate: `send`/`emit` stay **non-fatal in every state** (a bare host
reads `$rc = -3`, a lost broker `-1`), so a script isn't forced to guard Bus it
may never use — and because the bare-host path no longer fakes `rc=0`, a
`$rc == 0` now strictly means *delivered*. The forms that can't be faked
(`port_exists` once a broker was lost, `subscribe`/`unsubscribe`/`reply` in either
state) still raise, so a genuine outage is heard.

---

## Quick reference

| Form | Direction | Sets `$rc`/`$result`? | Waits? |
|---|---|---|---|
| `send t cmd k=v` | out, RPC | yes | yes (reply) |
| `$r = send t cmd` | out, RPC (expr) | yes | yes |
| `emit t cmd k=v` | out, fire-and-forget | **no** | no |
| `address t … end` | out, block of sends | yes (last line) | yes per line |
| `on cmd … end` | in, handler | — | event pump |
| `on cmd async … end` | in, Class C handler | — | yields on `send` |
| `reply(body)` / `reply(rc, body)` | answer a request | — | no |
| `subscribe(t)` / `unsubscribe(t)` | topic interest | — | yes (raises if no broker) |
| `port_exists(name)` | registry query | — | yes |
| `bus_reconnect()` | reset the probe | — | — |

Sharp edges:

- Command position matches the publisher's **inner verb**, not the topic/target.
- `emit` is a statement only (no `$x = emit …`) and sets **neither** `$rc` nor `$result`.
- Inside `address`, do **not** write `send` (each line is already a send).
- Synchronous request cycles deadlock — keep the graph acyclic.
- `send`/`emit` are **non-fatal in every failure state**: `send` writes a negative `$rc` (`-3` no broker, `-1` lost/transport, `-2` timeout) and returns `nil`, `emit` is silently dropped. `port_exists` raises once a broker was seen and lost; `subscribe`/`unsubscribe`/`reply` raise whenever the broker is unreachable.

## See also

- [strings](strings.md) — `..` concat for building dynamic targets/verbs; `${…}` vs literal `$name`
- [collections](collections.md) — field access into `$result` / `$event` maps
- [functions](functions.md) — handler bodies, lambdas, the pass-in/return/reassign state idiom
- [running commands](system.md) — `run` / `run_rc` / `ssh_run` for non-Bus I/O
- [builtins index](builtins.md) — `port_exists`, `subscribe`, `reply`, `bus_reconnect`, `sleep_ms`
- The [cos repo](https://github.com/markc/cosmix) — `cosmix-noded`, the Bus broker
- The [mix repo](https://github.com/markc/cosmix) — [AGENTS.md](https://github.com/markc/cosmix/blob/main/AGENTS.md) is the agent orientation sheet; this manual is the language reference
- ARexx background — [Wikipedia: ARexx](https://en.wikipedia.org/wiki/ARexx)
- `mix help` · `mix what send` · `mix what emit` · `mix what address` (`on`/`reply` are handler forms `mix what` does not index)
