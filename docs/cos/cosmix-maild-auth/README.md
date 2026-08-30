# cosmix-maild-auth

`cosmix-maild-auth` provides the mail-authentication library used by `cosmix-maild`: inbound SPF, DKIM, DMARC, ARC, and iprev verification; outbound DKIM signing; DKIM key generation; DNS resolver construction; and structured authentication results. It lives in the `cos` repository at the downstream end of the `bus <- mix <- cos` dependency chain. The default build does not depend on `mix` or `bus`; the optional `cosmix` feature adds direct dependencies on two `bus` crates.

## Synopsis

The Cargo package is named `cosmix-maild-auth`. Rust code imports it as `cosmix_maild_auth`.

```rust
use cosmix_maild_auth::keygen::generate;
use cosmix_maild_auth::KeySpec;

let material = generate(KeySpec::Ed25519)?;
```

This crate is a library. It does not install a binary, define command-line subcommands, register Bus verbs, or parse a standalone configuration file.

## What it provides

### Inbound verification

The `Verifier` trait defines two asynchronous operations:

- `verify` runs iprev, DKIM, SPF, DMARC, and ARC checks for a raw RFC 5322 message.
- `verify_spf_only` runs a pre-DATA SPF check from the peer address, envelope sender, and HELO identity.

`MailAuthVerifier` is the concrete implementation. Construct it with a shared `DnsResolver` and a `VerifierConfig`.

The full verification path runs serially. It:

1. Builds an internal message copy without prior `Authentication-Results` fields which claim the configured host identity.
2. Checks iprev for the peer address.
3. Parses the message for DKIM, ARC, and author-domain data.
4. Checks DKIM signatures.
5. Checks SPF, using HELO when the envelope sender is empty.
6. Checks DMARC from the DKIM and SPF outputs.
7. Validates the ARC chain.
8. Returns structured results and a rendered `Authentication-Results` field.

`VerifyResult` contains `SpfCheck`, `IprevResult`, `DkimAggregate`, `DmarcResult`, `ArcResult`, and `AuthResultsHeader`. The header value retains both the wire-format string and structured fields so callers do not need to parse the rendered field again.

An SPF timeout becomes `SpfResult::TempError`. iprev, DKIM, DMARC, and ARC timeouts currently return `Error::DnsTimeout`.

### Outbound signing

The `Signer` trait defines:

- `sign_dkim`, which signs a mutable message buffer and prepends a `DKIM-Signature` field.
- `seal_arc`, which reserves the ARC-sealing operation.

`MailAuthSigner::new` groups `DkimSignerConfig` values by domain. `lookup_active` selects an active signer for the exact author domain or walks towards its parent labels until it finds one.

DKIM signing supports:

- RSA-SHA256 keys in PKCS#1 or PKCS#8 PEM form.
- Ed25519-SHA256 keys in PKCS#8 PEM form.
- Simple or relaxed header and body canonicalisation.
- Explicit header selection.
- Header oversigning through `HeaderSpec::Oversign`.
- Optional DKIM `l=` body-length tags.
- Multiple loaded selectors per domain, with `active_for_signing` selecting the signing key.

`default_signed_headers` returns the crate's conventional header set. It oversigns `From` and includes common message and `List-*` fields.

`MailAuthSigner::validate_pem` checks that PEM data matches the selected algorithm. `detect_algorithm` recognises RSA PKCS#1, RSA PKCS#8, and Ed25519 PKCS#8 private keys.

`DkimSignerConfig` uses a custom `Debug` implementation which reports the private-key byte count without printing the key bytes.

### Key generation

`keygen::generate` accepts a `KeySpec` and returns `DkimKeyMaterial`.

`KeySpec` supports:

- `Rsa { bits }`; values below 1024 bits are rejected.
- `Ed25519`.

`DkimKeyMaterial` contains the selected `DkimAlgorithm`, private PEM, raw public-key base64, and a complete DKIM DNS TXT value. RSA output uses PKCS#1 PEM. Ed25519 output uses PKCS#8 PEM.

The generated TXT value does not add the DKIM `t=s` flag.

### DNS resolution

`DnsResolver` owns the `mail_auth::MessageAuthenticator` and its resolver. `ResolverChoice` selects:

| Choice | Behaviour |
|---|---|
| `System` | Reads the platform resolver configuration. This is the default. |
| `Cloudflare` | Uses the resolver preset supplied by `mail-auth`. |
| `Custom(Vec<IpAddr>)` | Adds each supplied address as a UDP resolver with TCP fallback on port 53. |

