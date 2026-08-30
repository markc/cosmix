# cosmix-indexd verbs

This page describes the JSON operation surface shared by Bus and the local Unix socket.

For Bus, call service `indexd` with a command such as `indexd.search`. The command arguments become the request object.

For the Unix socket, send one JSON object per line and include `"action":"search"`. The action names below are the values accepted by that field.

All responses are JSON. Failures have the form:

```json
{"error":"description"}
```

## `embed`

Produces one 768-element embedding for each input text.

Request fields:

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `texts` | array of strings | yes | Texts to embed, in result order. |
| `prefix` | string | no | Task prefix. Defaults to `search_document: `. |

Response fields:

| Field | Type | Meaning |
|---|---|---|
| `embeddings` | array of number arrays | L2-normalised vectors in input order. |

At most 256 texts are accepted.

## `store`

Embeds and stores text chunks.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `texts` | array of strings | yes | Chunk content. |
| `source` | string | no | Configured source type. Defaults to an empty string. |
| `metadata` | array of strings | no | JSON strings aligned by index with `texts`. Missing entries become empty metadata. |

The response contains `stored`, `duplicates`, and `ids`. Exact duplicates use the source-and-content hash, return the existing identifier, and may update metadata.

Non-empty metadata is parsed as JSON. For a non-empty source, the configured source-type policy controls required fields and any date field.

At most 256 texts are accepted.

## `search`

Embeds a query with the `search_query: ` prefix and performs nearest-neighbour search.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `query` | string | yes | Search text. |
| `limit` | integer | no | Neighbour count passed to `sqlite-vec`. Defaults to 10. |
| `source` | string | no | Exact source filter. Empty means all sources. |
| `metadata_filter` | array | no | JSON metadata comparisons. |

Each metadata filter contains:

| Field | Type | Meaning |
|---|---|---|
| `field` | string | Metadata field appended to the JSON path. |
| `op` | string | `eq`, `gt`, `lt`, `gte`, `lte`, or `contains`. |
| `value` | JSON value | Comparison value. `contains` uses the string value as a substring pattern. |

At most 32 metadata filters are accepted.

The response contains `results`. Each result includes `id`, `content`, `source`, `metadata`, `distance`, `feedback_score`, `retrieval_count`, `created`, and, when set, `last_retrieved`.

Search excludes superseded chunks. Returning a result increments its retrieval count and updates its last-retrieved time.

## `update`

Changes selected fields on one chunk.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `id` | integer | yes | Chunk identifier. |
| `content` | string or null | no | Replacement content. The daemon computes and stores a new vector. |
| `metadata` | string or null | no | Replacement metadata string. |
| `source` | string or null | no | Replacement source. |

The response contains `updated` and `re_embedded`. Supplying no changes returns `updated: false`.

## `delete`

Deletes chunks and their corresponding vector rows in one transaction.

```json
{"action":"delete","ids":[41,42]}
```

The response contains the number of chunk rows deleted.

## `list`

Lists chunks in descending creation order.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `source` | string | no | Exact source filter. Empty means all sources. |
| `limit` | integer | no | Page size. Defaults to 10. |
| `offset` | integer | no | Row offset. Defaults to 0. |

The response contains `items` and `total`. Each item contains `id`, `content`, `source`, `metadata`, and `created`.

## `feedback`

Adjusts a chunk's relevance score.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `id` | integer | yes | Chunk identifier. |
| `useful` | boolean | yes | Adds one when true and subtracts one when false. |

The response returns `ok`, `id`, and the new `feedback_score`.

## `supersede`

Marks an older chunk as replaced by a newer chunk.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `old_id` | integer | yes | Chunk to hide from normal search. |
| `new_id` | integer | yes | Existing replacement chunk. |
| `reason` | string | no | Human-readable reason written to the log. |

The old chunk remains stored for audit or rollback. The operation rejects self-supersession and a missing `new_id`.

## `stale`

Reports cleanup candidates in three independent buckets.

| Field | Type | Required | Default | Meaning |
|---|---|---:|---:|---|
| `source` | string | no | empty | Exact source filter. |
| `never_retrieved_age_days` | integer | no | 90 | Minimum age for a never-retrieved chunk. |
| `low_value_min_retrievals` | integer | no | 3 | Retrieval threshold for a non-positive-feedback chunk. |
| `long_dormant_days` | integer | no | 180 | Minimum time since last retrieval. |
| `per_bucket_limit` | integer | no | 50 | Maximum rows returned in each bucket. |

The response contains `never_retrieved_old`, `low_value`, `long_dormant`, and `total_chunks`. Candidate rows include a content preview and, when present in metadata, path and filename.

## `index_file`

Indexes a Markdown file as independently searchable sections.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `path` | string | yes | Path used for reading and metadata. |
| `content` | string or null | no | Caller-supplied content. When present, the daemon does not read the path. |
| `source` | string | no | Explicit source. Otherwise inferred as `doc` or `journal`. |
| `domain` | string | no | Explicit domain. Otherwise resolved from domains settings, then `general`. |
| `background` | boolean | no | Queue the work and return immediately. Defaults to false. |

Foreground success returns `indexed`, `file`, `sections`, and `domain`. A background acknowledgement returns `accepted`, `queued`, and `file`.

The indexer splits on `## ` headings. Sections with at most 50 bytes are omitted. Sections over 6,000 characters are split at line boundaries or, for an overlong line, at UTF-8 character boundaries. A file may produce at most 4,000 final chunks.

When re-indexing a path, old chunks remain searchable until all new sections store successfully. The daemon then removes old identifiers not reused by deduplication.

## `stats`

Takes no request fields.

The response contains:

- `total_vectors` and `db_size_bytes`;
- `model_loaded`, `model_circuit`, and `embed_circuit`;
- `embed_cache_entries`, `embed_cache_hits`, and `embed_cache_misses`;
- `by_source`, containing source names and counts.

## Property verbs

`indexd.props.get`, `indexd.props.list`, and `indexd.props.describe` dispatch through the property store. Arguments may arrive in the Bus `args` header, structured Bus arguments, or a JSON body.

`indexd.props.watch` and `props.watch` return the `indexd.props.changed` topic name. The caller subscribes through the broker.

The daemon also publishes a retained `world.indexd` snapshot.
