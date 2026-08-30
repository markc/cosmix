# Usage statistics

`mix stats` records how Mix is used and reports either dynamic usage or static
authorship coverage. Recording is best-effort: a missing, read-only, corrupt,
or busy state store never changes program output or exit status.

## Collection modes

| Invocation | Mode | Script label |
|---|---|---|
| interactive REPL | `interactive` | none |
| `mix file.mix` | `script` | basename only |
| `mix -c CODE` | `c` | none |
| `mix -` | `stdin` | none |
| `mix --serve file.mix` | `serve` | basename only |

Calls in functions, callbacks, handlers, `source`, and `include` retain the
top-level mode and script label. Paths are never stored. Basenames are truncated
to 96 Unicode characters; each weekly document keeps at most 128 distinct
labels and merges overflow into `(other)`.

The standard prelude and `~/.mixrc` load before collection is attached, so
bootstrap code does not pollute the invoking program's figures. Remote ssh
execution needs no special mode: the far-end process records its normal
`script`, `c`, or `stdin` invocation.

Enabled collection is not free. Exact statement/function counts make recursive
functions use the canonical evaluator path instead of the native sync-recursion
fast path, and tracked calls perform builtin membership lookup. This can be
measurable in tight recursive or call-heavy workloads; use `MIX_STATS=off` when
that cost matters.

## Disable collection

Set `MIX_STATS=off`; `0` and `false` are synonyms, ignoring ASCII case and
surrounding whitespace. This is the documented setting for remote and mesh
servers:

```text
MIX_STATS=off mix -c 'print("unrecorded")'
```

The setting is read once per process. When disabled, Mix allocates no collector
and opens no state file, database, or lock. Every runtime tracking hook first
checks the same cached boolean and returns before borrowing collector state,
formatting errors, or scanning builtin names. `mix stats`, including `coverage`,
prints a disabled message without reading its target.

## Storage and concurrency

State lives under `$XDG_STATE_HOME/mix`, falling back to
`$HOME/.local/state/mix`. One-shot runs accumulate counters in memory and flush
once after evaluation, including after `exit(code)` and ordinary uncaught
errors. Long-lived processes retain daily UTC buckets so a run crossing
midnight or an ISO-week boundary is split truthfully.

`current.json` is the current ISO week; archived weeks are `YYYY-WNN.json`.
Schema 2 adds daily context buckets while retaining the original aggregate
fields. A schema-1 file is loaded as one approximate legacy bucket on its
`last_date`; that is the limit of the old format's date precision. Unknown JSON
fields are ignored. A malformed JSON document, or one containing an
out-of-range persisted date or timestamp, is renamed with a `.corrupt` suffix
and skipped; recording continues with a fresh document and reports use the
remaining valid history. Set `MIX_DEBUG` to see the quarantine path.

Rotation stores a fingerprint of each imported `current.json` in the archive,
so retrying after a crash between archive commit and current-file removal does
not double its counters. Persisted week labels are accepted as filenames only
in canonical `YYYY-WNN` form; invalid labels are derived from bucket/session
dates instead.

`mix.db` retains the original `usage` and `sessions` tables and adds
`usage_context`, `runs`, and `stats_meta`. JSON is canonical. Mix takes one
bounded advisory lock across JSON merge/rotation and the SQLite transaction;
JSON writes use a unique same-directory temporary file and atomic rename. If
an automatic writer flush cannot acquire the lock within 200 milliseconds, the
batch is skipped so a one-shot never stalls its exit on stats contention.
Reader/report and explicit mutation paths retain a two-second lock deadline.
Details are printed only when `MIX_DEBUG` is set. Run IDs include 128 bits of
random entropy; the `runs` primary key also gates insertion into the legacy
`sessions` table, so retrying the same completed run cannot duplicate that row.

The store is required to be on a responsive local filesystem. The 200 ms writer
and two-second report bounds apply to lock acquisition, not reads, writes,
rename, `sync_all`, or SQLite; a stalled NFS/FUSE mount can therefore stall a
flush. Full asynchronous I/O deadlines are outside this local-state design.

The advisory lock coordinates Mix 0.61 and newer writers. During upgrade, stop
long-running pre-0.61 processes before starting 0.61; older processes do not
participate in this lock protocol.

In script, `-c`, stdin, and serve modes, SIGINT and SIGTERM take the normal
flush path. The interactive REPL stays alive on Ctrl-C and later flushes on EOF,
`exit`, or ordinary shutdown, but an OS SIGTERM uses the terminal process's
default termination and may lose its in-memory batch. `SIGKILL`, a panic, or
power loss can also lose the in-memory batch; stats are intentionally
best-effort.

If legacy history remains under `$COSMIX_SRC/_stats`, report commands print a
migration or acknowledgement command. History is never moved automatically.

## Reports and windows

Every usage report prints its window in the first line. The default window is
the current ISO week, and `overview` and `never` use the exact same snapshot.

```text
mix stats                 current-week overview
mix stats builtins       current-week builtin calls
mix stats functions      current-week user-function calls
mix stats aliases        current-week alias expansions
mix stats commands       current-week external commands
mix stats keywords       current-week canonical keyword executions
mix stats meta           current-week meta commands
mix stats errors         current-week errors
mix stats sessions       current-week completed runs
mix stats never          current-week builtins and keywords with zero use
mix stats modes          current-week per-mode breakdown
mix stats scripts [N]    top scripts; N defaults to 10 and is bounded to 1–100
mix stats raw            current-week schema-2 JSON snapshot
mix stats week YYYY-WNN  one ISO week
mix stats since DATE     buckets on or after YYYY-MM-DD
mix stats all            all weekly JSON documents
mix stats trend NAME     last 30 recorded days
mix stats query SQL      explicitly labelled all-time SQLite query
```

At the interactive REPL, a bare `mix stats …` line runs in-process and reports
this session's live counters. Since 0.61.1 a piped or redirected line
(`mix stats never | wc -l`, `mix stats never >> log`) runs as a real external
pipeline instead — live stats are flushed to disk first, so the child reads
current data including this session. Before 0.61.1 the plumbing was silently
dropped. See `mix man shell-mode` § "`mix` meta-commands under plumbing".

`clear NAME` removes a name from persisted counter categories. `reset` removes
the current week's JSON document. At a week boundary it archives the old
`current.json` first, and a live REPL retains older pending buckets. Week and
date arguments must use canonical `YYYY-WNN` and `YYYY-MM-DD` forms. These
mutation commands use the same lock as recording.

The canonical tracked keyword vocabulary is:

```text
if for while loop function return select print eprint parse die
try catch finally export alias break continue send address emit on
source include sh
```

Structural delimiters such as `then`, `end`, `done`, and `each` are not usage
events.

## Static authorship coverage

```text
mix stats coverage DIR
```

Coverage follows the root path explicitly named by the user, then recursively
scans sorted `.mix` files without following symlinks encountered below that
root. It tokenises and parses with Mix's real lexer and parser, then walks the
AST. Builtin call sites inside strings, heredocs, or comments cannot be mistaken
for code. Nested calls, higher-order builtins, and builtin method syntax are
counted. Calls through a function-valued expression are reported as “dynamic
calls not classifiable” and are never guessed.

The command reports used and never-authored builtins and canonical keywords.
If any selected file is unreadable or fails to lex or parse, diagnostics are
printed and the command exits non-zero without emitting a misleading coverage
report. Coverage performs no persistence writes.
