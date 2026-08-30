# MCP tools

`cosmix-mcp` exposes tools through the MCP `tools` capability. Parameter schemas are generated from the Rust request types. Unless noted otherwise, each tool returns text; JSON values are encoded as JSON text and operational failures begin with `ERROR:`.

## Bus tools

| Tool | Parameters | Description |
|---|---|---|
| `bus_call` | `to` string, `command` string, optional `args` string | Call a Bus command on a named service. `args` contains JSON text. Missing or invalid JSON becomes `null`. |
| `bus_list_services` | None | Return the broker service inventory, including available build provenance. |
| `bus_node_info` | None | Call `noded.info` for node identity, broker build, uptime, and registered-service count. |
| `bus_list_peers` | None | Call `noded.peers` and return the known mesh peers. |
| `noded_ping` | None | Call `noded.ping` to check broker connectivity. |

The broker connection is established on the first Bus-dependent call and retained for the process lifetime. A connection failure advises that `cosmix-noded` must be running.

Example tool arguments:

```json
{
  "to": "edit",
  "command": "edit.get-content",
  "args": "{\"path\":\"/tmp/example.md\"}"
}
```

## Log tools

| Tool | Parameters | Description |
|---|---|---|
| `log_tail` | `file` string, optional `lines` integer | Return the last lines from a log. `lines` defaults to 50. |
| `log_search` | `file` string, `pattern` string, optional `limit` integer | Return the newest case-insensitive substring matches. `limit` defaults to 20. |

The name `bus` resolves to `bus.log`. Other names first resolve as an exact entry in the CosMix user log directory. If no exact entry exists, the lexically last filename with the supplied prefix is selected.

`log_search` returns `No matches found` when the file is readable but contains no matching line.

## Context tools

| Tool | Parameters | Description |
|---|---|---|
| `context_search` | `query` string, optional `domain` string, optional `limit` integer | Search indexed project knowledge. The per-source limit defaults to 3. |
| `index_workspace` | optional `path` string, optional `filter` string | Index recognised Markdown content beneath a workspace. |
| `knowledge_digest` | optional `domain` string | Return index totals, source and domain counts, skill confidence tiers, popular skills, and stale-content counts. |
| `knowledge_brief` | `task` string, optional `domain` string, optional `limit` integer | Build a compact task briefing from skills, document pointers, and recent journals. The skill limit defaults to 3. |

For domain-aware tools, an omitted or empty domain is inferred from the current working directory. `context_search` accepts `all` to disable domain restriction. `knowledge_brief` also treats `all` as unrestricted.

`context_search` searches the selected domain first and backfills from other domains when a source returns fewer than the requested limit. Its result object can contain:

| Result key | Indexed material |
|---|---|
| `skills` | Active skill documents. |
| `docs` | Documentation chunks. |
| `specs` | Specification chunks. |
| `plans` | Plan chunks. |
| `notes` | Persistent note chunks. |
| `journals` | Journal chunks subject to ageing policy. |
| `memory` | Generated memory chunks subject to ageing policy. |
| `source` | Rust documentation comments. |
| `observations` | Tool-generated code observations. |
| `scripts` | Mix scripts, when matches exist. |

Search ranking applies source trust weights from shared knowledge settings. Journal and memory results also apply configured temporal decay and maximum age.

### Workspace indexing

When `path` is absent, `index_workspace` uses the server's current working directory. Every `~` substring in a supplied path is replaced with `HOME`.

The scanner recognises these project entries:

| Entry | Indexed source |
|---|---|
| `_doc/` | `doc` |
| `_decisions/` | `doc` |
| `_journal/` | `journal` |
| `_memory/` | `memory` |
| `_plan/` | `plan` |
| `_spec/` | `spec` |
| `_notes.md` | `notes` |

Markdown is collected recursively. Nested recognised content roots are indexed under their own source instead of through the parent root.

The scanner skips `.git`, `node_modules`, `target`, `.venv`, `vendor`, `.direnv`, `dist`, and `build` directories.

`filter` accepts either a filename substring or a glob containing `*` or `?`. Each file is split at `##` headings. Sections of 50 bytes or fewer are omitted.

Before storing a processed file, the tool deletes indexed entries whose metadata path exactly matches that file. This makes repeated indexing replace the file's previous chunks.

## Skill tools

