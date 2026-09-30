# cosmix-mcp

`cosmix-mcp` is a stdio Model Context Protocol server that bridges an MCP client to the CosMix appmesh, local CosMix logs, the knowledge and skill store, and the Mix evaluator. It belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain: it uses Bus client, logging, and build-information crates from `bus`, uses the Mix runtime from `mix`, and combines them with `cos` configuration and skill libraries.

## Synopsis

```sh
cosmix-mcp
cosmix-mcp --version
cosmix-mcp -V
```

Example MCP registration:

```sh
claude mcp add cosmix-mcp -- /opt/cosmix/bin/cosmix-mcp
```

## Description

The normal invocation starts an MCP server over standard input and standard output. The client owns the process lifetime; end-of-file or client disconnection ends the service.

Broker access is lazy. Startup does not contact `cosmix-noded`. The first broker-dependent tool registers a unique process-scoped `mcp-…` caller with build provenance. A new request reconnects after a dead connection; an in-flight command is never replayed.

Knowledge and skill tools connect to `cosmix-indexd` when called. The server can therefore start before either backend is available. Operational failures return MCP tool results with `isError: true` and readable error text.

Registration satisfies CTK's existing named-local-caller admission lane. It does **not** authenticate a principal or grant authority. Owning services retain admission and validation. The bridge exposes no application Bus verbs and refuses addressed commands. Tool annotations describe behaviour; they do not enforce permissions.

## Command-line interface

| Option | Effect |
|---|---|
| `--version` | Print embedded build provenance and exit before starting MCP transport. |
| `-V` | Alias for `--version`. |

The version line is the binary's version-discovery surface. Build time and source revision are captured by `build.rs`.

There are no subcommands and no crate-specific Cargo features.

## MCP tool groups

The server publishes Bus, terminal, semantic desktop, knowledge and execution tools.

| Group | Tools |
|---|---|
| Bus | `bus_call`, `bus_list_services`, `bus_node_info`, `bus_list_peers`, `noded_ping` |
| Terminal | `term_list`, `term_snapshot`, `term_type`, `term_tab`, `term_pane` |
| Semantic applications | `app_describe`, `app_controls_list`, `app_control_get`, `app_control_set`, `app_control_wait`, `app_actions_list`, `app_action_invoke` |
| Compositor observations | `desktop_windows`, `desktop_window_wait` |
| Logs | `log_tail`, `log_search` |
| Knowledge | `context_search`, `index_workspace`, `knowledge_digest`, `knowledge_brief` |
| Feedback | `docs_feedback`, `journal_feedback`, `memory_feedback`, `journal_supersede` |
| Skills | `skills_retrieve`, `skills_store`, `skills_refine`, `skills_graduate`, `skills_list`, `skills_delete` |
| Script execution | `mix_execute` |
| Status | `mcp_status` |

See [MCP tools](tools.md) for parameters, defaults, and return behaviour.

## Bus access

`bus_call` accepts a target service, a Bus command, and an optional JSON argument string. The other Bus tools provide broker connectivity, peer discovery, node information, and service inventory.

The inventory path includes service build provenance where supplied by the broker. `bus_node_info` calls `noded.info`; peer and connectivity tools call `noded.peers` and `noded.ping`.

Each Mix worker receives a lazy native Bus handler. Mix `send`, `emit`, and `port_exists` operations use ABP through noded. Incoming long-lived `on` delivery is not supported. Communication between nodes retains the native noded-to-noded ABP path.

## Knowledge and skills

`context_search` searches indexed skills and project material. It can infer a project domain from the server working directory, restrict results to a named domain, or search all domains.

`index_workspace` discovers recognised project content trees, splits Markdown at level-two headings, and replaces indexed entries for each processed file. Re-indexing the same files is intended to be idempotent.

The skill tools implement a retrieve, store, refine, graduate, list, and delete lifecycle. Refinement uses the configured language-model backend. Graduation writes a trusted skill into project instructions through the shared skill library.

Feedback tools update retrieval scoring for document, journal, and memory chunks. `journal_supersede` retains the older chunk for audit while hiding it from subsequent search results.

## Mix execution

