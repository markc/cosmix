# cosmix-lib-daemon

`cosmix-lib-daemon` supplies shared runtime infrastructure for Cosmix daemon services. It is a substrate library in the `bus <- mix <- cos` dependency chain: the package belongs to the `cos` layer and is imported in Rust as `cosmix_daemon`. It provides process-lifecycle helpers, strict HTTP host parsing, protocol-neutral listener management, connection guards, and optional TLS, SNI, Let's Encrypt validation, ACME, and self-signed certificate support.

## Synopsis

```toml
[dependencies]
cosmix-lib-daemon = { version = "0.6.0" }
```

```rust
use cosmix_daemon::{log_dir, shutdown_signal};

let path = log_dir();
shutdown_signal().await;
```

TLS consumers enable the `tls` feature:

```toml
[dependencies]
cosmix-lib-daemon = { version = "0.6.0", features = ["tls"] }
```

## Process helpers

`log_dir()` returns the conventional daemon log directory under the current user's home directory. It falls back to `/tmp/cosmix-log` when `HOME` is unavailable.

`shutdown_signal()` waits for Ctrl+C. On Unix it also waits for `SIGTERM`. It returns after either signal and emits a shutdown trace event.

`load_tls_config(cert_path, key_path)` is available with `tls`. It reads a PEM certificate chain and private key, installs the rustls `ring` provider if needed, and returns a `tokio_rustls::TlsAcceptor` configured without client authentication.

Tracing initialisation is outside this crate.

## HTTP host parsing

The `http_host` module provides:

- `parse_request_host(raw)` parses an HTTP `Host` value.

The parser returns a lower-case bare hostname or `None`. It accepts a trailing numeric port and strips it. It rejects empty values, hostnames longer than 253 bytes, non-LDH bytes, non-numeric or overlong ports, extra colons, IPv6 literals, and CR/LF input. Response policy remains with the caller.

## Listener sets

The `listen` module manages logical listeners across one or more resolved socket addresses. It binds sockets, runs one accept loop per bind, applies connection guards, optionally terminates TLS, attaches connection metadata, and dispatches accepted streams to a daemon-supplied handler.

Configuration deserialisation and address resolution remain outside this module. `ListenerSpec` accepts resolved `SocketAddr` values.

Key specification types are:

| Type | Purpose |
|---|---|
| `ListenerSpec` | Stable listener ID, bind addresses, enable state, external flag, TLS mode, guard policy, and partial-bind policy |
| `TlsMode` | `Plain`; with `tls`, also `Terminate` and `StartTlsPassthrough` |
| `BindPolicy` | `FailAll` or `BestEffort` for partial multi-bind failures |
| `GuardPolicy` | Optional rate limit, connection caps, strict-SNI intent, and IP ACL |

`ListenerSpec::new()` creates an enabled, internal, plain-TCP listener with no guards and `BindPolicy::FailAll`. Builder-style setters attach the remaining policy.

Key runtime types are:

| Type | Purpose |
|---|---|
| `ListenerSetBuilder` | Adds listener specifications and handlers, then builds a set |
| `ListenerSet` | Owns configured listeners and starts enabled entries |
| `ListenerSetControl` | Cloneable live control handle |
| `ListenerStatus` | Listener ID, running state, reported binds, external flag, and active connection count |
| `ConnHandler` | Asynchronous per-connection handler trait |
| `AcceptedStream` | Raw TCP stream or, with `tls`, a terminated TLS stream |
| `ConnCtx` | Listener ID, local and peer addresses, external flag, SNI, and optional TLS handle |

`ListenerSet::start_all()` starts enabled listeners. Individual bind failures are logged; it returns an error when listeners were requested but none started.

`ListenerSetControl` provides:

- `enable(id)` binds and starts a listener. Repeated enable calls are no-ops.
- `disable(id, drain)` stops accepting, releases listening sockets, and optionally waits for active connections up to a deadline.
- `is_running(id)` reports whether the listener is bound.
- `status()` returns ID-sorted snapshots for all configured listeners.
- `swap_guard(id, policy)` changes admission policy for subsequent connections without resetting live counters or rate buckets.
- `swap_tls(id, resolver)` changes TLS identities for subsequent handshakes when `tls` is enabled.

`BindPolicy::FailAll` releases successful partial binds if any address fails. `BindPolicy::BestEffort` keeps successful binds but still fails when no address binds.

## Connection guards

Listener guards are protocol-neutral and apply before the connection handler. Admission checks run in this order:

1. IP ACL.
2. Per-IP token-bucket rate limit.
3. Listener-wide concurrent-connection cap.
4. Per-IP concurrent-connection cap.

`RateLimit` defines the maximum admissions in a time window.

`Cidr` parses IPv4 or IPv6 CIDR strings and bare host addresses. Bare addresses use `/32` for IPv4 and `/128` for IPv6.

`IpAcl` contains allow and deny CIDRs. Deny entries win. An empty allow list permits all addresses not denied.

