# Mix — overview

**Mix** is an [ARexx](https://en.wikipedia.org/wiki/ARexx)-inspired, Bus-native
shell and scripting language, implemented in pure Rust. It is the orchestration
control surface for the [cosmix](https://github.com/markc/cosmix) substrate: one
language that covers one-off glue, daily shell work, and distributed mesh
coordination — with network messaging built into the grammar rather than bolted
on with external clients.

This page is the orientation. It says what Mix is, why it exists, where it came
from, and then hands you off to the rest of the manual.

> **One-line mental model.** Mix is a Bus-native shell where every variable is
> `$`-sigil, concatenation is `..`, statements use newline or `;` separators,
> blocks close with `end`, and command results come back as **structured maps —
> not string soup**. It is not "a better bash"; network messaging
> (`send` / `emit` / `on … end`) is part of the language.

## A taste

```mix
$name = "world"
print("hello, " .. $name)        -- concat is .. (never + or .)
print("hello, ${name}")          -- ${...} interpolates a var; bare $name is literal

-- square a list with a terse lambda (no return, no end)
print(map([1, 2, 3], fn($x) = $x * $x))

-- talk to a mesh service in one line — no SDK, no socket wiring
send noded noded.ping             -- assumes a local Bus broker is present
print("ping rc=" .. $rc)
```

```text
hello, world
hello, world
[1, 4, 9]
ping rc=0
```

> `${...}` performs a **variable** lookup (scope → env → nil), not arbitrary
> expression evaluation — `${name}` works, `${env("USER")}` does not. For a
> function result, concatenate: `"user: " .. env("USER")`. The `send` line does a
> real RPC (filling `$result`/`$rc`) on a host with a Bus broker; on a
> broker-less host it is still non-fatal — `$rc` reads `-3` ("Bus unavailable")
> and the script continues. See [strings](strings.md) and
> [Bus messaging](bus.md).

That last block is the differentiator: `send` is a keyword. On a host with a Bus
broker it does a real RPC and fills `$result` / `$rc` — and `$rc` is always a
**number** with signed bands: `0` delivered and accepted, `1..9` delivered with a
warning, `>= 10` a broker/app error, negative a local failure. On a host without
a broker the same script still runs — `send` is non-fatal (`$rc == -3`), `emit`
is silently dropped — and it lights up unchanged when a broker appears (more in
[Bus messaging](bus.md)).

## What Mix is

- **A Bus-native shell + scripting language.** `send`, `emit`, `address`, and `on … end` are language keywords that reach peer services over the [Bus](https://github.com/markc/cosmix) mesh. No client library, no boilerplate — remote orchestration reads like a local script. See [Bus messaging](bus.md).
- **A real scripting language.** First-class functions and terse lambdas, higher-order builtins (`map` / `filter` / `reduce` / `sort_by` / …), structured values (string, number, bool, bytes, list, map), `try`/`catch`, closures. See [functions](functions.md) and the [builtins index](builtins.md).
- **A shell.** It runs external commands, has a REPL with meta-commands (`mix status`, `mix help`, `mix what NAME`), and serves as a daily-driver login shell (`chsh -s /opt/cosmix/bin/mix`). Command results are **structured** — `run_rc("cmd")` returns `{rc, stdout, stderr, timed_out, interrupted}`, not exit-code soup. See [running commands](system.md), the [shell](shell-mode.md) page, and [the `mix` meta-command](cli.md).
- **A structured-data format.** Strict-data `.*.mix` files (`.conf.mix`, `.spec.mix`, …) are parsed without executing anything — the substrate's config/state format. See [data & serialization](data.md).
- **A Bus citizen.** `mix --serve service.mix` runs a script as a supervised, mesh-registered daemon. A script with one `on` handler is a complete service. See [serving as a citizen](serve.md).

Mix is **pure Rust** — no Lua, no embedded VM from another stack. Lexer → parser
→ evaluator all live in the `cosmix-lib-mix` crate.

## Why it exists

Controlling systems across many machines normally means stitching together a
stack of separate tools: shell scripts, SSH, Ansible, REST/gRPC clients, message
queues, and hand-rolled networking glue. Each layer is its own wiring, its own
failure mode, its own mental overhead.

Mix collapses that stack. Network communication is a *core part of the language*,
so coordinating remote machines and services feels like writing a plain local
script:

- **Built-in networking.** `send` / `address` / `emit` / `on` are keywords, not library calls — one line reaches a peer over the mesh.
- **Less boilerplate.** No repeated wiring for sockets, retries, serialization, queues, or service-to-service messaging.
- **Structured returns.** Command and RPC results are maps you index (`$r.rc`, `$result.pong`), not strings you re-parse with `awk`.
- **Rust-based safety.** Memory safety and safer concurrency than the older automation tools it replaces.

There is a second, deeper reason. Mix is the **agent-operable control surface**
for an AI-first computing substrate: a language legible enough that an AI agent
can read a system's state as structured data, modify it through structured
channels, and drive the whole mesh from a script. The same property that makes
remote orchestration terse for a human makes it tractable for an agent.

## Where it came from

The lineage is **ARexx** (1987, AmigaOS): every application exposed a named,
addressable message port, and one REXX script could orchestrate across all of
them. Mix updates that model for a WireGuard mesh and Rust-native daemons —
every service is a Bus-addressable port, one Mix script orchestrates across the
mesh.

Mix was created inside the **cosmix** project as its scripting language and
shell. An earlier sketch built the same role on Lua (via `mlua`); that approach
was **deprecated and fully removed** once the pure-Rust interpreter reached
parity. Mix now occupies that role exclusively across the cosmix stack.

It ships as two crates that version in lockstep (a third workspace member,
`mix-bench`, is a development-only benchmark harness):

| Crate | What it is |
|---|---|
| **`cosmix-lib-mix`** | The interpreter library — lexer, parser, evaluator, builtins, value model. Pure Rust, no internal deps; feature-gated capabilities (`json`, `regex`, `toml`, `datetime`, `url`, `crypto`, `http`, `sqlite`, `dkim`, `markdown`, `datastar`). |
| **`cosmix-mix`** | The `mix` binary — REPL, shell layer, Bus wiring, the `--serve` supervised-citizen runtime, and the `mix man`/`help`/`what` meta-commands. |

A standalone `mix` install runs as a normal scripting shell. When a Bus broker
(`cosmix-noded`, from the [cos](https://github.com/markc/cosmix) repo) later appears
on the host, the **same binary** becomes mesh-viable on next invocation — no
reinstall, no recompile, no script edits. A runtime lazy-probe decides
bare-vs-mesh at execution time.

## How to run it

```sh
mix                          # interactive REPL
mix script.mix arg1 arg2     # run a file; args land in $1, $2, args()
mix -c '<source>'            # one-liner (`;` or real newlines separate statements)
mix - < script.mix           # read a script from stdin (clean over ssh)
mix --check script.mix       # parse without executing (syntax check)
mix --serve service.mix      # run as a supervised Bus citizen
```

```sh
mix -c 'print("hi, " .. env("USER"))'
```

```text
hi, user
```

A minimal `service.mix` that *is* a complete mesh service:

```mix
on noded.ping
  reply("pong")
end
```

See [invocation & CLI](invocation.md) for every mode and flag, and
[serving as a citizen](serve.md) for the `--serve` runtime.

## The five things to get right first

Mix has near-zero presence in model training data, so anyone (human or agent)
extrapolating from bash/Python trips on the same five edges. Get these right and
most of the surprises disappear:

1. **Every variable is `$`-sigil.** `$x = 1`, not `x = 1` — a bare name is misread as a shell command. See [variables & scope](variables.md).
2. **Concatenation is `..`**, never `+` or `.`: `"a" .. $x .. "b"`. (`+` *adds* when both sides look numeric — `"5" + "5"` is `10`, not `"55"` — and `.` is field access.)
3. **Statements use newline or `;`.** Prefer newlines in files; use `;` for compact `mix -c` / generated source. On shell-dispatch lines the same glyph remains the shell command-list operator; classification is whole-line, never mixed. See [syntax](syntax.md).
4. **`${...}` interpolates, bare `$name` is literal** inside double quotes; `'single quotes'` are fully raw; `$(...)` is literal in a Mix string. See [strings](strings.md).
5. **`run` / `run_rc` spawn `/bin/sh` with a minimal `PATH`** — inside scripts, call binaries by full path (`/opt/cosmix/bin/mix`, never bare `mix`). See [running commands](system.md).

Close behind those five: blocks close with `end` (there is no `do` keyword);
`push()` mutates in place and returns `nil`; and `pos()` is 1-based (0 = not
found) while arrays and `substr` are 0-based. See
[control flow](control-flow.md) and [collections](collections.md).

```mix
$xs = [10, 20]
push($xs, 30)              -- statement form: push mutates, returns nil
print($xs)                 -- [10, 20, 30]   (NOT $xs = push(...) — that nils $xs)
print(length("café"))      -- 4   (codepoints, not bytes)
```

```text
[10, 20, 30]
4
```

## Map of the manual

A guided tour — read in roughly this order, or jump straight to what you need.

**Language core**

- [syntax & the classifier](syntax.md) — tokens, the newline rule, comments, and when a line runs as Mix versus dispatches to the shell.
- [variables & scope](variables.md) — the `$` sigil, `$1`/`$N` args, `$rc`, `$result`, `$event`, scope rules, function-local binding.
- [strings](strings.md) — `'raw'` vs `"${...}"`, `..` concat, codepoint vs byte vs grapheme ops, unicode escapes, padding/wrapping.
- [numbers](numbers.md) — the f64 model, `0o`/`0x`/`0b` radix literals, integer-clean printing, numeric coercion.
- [operators](operators.md) — `..`, arithmetic, comparison/ordering, `and`/`or`/ `not`, the `? :` ternary, `??` nil-coalesce, `&&`/`||` statement chaining.
- [control flow](control-flow.md) — `if`/`else`, `for $x in` (a.k.a. `for each`), `while`, `select`, the `end` rule, if-as-expression, inline single-statement bodies.
- [functions](functions.md) — `function`/`fn`, the terse `fn($x) = expr` lambda, first-class values, closures, the pass-in/return/reassign triad, modules.
- [collections](collections.md) — lists & maps, indexing (mixed base!), negative indices, `push`/`keys`/`merge`, slicing.
- [higher-order functions](hof.md) — `map`/`filter`/`reduce`/`sort_by`/ `group_by`/…, the lambda-passing rules.
- [errors](errors.md) — `try`/`catch`, `die`, `exit`, the uncatchable `panic`, how command failures surface.

**Builtins & I/O**

- [builtins index](builtins.md) — the categorized reference (`mix help`).
- [math](math.md) — numeric builtins: rounding, powers/roots, logs, `min`/`max`/ `clamp`, trig, `pi()`/`e()`.
- [running commands](system.md) — `run` / `run_rc` / `run_stream`, structured returns, wall-clock timeouts, the shell-dispatch vs Mix-statement split, `$(...)` and brace expansion.
- [files & I/O](io.md) — `read_file`/`write_file`, `glob`, `stat`/`chmod`/ `chown`, path helpers.
- [data & serialization](data.md) — `json_parse`/`jq`, `data_encode`, TOML, strict-data `.*.mix`.
- [regular expressions](regex.md) — `regex_match`/`find`/`replace`/`split`.
- [dates & time](datetime.md) — timestamps as plain numbers, formatting, parsing, durations.
- [http](http.md) — `http_get`/`http_post`/`http_request`, the 30-second default deadline.
- [datastar](datastar.md) — the `ds_*` Datastar SSE event-framing builtins.

**Distributed & mesh**

- [Bus messaging](bus.md) — `send` / `emit` / `address` / `on … end`, `$result`, the `$rc` signed bands, static `.bus` addressing, graceful no-broker degradation.
- [remote / SSH](remote.md) — the headline `ssh_mix` + heredoc idiom for remote
  Mix source, plus `ssh_run` / `ssh_must`, structured returns, env transports,
  and the mix-as-login-shell path.
- [serving as a citizen](serve.md) — `mix --serve`, the supervised runtime, writing a mesh service.
- [capabilities & embedding](capabilities.md) — the capability classes and the sandbox model daemons embed Mix with.

**Tooling**

- [invocation & CLI](invocation.md) — every run mode and flag.
- [the mix meta-command](cli.md) — `mix help`/`man`/`builtins`/`what`/`status`/ `trace`/… from the REPL or the OS shell.
- [shell](shell-mode.md) — shell-dispatch mode: pipes, `&&`/`||`, brace expansion, `$(...)`, redirects; the daily-driver login shell and `~/.mixrc`.
- [keywords](keywords.md) — the reserved words, keywords-as-names.

When this manual and the live binary disagree, **the binary wins** — probe it
with `mix -c '<code>'` and trust what it prints. The full topic list lives in
[the manual index](README.md).

## Related projects

- **[bus](https://github.com/markc/cosmix)** — the CosMix Agent Bus (the Bus) library family Mix consumes for `send` / `call` / topic subscription.
- **[cos](https://github.com/markc/cosmix)** — the cosmix daemon family: ships `cosmix-noded` (the broker `--serve` connects to) plus mail, web, DNS, the knowledge indexer, and the display compositor.
- **[Datastar](https://data-star.dev)** — the hypermedia framework the `ds_*` builtins frame SSE events for (server-side reactive UI from Mix).
- **[ARexx background](https://en.wikipedia.org/wiki/ARexx)** — the message-port scripting model Mix descends from.

## See also

```
mix man               this manual's index (README.md)
mix man TOPIC         read a manual page in the terminal
mix help              the full categorized builtin reference
mix builtins [CAT]    list builtins, optionally by category
mix what NAME         one-line description of a builtin or keyword
mix keywords          list the reserved words
```

Pages: [the manual index](README.md) · [invocation](invocation.md) ·
[cli](cli.md) · [syntax](syntax.md) · [variables](variables.md) ·
[strings](strings.md) · [numbers](numbers.md) · [operators](operators.md) ·
[control flow](control-flow.md) · [functions](functions.md) ·
[collections](collections.md) · [hof](hof.md) · [errors](errors.md) ·
[math](math.md) · [io](io.md) · [running commands](system.md) ·
[data](data.md) · [regex](regex.md) · [datetime](datetime.md) ·
[http](http.md) · [datastar](datastar.md) · [Bus messaging](bus.md) ·
[remote / SSH](remote.md) · [serving as a citizen](serve.md) ·
[capabilities](capabilities.md) · [shell](shell-mode.md) ·
[keywords](keywords.md) · [builtins index](builtins.md)
