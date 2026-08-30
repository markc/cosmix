# cosmix-dnsd Bus verbs

The default `cosmix` build registers the Bus service `dnsd`. The standalone
`--no-default-features` build has no Bus surface.

The service is read-only. It accepts exactly `dnsd.zone.snapshot` and
`dnsd.stats`.

## Return codes

| Code | Meaning |
|---:|---|
| `0` | Action completed. |
| `10` | Unknown action or action outside the read-only surface. |

An unsupported action returns a JSON body with the rejected action and the
accepted action names:

```json
{
  "error": "unknown or non-read-only action",
  "action": "dnsd.zone.set",
  "read_only_actions": [
    "dnsd.zone.snapshot",
    "dnsd.stats"
  ]
}
```

There are no zone mutation, adoption, or reload Bus actions.

## dnsd.zone.snapshot

Returns identity information for the currently served zone snapshot.

```json
{
  "config_hash": "18446744073709551615",
  "zone_count": 2,
  "zones": [
    "alpha.example.com.",
    "beta.example.com."
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `config_hash` | string | Lossless decimal representation of the snapshot's 64-bit configuration hash. |
| `zone_count` | number | Number of served zones. |
| `zones` | array of strings | Served zone names in deterministic sorted order. |

`config_hash` is a string because the full unsigned 64-bit range is larger
than the exact integer range of common JSON number consumers.

The hash excludes the daemon-owned emitted serial. It includes the configured
serial floor. The action therefore supports exact configuration-parity
comparison without treating a locally emitted serial as configuration drift.

The action does not export individual resource records.

## dnsd.stats

Returns process-lifetime response counts grouped by resolver-selected DNS
response code.

```json
{
  "noerror": 120,
  "nxdomain": 4,
  "refused": 2,
  "formerr": 0,
  "notimp": 0,
  "other": 0,
  "total": 126
}
```

| Field | Meaning |
|---|---|
| `noerror` | `NOERROR` responses. |
| `nxdomain` | `NXDOMAIN` responses. |
| `refused` | `REFUSED` responses. |
| `formerr` | `FORMERR` responses. |
| `notimp` | `NOTIMP` responses. |
| `other` | Any response code outside the named buckets. |
| `total` | Sum of all bucket values in this snapshot. |

The counters are monotone atomic values shared by every citizen UDP and TCP
serve task. The returned fields can reflect slightly different instants, but
`total` is computed from the same loaded bucket values and equals their sum.

The observer records the resolver's completed response before wire encoding.
An encoder fallback response can therefore differ from the counted response in
the encoding-failure edge case.

`refused` acts as an authority-health canary. The resolver uses `REFUSED` for
out-of-zone or wrong-class queries, not for an in-zone name.

## Availability

Bus runs in a sibling task and does not gate DNS service. Failure to reach the
broker leaves DNS serving active.

Connection and registration retry with exponential delays from one second up
to 60 seconds. A session lasting at least 30 seconds resets the delay. A short
accept-then-drop session continues increasing the delay, preventing a tight
reconnect loop.

When an incoming Bus stream ends, the client closes the old session and
registers again. The zone store and statistics counters remain shared with the
DNS tasks across reconnects.

Return to the [crate overview](README.md).
