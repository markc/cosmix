# Command-line interface

The `cosmix-mix` package installs the `mix` command. The command is an
interactive shell, a Mix program runner, a shell-compatible `-c` dispatcher,
and a collection of reference, diagnostic, and maintenance subcommands.

## Usage

```text
mix
mix [GLOBAL-OPTIONS] FILE [ARG...]
mix [GLOBAL-OPTIONS] -c CODE [ARG...]
mix -i -c CODE [ARG...]
mix [GLOBAL-OPTIONS] - [ARG...]
mix --check FILE
mix lint [LINT-OPTIONS] FILE...
mix SUBCOMMAND [ARG...]
mix --serve FILE [--name SERVICE] [--no-prelude]
```

Global options precede a script path. Arguments after a normal script path,
`-c` code string, or `-` are positional arguments for the Mix program.

## Primary options

| Option | Effect |
|---|---|
| `-h`, `--help` | Prints the capability overview. |
| `-V`, `--version` | Prints the `mix` version line. |
| `--builtins` | Prints the full builtin catalogue. |
| `--no-prelude` | Skips the standard prelude in script, command, or serve evaluation. |
| `--no-traceback` | Uses legacy single-line rendering for uncaught file and command-mode errors. |
| `--strict-arity` | Makes wrong-arity function and builtin calls raise `ARITY_MISMATCH`. |
| `--check FILE` | Lexes and parses a file without executing it. |
| `-c CODE` | Runs one classified command line. |
| `-i` | When used with `-c`, loads `~/.mixrc` before running the code. |
| `-ci`, `-ic` | Combined spellings of `-i -c`. |
| `--serve FILE` | Starts the supervised Bus citizen mode described in [Serve mode](serve-mode.md). |

Without `--strict-arity`, missing arguments bind to `nil` and extra arguments
are ignored. Tracebacks remain the default when an error crosses a function or
builtin boundary.

## Script arguments

For a file, `$0` is the script path and `$1`, `$2`, and later variables hold
the remaining arguments. For `mix -`, `$0` is `-`. Command mode assigns its
arguments from `$1`; it does not set a script filename.

`include` resolves relative content using the active script file. The
standard-input form has no temporary remote file and is executed only when the
explicit `-` operand is present.

## Command mode

`mix -c CODE` uses the REPL classifier rather than assuming that `CODE` is Mix
source. It may execute:

- Mix statements or expressions.
- A simple bareword call to a user-defined Mix function.
- An external command, pipeline, redirection, background launch, or command
  list.
- The in-process `cd` and `exit` builtins.

The exit status of an external command becomes the `mix` exit status. Spawn
failure returns 127. A signal result is converted to `128 + signal` on Unix.
Mix parse or runtime errors return 1.

`-i` sources `~/.mixrc` before classification, making its aliases, functions,
and `PATH` changes available. Command mode otherwise loads the prelude but does
not load the rc file.

## Interactive shell

The REPL loads the prelude, then loads a regular-file `~/.mixrc` when `HOME` is
available. It stores readline history in `~/.mix_history`.

Input classification follows this broad order:

1. Expand the first-word alias.
2. Route Mix keywords and variable-led forms to the Mix parser.
3. Route a simple call whose head names a Mix function to that function.
4. Route shell builtins and commands found on `PATH` to shell execution.
5. Surface a Mix parse error or command-not-found result for the remaining
   input.

The shell recognises pipelines, file-descriptor redirections, environment
prefixes, variable and tilde expansion, brace and glob expansion, command
substitution, background `&`, and `&&`, `||`, and `;` lists. Expansions cannot
inject new structural command-list operators.

REPL shell builtins are `cd`, `pushd`, `popd`, `history`, `exit`, `which`,
`type`, `unalias`, `jobs`, `fg`, `bg`, and `mix`. Files executed by `source`
support `cd` and `exit`; stateful REPL-only builtins are rejected there.

`time LINE` and `mix time LINE` time either Mix or shell input in the REPL.
`trace`, `history`, and `reload` are also REPL-only meta-command surfaces.

