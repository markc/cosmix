# cosmix-claud Bus verbs

This page defines the Bus command surface implemented by `cosmix-claud`.

[Back to cosmix-claud](README.md)

## Common contract

The request Bus message requires a `command` header. Its body is either empty or valid JSON.

The response Bus message sets an `rc` header and carries a JSON body:

| Return code | Meaning |
|---|---|
| `0` | The command completed successfully. |
| `10` | The command, input, backend, parser, evaluator, or result conversion failed. |

Several commands accept either a JSON object or a top-level JSON string for their primary argument. The sections below identify those forms explicitly.

## `ask`

Completes a prompt with caller context and domain-matched knowledge.

Object arguments:

| Field | Required | Meaning |
|---|---|---|
| `prompt` | Yes | User prompt sent to the LLM. |
| `context` | No | Text prepended to the generated system prompt. |

The body may instead be a JSON string containing the prompt.

```json
{"prompt":"Explain this module","context":"Answer for a new maintainer"}
```

Success body:

```json
{"response":"..."}
```

The command searches for relevant skills, documents, and journal entries before calling the LLM. Search errors are non-fatal. After a successful completion, eligible interactions enter asynchronous skill evaluation and extraction.

A missing prompt returns `rc=10` with `{"error":"Missing 'prompt' argument"}`.

## `ask_raw`

Uses the same request and response shapes as `ask`, but does not search the knowledge index and does not start skill extraction.

The optional `context` field still contributes to the system prompt. Backend selection and error handling are otherwise the same as `ask`.

## `analyze`

Constructs a concise analysis prompt from code, an error, or both. It uses the knowledge-augmented completion path.

Object arguments:

| Field | Required | Meaning |
|---|---|---|
| `code` | Conditional | Source text to analyse. |
| `error` | Conditional | Error text to analyse. |
| `language` | No | Language named in the generated prompt; defaults to `Mix`. |

At least one of `code` or `error` must be a non-empty string.

```json
{"language":"Rust","code":"fn main() {}","error":"example diagnostic"}
```

Success body:

```json
{"analysis":"..."}
```

If both inputs are absent or empty, the command returns `rc=10` with `{"error":"Provide 'code' and/or 'error' arguments"}`.

## `generate`

Constructs a prompt that asks the LLM to return code without explanation. It uses the knowledge-augmented completion path.

Object arguments:

| Field | Required | Meaning |
|---|---|---|
| `task` | Yes | Description of the code to generate. |
| `language` | No | Requested language; defaults to `Mix`. |

The body may instead be a JSON string containing the task. That form uses the default language.

Example body:

```json
{"task":"Print each argument on a separate line","language":"Mix"}
```

Success body:

```json
{"code":"..."}
```

## `mix.execute`

Parses and executes a Mix program in a fresh evaluator.

Object arguments:

| Field | Required | Meaning |
|---|---|---|
| `script` | Yes | Mix source to execute. |
| `vars` | No | Object whose fields become Mix globals. |

The body may instead be a JSON string containing the script.

Example body:

```json
{"script":"say greeting","vars":{"greeting":"hello"}}
```

Variable conversion follows these rules:

| JSON input | Mix global |
|---|---|
| String | String |
| Number | Number represented as `f64` |
| Boolean | Boolean |
| Null, array, or object | Serialised JSON string |

Success returns captured standard output:

```json
{"output":"hello\n"}
```

If the script writes to standard error without failing, the body also contains `stderr`.

Parse and evaluation failures return `rc=10`. An evaluation failure may include output produced before the error:

```json
{"error":"...","partial_output":"..."}
```

## `mix.expr`

Parses and evaluates a Mix expression by wrapping it in a `return` statement.

Object arguments:

| Field | Required | Meaning |
|---|---|---|
| `expr` | Yes | Mix expression to evaluate. |

The body may instead be a JSON string containing the expression.

Example body:

```json
{"expr":"1 + 2"}
```

Success returns the converted JSON value:

```json
{"result":3}
```

Lexer errors, parser errors, evaluator errors, task panics, and values that cannot be encoded as JSON return `rc=10` with an `error` field.

## `help`

Takes no arguments. It returns the complete command-name array:

```json
{"commands":["ask","ask_raw","analyze","generate","mix.execute","mix.expr","help","info"]}
```

## `info`

Takes no arguments. It returns:

| Field | Meaning |
|---|---|
| `port` | The fixed port name, `claud`. |
| `app` | The package name, `cosmix-claud`. |
| `version` | The package version compiled into the binary. |
| `knowledge_enabled` | Whether knowledge augmentation is enabled in daemon state. |
| `domain` | The workspace domain detected at startup. |
| `commands` | The command count, `8`. |
| `command_list` | The complete command-name array. |

## Unknown commands

Any other command returns `rc=10`:

```json
{"error":"Unknown command: example"}
```
