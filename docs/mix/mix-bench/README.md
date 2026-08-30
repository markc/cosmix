# mix-bench

`mix-bench` is the standalone correctness and performance metric harness for the Mix evaluator. Within the `bus <- mix <- cos` dependency chain, it sits in the Mix layer: it depends directly on `cosmix-lib-mix`, installs a mock `BusHandler` for one test policy, and has no direct Cos dependency or live Bus transport.

## Synopsis

Run the package from the source workspace:

```text
cargo run --release -p mix-bench
```

The harness locates the sibling `cosmix-lib-mix/tests/scripts/` directory,
changes its working directory to `cosmix-lib-mix`, and runs two phases:

1. Check every `*.mix` script in the test-script directory for correct output.
2. Time a fixed corpus of synthetic programs and replayed test scripts.

The final standard-output line is:

```text
score: <milliseconds>
```

Lower scores are better.

## Metric

The score is:

```text
bench_ms + failed_tests * 60000
```

`bench_ms` measures only the timed benchmark phase. The correctness phase is
timed for diagnostics but its duration does not enter the score.

Each failed correctness script adds 60,000 points. A correctness failure does
not stop the benchmark phase.

## Correctness corpus

The harness discovers and sorts every file ending in `.mix` under
`cosmix-lib-mix/tests/scripts/`.

Each script declares expected standard output in a comment block:

```text
-- Expected output:
-- first line
-- second line
```

An expected-output line starts with `-- `. A line containing only `--`
represents an empty output line. The block ends at the first line with another
form.

A script fails when:

- its source cannot be read;
- it has no expected-output block;
- lexing, parsing, or evaluation fails;
- the number of output lines differs; or
- an output line differs byte for byte.

Trailing newline characters are removed from captured output before comparison.
Other whitespace remains significant.

## Evaluator policies

Every run creates a fresh `Lexer`, `Parser`, and `Evaluator`. Standard output
and standard error are captured in `SharedBuf` values, so script execution is
non-interactive.

The evaluator recursion limit is set to 512. Other evaluation limits retain
their defaults.

Most scripts use the plain evaluator policy. Two filenames select additional
setup:

| Script | Policy | Added capability |
| --- | --- | --- |
| `extensions.mix` | Extensions | Registers `add_numbers`, `greet`, and `get_list` |
| `send.mix` | Bus | Installs the mock Bus handler |

The extension functions provide deterministic in-process values:

- `add_numbers` converts its first two arguments to numbers and adds them;
- `greet` returns `hello from ` followed by its first argument; and
- `get_list` returns the strings `alpha`, `bravo`, and `charlie`.

The mock Bus handler performs no network activity. Its `send` implementation
returns status `0` and a deterministic string value, `emit` succeeds,
`port_exists` returns true, and `next_incoming` returns no event.

## Timed corpus

The synthetic corpus exercises interpreter hot paths:

| Label | Workload | Iterations |
| --- | --- | ---: |
| `fib` | Recursive `fib(22)` | 8 |
| `loop_arith` | Numeric loop and arithmetic | 1 |
| `list_build` | List construction and traversal | 1 |
| `hofs` | `map`, `filter`, and `reduce` | 2 |
| `strings` | String concatenation and search | 1 |
| `table` | Table insertion, key traversal, and lookup | 1 |

The harness also replays a fixed set of test scripts 50 times each:

```text
variables.mix       strings.mix         control.mix
loops.mix           functions.mix       lists.mix
list_hofs.mix       lambdas.mix         slicing.mix
printf.mix          tables.mix          terminators.mix
parse.mix           json.mix            jsonl.mix
labels.mix          continue_for_each.mix
```

An unreadable replay script is skipped. A lexing, parsing, or evaluation error
in a synthetic or replay workload stops the harness with exit status `3`.

Timing uses wall-clock elapsed time. Per-workload timings and the total are
diagnostics rather than a stable machine-independent performance baseline.

## Command-line surface

`mix-bench` has no subcommands and no configuration file or environment-variable
surface.

The `--json` argument is accepted for invocation compatibility, but the harness
does not emit JSON. All command-line arguments are currently collected and
otherwise ignored.

## Output

Progress and diagnostics go to standard error. They include:

- correctness corpus size;
- individual correctness failures;
- passed and failed totals;
- elapsed time for each synthetic and replay workload;
- total benchmark milliseconds; and
- the raw `failed_tests` and `bench_ms` components.

Standard output contains only the final score line during a normal run.

## Exit status

| Status | Meaning |
| ---: | --- |
| `0` | Both phases complete, including runs with correctness failures |
| `2` | The harness cannot enter the Mix library directory or find the script corpus |
| `3` | A timed synthetic or replay workload fails |
| Other non-zero | The process panics or encounters another unhandled failure |

## Workspace discovery

The preferred lookup starts at the compile-time package directory and checks
for the sibling `cosmix-lib-mix/tests/scripts/` tree.

If that lookup fails, the harness walks upwards from its current working
directory. It selects the first directory containing that same tree. If no
match exists, later setup fails with exit status `2`.

The benchmark therefore targets a Mix source workspace. It is not a general
script runner or an installed-system benchmark.

## Package dependencies

| Dependency | Use |
| --- | --- |
| `cosmix-lib-mix` | Lexer, parser, evaluator, values, extension adapter, evaluation limits, and Bus handler interface |
| `tokio` | Current-thread asynchronous entry point and runtime support |

The package enables the Mix library's `json`, `regex`, `toml`, `datetime`,
`url`, `crypto`, `sqlite`, and `tokio-sleep` features so the discovered script
corpus runs with those builtins available.

`mix-bench` declares no package-specific Cargo features and exposes no Rust
library API.
