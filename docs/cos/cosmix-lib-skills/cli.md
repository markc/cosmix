# cosmix-skills-cli

`cosmix-skills-cli` exercises the skill learning loop and provides index maintenance, health reporting, source indexing, and workspace audit commands.

## Invocation

From the workspace:

```sh
cargo run -p cosmix-lib-skills --bin cosmix-skills-cli -- [GLOBAL OPTIONS] COMMAND
```

The process writes diagnostics, including the resolved domain, to standard error. Command results and Markdown reports go to standard output.

## Global options

| Option | Meaning |
|---|---|
| `--backend NAME` | Select an LLM backend by name |
| `--domain DOMAIN` | Use an explicit project-domain filter |
| `--all-domains` | Disable domain filtering |
| `--socket PATH` | Override the indexd Unix socket |

Backend selection uses the command-line value first, then the skills service's backend override, then the LLM client's configured default.

Domain selection uses `--all-domains` first, then `--domain`, then automatic detection from the current directory.

Socket selection uses `--socket` first and the indexd service setting otherwise.

## Learning-loop commands

| Command | Arguments | Operation |
|---|---|---|
| `evaluate` | `PATH` | Evaluate a transcript JSON file and report whether it is worth extracting |
| `extract` | `PATH` | Extract a skill from a transcript and print the skill as JSON |
| `learn` | `PATH` | Evaluate, extract, store, and verify a skill |
| `search` | `QUERY`, `-n LIMIT` | Search for skills; the default limit is 5 |
| `list` | `-n LIMIT`, `--offset OFFSET` | List stored skills; defaults to 20 rows from offset 0 |
| `refine` | `ID`, `--success`, `--notes TEXT` | Refine one skill from a reported outcome |
| `delete` | `ID` | Delete one skill chunk |
| `format` | `QUERY`, `-n LIMIT` | Print the prompt section generated from matching skills; the default limit is 3 |
| `sample-transcript` | none | Print a sample `TaskTranscript` as JSON |

`extract` performs manual extraction without first evaluating the transcript.

`learn` stops without storing a skill when evaluation does not meet the success and novelty threshold.

`refine --success` records a successful use. Omitting the flag records an unsuccessful use. Refinement stores a new version and marks the old chunk as superseded.

`delete` removes data. It does not preserve a replacement version.

## Index inspection and lifecycle commands

| Command | Arguments | Operation |
|---|---|---|
| `stats` | `-n BENCH_RUNS` | Print index statistics and run search-latency probes; defaults to 5 probes |
| `dashboard` | none | Print index totals, confidence tiers, popular skills, unused skills, and stale counts |
| `graduate-all` | none | Check every active skill against graduation thresholds |
| `staleness-report` | `--source SOURCE` | Print the three index staleness buckets |
| `skill-composition` | none | Compare active skills for trigger and tool overlap |

`stats -n 0` suppresses latency probing.

`graduate-all` may modify a domain's `CLAUDE.md`. It skips superseded records and reports graduation failures without stopping the sweep.

`staleness-report` uses fixed report thresholds:

- never retrieved and older than 90 days;
- retrieved more than three times with non-positive feedback;
- not retrieved for 180 days.

`skill-composition` uses word-set similarity for triggers and set overlap for tool lists. Co-retrieval analysis is reported as not implemented.

## Workspace indexing commands

| Command | Arguments | Operation |
|---|---|---|
| `bootstrap-index` | `--path PATH`, `--filter TEXT`, `--skip-delete` | Index tracked project documents from one Git workspace |
| `bootstrap-all-workspaces` | `--skip-delete` | Index documents and source from every configured domain workspace |
| `index-source` | `--path PATH`, `--filter TEXT` | Index Rust doc comments and Mix scripts |
| `delete-paths` | `PATH...` | Remove indexed document, journal, and memory chunks for absolute file paths |

### `bootstrap-index`

