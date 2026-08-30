# Rule packs

`cosmix-maild-rules` loads deterministic rules from strict-data `.conf.mix`
files. The bundled pack is available through `default_pack_str`; file-backed
engines can atomically reload an operator-supplied pack.

## Top-level shape

A pack contains a version string and a list of rules.

```text
pack_version: "v1.0"

rule: [
  {
    id: "example-subject",
    description: "Subject contains an example marker",
    weight: 2,
    match: {
      kind: "header_substring",
      name: "Subject",
      substring: "example"
    }
  },
  {
    id: "documentation-net",
    description: "Peer belongs to a documentation network",
    weight: 1,
    match: {
      kind: "peer_ip_in_cidr",
      cidrs: ["192.0.2.0/24"]
    }
  }
]
```

Every rule requires `id`, `description`, `weight`, and `match`. Rule
identifiers are stable strings within a pack version. A matched rule contributes
`max(weight, 0)` to the raw score.

The `match` map rejects unknown fields. Required fields depend on `kind`.
Regexes and CIDRs compile when the pack loads.

Top-level parse failure rejects the pack. A matcher compile failure rejects
only that rule; the loader returns the remaining compiled rules and a list of
`(rule_id, error)` pairs.

## Matcher kinds

| `kind` | Fields | Match condition |
|---|---|---|
| `header_present` | `name` | The named top-level header occurs. |
| `header_absent` | `name` | The named top-level header does not occur. |
| `header_regex` | `name`, `regex` | Any value of the named header matches the Rust regex. |
| `header_substring` | `name`, `substring` | Any value contains the substring, case-insensitively. |
| `body_regex` | `regex`, optional `view` | The selected decoded body view matches the Rust regex. |
| `body_substring` | `substring`, optional `view` | The selected body view contains the substring, case-insensitively. |
| `mail_auth` | One or more of `spf`, `dkim`, `dmarc` | Every configured authentication predicate matches. |
| `peer_ip_in_cidr` | `cidrs` | The peer IPv4 or IPv6 address belongs to any CIDR. |
| `peer_ip_in_dnsbl` | optional `zones` | Any zone reports the peer address as listed. |
| `url_count` | `gt` | Extracted URL count is greater than `gt`. |
| `attachment_count` | `gt`, optional `executable_only` | Selected attachment count is greater than `gt`. |
| `recipient_count` | `gt` | Envelope recipient count is greater than `gt`. |
| `structure` | One structural boolean | The selected structural condition holds. |
| `alignment` | optional `what` | Reserved; accepted by the loader but always returns no match. |

## Body views

`body_regex` and `body_substring` accept these `view` values:

| Value | Content |
|---|---|
| `plain` | Decoded `text/plain` parts. |
| `html` | Decoded `text/html` parts. |
| `combined` | Plain then HTML content, bounded as one combined view. |

The default is `combined`.

Each individual plain and HTML view is bounded by `body_scan_bytes`. The
combined view is independently bounded to the same size.

## Authentication predicates

`mail_auth` accepts one or more predicates. Multiple predicates form an AND
condition.

| Field | Accepted values |
|---|---|
| `spf` | `pass`, `fail`, `softfail`, `neutral`, `none`, `temperror`, `permerror` |
| `dkim` | `pass`, `fail`, `none`, `temperror`, `permerror` |
| `dmarc` | `pass`, `fail`, `none` |

These rules add score. Engine-level authentication hard failures are configured
separately through `EngineConfig::mail_auth_hard_fail_kinds`.

## Structural rules

A `structure` matcher selects one of these boolean fields with value `true`:

| Field | Condition |
|---|---|
| `html_only` | HTML body exists without a plain-text alternative. |
| `deep_multipart_nesting` | The parsed message crosses the structural nesting heuristic. |
| `missing_message_id` | No top-level `Message-ID` header is visible within configured limits. |
| `has_executable_attachment` | An attachment has a recognised executable extension or content type. |

Use one structural field per rule.

For `attachment_count`, `executable_only` defaults to `false`. Recognised
executable extensions are `exe`, `scr`, `bat`, `cmd`, `com`, `pif`, `vbs`,
`js`, and `jar`. Recognised executable content types are
`application/x-msdownload` and `application/x-executable`.

## Count rules and limits

`url_count`, `attachment_count`, and `recipient_count` use strict greater-than
comparison. A rule with `gt: 20` starts matching at `21`.

URL extraction uses the combined body view and stops at
`EngineConfig::url_extraction_cap`. Links with `cid:` and `data:` schemes do
not contribute.

Header matchers inspect top-level headers only. Header count and rendered value
length are bounded by `header_line_cap` and `header_line_byte_cap`.

## DNSBL rules

`peer_ip_in_dnsbl` accepts a list of zone names.

```text
{
  id: "listed-peer",
  description: "Peer appears on an example DNS blocklist",
  weight: 5,
  match: {
    kind: "peer_ip_in_dnsbl",
    zones: ["blocklist.example.com"]
  }
}
```

The engine deduplicates zones across active rules and resolves them in parallel.
An absent DNSBL implementation, an unknown zone result, timeout, or transient
lookup failure counts as no match. A rule matches if at least one zone reports
`Listed`.

The `dnsbl` Cargo feature provides the Hickory-backed resolver. The base API
also permits a caller-supplied `DnsblLookup`.

## Alignment

The `alignment` kind accepts `what: "spf"`, `what: "dkim"`, or the default
`"either"` representation. It is reserved in this version: the loader retains
its data, but classification never matches it.

## Bundled pack

The embedded pack declares `pack_version: "v1.0"` and contains 17 rules. It
covers missing and patterned headers, body patterns, URL and recipient counts,
executable attachments, message structure, and soft SPF and DKIM failures.

The default engine thresholds are calibrated separately in `EngineConfig`.
Callers can load the embedded text without locating a file:

```rust
let pack = cosmix_maild_rules::default_pack_str();
```

Return to [cosmix-maild-rules](README.md).