`resolver::parse_choice` accepts `system`, `cloudflare`, or a non-empty comma-separated form such as `custom:192.0.2.53,192.0.2.54`. The `system` and `cloudflare` names are case-insensitive; the `custom:` prefix is lower-case.

Resolver construction is fallible. Invalid custom addresses, an empty custom list, or failure to read the system resolver configuration returns the crate's `Error` type.

### Authentication-Results trimming

`ar_trim::trim_forged_authentication_results` removes every prior `Authentication-Results` field whose authserv-id matches the configured identity, ignoring ASCII case.

The trimmer:

- Removes folded continuation lines with the matching field.
- Preserves fields for other authserv-ids.
- Preserves CRLF or LF line endings.
- Stops at the first header/body boundary.
- Leaves the message body byte-identical.
- Limits authserv-id inspection to 256 bytes.

It returns the rebuilt message and the number of removed fields.

## Public modules

| Module | Surface |
|---|---|
| `ar_trim` | Byte-level removal of forged local `Authentication-Results` fields. |
| `auth_results` | Header rendering helper; see current limitations. |
| `error` | `Error` and the crate-wide `Result<T>` alias. |
| `keygen` | `KeySpec`, `DkimKeyMaterial`, and `generate`. |
| `mapping` | Internal result mapping plus public `naive_org_domain`. |
| `resolver` | `ResolverChoice`, `parse_choice`, and `DnsResolver`. |
| `signer` | `Signer`, `MailAuthSigner`, and `detect_algorithm`. |
| `types` | Configuration, result, policy, algorithm, and header-selection types. |
| `verifier` | `Verifier` and `MailAuthVerifier`. |

The crate root re-exports `Error`, `Result`, key-material types, signer and verifier traits and implementations, `detect_algorithm`, and every public item from `types`.

## Verifier configuration

`VerifierConfig::default` supplies:

| Field group | Default |
|---|---|
| `host_identity` | `localhost` |
| SPF timeout | 5 seconds |
| DKIM timeout | 10 seconds |
| DMARC timeout | 5 seconds |
| ARC timeout | 5 seconds |
| iprev timeout | 5 seconds |
| All policy modes | `PolicyMode::Advisory` |
| Maximum DKIM signatures | 10 |
| Maximum ARC instances | 50 |

`PolicyMode` has `Off`, `Advisory`, and `Enforce` variants. The current verifier stores these mode fields but does not branch on them.

The DKIM signature limit is applied while mapping results. Excess outputs are omitted and `DkimAggregate::capped` is set.

## Cargo features

The default feature set is `core`.

| Feature | Default | Effect |
|---|---:|---|
| `core` | Yes | Empty marker feature for the standalone authentication surface. |
| `cosmix` | No | Enables optional `cosmix-lib-bus` and native `cosmix-lib-client` dependencies. |

The current source tree has no feature-gated modules or functions. Enabling `cosmix` changes the dependency graph only.

## Dependencies

| Dependency | Use |
|---|---|
| `mail-auth` | SPF, DKIM, DMARC, ARC, iprev, DNS integration, signing, and key generation. |
| `rustls-pki-types` | PEM decoding and private-key representation. |
| `tokio` | Per-check timeouts and asynchronous trait implementations. |
| `thiserror` | The public error enum. |
| `serde`, `tracing` | Declared workspace dependencies; the current crate source does not call them directly. |
| `cosmix-lib-bus`, `cosmix-lib-client` | Optional dependencies enabled by `cosmix`. |

`mail-auth` disables its default features and enables its `ring` and `generate` features.

## Errors

`Error` distinguishes DNS timeout, DNS failure, signing-key load failure, malformed prior authentication results, missing domain signer, upstream library failure, and internal failure.

Public operations use the crate-wide `Result<T>` alias.

## Current limitations

- `Signer::seal_arc` is present but `MailAuthSigner` currently returns `Error::Internal`; ARC sealing is not implemented.
- `auth_results::render` currently returns an empty string. Normal verification callers use `AuthResultsHeader::rendered`.
- `VerifierConfig` policy modes do not currently change verification behaviour.
- `max_arc_instances` is passed into result mapping but is not currently enforced.
- `mapping::naive_org_domain` returns the rightmost two labels. It is not Public Suffix List aware and gives incorrect organisational domains for some multi-label public suffixes.
- Only SPF converts a timeout into a structured temporary result; other verification-stage timeouts stop the full operation with `Error::DnsTimeout`.
