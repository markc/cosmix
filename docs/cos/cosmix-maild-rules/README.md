# cosmix-maild-rules

`cosmix-maild-rules` is the deterministic second stage of the inbound
`cosmix-maild` DATA filtering pipeline. It consumes the authentication result,
message, SMTP envelope, peer address, account identity, and account overrides,
then returns a rule verdict for delivery or Bayesian classification. In the
`bus <- mix <- cos` dependency chain it belongs to `cos`: it uses Mix to parse
strict-data `.conf.mix` rule packs and can optionally use Bus client libraries
when built for the wider Cosmix daemon family.

## Synopsis

The pipeline order is:

```text
cosmix-maild-auth -> cosmix-maild-rules -> cosmix-maild-bayesian
```

Authentication establishes RFC-derived facts. This crate applies deterministic
policy and content rules. Ambiguous messages continue to the Bayesian stage
with their score and matched rule identifiers as contextual features.

The library crate is named `cosmix_maild_rules`. The package also builds the
`cosmix-maild-rules-smoke` diagnostic binary.

## Verdicts

`RuleEngine::classify` returns one of three `RuleVerdict` shapes.

| Shape | Meaning |
|---|---|
| `HardAccept` | Skip Bayesian classification and deliver to the inbox. |
| `HardJunk` | Skip Bayesian classification and route to junk. |
| `Continue` | Pass the score and matched rule identifiers to Bayesian classification. |

`HardAccept` currently uses `AcceptReason::AllowlistSender`.

`HardJunk` can report `BlocklistSender`, `MailAuthHardFail`,
`StructuralAnomaly`, or `ScoreBreach`.

When `EngineConfig::shadow_mode` is true, every `HardJunk` result becomes
`Continue { would_junk: true, .. }`. Shadow mode does not alter `HardAccept`.

The engine applies decisive outcomes in this order:

1. Account sender allowlist.
2. Account sender blocklist.
3. Configured mail-auth hard-fail combinations.
4. Executable attachment combined with an SPF, DKIM, or DMARC failure.
5. Score at or above `hard_junk_threshold`.
6. `Continue`.

Malformed MIME adds the implicit `mime_parse_error` rule with weight `1`.
Configured negative rule weights contribute `0`.

## Public API

| Module or export | Purpose |
|---|---|
| `config` and `EngineConfig` | Classification limits, thresholds, shadow mode, and DNSBL settings. |
| `engine::RuleEngine` | Object-safe async `classify`, `explain`, and `reload` interface. |
| `engine::DefaultRuleEngine` | Rule-pack-backed implementation with atomic pack and configuration swaps. |
| `engine::RuleMatchHook` | Callback fired once for each matched pack rule during `classify`. |
| `types` | Context, overrides, verdicts, explanations, reload reports, identifiers, and match views. |
| `dnsbl` | DNSBL traits, query encoding, cache and single-flight wrapper, and optional production resolver. |
| `preflight` | Parallel DNSBL preflight and its per-zone result. |
| `glob_match::validate_sender_glob` | Validates account allowlist and blocklist patterns. |
| `rules` | Public matcher data types and CIDR helpers. |
| `default_pack_str` | Returns the embedded v1.0 default `.conf.mix` rule pack. |
| `error` | Crate-wide `Error` and `Result` types. |

`RuleContext` borrows all classify-time data. It carries `peer_ip`,
`envelope_from`, `envelope_to`, raw RFC 5322 message bytes, the
`cosmix-maild-auth` `VerifyResult`, an `AccountId`, and `AccountOverrides`.

`AccountOverrides` supports disabled rule identifiers, a threshold override,
and sender allowlist and blocklist globs. Literal addresses and `*` or `?`
wildcards match case-insensitively against the envelope sender.

`Explanation` records every evaluated rule, match state, configured and
effective weights, contribution, total score, threshold, pack version, and
verdict shape. Calling `explain` does not fire the rule-match hook.

## Constructing an engine

`DefaultRuleEngine::new` creates an engine with an empty pack. When no account
override or engine-level hard-fail decides the result, classification returns
`Continue` with score `0`.