The target must be a Git repository. The command uses `git ls-files`, so ignored and untracked files are not included.

It recognises:

- Markdown under `_doc/` as `doc`;
- Markdown under `_journal/` as `journal`;
- Markdown under `_memory/` as `memory`;
- `_notes.md` as `doc`.

Markdown is split at level-two headings. Sections shorter than 50 characters are omitted. Sections longer than 8,000 characters are truncated before storage.

By default, existing chunks for each file are deleted before new chunks are stored. This makes re-indexing idempotent. `--skip-delete` avoids that pass but can leave duplicates.

`--filter TEXT` selects files whose workspace-relative path contains the supplied text.

### `bootstrap-all-workspaces`

Workspace paths come from configured domain mappings. Paths without a `.git` directory are skipped.

For every accepted workspace, the command runs both document bootstrapping and source indexing. An error in one workspace is reported and does not prevent the remaining workspaces from being attempted.

### `index-source`

Rust source files come from `git ls-files`. Each contiguous `//!` module comment and `///` item comment becomes an index chunk. Item chunks include the following item signature when one can be identified.

Each tracked `.mix` file and `_mixrc` is stored as one `mix-script` chunk.

`--filter TEXT` retains only tracked paths containing the supplied text.

### `delete-paths`

`delete-paths` searches the `doc`, `journal`, and `memory` sources for metadata containing each supplied absolute path, then deletes the matching chunk IDs.

This command is destructive. An empty path list performs no work.

## Audit and report commands

| Command | Arguments | Operation |
|---|---|---|
| `doc-freshness` | `--path PATH` | Report documents whose referenced source paths changed after the document |
| `claude-md-audit` | `--path PATH` | Compare `CLAUDE.md` crate tables and selected references with a workspace |
| `memory-hygiene` | `--stale-days DAYS` | Check memory entries, references, frontmatter, age, and orphan files |
| `doc-coverage` | `--path PATH`, `--days DAYS` | Compare recent crate activity with document references |
| `contradiction-check` | `--path PATH` | Find conflicting recommendations in skills and project documents |

The default path for `doc-freshness`, `claude-md-audit`, `doc-coverage`, `contradiction-check`, and `index-source` is the current directory.

`memory-hygiene` defaults to a 30-day stale threshold. It reports findings and performs no automatic deletion.

`doc-coverage` defaults to the previous 30 days. It classifies active crates with zero or one document reference as under-documented.

`contradiction-check` scans active skills for use, avoidance, deprecation, and replacement claims. It also reports missing skill provenance files.

## Transcript input

`evaluate`, `extract`, and `learn` read a JSON-encoded `TaskTranscript`.

The required shape is:

```json
{
  "task_description": "Update the alpha service",
  "system_prompt": "You are a coding assistant.",
  "messages": [
    {
      "role": "user",
      "content": "Update the alpha service."
    }
  ],
  "tool_calls": [
    {
      "name": "read_file",
      "input": "src/service.rs",
      "output": "file contents"
    }
  ],
  "final_output": "The service was updated.",
  "duration_ms": 1200,
  "token_count": 800,
  "success": true
}
```

Invalid JSON or missing required fields terminates the command with an error.

## Configuration surface

The binary reads settings through `cosmix-lib-config`.

| Service settings | Fields used by this crate |
|---|---|
| skills | LLM backend override, minimum retrieval confidence, graduation confidence, minimum uses, minimum successes |
| indexd | Unix socket path |
| domains | path-prefix to domain mappings |
| LLM | default backend and named backend selection through `LlmClient` |

Command-line backend, domain, and socket options override the corresponding automatic or configured selection.

## Exit behaviour

Commands return a non-zero status when an unhandled file, Git, socket, protocol, configuration, LLM, or JSON error reaches `main`.

Some sweep commands handle per-item failures, print a warning, and continue. Reports with findings normally complete successfully; findings are report data rather than process failures.
