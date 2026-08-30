# cosmix-indexd

`cosmix-indexd` is the CosMix semantic indexing and vector-storage daemon. It embeds text with a Nomic BERT model through Candle, stores 768-dimensional vectors in SQLite through `sqlite-vec`, and serves indexing and retrieval operations over Bus and a local Unix socket. It belongs to the downstream `cos` repository in the `bus <- mix <- cos` dependency chain: it uses Bus protocol and client crates together with `cos` daemon, configuration, logging, and property-store substrate.

## Synopsis

```text
cosmix-indexd [--config PATH] [--f32]
cosmix-indexd --version
```

The daemon:

- loads configuration before accepting requests;
- opens or creates the vector database;
- accepts newline-delimited JSON on a Unix socket;
- registers the Bus service `indexd` when the broker is available;
- loads the embedding model on the first embedding request;
- optionally unloads the model after an idle period;
- reconnects to the Bus broker after connection loss.

The local socket remains available while Bus registration is unavailable.

## Command-line options

| Option | Meaning |
|---|---|
| `-c PATH`, `--config PATH` | Load the named `.conf.mix` file. A missing, unreadable, or invalid explicit file is fatal. |
| `--f32` | Force `f32` model precision, overriding the configured `dtype`. |
| `--version` | Print the crate version, Git revision, and build time. |

See [configuration.md](configuration.md) for configuration lookup and fields.

## Client surfaces

The daemon exposes the same data operations through two transports.

The Bus surface registers as service `indexd`. Commands normally use names such as `indexd.search` and carry their request fields as Bus arguments. Responses use Bus return code `0` for success and `10` when the generated JSON contains a top-level error.

The Unix socket accepts one JSON object per line. Each request contains an `action` field. Each response is one JSON object followed by a newline.

```json
{"action":"search","query":"property snapshots","limit":5,"source":"doc"}
```

See [verbs.md](verbs.md) for the complete request surface.

## Operations

| Action or Bus verb | Purpose |
|---|---|
| `embed`, `indexd.embed` | Produce embeddings for one or more texts. |
| `store`, `indexd.store` | Embed and persist text chunks with source and metadata. |
| `search`, `indexd.search` | Run nearest-neighbour search with optional source and metadata filters. |
| `update`, `indexd.update` | Change a chunk and re-embed it when its content changes. |
| `delete`, `indexd.delete` | Delete chunks and their vectors. |
| `list`, `indexd.list` | List stored chunks with pagination and optional source filtering. |
| `feedback`, `indexd.feedback` | Add positive or negative relevance feedback to a chunk. |
| `supersede`, `indexd.supersede` | Mark an older chunk as replaced while retaining it for audit. |
| `stale`, `indexd.stale` | Report never-used, low-value, and dormant chunks. |
| `index_file`, `indexd.index_file` | Split and index a Markdown file. |
| `stats`, `indexd.stats` | Report corpus, database, model, circuit-breaker, and cache state. |

## Embedding model

The daemon loads `config.json`, `tokenizer.json`, and `model.safetensors` from the Hugging Face cache when all three are present. It fetches missing model files through `hf-hub`.

Embedding runs on the CPU in `f16` or `f32`. Input receives a task prefix:

- documents use `search_document: `;
- queries use `search_query: `;
- direct `embed` requests default to `search_document: ` and may supply another prefix.

Mean-pooled embeddings are L2-normalised. The fixed database vector width is 768.

Model loading and inference have separate circuit breakers. A model-load breaker opens after two consecutive failures and retries after 60 seconds. An inference breaker opens after three consecutive failures or timeouts and retries after 30 seconds. A single inference batch has a 120-second wall-clock limit.

The in-memory embedding cache keys entries by text and prefix. It holds at most 512 entries and expires entries after five minutes.

## Vector store

The database uses SQLite in WAL mode with `sqlite-vec`. A `chunks` table holds content, source, metadata, timestamps, feedback, retrieval counters, supersession state, and a content hash. A `vec_chunks` virtual table holds the vectors.

The content hash covers the source and text. Re-storing the same source and text returns the existing identifier and may refresh its metadata without recomputing an embedding.

Search:

- excludes chunks whose `superseded_by` field is set;
- supports an exact source filter;
- supports JSON metadata comparisons;
- records retrieval count and last-retrieved time;
- adjusts vector distance with explicit feedback, repeated unrewarded retrievals, and age.

Lower adjusted distance ranks first.

## Markdown indexing

`index_file` accepts a path and may also accept caller-supplied content. Supplied content takes precedence and lets a caller index a file that the daemon cannot read directly.

The indexer:

- detects `doc` and `journal` sources from `_doc` and `_journal` path components when no source is supplied;
- resolves a domain from the separate domains settings when no domain is supplied;
- splits Markdown at level-two headings;
- ignores sections of 50 bytes or fewer;
- splits oversized sections at a 6,000-character budget;
- stores path, filename, section, domain, type, and date metadata;
- removes obsolete chunks only after every replacement section stores successfully.

A request may set `background` to `true`. The daemon then acknowledges the request immediately and processes it through a single serial background worker.

## Properties and events

The Bus property surface implements:

- `indexd.props.get`;
- `indexd.props.list`;
- `indexd.props.describe`;
- `indexd.props.watch`.

`props.watch` is also accepted as an alias. It returns the topic name that the caller must subscribe to.

Queryable paths cover:

- `config.socket_path`, `config.model_id`, `config.dtype`, `config.idle_timeout_secs`, and `config.embed_dim`;
- `lifecycle.started_at`, `lifecycle.uptime_s`, `lifecycle.health`, `lifecycle.props_level`, `lifecycle.model_loaded`, `lifecycle.model_circuit`, and `lifecycle.embed_circuit`;
- `corpus.chunks`, `corpus.bytes_db`, and `corpus.kinds`.

The daemon publishes retained full snapshots to `world.indexd` and non-retained leaf changes to `indexd.props.changed`. It polls once per second, ignores transient uptime changes, and debounces corpus-only changes to at most one publication every ten seconds.

## Limits

| Limit | Value |
|---|---:|
| Texts in one `embed` or `store` request | 256 |
| Metadata filters in one `search` request | 32 |
| Chunks produced by one `index_file` request | 4,000 |
| Markdown chunk character budget | 6,000 |
| Embedding timeout | 120 seconds |

## Cargo features

This crate defines no crate-specific Cargo features.

## Implementation layout

| File | Role |
|---|---|
| `src/main.rs` | CLI, configuration lookup, model lifecycle, vector database, request dispatch, Bus registration, and Unix socket server. |
| `src/props.rs` | SPEC 07 property snapshot, path listing, and descriptions. |
| `src/world.rs` | Property diff loop and Bus topic publication. |
| `build.rs` | Build revision and timestamp metadata. |
