# cosmix-mix

`cosmix-mix` builds the `mix` executable: the interactive shell, script runner,
command dispatcher, linter, and supervised Bus citizen host for Mix programs.
In the `bus <- mix <- cos` dependency chain it occupies the Mix layer: it
depends on Bus protocol, client, logging, build-information, and property
crates, and on the `cosmix-lib-mix` language runtime, but it does not depend on
the Cos daemon layer.

## Synopsis

```text
mix
mix script.mix [args...]
mix -c 'CODE' [args...]
mix -i -c 'CODE' [args...]
mix - [args...]
mix --check script.mix
mix lint [OPTIONS] FILE...
mix --serve script.mix [--name SERVICE] [--no-prelude]
```

The Cargo package produces one binary named `mix`. It does not expose a Rust
library target. References to `cosmix_mix` in its source name the
`cosmix-lib-mix` dependency, not this package.

See [Command-line interface](cli.md) for modes, options, subcommands, files,
and environment variables. See [Serve mode](serve-mode.md) for the supervised
Bus runtime and its reserved verbs.

## What it provides

- An interactive readline shell with history, completion, aliases, job
  control, directory stacks, and a Mix-or-shell input classifier.
- A `.mix` script runner with positional arguments, traceback rendering,
  strict-arity selection, interrupt handling, and an optional standard
  prelude.
- A `-c` mode that applies the same classifier as the REPL, so the input may be
  Mix source or an external command line.
- Shell execution for pipelines, redirections, environment prefixes,
  expansions, command substitution, background jobs, and `&&`, `||`, and `;`
  command lists.
- Reference and introspection commands for help, manuals, builtins, keywords,
  operators, runtime state, and Bus services.
- `mix lint`, a semantic-analysis CLI with human, JSON, and strict-data output.
- Usage statistics stored under the XDG state directory, with JSON rotation
  and optional SQLite queries.
- Bus adapters for transient scripts and supervised `--serve` citizens.
- Runtime-owned `HELP`, `INFO`, `QUIT`, and L1 property verbs for every served
  citizen.

## Execution modes

| Mode | Entry point | Behaviour |
|---|---|---|
| Interactive | `mix` | Starts the REPL, loads the prelude and `~/.mixrc`, and maintains history and jobs. |
| Script | `mix FILE` | Parses and executes a file, then runs its event pump when handlers are registered. |
| Command | `mix -c CODE` | Classifies and runs Mix code, a Mix function command, or an external command line. |
| Configured command | `mix -i -c CODE` | Loads `~/.mixrc` before classification and execution. |
| Standard input | `mix -` | Reads Mix source from standard input; bare piped input does not execute implicitly. |
| Check | `mix --check FILE` | Lexes and parses without evaluating. |
| Lint | `mix lint ...` | Lexes, parses, and performs semantic analysis. |
| Serve | `mix --serve FILE` | Registers a named citizen and runs a permanent supervised Bus event pump. |

The REPL, `-c`, file, and serve evaluators use a recursion limit of 128. Time
and collection limits remain unset. The process runs evaluation on a dedicated
64 MiB stack.

## Module map

| Module | Responsibility |
|---|---|
| `main` | Process entry, option parsing, script execution, syntax checking, and serve lifecycle. |
| `bus` | Transient and supervised implementations of the language runtime's `BusHandler`. |
| `completion` | Readline completion for variables, aliases, Mix words, subcommands, and commands on `PATH`. |
| `cosmix_paths` | Cached resolution of source, configuration, and binary directories. |
| `exec` | Shell tokenisation, expansion, pipelines, redirection, command lists, process launch, and `cd`. |
| `jobs` | REPL background-job tracking and foreground selection. |
| `lint` | `mix lint` argument parsing, report formatting, and analyser invocation. |
| `meta` | Help, manuals, introspection, build, diagnostics, mesh, AI, and orchestration commands. |
| `node_config` | Read-only broker URL resolution from `node.conf.mix`. |
| `repl` | Interactive loop, history, prompt, tracing, directory stack, jobs, and `.mixrc` loading. |
| `serve_runtime` | Reserved Bus verbs and the lifecycle property tree for served citizens. |
| `shell` | REPL and `-c` classification between Mix, functions, builtins, and external commands. |
| `shell_handler` | Per-line shell fallback for files loaded through `source`. |
| `stats_io` | XDG state paths, weekly JSON statistics, SQLite batches, and stats reports. |

These modules are private implementation modules of the binary.

## Bus integration

Normal REPL and script execution use `MixAmpHandler`. It checks a local port
socket first for simple target names, then lazily probes the configured broker.
The handler supports request/reply calls, fire-and-forget emits, port checks,
incoming events, registration, topic subscription, replies, and an explicit
reconnect.

The first failed broker probe marks the process as a host on which the broker
was never present. Calls then report Bus unavailable without repeatedly
probing, emits remain no-ops, and port checks return false. A connection that
was established and later fails is reported as a mesh error. Calling
`bus_reconnect()` resets the transient handler for a fresh probe.

Serve mode uses `MixServeHandler` instead. It keeps one supervised broker
connection and registered identity, reconnects after transient broker loss,
and replays its topic subscriptions. It does not use the local-socket shortcut.

## Cargo surface

The package declares no Cargo features. It ships one binary flavour with Bus
client and citizen support always enabled.

The `cosmix-lib-mix` dependency enables its JSON, regular-expression, TOML,
date/time, URL, cryptography, HTTP, SQLite, Tokio sleep, DKIM, serde, Markdown,
Datastar, and XML capabilities. Other direct dependencies provide:

| Group | Crates |
|---|---|
| Bus | `cosmix-lib-bus`, `cosmix-lib-client`, `cosmix-lib-buildinfo`, `cosmix-lib-props-core`, `cosmix-lib-log` |
| Interactive shell | `rustyline`, `dirs`, `libc` |
| Async runtime | `tokio`, `tokio-util` |
| Data and utilities | `serde`, `serde_json`, `glob`, `indexmap`, `chrono`, `ureq`, `tracing` |
| Tests | `proptest` |

`cosmix-lib-props-core` is built with its `bus` feature and without default
features. `cosmix-lib-client` is built with its `native` feature.

## Build information

The build script emits the repository revision, dirty state, and build time.
Serve mode supplies that provenance when it registers the citizen. `INFO`
reports the `mix` package version because a served script has no separate
binary version.

## Limits

- The package has no embeddable library API; use `cosmix-lib-mix` to embed the
  language.
- The local node configuration reader is deliberately narrow and read-only.
- Serve mode provides property conformance through L1. It does not provide
  runtime-owned property watch, mutation, or world aggregation.
- Serve-mode log filtering is configured through `RUST_LOG` and a restart; the
  package does not include the Cos-side mutable property store.
