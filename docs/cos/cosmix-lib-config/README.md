# cosmix-lib-config

`cosmix-lib-config` is the shared Rust settings and path-resolution library for
CosMix applications and daemons. It provides typed configuration structures,
strict-data Mix parsing, `.conf.mix` load and save operations, node configuration,
and ACME policy resolution. The crate belongs to the `cos` layer of the
`bus <- mix <- cos` dependency chain: it uses `cosmix-lib-mix` for parsing and
serialisation, and its optional client helpers depend downwards on Bus client and
wire crates. Bus and Mix do not depend on this crate.

The Cargo package is named `cosmix-lib-config`. Rust code imports it as
`cosmix_config`.

## Synopsis

```rust
use cosmix_config::{DomainsSettings, load_conf_mix_path};
use cosmix_config::store::load_service;

fn main() -> anyhow::Result<()> {
    let skills = load_service::<cosmix_config::SkillsSettings>("skills")?;
    let minimum = skills.min_confidence;

    let domains = load_service::<DomainsSettings>("domains")?;
    let domain = domains.resolve(std::path::Path::new("/srv/example"));

    let node: cosmix_config::node::NodeConfig =
        load_conf_mix_path(std::path::Path::new("node.conf.mix"))?;

    println!("{minimum}: {domain:?}: {}", node.node);
    Ok(())
}
```

This crate is a library. It defines no executable, CLI subcommands, or Bus
verbs.

## Configuration format

`.conf.mix` is the only supported configuration format. The serde bridge
accepts the strict-data subset of Mix and rejects executable constructs such as
variable references, function calls, shell commands, interpolation, control
flow, and arithmetic.

Per-service files use the name `{service}.conf.mix`. Node-wide identity and
service configuration uses `node.conf.mix`. See [Node configuration](node-config.md)
for the node schema, discovery order, listener rules, and ACME policy.

## Modules

| Module | Purpose |
|---|---|
| `acme_policy` | Validates webd ACME configuration and produces typed issuance plans and a production ToS-acceptance proof. |
| `client_helpers` | Resolves the local broker URL and opens named, provenance-bearing, or anonymous Bus client connections. |
| `mix_data` | Parses strict-data Mix text or files into an untyped `Value` tree. |
| `node` | Defines `node.conf.mix` types, loaders, derived addresses, and webd listener synthesis. |
| `paths` | Resolves source, configuration, data, binary, runtime, log, and temporary directories. |
| `store` | Loads, saves, and materialises typed per-service `.conf.mix` files. |

The private `settings` module is re-exported at the crate root.

## Per-service settings

The crate exports these typed settings structures:

| Type | Configuration surface |
|---|---|
| `DomainsSettings` | Maps filesystem path prefixes to domain names; `resolve` uses the longest matching prefix. |
| `LlmSettings` | Selects a default LLM backend and holds named backend definitions. |
| `LlmBackendConfig` | Describes HTTP, command, or Bus-backed LLM access. |
| `SkillsSettings` | Sets retrieval limits, confidence thresholds, backend selection, and graduation thresholds. |
| `KnowledgeSettings` | Sets source trust weights, journal decay, and maximum journal age. |
| `IndexdSettings` | Holds schema version, index service settings, and source-type policies. |
| `IndexdServiceSettings` | Holds vector database, embedding model, socket, idle timeout, and precision settings. |
| `SourceTypeSpec` | Declares required metadata and an optional date field for one indexed source type. |
| `MusicdSettings` | Holds SoundFont source and digest, state directory, sample rate, polyphony, and gain. |

The service settings types implement `Default`. `SourceTypeSpec` is a nested
policy value. `store::load_service` requires
`Default + DeserializeOwned + Serialize`.

## Store API

`store::load_service::<T>(name)` reads
`cosmix_path(CosmixDir::Etc)/{name}.conf.mix`. When the file does not exist, it
serialises `T::default()`, creates the configuration directory, writes the new
file, and returns the same default value.

