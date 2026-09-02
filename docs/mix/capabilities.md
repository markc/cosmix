# Capabilities & embedding

Mix is two things at once: a **standalone shell** (where every builtin is allowed and the only limits are the self-protection caps) and an **embeddable interpreter** that daemons like `webd` and `maild` link as a library to run operator-authored scripts in-process. The second use case is why Mix has a **capability model**: a way to declare, per builtin, what *host authority* it exercises, so an embedder can hand a script a sandbox — "you may read the database and call JMAP, but not write files or spawn processes" — and have the evaluator enforce it before each call.

This page covers the `CapabilityClass` taxonomy (the exact enum), the `CategoryAllowList` policy embedders use, how shell syntax is gated, the resource caps that bound any single run, and the in-proc-trusted vs out-of-proc-untrusted embedding model. It is accurate to the Rust source at `cosmix-lib-mix/src/builtins.rs` and `evaluator.rs` as of Mix 0.21.x.

> If you are just **writing scripts for the `mix` CLI**, you never see any of this — the standalone binary allows every class. The capability gate only fires when Mix is *embedded* and the embedder installs a policy. Read on if you author scripts that run inside a daemon, or if you embed Mix yourself.

## The nine capability classes

Every builtin carries exactly one `CapabilityClass`, **declared at its table entry** in `BUILTINS` / `HOFS` (the `builtin_table!` macro) — a single source of truth. Adding a builtin without naming its class is a *compile error*, not a silent `Pure`-default that would open a sandbox hole. (An older hand-maintained `match` failed open; the table-declared form fails closed.)

```rust
// cosmix-lib-mix/src/builtins.rs
pub enum CapabilityClass {
    Pure,     // no host authority: string/number/collection/in-memory ops
    FsRead,   // reads the filesystem
    FsWrite,  // mutates the filesystem
    Network,  // talks to the network
    Process,  // controls other processes
    Env,      // reads host/environment info or stdin
    Db,       // host-injected database seam (mediated)
    Jmap,     // host-injected JMAP seam (mediated)
    Bus,      // host-injected delegated-Bus seam (mediated)
}
```

There are **nine** classes, no more — `Bus` is the newest, added in 0.20.3 for the delegated-Bus seam (below). (Mix has no separate `Random`, `Crypto`, or `Time` capability — `time`, `uuid`, `random_password`, `sleep`, and the hash/base64 builtins are all `Pure`: they compute over their arguments and carry no host authority worth gating.)

| Class | Meaning | Representative builtins |
|---|---|---|
| **Pure** | No host authority — pure functions over their arguments. **Always allowed.** | `length` `upper` `split` `join` `replace` `round` `sqrt` `json_parse` `markdown` `html_escape` `time` `uuid` `random_password` `sleep` `hash_sha256` and all the `map`/`filter`/`reduce` HOFs |
| **FsRead** | Reads the filesystem. | `read_file` `read_file_bytes` `read_lines` `read_json` `read_jsonl` `load_data` `exists` `is_file` `is_dir` `ls` `glob` `walk` `stat` `grep` `line_count` |
| **FsWrite** | Mutates the filesystem. | `write_file` `write_new` `append_file` `mkdir` `flock` `funlock` `chmod` `chown` `sqlopen` `sqlexec` `sqlclose` |
| **Network** | Talks to the network (arbitrary outbound). | `http_get` `http_post` `http_request` `dns_lookup` `ssh_run` `ssh_must` `ssh_mix` |
| **Process** | Controls other processes / process lifecycle. | `run` `run_rc` `run_stream` `spawn` `kill` `process_alive` `chdir` `exit` `panic` |
| **Env** | Reads host/environment info or stdin. | `env` `cwd` `hostname` `pid` `platform` `which` `readline` `read_stdin` |
| **Db** | Host-injected, *mediated* database seam. | `db_query` `db_exec` |
| **Jmap** | Host-injected, *mediated* JMAP seam. | `jmap` `jmap_upload` |
| **Bus** | Host-injected, *mediated* delegated-Bus seam. | `bus_call` |

