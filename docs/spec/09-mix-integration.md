---
title: Mix integration and supervised citizens
chapter: 9
version: 0.1.1
status: draft
date: 2026-09-05
---

# Mix integration and supervised citizens

## Language authority and execution modes

**MIX-001:** Mix grammar and builtin semantics belong to the shipped manual
and interpreter. This suite specifies integration requirements. Consult
`mix --help`, `mix man overview`, `mix man syntax`, `mix man bus`,
`mix builtins --json`, `mix what NAME` and `mix apropos TERM`; verify uncertain
behaviour with a bounded probe of the actual installed version. An installed
binary's behaviour is evidence of that build, not authority to silently amend
a protocol requirement. Use `mix --check SCRIPT` and `mix lint SCRIPT` for
non-executing validation before running a script.

**MIX-002:** Distinguish transient clients (`mix script.mix`) from registered
citizens (`mix --serve script.mix --name service`). A transient client need
not register and can still use broker topics. It has no named request service
unless explicitly registered. Serve mode requires a valid service name,
initialisation, supervised registration and a resident event pump.
Do not treat `--serve` as an untrusted-code sandbox.

## Calls, events and payloads

**MIX-003:** `send target command ...` performs RPC and sets numeric `$rc`
and `$result`; `emit target command ...` is fire-and-forget and sets neither.
An emitted command is not automatically a topic publication. Use the
documented `publish`/`subscribe` primitives or explicit `topic.*` commands.
`address` supplies a block's target; it does not license inventing command
aliases. Use fully documented wire command names. The examined evaluator and
Bus adapter pass the command expression through; the old blanket claim that
Mix prefixes every shorthand command is not an established contract.

```mix
send "noded" "noded.ping" timeout=2
if $rc != 0 then
  print($result)
end
```

**MIX-004:** Preserve status distinctions: 0 success, 1–9 warning, ≥10 peer
application error, -1 transport failure, -2 local per-send timeout, -3 Bus
unavailable before delivery. Peer error codes remain intact. The old timeout
example using string `"-1"` is superseded. `timeout=` must be finite and
positive, is consumed locally and is not a command argument. No local timeout
argument does not remove an underlying client's transport deadline. A timeout
does not cancel a remote side effect; retry only under a declared idempotency
contract.

`send` and `emit` have intentionally non-fatal transport-failure behaviour
in the evaluator; callers requiring delivery must inspect RPC status.
`subscribe`, `unsubscribe` and `reply` surface errors when their required
broker operation cannot happen. Ordinary transient clients distinguish
never-present and lost brokers; `bus_reconnect()` reopens discovery.

**MIX-005:** Validate incoming values before constructing paths, process argv,
SQL statements or outbound commands. Prefer `run_argv`/`run_argv_must`,
`ssh_exec`/`ssh_mix`, and bound SQLite parameters to string-built shell or SQL.
The wire envelope, Mix dynamic values and Rust service-owned types are
separate validation boundaries. Existing validated constructors and Serde
conversions SHOULD be used before introducing a new generic validation trait.

The adapter supports JSON-body RPC and explicit header/body routing. A `body`
map key selects header routing; property operations carrying `namespace` also
need their contract's header form. See the adapter and maintained manual for
exact conversion rules. Bodies may contain structured JSON or text.

## Registration and reconnect

**MIX-006:** A supervised citizen MUST register its allocated Bus identity
and preserve process-start provenance across reconnection. Identity and OS
permissions are supplied by its supervisor, not by self-`setuid` in Mix.
Registered citizens use the system's citizen identity policy; transient jobs
inherit their supervisor's authorised identity. UID allocation and reuse
belong to the identity chapter, not this runtime.

**MIX-007:** On transport loss, outbound RPC MUST fail visibly without an
offline replay queue. Reconnect MUST re-register and replay the complete
successful subscription registry, including subscriptions created inside
handlers. Registry changes occur only after successful subscribe/unsubscribe
acknowledgement. Partial replay MUST keep the client disconnected and retry;
socket recovery alone is not readiness. The script owns recovery of derived
state and in-flight workflows and MUST be able to observe a reconnect.