`store::save_service(settings, name)` serialises a typed value to the service
file. `load_conf_mix_path::<T>(path)` reads and deserialises an explicit path
without applying service-name discovery.

`store::config_dir()` returns the resolved configuration directory.
`cosmix_src()` returns the resolved source directory.

Read and parse failures include the source path in the error chain. The store
does not read or upgrade legacy TOML files.

## Strict-data API

`parse_mix_data(source)` parses a string and returns `MixResult<Value>`.
`load_mix_data(path)` reads and parses a strict-data file. A read failure is
reported as `MixError::RuntimeError`; parser errors pass through unchanged.

The crate re-exports `Value`, `MixError`, and `MixResult` for callers that walk
untyped maps and lists. It also re-exports `from_conf_mix_str` and
`to_conf_mix_string` for typed serde conversion.

```rust
use cosmix_config::{Value, parse_mix_data};

fn main() -> cosmix_config::MixResult<()> {
    let value = parse_mix_data(
        "name: \"alpha\"\npriority: 2\ntags: [\"config\", \"public\"]\n"
    )?;

    if let Value::Map(map) = value {
        assert_eq!(map.get("name"), Some(&Value::String("alpha".into())));
    }
    Ok(())
}
```

## Path resolution

`cosmix_path(CosmixDir)` resolves each directory once and caches the complete
set for the process. An environment override wins over user or system defaults.

| Kind | Environment variable | User default | System default |
|---|---|---|---|
| `Src` | `COSMIX_SRC` | `$COSMIX` | none |
| `Etc` | `COSMIX_ETC` | XDG config directory under `cosmix` | `/etc/cosmix` |
| `Var` | `COSMIX_VAR` | XDG data directory under `cosmix` | `/var/lib/cosmix` |
| `Bin` | `COSMIX_BIN` | `~/.local/bin` | `/usr/local/bin` |
| `Run` | `COSMIX_RUN` | XDG runtime directory under `cosmix` | `/run/cosmix` |
| `Log` | `COSMIX_LOG` | `COSMIX_VAR/log` | `/var/log/cosmix` |
| `Tmp` | `COSMIX_TMP` | `/tmp/cosmix` | `/tmp/cosmix` |

`current_uid()` is the crate's infallible wrapper around POSIX `getuid(2)`.
Path selection uses it to distinguish user and root execution.

## Node and ACME API

`node::load_from` parses an explicit node file. `node::load_node_config`
performs standard discovery and returns `None` when no file exists.
`node::require_node_config` converts that absence into an error.

`NodeConfig` derives broker, mail, and web listen addresses from the configured
node address and service ports. An empty node address makes `noded_url` use
loopback. `NodeConfig::synthesize_listeners` validates explicit web listener
IDs, binds, wildcard conflicts, vhost ownership, and enabled state.

`resolve_webd_acme` converts webd vhost ACME blocks into
`ResolvedWebdAcme`. It rejects mixed ACME and manual TLS on one vhost,
unsupported DNS-01 requests, implausible contact addresses, missing HTTP
listeners, and production use without a valid Let's Encrypt
Subscriber-Agreement URL. The returned `AcmeTosAcceptance` can be inspected
with `url()` but cannot be constructed by downstream crates.

## Features

| Feature | Default | Effect |
|---|---|---|
| `default` | yes | Enables no optional functionality. |
| `client-helpers` | no | Adds native-only broker URL resolution and Bus client connection helpers. |

`client-helpers` is not compiled for `wasm32`, even when enabled. On native
targets it adds `resolve_noded_url`, `connect_default`,
`connect_default_with_provenance`, and `connect_anonymous_default`.

If node configuration is absent or invalid, `resolve_noded_url` logs a warning
and returns `ws://127.0.0.1:4200/ws`. The fallback stays on the local host.

## Dependencies

The core crate uses serde, serde JSON, `directories`, `anyhow`, `libc`,
`tracing`, `thiserror`, and `cosmix-lib-mix` with serde support. The
`client-helpers` feature adds native optional dependencies on
`cosmix-lib-client` and `cosmix-lib-bus`.
