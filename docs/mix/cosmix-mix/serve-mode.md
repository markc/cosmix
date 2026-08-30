# Serve mode

`mix --serve` runs a Mix script as a long-lived, supervised Bus citizen. The
runtime registers one service identity, executes the script's initialisation
body, and then runs an unconditional event pump for author-defined `on`
handlers and runtime-reserved verbs.

## Synopsis

```text
mix [--no-prelude] [--strict-arity] \
  --serve citizen.mix [--name alpha] [--no-prelude]
```

After the script path, serve mode accepts only `--name SERVICE` and
`--no-prelude`. It does not pass positional arguments to a served script.
`$0` is set to the script path.

## Service identity

`--name` takes precedence. Without it, the service name is the script's file
stem:

```text
citizen.mix        -> citizen
alpha.worker.mix   -> alpha.worker
```

A leading `cosmix-` prefix is removed from either the explicit name or derived
file stem. Empty names and names beginning with `.` are rejected before the
broker connection. The broker performs the remaining identifier validation.

The registered provenance contains the `mix` package, version, repository
revision, dirty state, build time, and a current RFC 3339 timestamp.

## Startup

Serve mode performs these operations in order:

1. Initialises structured logging.
2. Reads, lexes, and parses the script.
3. Resolves the broker URL.
4. Establishes a supervised connection and registers the service name.
5. Constructs an evaluator with the serve Bus handler and reserved runtime.
6. Loads the prelude unless disabled.
7. Executes the initialisation body and enters the event pump.

An unreadable script, lexer or parser failure, logging failure, runtime creation
failure, exhausted initial connection budget, or initialisation error exits
non-zero. Initial broker connection failure is fatal; it does not enter a
background retry loop before the first registration succeeds.

## Supervision

`MixServeHandler` wraps one `SupervisedClient`. All requests, emits, replies,
registrations, and topic operations use that connection. Serve mode does not
route simple names through the transient local-port shortcut.

After registration, a broker interruption is treated as transient. The client
reconnects, re-registers the service, and replays its topic-subscription
registry. Its incoming receiver survives those reconnects. A terminal receiver
close ends the event pump.

Calling `bus_reconnect()` is unnecessary in serve mode; the supervised client
owns recovery.

## Runtime-reserved verbs

The runtime checks reserved verbs before author handlers. A script cannot
shadow them.

| Command | Arguments | Result |
|---|---|---|
| `HELP` | None | JSON array of accepted commands. |
| `INFO` | None | JSON object containing `name`, `version`, and `description`. |
| `QUIT` | None | Replies with status 0 and `{}`, then starts graceful shutdown. |
| `SERVICE.props.get` | Optional `path` | Returns the root property snapshot or one value. |
| `SERVICE.props.list` | None | Returns all defined lifecycle leaf paths. |
| `SERVICE.props.describe` | Required `path` | Returns type and property metadata for one path. |

Replace `SERVICE` with the registered name. For example:

```text
alpha.props.get
alpha.props.list
alpha.props.describe
```

`HELP` lists the six runtime commands first. It then appends sorted,
deduplicated author-handler command names. Author handlers that collide with a
reserved name are omitted because they are unreachable.

`INFO.version` is the `mix` runtime version. A served script does not provide a
separate executable version.

For property commands, a successfully parsed Bus `args` header takes
precedence. When the header is absent or cannot be parsed, the runtime parses
the request body as JSON. An empty request selects the root snapshot.

## Lifecycle properties

The property tree is read-only and reports L1 conformance.

| Path | Type | Value |
|---|---|---|
| `lifecycle.started_at` | string | RFC 3339 process start time. |
| `lifecycle.uptime_s` | number | Live seconds since process start; marked transient. |
| `lifecycle.mode` | string | `serving`. |
| `lifecycle.health` | string | `ok`. |
| `lifecycle.props_level` | string | `L1`. |

The runtime uses the shared Bus property encoder with sensitive-value redaction
enabled.

`props.watch`, `props.set`, and `props.delete` are not runtime-reserved. The
runtime does not provide L2 watches, property mutation, or world aggregation.
An author may define handlers with those names as domain behaviour.

## Author handlers

The script defines domain commands with Mix `on` handlers. The event pump
receives the Bus command, headers, and body through the supervised connection.
The evaluator supplies replies through the same connection and preserves
application return codes separately from transport failures.

Request-style sends await a reply. Map arguments use Bus header/body routing
when a `body` key is present or the command is a property command. Emits are
fire-and-forget and use header/body routing for map arguments.

Application error return codes remain application results. A transport failure
raises a mesh-unavailable runtime error.

## Broker configuration

The broker URL is resolved from the first applicable source:

1. The file named by `COSMIX_NODE_CONFIG`.
2. `node.conf.mix` under the resolved Cosmix configuration directory.
3. The system `node.conf.mix`, unless `COSMIX_ETC` explicitly isolates the
   configuration directory.

The reader consumes only these strict-data fields:

```text
wg_ip: "192.0.2.5"
noded: { port: 4200 }
```

Unknown fields and sections are ignored. An absent `wg_ip` uses loopback. An
absent `noded.port` uses 4200. When no file exists, or the selected file cannot
be read or parsed, the URL falls back to the loopback broker on port 4200.

The first existing file is authoritative. A parse failure does not continue to
a later candidate. The resolver never writes or migrates node configuration,
and it does not read a legacy TOML format.

## Logging

Serve mode uses the shared logging core with a journald-first preset. It enables
standard error when no journal socket is available and also for a foreground
run attached to a terminal. Structured entries carry the registered service
name. `RUST_LOG` overrides the default filter.

There is no live property-backed log-filter mutation in this package. Change
`RUST_LOG` and restart the citizen to change filtering.

## Shutdown

SIGTERM, Ctrl-C, `QUIT`, and a terminal event-pump close converge on one
shutdown path:

1. Stop accepting new inbound work.
2. Deregister from the broker with a five-second bound.
3. Drain in-flight asynchronous handler tasks.
4. Abort remaining tasks and synthesise shutdown replies when the connection
   can still deliver them.
5. Exit.

A clean interrupt, `QUIT`, or clean pump end returns status 0 when
deregistration succeeds or the connection is already gone. Script failure,
deregistration failure, or expiration of the deregistration bound returns a
non-zero status.
