# cosmix-indexd configuration

`cosmix-indexd` loads one `IndexdSettings` document at startup. The document uses the `.conf.mix` format. The daemon does not reload it while running.

## Lookup order

Without `--config`, lookup proceeds in this order:

1. `/etc/cosmix/indexd/config.conf.mix`
2. `~/.config/cosmix/indexd.conf.mix`

The CosMix configuration store materialises default settings when the user-mode file is missing.

With `--config PATH`, the daemon loads only `PATH`.

Only a missing system-mode file falls through to user mode. Permission errors, other I/O errors, and parse errors are fatal. A missing explicit file is also fatal.

The legacy `config.toml` name is not read.

## Top-level surface

The source uses these top-level settings:

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | integer | Configuration schema version. |
| `service` | object | Runtime settings for the daemon. |
| `source_types` | map | Accepted source names and their metadata contracts. |

The crate's configuration test demonstrates the `.conf.mix` object form:

```text
schema_version: 7
service: { socket_path: "/run/cosmix/indexd.sock" }
```

Other fields may be supplied by the configuration type's defaults. This crate reads only the fields documented below.

## Service settings

| Field | Type | Meaning |
|---|---|---|
| `service.socket_path` | string | Unix socket path used when socket activation does not provide a listener. |
| `service.vectors_db` | string | SQLite vector database path. |
| `service.model_id` | string | Hugging Face model repository identifier. |
| `service.dtype` | string | Model precision. The value `f32` selects `f32`; other values select `f16`. |
| `service.idle_timeout_secs` | integer | Seconds after embedding activity before unloading the model. Zero disables idle unload. |

The daemon creates the database parent directory when needed.

When it creates its own Unix socket, it creates the parent directory, removes a stale socket path, binds the listener, and sets mode `0666`.

## Source-type policy

`source_types` maps each accepted non-empty source name to a policy with:

| Field | Type | Meaning |
|---|---|---|
| `required` | array of strings | Metadata fields that must exist and must not be null. |
| `date_field` | optional string | Metadata field that must contain a `YYYY-MM-DD` date. |

`store` applies the policy to every non-empty metadata string when the request also has a non-empty source.

Validation behaves as follows:

- empty metadata or an empty source bypasses source-type validation;
- metadata must parse as JSON;
- an unknown source is rejected and the error names the loaded configuration path;
- every configured required field must exist and be non-null;
- a configured date field must be a string shaped as a valid year, month, and day.

The date validator accepts years from 1970 through 9999, months from 1 through 12, and days from 1 through 31. It does not validate the day against the selected month.

Changing a source-type policy requires a daemon restart because the parsed settings are stored once at startup.

## `index_file` source and domain selection

When an `index_file` request omits `source`, the daemon selects:

- `doc` for a path containing `/_doc/`;
- `journal` for a path containing `/_journal/`;
- `doc` for any other path.

An automatically detected journal whose filename does not start with a date-shaped `YYYY-MM-DD` prefix is changed to `doc`. A date-shaped but invalid prefix remains a journal so metadata validation reports the error. An explicitly supplied source is never changed.

When `domain` is absent, the daemon loads the separate `domains` service settings and resolves the file path through its domain map. It uses `general` when no mapping resolves.

## Overrides

Precedence for model precision is:

1. `--f32`
2. `service.dtype`

The `COSMIX_VECTORS_DB` environment variable overrides `service.vectors_db`.

## Socket activation

The daemon first attempts systemd-compatible socket activation.

It accepts file descriptor 3 when:

- `LISTEN_PID` equals the daemon's process identifier;
- `LISTEN_FDS` parses as an integer of at least one.

If those conditions are not met, it binds `service.socket_path` itself.

## Model files and cache

The configured model repository must provide:

- `config.json`;
- `tokenizer.json`;
- `model.safetensors`.

The daemon first checks the local Hugging Face cache for all three files. If any file is absent, it uses `hf-hub` to fetch the required files.

The model runs on the CPU. The tokenizer's model-specific position count provides a hard maximum sequence length; direct inputs beyond that length are truncated before inference. Markdown indexing splits oversized content earlier to keep chunks below the trained range.

## Observable configuration

The property surface exposes the effective runtime values:

- `config.socket_path`;
- `config.model_id`;
- `config.dtype`;
- `config.idle_timeout_secs`;
- `config.embed_dim`.

`config.embed_dim` is fixed at 768 by the daemon and is not read from the configuration file.
