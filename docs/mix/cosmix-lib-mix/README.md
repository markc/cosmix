# cosmix-lib-mix

`cosmix-lib-mix` is the embeddable Rust implementation of the Mix language: it tokenises, parses, analyses, and evaluates Mix source, exposes the Mix value and error models, and supplies host seams for Bus and other mediated services. In the `bus <- mix <- cos` dependency chain it is the Mix execution layer: Bus-facing behaviour enters through injected traits, while higher-level Cos components can embed the evaluator without moving protocol or daemon dependencies into this crate.

The Cargo package is `cosmix-lib-mix`. The Rust library name is `cosmix_mix`.

## Synopsis

```rust
use cosmix_mix::{MixResult, run_capturing};

async fn evaluate() -> MixResult<()> {
    let (_value, stdout, stderr) =
        run_capturing("print(1 + 2)\n").await?;

    assert_eq!(stdout, "3\n");
    assert!(stderr.is_empty());
    Ok(())
}
```

For strict Mix data:

```rust
use cosmix_mix::{MixResult, parse_data};

fn load_data() -> MixResult<String> {
    let value = parse_data(r#"{service: "alpha", retries: 3}"#)?;
    value.to_mix_data_string()
}
```

## Entry points

| Item | Purpose |
|---|---|
| `run(source)` | Tokenises, parses, and evaluates Mix source with a new default `Evaluator`. |
| `run_capturing(source)` | Evaluates source and returns the result value plus captured stdout and stderr. |
| `parse_data(source)` | Parses only the literal-data subset and returns a `Value`. |
| `parse_data_file(path)` | Reads a Mix data file and applies `parse_data`. |
| `Lexer::tokenize` | Converts source text into spanned tokens. |
| `Parser::parse_program` | Converts tokens into a statement AST. |
| `Parser::parse_data` | Parses tokens under strict-data rules. |
| `analyzer::analyze` | Produces conservative semantic diagnostics and a capability inventory. |
| `Evaluator::execute` | Executes parsed statements in a configured evaluator. |

`parse_data` accepts scalar literals, lists, maps, and nested combinations. It rejects variable reads, calls, messaging, shell execution, interpolation, control flow, and arithmetic with `MixError::StrictDataViolation`.

`Value::to_mix_data_string` and `Value::to_mix_data_string_pretty` emit strict-data text that can be parsed again. Human-oriented `Value::write_mix` output is a separate format and is not the strict-data round-trip path.

## Evaluator

`Evaluator` is the configurable interpreter host. `Evaluator::new` uses normal process output; `Evaluator::with_output` accepts separate `Write` sinks. `SharedBuf` provides an in-memory sink for captured bytes.

The evaluator supports:

- async native extensions through `ExtFn`, `sync_ext`, and `Evaluator::register`;
- global injection and lookup through `set_global` and `get_global`;
- source identity, tracing, usage statistics, and prelude loading;
- compatible or strict function arity through `ArityMode`;
- recursion, wall-clock, list, map, and string limits through `EvalLimits`;
- builtin authority checks through `CapabilityPolicy`;
- Bus, database, JMAP, delegated Bus-call, shell, and serve-runtime host traits;
- direct event dispatch and an incoming Bus event pump.

The default capability policy is permissive. `CategoryAllowList` allows pure builtins plus selected `CapabilityClass` values: `fs-read`, `fs-write`, `network`, `process`, `env`, `db`, `jmap`, and `bus`.

The capability policy is an in-process robustness boundary for trusted embedded scripts. It is not process isolation for untrusted code.

`EvalLimits` checks wall-clock time at statement polls. A single blocking builtin cannot be interrupted in the middle of its blocking operation.

## Host seams

`BusHandler` connects language-level messaging to a host. It covers request/reply sends, fire-and-forget emits, port checks, incoming events, service registration, topic subscription, correlated replies, and reconnect requests.

`BusCallHandler` backs the scoped `bus_call(verb, args)` builtin. The host chooses the reachable verbs and supplies trusted delegation context; the script supplies only the verb and arguments.

`DbHandler` backs `db_query` and `db_exec` with positional bind values. Queries return lists of row maps. Executions return a map containing affected-row and last-insert information.

`JmapHandler` backs JMAP method batches and blob uploads without exposing network credentials or an arbitrary destination to the script.

`ShellHandler` classifies, validates, and executes shell lines when a host enables mixed `source` handling. Without a registered handler, sourced files retain strict Mix semantics.

`ServeRuntime` handles runtime-reserved Bus verbs before author-defined handlers. It returns `ReservedOutcome` values containing a Bus return code, response body, and graceful-quit flag.

The evaluator and its boxed host futures use single-threaded `Rc` and `RefCell` state. Host implementations are therefore not required to return `Send` futures.

## Values and errors

`Value` represents strings, numbers, booleans, lists, maps, functions, raw bytes, mutable byte buffers, and `nil`. Lists, maps, and raw bytes use reference-counted copy-on-write storage. Buffers use shared reference semantics, so mutations are visible through aliases.