`DefaultRuleEngine::with_pack_str` parses a `.conf.mix` string.
`DefaultRuleEngine::with_pack_path` loads a file and remembers its path for
later `reload` calls. Both constructors return the engine plus any per-rule
compile failures.

`reload` compiles the next file before taking the write lock, then swaps the
loaded rule set atomically. A top-level parse or I/O error leaves the current
pack live. Individual invalid rules are omitted and reported in
`ReloadReport::rules_failed`; valid rules from the same pack are loaded.

`pack_metadata`, `shadow_mode`, and `config_snapshot` return consistent
snapshots. `set_config` atomically replaces the complete configuration. It does
not merge fields or validate values.

`set_rule_match_hook` and `with_rule_match_hook` attach a callback before the
engine is shared. The hook runs during `classify`, including for the implicit
`mime_parse_error`, but not during `explain`.

See [Rule packs](rule-packs.md) for the `.conf.mix` schema and matcher kinds.

## Configuration

`EngineConfig::default` supplies these values.

| Field | Default | Effect |
|---|---:|---|
| `threshold` | `5.0` | Soft threshold carried in explanations and replaceable per account. |
| `hard_junk_threshold` | `15.0` | Score at or above this value produces `ScoreBreach`. |
| `shadow_mode` | `false` | Downgrades hard-junk outcomes when enabled. |
| `body_scan_bytes` | `1048576` | Bounds each prepared body view. |
| `url_extraction_cap` | `1024` | Bounds extracted URL count. |
| `header_line_cap` | `256` | Bounds top-level headers considered. |
| `header_line_byte_cap` | `8192` | Bounds each rendered header value. |
| `budget_ms` | `50` | Stored classification budget value; this crate does not apply a timer. |
| `mail_auth_hard_fail_kinds` | SPF/DMARC and DKIM/DMARC reject pairs | Selects engine-level authentication hard failures. |
| `dnsbl_query_timeout_ms` | `2000` | Timeout supplied when constructing DNSBL lookup wiring. |
| `dnsbl_negative_ttl_secs` | `300` | Cache lifetime supplied to DNSBL lookup wiring. |
| `dnsbl_resolver_addresses` | empty | Resolver socket overrides; empty selects system resolver wiring. |

The default hard-fail list contains `SpfFailDmarcReject` and
`DkimFailDmarcReject`. `BothAuthFailNoArc` is available but not enabled by
default.

## DNSBL support

`DnsblLookup` is the engine-facing async interface. `Dnsbl<R>` adds a bounded
cache and leader/follower single-flight behaviour to any `AsyncDnsResolver`.

`DnsblResult` distinguishes `Listed`, `NotListed`, and `LookupFailed`.
Successful and negative answers are cached. Transient failures are not cached
across messages and count as no match.

The engine resolves the deduplicated zones for active DNSBL rules in parallel
before running synchronous matchers. Disabled rules do not cause DNS queries.
A DNSBL rule matches when any configured zone returns `Listed`.

## Cargo features

| Feature | Default | Effect |
|---|---|---|
| `core` | Yes | Base standalone rule-engine surface; adds no optional dependency. |
| `dnsbl` | No | Adds the Hickory-backed production DNS resolver and `HickoryDnsbl` alias. |
| `cosmix` | No | Adds Bus and native client dependencies and also enables `dnsbl`. |

The default feature set is `core`.

## Smoke program

`cosmix-maild-rules-smoke` reads one RFC 5322 message from standard input,
loads the bundled default pack, uses a synthetic successful authentication
result, and prints the debug verdict and explanation.

```text
cosmix-maild-rules-smoke < message.eml
```

If the default pack cannot be found beside the crate source, the program uses
an empty engine. Per-rule compile failures are written to standard error.

## Errors

The public `Error` type covers pack parse and file errors, regex compilation,
unknown matcher kinds, missing fields, invalid rule configuration, MIME parse
failure, and internal errors. Runtime MIME parse failure is normally represented
by the implicit scored rule rather than returned from `classify`.
