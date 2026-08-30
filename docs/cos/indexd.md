# cosmix-indexd — vector knowledge base + indexer

**`cosmix-indexd` is the mesh's semantic memory: it embeds markdown into
vectors, stores them in SQLite, and answers similarity search — so an agent can
recall docs, journals, and skills by meaning rather than keyword.** It
auto-indexes workspaces on commit, keeping recall fresh without a manual step.

## What it is

A long-running Rust daemon that runs a text-embedding model
(`nomic-embed-text-v1.5`, via [candle](https://github.com/huggingface/candle))
in-process and keeps a vector store in SQLite. It is the retrieval backend
behind the knowledge loop: the `context_search` an agent runs before a task,
and the skills/docs/journal recall the [mcp](mcp.md) bridge exposes.

Content is chunked by markdown section, embedded, and stored with source
metadata (`_doc`, `_journal`, `_memory`, `_plan`, `_spec`). Search embeds the
query the same way and returns the nearest chunks by cosine distance.

## What it does

- **Embed + store** markdown chunks as L2-normalised vectors in SQLite.
- **Semantic search** — nearest-neighbour recall over stored chunks, filtered by source type or metadata, top-`k` by distance.
- **Auto-index on commit** — a git post-commit hook calls `indexd.index_file` per changed file, so a workspace re-indexes incrementally as it changes (never a full re-index).
- **Incremental single-file indexing** — `index_file` splits, embeds, and upserts one file; a background mode enqueues it fire-and-forget.
- **Model lifecycle** — loads the embedding model on demand and can unload to return RSS to the OS.

## Running it

```sh
/opt/cosmix/bin/cosmix-indexd
```

Runs under systemd as `cosmix-indexd.service` (identity `User=cosmix-indexd`).
Config loads from `/etc/cosmix/indexd/config.conf.mix` (system) or the XDG
user path (the legacy `.toml` fallback was removed). The daemon also listens on
a Unix socket (`embed.sock`, path from config) for a raw embedding API used by
in-process consumers, alongside the Bus surface. The embedding model is fetched
and cached via the Hugging Face hub cache on first use.

## Interfaces

- **Bus service `indexd`** — registered on the local broker.

| Verb | Purpose |
|---|---|
| `indexd.search` | semantic search: `{query, limit, ...}` → ranked chunks |
| `indexd.index_file` | embed + store one file: `{path}` |
| `indexd.props.{list,get,watch}` | SPEC-12 property surface |

- **Unix socket** (`embed.sock`) — a local embedding API for direct consumers.

Example Bus call:

```text
bus_call("indexd", "indexd.search", {"query": "mesh DNS zones", "limit": 5})
```

## Where it fits

Depends on `cosmix-lib-config`, `cosmix-lib-props-store` (SPEC-12 state), and
`cosmix-lib-daemon`, plus candle + a SQLite backend. It registers on the local
[cosmix-noded](noded.md) broker. The [cosmix-mcp](mcp.md) bridge is its main
client — `context_search`, `skills_*`, and the `*_feedback` tools all route
through indexd — and git hooks in indexed workspaces call it directly.

## See also

- [mcp](mcp.md) — the Claude Code bridge that drives indexd's recall + skills
- [noded](noded.md) — the Bus broker indexd registers with
- [libraries](libraries.md) — `cosmix-lib-skills`, `cosmix-lib-props-store`
- [overview](overview.md) — the daemon family at a glance
