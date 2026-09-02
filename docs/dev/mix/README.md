# mix

**Mix** is a lightweight orchestration language for controlling distributed systems — a pure-Rust, ARexx-inspired scripting language and interactive shell with first-class network messaging built into the grammar.

## Why Mix

Controlling systems across many machines normally means stitching together a stack of separate tools: shell scripts, SSH, Ansible, REST or gRPC clients, message queues, and hand-rolled networking code. Each layer is its own glue, its own failure mode, its own mental overhead.

Mix collapses that stack. It makes network communication a core part of the language rather than something bolted on with external tools, so coordinating remote machines and services feels like writing a plain local script.

- **Built-in networking.** Communication between machines is a language feature, not an add-on. `send`, `address`, `emit`, and `on … end` are keywords — not library calls. A Mix script reaches a peer service over the [Bus](https://github.com/markc/cosmix) mesh in one line, with no SDK boilerplate.
- **Less boilerplate.** No repeated wiring for sockets, retries, serialization, queues, or service-to-service messaging — Mix handles that underneath.
- **Modern ARexx-style scripting.** Like ARexx on the Amiga, Mix lets independent programs and machines talk to each other through a lightweight scripting layer.
- **Rust-based safety.** Written in Rust, so it inherits stronger memory safety and safer concurrency than the older automation tools it replaces.

Mix also runs as an ordinary scripting language and as a login shell, so the same tool covers one-off glue, daily shell work, and mesh orchestration.

## Use cases

- **Glue scripts** — replace ad-hoc Bash for orchestration, deployment, system automation.
- **Shell** — daily-driver login shell (`chsh -s /opt/cosmix/bin/mix`), with REPL meta-commands (`stats`, `help`, `what`) and shell-style command running.
- **Bus citizen** — `mix --serve script.mix` runs the script as a supervised, mesh-registered Bus daemon. A script with just an `on noded.ping` handler that calls `reply("pong")` is a complete service (full multi-line example below).

## Documentation

The **Mix manual** — one page per topic, from syntax to Bus messaging — lives in
this repo and renders in three places from the same files:

- **[docs/_man/](https://github.com/markc/cosmix/tree/main/docs/mix)** — the
  canonical pages on GitHub (the directory README is the index).
- **[markc.github.io/mix](https://markc.github.io/mix/#_man/overview.md)** — the
  same pages in the website's Manual pane.
- **`mix man TOPIC`** — read any page in the terminal (`mix man` alone prints the
  index; `mix help` / `mix what NAME` cover the builtins).

AI agents get a short orientation sheet at [`AGENTS.md`](AGENTS.md), which defers
to the manual as the canonical reference.

## Crates

| Crate | What it is |
|---|---|
| **`cosmix-lib-mix`** | Interpreter library. Lexer, parser, evaluator, builtins, value model. Pure Rust, no internal deps. |
| **`cosmix-mix`** | The `mix` binary. REPL, shell layer, Bus wiring, the `--serve` supervised citizen runtime. |
| **`mix-bench`** | Autoresearch / micro-benchmark harness for the interpreter. |

The interpreter is feature-gated for opt-in capabilities (`json`, `regex`, `toml`, `datetime`, `url`, `crypto`, `http`, `sqlite`, `dkim`) so embedders can pull only what they need; the `mix` binary turns them all on.

## Building

Mix depends on the [bus](https://github.com/markc/cosmix) library family. Both repos must be present as sibling checkouts under `$HOME`:

```sh
git clone https://github.com/markc/cosmix $COSMIX
git clone https://github.com/markc/cosmix $COSMIX
cd $COSMIX/src && cargo build --release
```

Install the resulting binary:

```sh
sudo install -m 0755 $COSMIX/src/target/release/mix /opt/cosmix/bin/mix
```

(Adjust the install path to taste; `/opt/cosmix/bin/` is the convention used by the cosmix daemon family in [cos](https://github.com/markc/cosmix).)

## Using

Interactive REPL:

```sh
mix
```

Run a script:

```sh
mix path/to/script.mix arg1 arg2
```

One-liner:

```sh
mix -c 'print "hello, " .. env("USER")'
```

Read from stdin (useful over `ssh`):

```sh
ssh host mix - <local-script.mix
```

As a supervised Bus citizen (needs a running `cosmix-noded` broker — ships with [cos](https://github.com/markc/cosmix)):

```sh
mix --serve service.mix --name my-service
```

A minimal `service.mix`:

```mix
on noded.ping
    reply("pong")
end
```

Mix is also a real scripting language with first-class functions and terse lambdas. The `fn(...) = expr` form gives a single-expression body — no `return`, no `end` — which reads cleanly inside higher-order builtins:

```mix
$nodes = ["web1", "web2", "db1", "cache1"]

-- ping each node, keep the ones that answer
$up = filter($nodes, fn($n) = ssh_run($n, "mix -c 'print(1)'").ok)

-- square a list
map([1, 2, 3], fn($x) = $x * $x)        -- [1, 4, 9]

-- sum a column of maps
sum_by($rows, fn($r) = $r["bytes"])
```

`mix help` lists every builtin; `mix what <name>` looks up one.

## Auto-upgrade story (bare host → mesh node)

A standalone `mix` install runs as a normal scripting shell. When `cosmix-noded` is later installed on the same host, the **same `mix` binary** becomes mesh-viable on next invocation — no reinstall, no recompile, no script edits. A runtime lazy-probe state machine in `MixBusHandler` decides bare-vs-mesh at execution time:

- **`Unprobed`** — initial state; no Bus form has run yet.
- **`NeverPresent`** — first probe found no broker. Bus forms (`send`, `emit`) return `nil` silently; `noded_register`, `subscribe`, `reply` raise `mesh unavailable`. Bare-system steady state.
- **`Connected(handle)`** — probe succeeded; Bus forms call through.
- **`Lost`** — was connected, transport broke. `bus_reconnect()` is the recovery primitive named in the error message.

Scripts that don't touch the mesh keep running unchanged on either kind of host.

## Releasing a new mix binary

Both crate versions are kept in lockstep (the `mix` binary the user installs is one unit, semver-wise):

1. Bump the `version` field in **both** `cosmix-lib-mix/Cargo.toml` and `cosmix-mix/Cargo.toml`.
2. Commit: `chore: bump mix to X.Y.Z`.
3. `cargo build --release`.
4. Verify: `./target/release/mix --version` matches.

## Related projects

- **[bus](https://github.com/markc/cosmix)** — the Bus protocol library family that mix consumes for `send` / `call` / topic-subscribe.
- **[cos](https://github.com/markc/cosmix)** — the cosmix daemon family. Ships `cosmix-noded` (the broker mix's `--serve` mode connects to) plus mail, web, DNS, knowledge indexer, display compositor.

## License

MIT. See `LICENSE`.