| Tool | Parameters | Description |
|---|---|---|
| `skills_retrieve` | `task` string, optional `domain` string, optional `limit` integer | Retrieve matching active skills with IDs and lifecycle metadata. |
| `skills_store` | `name`, `trigger`, `approach`; optional `domain`, `tools_required`, `failure_modes`, `confidence`, `source_commit`, `source_file` | Store version 1 of a skill. Confidence defaults to 0.5. |
| `skills_refine` | `id` integer, `success` boolean, `notes` string | Refine a stored skill from a task outcome using the configured language-model backend. |
| `skills_graduate` | `id` integer | Promote a skill into project instructions, bypassing normal thresholds when necessary. |
| `skills_list` | optional `limit` integer, optional `offset` integer | List stored skills. Defaults are 20 and 0. |
| `skills_delete` | `id` integer | Delete a stored skill. |

`skills_retrieve` uses the configured maximum when `limit` is absent. If configuration cannot be loaded, its fallback is 3.

`skills_store` infers an omitted domain from the current working directory. `tools_required` and `failure_modes` default to empty arrays. Stored records begin with one use and one success.

`skills_refine` loads the existing record, sends the outcome to the configured refinement backend, stores the revised version, and checks whether it qualifies for graduation.

`skills_graduate` returns without change when the skill is already graduated. For a manual promotion below normal thresholds, it supplies threshold-satisfying values to the shared graduation check.

## Feedback tools

| Tool | Parameters | Description |
|---|---|---|
| `docs_feedback` | `id` integer, `useful` boolean | Update retrieval feedback for a document chunk. |
| `journal_feedback` | `id` integer, `useful` boolean | Update retrieval feedback for a journal chunk. |
| `memory_feedback` | `id` integer, `useful` boolean | Update retrieval feedback for a generated memory chunk. |
| `journal_supersede` | `old_id` integer, `new_id` integer, optional `reason` string | Mark an older journal chunk as superseded by a newer chunk. |

Superseding retains the old journal entry for audit and rollback but filters it from later `context_search` results.

## Mix tool

| Tool | Parameters | Description |
|---|---|---|
| `mix_execute` | `script` string, optional `cwd` string | Parse and execute inline Mix source and return captured output. |

The evaluator runs on a dedicated thread with JSON, regular expression, TOML, date and time, URL, and cryptographic facilities enabled.

If a broker connection is available, Mix `send`, `emit`, and `port_exists` route through Bus. A failed attempt to establish the broker does not prevent a script that does not require Bus from running.

`send` preserves Bus return-code bands. Transport failures remain Mix transport errors, while application replies retain their return codes. Values that cannot be encoded as JSON produce an error rather than a modified payload.

`emit` is fire-and-forget. Payload encoding errors are reported; the send result itself is discarded.

`port_exists` obtains the current broker service list and returns false if that lookup fails.

The MCP process does not deliver incoming events to long-lived Mix `on` handlers.

When `cwd` begins with `~/`, the prefix is expanded from `HOME`. A directory change applies to the MCP process and therefore remains in effect for later operations.

Successful execution returns standard output. Empty output becomes `(no output)`. When standard error is present it follows a `--- stderr ---` separator. Parse and runtime failures include any output already produced.

## Status tool

| Tool | Parameters | Description |
|---|---|---|
| `mcp_status` | None | Return process start time, uptime, broker state, aggregate counts, per-tool counts, and recent calls. |

The recent-call queue retains at most 50 records, newest first. Each record contains the tool name, UTC timestamp, elapsed milliseconds, and success flag.

`broker_connected` means that the lazy broker client has been initialised. It does not perform a fresh connectivity probe; use `noded_ping` for that.

## Recommended knowledge sequence

The server advertises this workflow to MCP clients:

1. Call `context_search` at the start of a non-trivial task.
2. Review the returned skills, documents, and journals.
3. Call `skills_store` after successful non-trivial work worth retaining.
4. Call `skills_refine` after using a retrieved skill.
5. Call `index_workspace` to refresh recognised workspace content.
6. Send useful or not-useful feedback for every retrieved document, journal, or memory chunk used.
7. Call `journal_supersede` when a later journal directly invalidates an earlier one.
8. Call `knowledge_brief` before delegating a non-trivial task.

## Logging and redaction

Every tool invocation passes through one instrumentation point.

Normal call logs include the tool name, redacted argument shapes, duration, outcome, and a shape-only result summary. String argument values and result bodies are not logged verbatim. Numeric, Boolean, and null arguments remain visible.

`RUST_LOG=cosmix_mcp=trace` adds the complete request argument object. Protocol error details are also trace-only because deserialisation messages can echo caller input.

`mcp_status` contains call metadata but no arguments or result bodies.