A few classifications are deliberate and worth knowing:

- **`panic` is `Process`,** not `Pure`. It is a real Rust `panic!`, **uncatchable** by Mix `try`/`catch` — a termination primitive a sandbox must be able to deny, exactly like `exit`. (In `--serve` mode the SPEC 18 §3.4 handler boundary isolates a panicking handler from the rest of the citizen.)
- **`grep` is `FsRead`** even though it operates on an in-memory string — a deliberately conservative classification (the exact class of every sensitive builtin is pinned by `sensitive_builtins_stay_categorized` in `tests/limits.rs`).
- **`chdir` is `Process`** (it mutates the process's working directory), not `FsRead`.

### Why `Db`, `Jmap`, and `Bus` are separate from `FsWrite` / `Network`

You could imagine folding `db_query` into `FsWrite` (it touches a SQLite file) and `jmap` into `Network` (it makes an HTTP call). They are kept distinct on purpose, because each is a **scoped, mediated seam the embedder controls** — not raw authority the script wields itself:

- The **`Db` seam** (`DbHandler`) is a database connection the *embedder opens and hands in*. Granting `Db` lets a CMS handler run `db_query`/`db_exec` against that one store **without** granting raw `FsWrite` (which would let it `write_file` anywhere). The raw file-level SQLite builtins (`sqlopen`/`sqlexec`) stay `FsWrite` — those *are* arbitrary filesystem authority.
- The **`Jmap` seam** (`JmapHandler`) is a single upstream the embedder configured. The script **never names a host**; its reach is only the embedder's one configured endpoint. Granting `Jmap` to a PIM handler lets it talk to *that* mail server without granting `Network` (arbitrary outbound HTTP to anywhere). This is the whole point of the seam — `jmap` is host-agnostic by construction.
- The **`Bus` seam** (`BusCallHandler`, installed via `set_bus_call_handler`) is a **delegated** Bus control-plane channel — deliberately **not** the raw broker (`BusHandler`, the `send`/`emit` keywords, which a sandboxed handler never gets). The embedder bounds **which verbs** are reachable (a per-route exact-verb allowlist) and injects the **delegation envelope** — the authenticated actor, vhost, route — from *trusted request state*. The script calls `bus_call(verb, args)` and names **no host, peer, or actor**: it cannot forge identity or reach an unlisted verb. This is how a sandboxed webd handler calls a maild verb under the requesting user's delegated identity without holding broker authority.

This is the legibility win: an embedder grants the *narrow, mediated* thing (`Db`, `Jmap`, `Bus`) instead of the *broad, raw* thing (`FsWrite`, `Network`, the broker).

### The `bus_call` builtin

`bus_call(verb, args)` returns the verb's reply as a Mix value. Its checks run in a fixed order: the **capability gate fires first** (a policy that doesn't allow `Bus` denies the call before the handler is even consulted), then argument validation — `verb` must be a non-empty string; `args` is an optional map (omitted or `nil` means an empty map); the args must be JSON-encodable, so a function or raw-bytes value anywhere inside is rejected up front (the same footgun guard as `jmap`) — then the handler runs. A handler refusal (a verb not in the route's allowlist, an unreachable broker, an error rc from the peer) raises a **catchable** error, and the reply is bounded by the collection-size caps below.

With no handler installed — the standalone `mix` binary, a plain library embed — all three seam builtins exist but raise a catchable "not available" error:

```mix
try
  bus_call("mail.send", {to: "x"})
catch $e
  print("" .. $e)
end
```
```text
bus_call not available (no bus_call handler registered)
```

(`db_query` and `jmap` behave the same way: `database not available (no db handler registered)`, `jmap not available (no jmap handler registered)`. A seam builtin never silently no-ops.)

### Inspecting a builtin's class

The `capability_category(name)` function returns the class for any name (a non-builtin name returns `Pure` — it has no authority of its own, and the `is_builtin` gate rejects non-builtins before dispatch anyway). The descriptive *doc category* shown by `mix what` / `mix builtins` (string/math/io/system) is a **different axis** — it organises the help output and does not map 1:1 to `CapabilityClass`:

```text
$ mix what env
env: Get environment variable value

$ mix builtins system | head -3
system builtins:
  env             Get environment variable value
  time            Return current Unix timestamp as float
```

`time` and `env` are both in the `system` doc category, but their capability classes differ (`Pure` vs `Env`). The doc category is for humans browsing; the `CapabilityClass` is for the sandbox. The three seam builtins carry their own doc categories (`db`, `jmap`, `bus`) that are deliberately **absent** from the `mix builtins` category list — they are embedder-only surface, so `mix builtins db` reports an unknown category — but `mix what db_query` / `mix what bus_call` still describe them.

## CategoryAllowList — the ready-made policy

An embedder gates a script by installing a `CapabilityPolicy`. The shipped, batteries-included one is `CategoryAllowList`: it **always allows `Pure`**, plus an explicit allow-set of other classes, and **denies everything else**.

```rust
// cosmix-lib-mix/src/builtins.rs
use std::rc::Rc;
use cosmix_lib_mix::{CategoryAllowList, CapabilityClass};

// A CMS handler: may read files + use the mediated DB seam, nothing else.
let policy = CategoryAllowList::new(&[
    CapabilityClass::FsRead,
    CapabilityClass::Db,
]);
evaluator.set_capability_policy(Rc::new(policy));   // stored as Rc<dyn CapabilityPolicy>
```

With that policy installed, the evaluator consults `check_builtin(name)` **once per builtin/HOF dispatch, before the builtin runs**:

```rust
fn check_builtin(&self, name: &str) -> Result<(), String> {
    let class = capability_category(name);
    if class == CapabilityClass::Pure || self.allowed.contains(&class) {
        Ok(())
    } else {
        Err(format!("{name} requires {class:?} capability"))
    }
}
```

So under the policy above, `read_file(...)` and `db_query(...)` run, `upper(...)` runs (Pure is free), but `write_file(...)` fails with `capability denied: write_file requires FsWrite capability` and `http_get(...)` with `capability denied: http_get requires Network capability` — the evaluator prefixes `capability denied:` to whatever reason the policy returns, and raises it as a normal catchable runtime error. A daemon that wants finer control than "by class" — e.g. allow `read_file` but not `glob`, both `FsRead` — writes its own `CapabilityPolicy` and matches exact builtin names.

The trait is intentionally tiny — two methods, one with a default:

```rust
pub trait CapabilityPolicy {
    fn check_builtin(&self, name: &str) -> Result<(), String>;
    fn check_class(&self, class: CapabilityClass) -> Result<(), String> { /* default */ }
}
```

The standalone `mix` CLI installs **no** policy — every class is allowed and the only bounds are the resource caps below. That is correct: the operator running `mix script.mix` already has full authority on the host; gating it would be theatre.

## Gating shell syntax (the `check_class` escape hatch)

Here is the trap an embedder must not fall into: Mix's **shell syntax does not go through the builtin table**. `sh "..."`, the `$(...)` command substitution, `... | cmd` pipes, and a bare shell-dispatch line all spawn `/bin/sh` directly. A policy that only implemented `check_builtin` would gate `run(...)` but let `$(rm -rf /)` straight through — a sandbox escape.

So those constructs are gated through a **second seam, `check_class(CapabilityClass::Process)`**, before the shell runs. The evaluator calls it at every site that reaches process authority without a builtin name: `sh`, the `$()` command substitution, `… | cmd` pipes, a bare shell-dispatch line executed from a `source`d file, and the `export` statement (it mutates the process-global environment that every thread reads). A denial reads `capability denied: <construct>: <reason>` — e.g. `capability denied: sh: Process capability not allowed`. The trait's *default* `check_class` maps each class to a representative builtin (`Process` → `"run"`, `FsRead` → `"read_file"`, `FsWrite` → `"write_file"`, `Network` → `"http_get"`, `Env` → `"env"`, `Db` → `"db_query"`, `Jmap` → `"jmap"`, `Bus` → `"bus_call"`; `Pure` is always allowed) and reuses `check_builtin`, so a policy that only implements `check_builtin` **still gates shell execution correctly** — it can't accidentally leave the hole open. `CategoryAllowList` overrides `check_class` to gate against its allow-set directly:

```rust
fn check_class(&self, class: CapabilityClass) -> Result<(), String> {
    if class == CapabilityClass::Pure || self.allowed.contains(&class) {
        Ok(())
    } else {
        Err(format!("{class:?} capability not allowed"))
    }
}
```

`include` and `source` (which read a file off disk) are likewise gated as `FsRead` at the evaluator, not left ungated. The principle: **every path that reaches host authority is gated**, whether it is a named builtin or a piece of syntax. See [shell dispatch](shell-mode.md) for what `sh`/`$()`/pipes do and [Bus messaging](bus.md) for `send`/`emit`/`on` (gated separately by their own handler seams, not the capability policy).

## Resource caps — the self-protection limits

Capabilities answer *what authority*; resource caps answer *how much*. These are enforced **natively in the tree-walking evaluator** — there is no external sandbox engine, no added dependency. They protect even a naive embedder that never installs a policy, and they protect the standalone `mix` binary from a runaway script taking down the host. Three categories, plus a parser cap and per-builtin output caps.

### Call-recursion depth

Unbounded Mix recursion overflows the **native Rust stack** before any cooperative check can fire — an uncatchable SIGSEGV that takes the whole host process down. So unlike the others this knob is **finite by default** (`DEFAULT_RECURSION_LIMIT = 16`) — it must protect a naive embedder. Each consumer raises it for its own stack: the `mix` binary uses **128** on its ~8 MB main thread.

```mix
function f($n)
  if $n <= 0 then return 0 end
  return f($n - 1)
end
print(f(500))
```
```text
Runtime error at line 3: recursion depth exceeded (limit 128)
```

A clean catchable error, never a crash. An embedder running on a smaller stack (e.g. maild's per-message inbound filter on a tokio worker, where the async call path burns tens of KB per level and overflows around depth ~32) keeps the lower default so it fails *reliably* with this error rather than flakily overflowing.

### Wall-clock time, list / map / string size

`EvalLimits` carries the other knobs, all `None` (uncapped) by default and set via `set_limits`:

```rust
pub struct EvalLimits {
    pub recursion_limit: usize,            // depth knob (above)
    pub time_limit: Option<Duration>,      // wall-clock budget for one run
    pub max_list_len: Option<usize>,       // cap on any single list's length
    pub max_map_len: Option<usize>,        // cap on any single map's entries
    pub max_string_len: Option<usize>,     // cap on any single string's bytes
}
```

The **time budget** is checked at the per-statement poll, so a loop-based runaway is bounded; but a single blocking builtin (`run`, `ssh_run`, `http_*`) **cannot be interrupted mid-syscall** — the deadline fires at the next statement, not inside the syscall. Keep that in mind when budgeting an embedded handler that shells out.

### Parser nesting cap

The parser caps expression/statement nesting at **depth 200**, so deeply-nested input is a clean `ParseError`, not a stack-overflow abort — neither a login-shell line nor an embedded daemon can be crashed by `((((((...`:

```text
$ mix -c 'print(((( ... 210 deep ... ))))'
Parse error at line 1:202: nesting too deep (limit 200)
```

### Per-builtin output caps (always on)

Several builtins reject an oversized result with a normal runtime error rather than OOMing the process — independent of any policy, in the standalone binary too:

```mix
print(repeat("x", 300000000))
```
```text
Runtime error at line 1: repeat() result would exceed 268435456 bytes (256 MiB cap)
```

```mix
$r = range(1, 20000000)
```
```text
Runtime error at line 1: range() would produce 20000000 elements (cap 10000000)
```

- `repeat` / `lpad` / `rpad` (and the `_w` twins): **256 MiB** result cap.
- `range`: **10,000,000** elements.
- `http_get` / `http_post` / `http_request`: **64 MiB** response body (over-cap → `{status: 0, error}`).
- `substr` clamps an out-of-range length/offset rather than panicking.

## The embedding model — trusted in-proc vs untrusted out-of-proc

This is the load-bearing distinction, and the capability gate's honest scope.

> **An in-process capability gate is a *robustness* boundary for trusted scripts, not a *containment* boundary for untrusted code.** A compromise owns the address space the gate runs in. This is stated plainly in the source and in SPEC 18 §6: *"Not a sandbox."*

What that means in practice:

- **In-proc, trusted (the supported embedding).** A daemon links `cosmix-lib-mix` and runs **operator- or agent-authored, reviewed** scripts in its own process — a webd CMS handler, a maild inbound filter, a Mix citizen daemon (SPEC 18). These scripts are the **same trust class as the Rust daemon itself**; they run under a real SPEC-10 UID (a registered serve-mode citizen takes a 600–699 identity UID; an unregistered transient citizen inherits its supervisor's 500–599 daemon UID). The `CategoryAllowList` here is **defence in depth** — it stops a *buggy* trusted script from doing something surprising (a filter that accidentally calls `run`), and it makes the script's authority **legible** ("this handler is FsRead + Db only"). It is not, and is not claimed to be, a wall against a malicious script.

- **Out-of-proc, untrusted (the only safe path for arbitrary code).** Genuinely untrusted or multi-tenant Mix must run in a **separate process** under OS-level isolation (its own UID, namespaces, cgroups, seccomp) — the maild/webd trust-split ADR. SPEC 18 §6 is explicit: *no in-process untrusted eval mode is specified or permitted.* The capability classes still apply inside that worker as defence in depth, but the real boundary is the process and the kernel, not the `CapabilityPolicy`.

And the standing rule for either mode — **WG-trusted ≠ caller-benign**: a Mix citizen reached over the trusted WireGuard mesh must still **validate inbound Bus request fields** before they parameterise a path, an exec, SQL, or an outbound call. Trust-domain membership bounds *who* can reach the handler; it does not make the request *content* safe. External exposure is mediated only — a citizen never binds a public port; it is reached from outside its broker solely through an explicit `webd` route + authz entry, default-deny. See [Bus messaging](bus.md) for the citizen runtime and `send`/`emit`/`on`.

### Choosing what to grant

A short decision guide for an embedder wiring up a `CategoryAllowList`:

- Start from **`Pure` only** (the default allow-set is empty) and add the *narrowest* class that lets the handler do its job.
- Prefer the **mediated seams** (`Db`, `Jmap`) over the **raw** classes (`FsWrite`, `Network`) wherever the handler's real need is "talk to *my* store / *my* upstream", not "arbitrary filesystem / arbitrary outbound".
- Grant **`Process` last and reluctantly** — it implies `run`, `spawn`, `kill`, `exit`, *and* all shell syntax (`sh`/`$()`/pipes). A handler that needs to shell out is a candidate for moving its logic into a builtin or out-of-proc worker.
- Remember `Process` also covers `panic` and `exit` — denying `Process` is what keeps a handler from terminating the host daemon.

## See also

- [shell dispatch](shell-mode.md) — `sh`, `$(...)`, pipes; gated as `Process`
- [Bus messaging](bus.md) — `send`/`emit`/`on`, the citizen runtime, handler seams
- [builtins index](builtins.md) — every builtin and its doc category
- [functions](functions.md) · [strings](strings.md) · [running commands](system.md)
- `mix what NAME` — one-line description of a builtin or keyword
- `mix builtins [CATEGORY]` — list builtins by (doc) category
- `mix help` — the `mix` command surface and CLI flags
- Source of truth: [`builtins.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/builtins.rs) (`CapabilityClass`, `CategoryAllowList`, `capability_category`), [`evaluator.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/evaluator.rs) (`CapabilityPolicy`, `EvalLimits`)
- [AGENTS.md](https://github.com/markc/cosmix/blob/main/AGENTS.md) — the agent orientation sheet (this manual is the language reference)
- The [mix](https://github.com/markc/cosmix) · [bus](https://github.com/markc/cosmix) · [cos](https://github.com/markc/cosmix) repos
