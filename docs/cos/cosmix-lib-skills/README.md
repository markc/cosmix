# cosmix-lib-skills

`cosmix-lib-skills` implements a Hermes-style learning loop for evaluating completed agent tasks, extracting reusable skills, retrieving them by relevance, refining them from later outcomes, and graduating proven guidance into project instructions. It is a substrate library in the `cos` layer of the `bus <- mix <- cos` dependency chain and also builds the `cosmix-skills-cli` maintenance and inspection tool.

The Rust library name is `cosmix_skills`.

## Scope

The crate provides:

- serialisable records for transcripts, evaluations, skills, and outcomes;
- LLM-driven task evaluation, skill extraction, and skill refinement;
- domain detection from configured path prefixes or a nearby `CLAUDE.md`;
- storage and semantic retrieval through an indexd Unix socket;
- confidence filtering and prompt-section formatting;
- supersede-don't-delete skill versioning;
- threshold-based graduation into a dedicated section of `CLAUDE.md`;
- a CLI for exercising the loop and maintaining the knowledge index.

The detailed command reference is in [cli.md](cli.md).

## Package targets

| Target | Kind | Purpose |
|---|---|---|
| `cosmix_skills` | library | Public data types, clients, and learning-loop functions |
| `cosmix-skills-cli` | binary | Learning-loop, indexing, audit, and reporting commands |

The package declares no Cargo features.

## Public data types

### `SkillDocument`

`SkillDocument` is the stored form of a reusable skill. It records:

- identity, version, domain, trigger, and approach;
- required tools and known failure modes;
- confidence, use count, success count, and last-use date;
- creation and update dates;
- graduation state;
- optional source commit and source file provenance;
- an optional replacement chunk ID when the version is superseded.

`SkillDocument::to_markdown` renders the searchable content stored in indexd. The complete structure is also serialised as JSON metadata.

### `TaskTranscript`

`TaskTranscript` captures the input to evaluation and extraction:

- task description and system prompt;
- ordered `Message` values;
- recorded `ToolCall` values;
- final output;
- duration, token count, and success state.

`Message` contains a role and content. `ToolCall` contains a name, input, and output.

### `EvalScore`

`EvalScore` contains `success` and `novelty` ratings on a 1–5 scale plus the evaluator's reasoning.

`EvalScore::worth_extracting` returns true only when both ratings are at least 3.

### `TaskOutcome`

`TaskOutcome` reports whether a stored skill helped a later task. It carries the skill ID, success state, notes, and task duration.

### Index responses

`StatsResponse` and `SourceCount` describe index size, model state, cache counters, and counts by source.

`StaleResponse` groups `StaleChunk` values into never-retrieved, low-value, and long-dormant buckets.

## Learning-loop functions

### `evaluate_task`

`evaluate_task` sends a truncated task summary to an `LlmClient`. It returns `Some(EvalScore)` only when the response meets the extraction threshold; routine or unsuccessful work returns `None`.

### `extract_skill`

`extract_skill` asks the LLM to convert a successful transcript into a `SkillDocument`. A new skill starts at version 1 with confidence `0.5`, one use, one success, and the domain detected from the current directory.

### `retrieve_skills`

`retrieve_skills` searches within the current detected domain. It applies the configured minimum-confidence threshold before returning skill IDs and documents.

`retrieve_skills_domain` accepts an explicit domain. Passing `None` searches all domains.

### `format_skills_for_prompt`

`format_skills_for_prompt` renders retrieved skills as a Markdown system-prompt section. An empty input produces an empty string.

### `refine_skill`

`refine_skill` sends the existing skill and a later outcome to the LLM. It increments the version and usage counters, preserves provenance and creation time, and stores the result as a new index chunk.

The previous chunk remains stored with its `superseded_by` field set to the new chunk ID. Normal searches omit superseded versions.

### `check_graduation`

`check_graduation` compares confidence, use count, and success count with configured thresholds. An eligible skill is appended to the `Graduated Skills (auto-generated)` section of the domain's `CLAUDE.md`, then marked as graduated in indexd.

The function does not rewrite the human-authored section above that marker. It returns false for an already-graduated or below-threshold skill.

## Domain detection

`detect_domain` resolves a path in this order:

1. Load configured domain-prefix mappings and choose the matching mapping.
2. Walk towards the user's home directory looking for `CLAUDE.md`.
3. Derive a slash-separated domain from the project path.
4. Return `general` when no project domain can be found.

`detect_domain_cwd` applies the same process to the current working directory. Failure to read the current directory also returns `general`.

## `IndexdClient`

`IndexdClient` is an asynchronous, newline-delimited JSON client over a Unix socket.

`IndexdClient::from_config` uses the configured indexd socket. `IndexdClient::connect` accepts an optional socket path.

The typed operations are:

- `store_skill`;
- `search_skills` and `search_skills_domain`;
- `update_skill` and `supersede_skill`;
- `list_skills` and `delete_skill`;
- `stats` and `stale`;
- `embed`.

`raw_request` exposes indexd actions that have no typed wrapper.

## LLM client

The crate re-exports `cosmix_llm::LlmClient` as `cosmix_skills::LlmClient` for backwards compatibility. Evaluation, extraction, and refinement use its `complete` operation and require JSON-shaped LLM responses.

The response parser accepts plain JSON and JSON wrapped in a Markdown code fence.

## Error behaviour

Fallible public operations return `anyhow::Result`.

Index requests fail when the socket cannot be opened, indexd returns an error object, the response is missing required fields, or response JSON cannot be decoded.

LLM loop operations add context for evaluation, extraction, refinement, and JSON-decoding failures.

Graduation also fails when the target `CLAUDE.md` cannot be found, read, or written.

## Dependencies

The crate uses:

- `cosmix-lib-config` for service settings, runtime paths, and domain mappings;
- `cosmix-lib-llm` for configured LLM completion;
- Tokio for asynchronous Unix-socket I/O;
- Serde and `serde_json` for transcripts, skill metadata, and protocol messages;
- Chrono for skill dates;
- Clap for the CLI;
- Tracing and `tracing-subscriber` for diagnostics;
- `directories` for the user home directory.