Source profile: the shared supervised client implements five initial attempts,
then an unbounded resident reconnect loop with full jitter, a 250 ms base and
30-second backoff ceiling. It publishes connection state/generation, preserves
the outward incoming lane and replays subscriptions in recorded order. A
bounded delay is not a bounded total reconnect lifetime. The old assertion
that reconnect does not exist is obsolete; adoption and live behaviour still
need to be verified for each citizen.

## Handler isolation, concurrency and shutdown

**MIX-008:** Plain `on command ... end` handlers run to completion serially.
An explicit `on command async ... end` permits cooperative interleaving at
await points such as `send`, `reply` and `sleep_ms`. An invocation retains its
own suspended frame. Authors MUST review shared-state invariants across
awaits; `async` does not make synchronous blocking work non-blocking.

**MIX-009:** Synchronous request cycles (A → B → A) MUST NOT be deployed.
The broker does not detect them; timeouts can bound waiting but do not make
the cycle correct. Use acyclic request graphs or explicit asynchronous
events and correlated replies. A registered service with slow downstream
calls needs the async concurrency path even if it currently has only one
caller. A transient sequential orchestration driver has no such shared-service
claim. Pipeline, scatter/gather and review workflows remain application policy.

**MIX-010:** A handler error or recoverable panic MUST be isolated at the
invocation boundary, reflected in health/logging and answered when a request
expects a reply. Process aborts and external termination are outside panic
recovery. `reply(body)` or `reply(rc, body)` requires a request context and an
integer code 0–255; a topic delivery does not acquire an RPC caller.

**MIX-011:** `QUIT` and SIGTERM MUST use the same bounded drain and deregister
path. Stop accepting new work, resolve or terminate outstanding invocations
within the shutdown budget, deregister and close the client. A graceful exit
returns success; an exceeded grace period must remain observable. Restart
policy, child lifetime, resource limits and journal capture belong to the
supervisor. Validate child parent-death behaviour where scripts spawn work.

**MIX-012:** Serve mode MUST reserve runtime `HELP`, `INFO`, `QUIT` and
`<service>.props.get/list/describe` ahead of author handlers. The source
implements lifecycle `started_at`, `uptime_s`, `mode`, `health`, `props_level`,
`handler_faults`, `last_fault`, and reports L1. A caught handler fault degrades
health. This does not establish L2 watch or L3 world publication. Readiness
notification is conditional on implementation and unit type; do not configure
`Type=notify` without verifying the ready signal.

## External exposure and application integration

**MIX-013 (intended, gated):** Externally exposed citizens MUST use an explicit
gateway route and default-deny command authorisation. Gateway duties include
TLS, host/path/header validation, bounded request bodies, rate controls and
response translation. Citizen code owns business logic. No external citizen
route may rely solely on a gateway-supplied string claiming a principal.

A protected authenticated principal distinct from transport origin, its
broker injection mechanism and route/authz configuration remain cross-cutting
design dependencies. `broker_origin: local` proves the gateway's local
delivery class, not that its remote caller is trusted. These dependencies
must be resolved and tested before enabling the proposed external citizen
surface. This chapter does not claim that current generic web proxy behaviour
already implements that design.

MCP use remains outbound capability use under the citizen's identity; it does
not create a second trust regime. Native desktop applications expose semantic
Bus commands; the old `ui.window`/`ui.data` rendering citizen model is retired.

## Evidence and acceptance

Source: [Bus manual](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/docs/mix/bus.md),
[adapter](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-mix/src/bus.rs),
[runtime introspection](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-mix/src/serve_runtime.rs),
[supervisor](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-client/src/supervised.rs),
[evaluator](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-mix/src/evaluator.rs).

Acceptance uses an isolated broker and a state-holding reference citizen:
prove registration, initial and handler-added subscription replay after broker
restart, disconnected failures, post-reconnect state refresh, fault isolation,
runtime command precedence, async fast-call interleaving, plain-handler
serialisation, numeric timeout status, and both graceful shutdown entry points.
System identity and child lifetime need a disposable systemd environment.
External routes additionally need forged-principal and missing-authorisation
tests. These are required evidence, not tests reported as run by this audit.