`Guards::admit()` returns a `ConnPermit` on success. Dropping the permit releases global and per-IP connection counts. Guard policies default to off.

## TLS and SNI

The `tls` module is present only with the `tls` feature. It provides multi-identity SNI selection and Let's Encrypt chain validation.

`SniCertResolver::from_config()` loads certificate and key files described by `cosmix_config::node::TlsIdentityConfig`. Server names are normalised to lower case and duplicate names are rejected case-insensitively.

`TlsIdentity` exposes the normalised name, rustls `CertifiedKey`, default flag, and no-SNI fallback flag.

`pick_identity()` contains the shared selection policy:

- An exact SNI match wins.
- An unknown SNI falls back to the default identity unless strict SNI is active.
- Missing SNI uses the no-SNI fallback, then the default identity.
- Strict SNI rejects unknown and missing SNI.

If no identity has either fallback flag, the first configured identity acts as both fallbacks.

`ListenerTls` holds a hot-swappable resolver and caches rustls server configurations by resolver identity. A missing or empty resolver disables TLS. Resolver swaps leave in-flight handshakes using their existing configuration.

TLS termination has a 15-second handshake timeout. `Terminate` listeners refuse to bind without a usable resolver. `StartTlsPassthrough` passes raw TCP plus the listener's TLS handle to the connection handler.

## Let's Encrypt validation

`validate_le_chain()` validates a PEM chain against the production Let's Encrypt trust set.

`validate_le_chain_for_environment()` validates against an explicit `ChainEnvironment::Production` or `ChainEnvironment::Staging` trust set.

Validation requires all of the following:

1. X.509 path validation for the selected environment and validation time.
2. A permitted SPKI-SHA-256 pin for the leaf's issuing intermediate.
3. Subject Alternative Name coverage for every expected DNS name.

The expected-name list must not be empty. Production and staging trust anchors and intermediate pin tables are separate. `IntermediateState::Active` and `Backup` pins are accepted; `Retired` is a kill switch.

The module exports the production and staging intermediate tables, the staging trust-root DER set, and their associated environment and pin metadata types.

## ACME

The `acme` module is present with `tls`. It wraps `instant-acme` for Let's Encrypt HTTP-01 issuance.

`AcmeProvider` admits only `LetsencryptStaging` and `LetsencryptProd`. It maps each provider to its directory URL, stable file-name component, and chain-validation environment.

`WebdAcmeClient` provides:

- `load_or_create_staging(account_dir, contact_email)` for staging accounts.
- `load_or_create_prod(account_dir, contact_email, tos)` for production accounts with a typed `AcmeTosAcceptance`.
- `order(domain, solver)` for a single-domain HTTP-01 order.
- `provider()` and `account_dir()` accessors.

`Http01Solver` is the caller-implemented asynchronous interface for publishing and removing challenge tokens. Cleanup is attempted after both successful and failed orders.

`IssuedChain` returns the full PEM chain, a locally generated PKCS#8 private key, the provider, and the parsed leaf validity window. The caller must validate the returned chain with the matching chain environment before installation.

Account credentials and metadata use provider-specific JSON files in the supplied account directory. File writes use a temporary file, file synchronisation, atomic rename, and parent-directory synchronisation. Missing halves of an account file pair, provider changes, directory URL changes, and production terms-of-service URL changes return `AcmeError::AccountMetaMismatch`.

`verify_account_meta_local()` checks stored provider metadata without loading credentials or contacting the ACME directory.

## Self-signed certificates

The `selfcert` module is present with `tls`.

`write_self_signed(fqdn, cert_path, key_path)` generates a P-256 self-signed leaf with the requested DNS name and loopback IP in its Subject Alternative Names. It writes the certificate as mode `0644` and the private key as mode `0600`.

Writes use a same-directory exclusive temporary file, synchronisation, and atomic rename. The caller remains responsible for any ownership changes.

## Features

| Feature | Default | Effect |
|---|---:|---|
| `tls` | No | Enables rustls and tokio-rustls support, the `tls`, `acme`, and `selfcert` modules, TLS listener modes, SNI routing, Let's Encrypt validation, ACME issuance, and self-signed certificate generation |

The default feature set is empty. Plain-TCP listener management, connection guards, host parsing, log-directory lookup, and shutdown signalling remain available without TLS dependencies.

## Dependencies

The base crate uses Tokio for signals, sockets, tasks, and listener control; tracing for runtime events; anyhow for fallible operations; async-trait for handler dispatch; and arc-swap for live policy replacement.

The `tls` feature adds the Cosmix configuration library and the certificate, cryptography, serialisation, ACME, time, temporary-file, and typed-error dependencies required by the optional modules.

## Scope

This crate has no binary target, command-line interface, subcommands, or Bus verbs. Daemons supply protocol handlers and expose any runtime controls through their own surfaces.

It does not deserialize daemon listener configuration. It operates on resolved listener specifications and TLS identity configuration supplied by its callers.