`mix_execute` parses and evaluates inline Mix source in an owned Rust subprocess of the same binary, with a current-thread Tokio runtime. The crate enables Mix support for JSON, regular expressions, TOML, date and time values, URLs, and cryptography. Local scripts do not connect to the broker.

The worker isolates process cwd and inherited stdio from the MCP transport. Results include readable text and typed `stdout`, `stderr` and `exit_code`. Scripts are limited to 256 KiB; captured output and each worker pipe are bounded to 1 MiB. Limit failures report that effects may already have occurred.

An optional working directory may be supplied. A leading `~/` is expanded from `HOME`. Directory changes affect only that worker. Completion or cancellation terminates its owned process group, including descendants; persistent application lifetimes belong to their native service. This is process ownership, not a security sandbox.

## Configuration

The crate defines no standalone configuration file or command-line configuration flags.

| Surface | Use |
|---|---|
| Shared client configuration | Resolves the native default broker connection. |
| Shared index configuration | Resolves the `cosmix-indexd` client. |
| `knowledge` settings | Supply search trust weights and journal ageing policy. |
| `skills` settings | Supply retrieval limits, refinement backend, and skill lifecycle policy. |
| `HOME` | Locates persistent logs and expands `~/` for Mix execution. |
| `RUST_LOG` | Overrides the default logging filter. |

## Logs

Logging starts before the MCP transport. The default filter is `info,cosmix_mcp=debug`.

Records are written to standard error, the system journal, and the persistent `cosmix-mcp.log` under the CosMix user log directory. Rotation is disabled by this binary.

The `log_tail` and `log_search` tools read files from the same user log directory. The special name `bus` selects `bus.log`.

Every MCP tool call records the tool name, a redacted argument summary, elapsed milliseconds, success or failure, and a shape-only result summary. The in-memory status store retains the newest 50 call records.

String argument values are not present in normal log summaries. Container values are reduced to shape and size, control characters in keys are scrubbed, and result bodies are reduced to content type and size.

Setting `RUST_LOG=cosmix_mcp=trace` enables full request arguments. This can expose sensitive caller data and is an explicit diagnostic mode.

## Status

`mcp_status` returns:

- process start time and uptime;
- whether the cached native client is currently connected;
- total and failed tool-call counts;
- counts by tool name;
- the newest retained call records, including timestamp, duration, and outcome.

The status data is process-local and resets when the MCP server restarts.

## Return conventions

Every tool advertises an `outputSchema` and descriptive annotations. Successful results contain `structuredContent` alongside readable text. `term_list`, `mcp_status`, Mix execution and semantic desktop tools have dedicated schemas; legacy text tools retain their text and add a `{ "result": … }` envelope. Owning-service payloads remain JSON values inside the relevant envelope.

Operational and parameter-validation failures use `isError: true`; unknown-tool routing remains a protocol error. Observed log or terminal text beginning with `ERROR:` remains data. Generic `bus_call` preserves native application return codes and messages.

`bus_call` treats an absent argument string as JSON `null` and rejects malformed JSON before connecting or delivering anything. Request structures reject unknown fields.

When a client supplies a progress token, the bridge sends correlated, increasing receipt and finish notifications. These report request lifecycle, not a percentage of native work completed.

Cancellation or timeout stops waiting for a native reply; it does not undo delivered work. Outcomes can be unknown. Never blindly retry typing, actions, clipboard writes or other mutations. Mix cancellation terminates owned local work but cannot reverse completed filesystem or Bus effects. Existing non-transactional knowledge pipelines and feedback operations retain their completion policy on client cancellation; they need owning-service transaction or idempotency support before safe interruption.

## Native acceptance and remaining desktop facilities

See [native acceptance](native-acceptance.md) for the real stdio MCP harness, CTK control probe and the remaining owning-component acceptance tests. Screenshot delivery, a complete accessibility tree and clipboard-based Unicode fallback require native contracts; this bridge does not substitute KWin internals, a Python sidecar or another control transport.

## Build dependencies

The binary is built with `rmcp` server, stdio transport, and macro support. It also depends on Tokio, Serde, JSON Schema generation, Chrono, glob matching, and the CosMix Bus, Mix, configuration, logging, build-information, and skill libraries.

Because the Bus and Mix crates are path dependencies, their sibling source trees must be available when building this crate from the `cos` workspace.