`MixError` covers lexing, parsing, strict-data, and runtime failures. `MixResult<T>` is the crate result alias. Structured runtime errors use stable codes, optional details, source spans, and traceback frames through `ErrorInfo`.

`IndexMap` is re-exported so embedders can construct the ordered map used by `Value::Map` without coupling to a separate `indexmap` version.

## Builtin discovery and policy

`builtins` contains the builtin registry and dispatch implementation. `builtins_hof` contains async higher-order builtins that can call back into Mix functions.

Every builtin has a `BuiltinInfo` and `BuiltinContract`. The contract records arguments, return shape, effect flags, operational-failure behaviour, conditional capabilities, and any non-contiguous accepted arities. `BuiltinInfo::signature` derives a human-readable signature from this metadata.

`capability_category`, `conditional_capabilities`, `conditional_cap_engaged`, and `builtin_info_of` expose the registry to embedders. `CategoryAllowList` is the supplied class-based policy.

## Modules

| Module | Surface |
|---|---|
| `analyzer` | Semantic diagnostics, analyzer configuration, and script capability inventory. |
| `ast` | Expression, statement, function-body, operator, and assignment-path nodes. |
| `builtin_info` | Builtin metadata, contracts, type shapes, effects, and signature rendering. |
| `builtins` | Core builtin dispatch, metadata tables, capability classes, and helpers. |
| `builtins_hof` | Higher-order builtin registry with evaluator callbacks. |
| `error` | Source spans, structured error information, traceback frames, and `MixError`. |
| `evaluator` | Interpreter, limits, extensions, host traits, event handling, and output capture. |
| `interrupt` | Process-wide SIGINT cooperation for evaluator and blocking builtin polling. |
| `lexer` | Source tokenizer. |
| `parser` | Program and strict-data parser. |
| `scope` | Variables, functions, call frames, and scope storage. |
| `stats` | Session and language-usage counters. |
| `token` | Tokens, spanned tokens, and interpolated string parts. |
| `value` | Runtime value representation and Mix data serialisation. |
| `json` | JSON conversion when the `json` feature is enabled. |
| `jq` | Embedded jq-compatible filtering when the `json` feature is enabled. |
| `serde_de` | Serde deserialisation from `Value` when the `serde` feature is enabled. |
| `serde_ser` | Serde serialisation to `Value` and `.conf.mix` text when `serde` is enabled. |

## Cargo features

The default feature set is empty. Tokio remains an unconditional dependency because evaluator task, synchronisation, timer, and macro APIs are part of the core implementation.

| Feature | Adds |
|---|---|
| `default` | No optional features. |
| `json` | JSON conversion, JSON and JSONL builtins, jq filtering, and JSON-backed stats helpers. |
| `regex` | `regex_match`, `regex_find`, `regex_replace`, and `regex_split`. |
| `markdown` | Safe CommonMark and GFM rendering through `markdown`; raw HTML is escaped and dangerous URL schemes are neutralised. |
| `toml` | `toml_parse` and `toml_encode`. |
| `serde` | Serde `Deserializer` and `Serializer` bridges for `Value` and strict `.conf.mix` text. |
| `datetime` | Date parsing and formatting, ISO timestamps, duration formatting, and relative time. |
| `url` | Structured URL parsing through `url_parse`. |
| `crypto` | Base64, BLAKE3, SHA-256, HMAC-SHA-256, constant-time comparison, file hashing, and UUID generation. |
| `http` | HTTP GET, POST, and general request builtins with web roots and optional PEM CA loading. |
| `sqlite` | Direct SQLite open, execute, and close builtins. |
| `tokio-sleep` | Backwards-compatible feature alias; it adds no dependency or code gate. |
| `dkim` | RSA-2048 and Ed25519 DKIM key generation plus DNS TXT record text. |
| `datastar` | Datastar element patches, signal patches, and SSE serialisation; also enables `json`. |
| `xml` | Strict XML parsing in simple or full-fidelity tree mode. It is not an HTML parser. |

## Analysis

The semantic analyzer sits between parsing and evaluation and can be used without the command-line frontend. It reports definitely undefined variables and callables, arity mismatches, duplicate declarations, unreachable statements, discarded must-use results, and statically resolvable `require` failures.

`AnalyzerConfig` declares host-provided globals and functions. `Analysis` returns diagnostics plus the capability classes used by the script. The analyzer favours very low false-positive rates for dynamic language constructs.

## Testing support

The crate contains unit and integration coverage for parsing, evaluation, strict data, limits, capabilities, host seams, structured errors, and optional builtins.

The nested `fuzz` workspace supplies `fuzz_lex` and `fuzz_parse` libFuzzer targets. It runs on demand with a nightly Rust toolchain and is not part of the parent workspace's normal test gate.

## Package limits

This package defines no binary target, command-line subcommands, command-line configuration, daemon process, or standalone configuration loader. Those surfaces belong to hosts that embed `cosmix_mix`.
