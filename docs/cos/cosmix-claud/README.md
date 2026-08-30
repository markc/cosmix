# cosmix-claud

`cosmix-claud` is a local Bus port daemon that exposes LLM completion, code assistance, and embedded Mix evaluation through the `claud` port. It is part of the `cos` daemon family in the `bus <- mix <- cos` dependency chain: it consumes Bus framing from `bus`, embeds the Mix evaluator from `mix`, and combines those lower layers with Cos configuration, logging, LLM, and knowledge services.

## Synopsis

```text
cosmix-claud
```

The binary takes no command-line options. It starts the service, accepts Bus requests until interrupted, and removes its socket during an orderly shutdown.

## Service endpoint

The daemon listens on:

```text
/run/user/{uid}/cosmix/ports/claud.sock
```

The current user ID supplies `{uid}`. The daemon creates the parent directory when required, removes any existing socket at the path, and binds a Unix listener.

Each accepted connection runs in its own Tokio task. A connection carries one Bus request and one Bus response.

## Request and response format

Requests are Bus messages with a required `command` header. A non-empty message body must contain JSON. An empty body is treated as JSON `null`.

Responses contain:

- an `rc` Bus header;
- a JSON response body.

`rc=0` reports success. `rc=10` reports command, validation, evaluation, initialisation, or backend errors. An unknown command also returns `rc=10`.

See [Bus verbs](verbs.md) for the complete request and response surface.

## Commands

| Command | Purpose |
|---|---|
| `ask` | Complete a prompt with optional caller context and knowledge injection. |
| `ask_raw` | Complete a prompt without knowledge search or skill extraction. |
| `analyze` | Build and complete a concise code-error analysis prompt. |
| `generate` | Build and complete a code-generation prompt. |
| `mix.execute` | Execute a Mix script and return captured output. |
| `mix.expr` | Evaluate a Mix expression and return its JSON representation. |
| `help` | Return the command names. |
| `info` | Return port, package, version, domain, and command metadata. |

## Mix client examples

Send a knowledge-augmented prompt:

```mix
send "claud" "ask" prompt="Explain recursion"
```

Supply caller context:

```mix
address "claud"
    ask prompt="Explain recursion" context="Teaching a beginner"
end
```

Execute a Mix script:

```mix
send "claud" "mix.execute" script="say json_encode(\[1,2,3\])"
```

## Knowledge flow

`ask`, `analyze`, and `generate` use the knowledge-augmented path.

1. The daemon detects a workspace domain from its current working directory.
2. It searches the knowledge index for up to three domain-matched skills, three documents, and two journal entries.
3. It formats successful hits and appends them to the system prompt after any caller-supplied context.
4. It sends the system prompt and user prompt to the configured LLM backend.
5. It schedules optional skill evaluation and extraction in a detached background task without awaiting them.

Knowledge search failure does not fail the completion. The daemon logs the failure and continues without injected results.

The background learning path skips interactions whose prompt or response is shorter than 100 bytes. For longer interactions it evaluates the task, extracts a reusable skill when warranted, assigns the detected domain, and attempts to store the result in the knowledge index.

`ask_raw` bypasses both knowledge search and post-response skill extraction. Caller-supplied `context` still contributes to its system prompt.

## Configuration

At startup the daemon loads the `skills` service settings through `cosmix-lib-config`.

The only setting read directly by this crate is `llm_backend`:

- a non-empty value selects that backend;
- an empty value leaves backend selection to `cosmix-lib-llm`.

Knowledge augmentation is enabled in the daemon state at startup. The source exposes no command-line switch or crate-local setting to disable it globally; use `ask_raw` for an individual request without augmentation.

## Embedded Mix

The crate embeds `cosmix-lib-mix` rather than launching a separate Mix process. Its dependency enables the Mix `json`, `regex`, `toml`, `datetime`, `url`, and `crypto` features.

Script and expression evaluation run on blocking worker tasks because the evaluator is not `Send`. Each request creates a fresh evaluator with captured standard output and standard error.

`mix.execute` may inject JSON values as Mix globals. Strings, numbers, and booleans retain their basic value kind; other JSON values are injected as their serialised string form.

`mix.expr` converts the final Mix value to JSON. Values without a JSON representation return `rc=10`.

## Cargo features

This package declares no Cargo features of its own.

## Runtime characteristics

The binary uses:

- Tokio for the Unix listener, signals, connection tasks, and blocking evaluation tasks;
- `cosmix-lib-bus` for Bus stream framing and messages;
- `cosmix-lib-llm` for completions;
- `cosmix-lib-skills` for domain detection, index access, task evaluation, and skill extraction;
- `cosmix-lib-config` for service settings and the current user ID;
- `cosmix-lib-log` and `tracing` for daemon logging;
- mimalloc as the global allocator.

The daemon handles `Ctrl-C` as its shutdown signal. A connection-level read, JSON decoding, or write failure is logged as a connection error.

## See also

- [Bus verbs](verbs.md)
- [Cos overview](../overview.md)
