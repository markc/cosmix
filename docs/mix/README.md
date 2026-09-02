# Mix — manual

The reference manual for the **Mix** language, one page per topic. The canonical
home of these pages is
[`docs/_man/`](https://github.com/markc/cosmix/tree/main/docs/mix) in the public
[markc/mix](https://github.com/markc/cosmix) repo; add a page by dropping `TOPIC.md`
there. The same files render everywhere:

- **Terminal** — `mix man TOPIC` (`mix man` alone prints this index).
- **`mix man`** — reads [cosmix.dev/mix](https://cosmix.dev/mix/overview), a mirror of this directory in the `cosmix` repo (`$COSMIX/mix/`). It is kept in step automatically: the mix repo's post-commit hook runs `$COSMIX/build/sync-man.mix` whenever a page here is committed (copy, prune, regenerate HTML stubs, commit, push). If `mix man` looks stale, run that script by hand and check `~/.cache/cosmix/man/` (24 h cache).
- **Web** — [markc.github.io/mix](https://markc.github.io/mix/#_man/overview.md) serves the manual in the site's left-hand **Manual** pane; any page deep-links as `#_man/PAGE.md`.
- **GitHub** — browse [`docs/_man/`](https://github.com/markc/cosmix/tree/main/docs/mix) directly; this file doubles as the directory README.
- **Local clone** — plain markdown with relative links; any editor or viewer works.

## Start here

- **[overview](overview.md)** — what Mix is, why it exists, where it came from.
- **[invocation & CLI](invocation.md)** — `mix file`, `-c`, `-`, `-i`, login shell, `--serve`, flags.
- **[the mix command](cli.md)** — `mix help`/`man`/`builtins`/`what`/`status`/`trace`/… meta-commands.
- **[syntax & the classifier](syntax.md)** — tokens, the newline rule, shell-vs-Mix dispatch.

## The language

- **[variables, sigils & scope](variables.md)** — `$` sigils, `${...}`, function-local binding.
- **[strings](strings.md)** — `'raw'` vs `"interp"`, `..` concat, codepoint/byte/grapheme ops.
- **[numbers](numbers.md)** — f64, radix literals, ordering, coercion.
- **[operators](operators.md)** — arithmetic, comparison, `and`/`or`/`not`, `..`, `?:`, `??`.
- **[control flow](control-flow.md)** — `if`/`while`/`for`/`loop`, if-as-expression, `break`/`continue`.
- **[functions, lambdas & modules](functions.md)** — `fn`, closures, the pass-in/return/reassign triad.
- **[modules — require/include/source](modules.md)** — `require()` isolated module loading vs the splice loaders.
- **[lists & maps](collections.md)** — literals, indexing, `push`/`pop`, the base footguns.
- **[higher-order functions](hof.md)** — `map`/`filter`/`reduce`/`sort_by`/`group_by`/…
- **[errors & exit handling](errors.md)** — `try`/`catch`/`die`/`panic`, `run_rc().rc`, timeouts.

## Builtins & I/O

- **[math](math.md)** — rounding, powers, logs, trig, `min`/`max`/`clamp`, constants.
- **[files & I/O](io.md)** — `read_file`/`write_file`/`glob`/`stat`/`chmod`/`walk`/…
- **[processes & system](system.md)** — `run`/`run_rc`/`run_stream`/`spawn`/`env`/`exit`/…
- **[data & serialization](data.md)** — JSON, TOML, `jq`, `data_encode`, strict-data `.mix`.
- **[byte buffers](buffer.md)** — `buffer`/`buffer_push`/`freeze`, the one reference-semantic type.
- **[regular expressions](regex.md)** — `regex_match`/`find`/`replace`/`split`.
- **[dates & time](datetime.md)** — `time`/`date_format`/`now_iso`/`duration_format`.
- **[http](http.md)** — `http_get`/`http_post`/`http_request`, deadlines.
- **[datastar](datastar.md)** — `ds_*` SSE event framing.

## Mesh & runtime

- **[Bus messaging](bus.md)** — `send`/`emit`/`address`/`on`, `$result`/`$rc` bands, the mesh.
- **[serving as a citizen](serve.md)** — `mix --serve service.mix`, the supervised runtime.
- **[remote execution (ssh)](remote.md)** — `ssh_run`/`ssh_must`/`ssh_mix`, env transports.
- **[capabilities & embedding](capabilities.md)** — the capability classes + sandbox model.

## Reference

- **[builtin index](builtins.md)** — every builtin by category.
- **[reserved words](keywords.md)** — the keyword set, keywords-as-names.
- **[shell-dispatch mode](shell-mode.md)** — pipes, `&&`, brace expansion, `$(...)`, redirects.
- **[usage statistics](stats.md)** — runtime modes, report windows, persistence, kill switch, static coverage.

## Design docs & specs
Design docs, background rationale, and the formal specs (04 language
reference, 18 citizen runtime) all live in the operator control repo since
2026-07-23; this manual is the complete public reference.

## See also

```
mix help              the full categorized builtin reference
mix builtins [CAT]    list builtins, optionally by category
mix what NAME         one-line description of a builtin or keyword
mix man TOPIC         read any of the pages above in the terminal
```

For AI agents: [`AGENTS.md`](https://github.com/markc/cosmix/blob/main/AGENTS.md) at
the repo root is a short orientation sheet whose canonical reference is this
manual.