## One-shot subcommands

The following names are recognised before the command attempts to open a
same-named script. Use an explicit path such as `./build` for a script that
collides with a subcommand name.

| Group | Commands |
|---|---|
| Session inspection | `vars`, `aliases`, `functions`, `all`, `type`, `config`, `status`, `context`, `snapshot` |
| Reference | `help`, `tutorial`, `examples`, `man`, `keywords`, `builtins`, `what`, `syntax`, `operators`, `diff` |
| Project maintenance | `build`, `clean`, `update`, `test`, `self` |
| Validation | `check`, `lint` |
| Bus discovery | `mesh`, `ports`, `ping` |
| AI integration | `fix`, `extend`, `review`, `explain`, `evolve`, `dogfood`, `fuzz`, `teach`, `ask`, `chat` |
| Service tools | `deploy`, `health`, `logs` |
| Claude port tools | `claude-start`, `claude-stop`, `claude-status` |
| Automation | `watch` |
| Usage data | `stats` |

One-shot session inspection uses a fresh evaluator. It therefore has no user
variables, aliases, or functions unless the command itself loads them.

`builtins` accepts `--json`, `--data`, `--names`, one builtin name, or one
category. `syntax` opens the variables manual page and `operators` opens the
operators page.

The AI commands and the `ai`, `ai_diagnose`, and `context` extension functions
invoke an external Claude command-line program. Build and maintenance commands
invoke tools such as Cargo and Git. Service commands invoke local service and
logging tools.

## Lint

```text
mix lint [--json | --data] [--deny-warnings]
         [--allow-global NAME]... [--allow-function NAME]...
         FILE...
```

`-` reads standard input and may appear once. Human diagnostics and structured
reports go to standard output. Usage and internal errors go to standard error.

| Status | Meaning |
|---|---|
| 0 | No errors; warnings are allowed unless denied. |
| 1 | Error diagnostics, or warnings with `--deny-warnings`. |
| 2 | Invalid usage, unreadable input, or internal analyser failure. |

JSON and strict-data reports include schema version 1, tool and Mix versions,
input files, diagnostics, capabilities, and an error/warning summary.

## Statistics

`mix stats` reports tracked builtins, functions, aliases, external commands,
keywords, meta-commands, errors, and sessions. Its subcommands are:

```text
builtins functions aliases commands keywords meta errors sessions
never all raw reset clear week query trend since help
```

Current data is stored as `current.json` under the stats directory and rotates
by ISO week. The implementation can batch data into `mix.db` and delegates SQL
queries to an external `sqlite3` command when it is present.

## Files and environment

| Name | Purpose |
|---|---|
| `~/.mixrc` | Interactive and `-i -c` startup file. |
| `~/.mix_history` | Readline history. |
| `node.conf.mix` | Read-only broker address input; see [Serve mode](serve-mode.md). |
| `COSMIX_NODE_CONFIG` | Overrides the node configuration file path. |
| `COSMIX_SRC` | Overrides the Cosmix source/documentation root used by project and manual commands. |
| `COSMIX_ETC` | Overrides the Cosmix configuration directory. |
| `COSMIX_BIN` | Overrides the install directory used by `mix build`. |
| `COSMIX_MAN_SOURCE` | Selects `auto` or `local` manual resolution. |
| `COSMIX_MAN_URL` | Overrides the canonical manual base URL. |
| `XDG_CACHE_HOME` | Sets the manual cache root; it must be absolute to be used as an XDG path. |
| `XDG_STATE_HOME` | Sets the usage-statistics root; it must be absolute. |
| `MIX_STATS=0` | Disables statistics for the process. |
| `MIX_DEBUG` | Enables warnings for statistics path and persistence failures. |
| `HOME` | Supplies rc, history, cache, state, and user-mode defaults. |
| `PATH` | Supplies external-command discovery and completion. |
| `RUST_LOG` | Overrides serve-mode log filtering. |

Manual pages are fetched in automatic mode, cached for 24 hours, and fall back
to a local checkout or stale cache when the network request fails. Local mode
does not fetch from the network.
