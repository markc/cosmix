# cosmix-lib-dns

`cosmix-lib-dns` is the pure Rust core of the authoritative mesh DNS service. It defines the zone model, strict-data loader, candidate validation and adoption path, snapshot storage, deterministic resolver, DNS wire codec, and Tokio UDP/TCP serve loops. It lives in the `cos` repository at the `cos` end of the `bus <- mix <- cos` dependency chain: it depends on `cosmix-lib-mix` for parsing zone data, but has no direct Bus, mesh, daemon-configuration, or citizen dependency.

The package version is `0.2.0`. The library target is imported as `cosmix_dns`.

## Scope

The crate provides library APIs only. It has no binary, CLI, subcommands, or Bus verbs.

The core accepts hand-maintained `zones.mix` data and produces immutable answer snapshots. The query path reads only the flattened snapshot. Owner identity, bundle serials, persistence, transport size, and reload state do not enter the resolver.

`hickory-proto` is used as a DNS message and record codec. The crate does not use Hickory server, resolver, recursor, catalogue, or authority implementations, and it has no forwarding or recursive lookup path.

## Data model

The model has two layers.

- Layer 1 is owner-aware source and candidate state. Its main types are `OwnerId`, `ZoneName`, `RecordAssertion`, `Bundle`, `ZoneSource`, `ParsedZones`, and `CandidateSnapshot`. This layer carries owners, bundle serials, configured serial floors, and withdrawn records. It is validated and consumed, never served.
- Layer 2 is flattened answer state. Its main types are `RrsetKey`, `RrsetValue`, `ZoneAnswerState`, and `ZoneSnapshot`. It contains answerable RRsets, apex SOA and NS data, existing-name indexes, emitted serials, and a configuration hash. It contains no owner attribution.

`Serial` implements RFC 1982 comparison through `Serial::compare`. The exactly half-range case returns `SerialCmp::Undefined`; callers reject it rather than imposing a total order. `Serial::successor` wraps modulo 2³², and `Serial::rfc1982_max` returns `None` for an undefined comparison.

## Zone loading

`strict_data` parses the pinned map/list `zones.mix` schema through `cosmix_mix::parse_data`.

- `parse_zones(&str)` parses source text into `ParsedZones`.
- `parse_zones_file(&Path)` reads and parses a file.
- `ZonesParseError` reports schema, name, number, and RDATA errors.

A minimal zone has this shape:

```mix
zones: {
  "example.com": {
    soa: {
      primary: "ns1.example.com",
      mbox: "hostmaster.example.com",
      ttl: 300,
      minimum: 60
    },
    ns: [ "ns1.example.com" ],
    serial_floor: 1,
    bundles: [
      {
        owner: "alpha",
        serial: 1,
        records: [
          {
            name: "alpha.example.com",
            type: "A",
            ttl: 300,
            data: "192.0.2.10"
          }
        ]
      }
    ]
  }
}
```

Maps inside braces require commas between entries. The top-level `zones` map must contain at least one zone. Each zone requires `soa`, `ns`, `serial_floor`, and `bundles`. Each bundle requires `owner`, `serial`, and `records`. Duplicate bundles for one owner in one zone are rejected.

Each record requires `name`, `type`, `ttl`, and `data`. `withdrawn` is an optional boolean and defaults to `false`. A withdrawn record remains representable in Layer 1 but does not create an RRset or existing name in Layer 2.

Supported record data forms are:

| Type | `data` form |
|---|---|
| `A` | Dotted IPv4 address |
| `AAAA` | IPv6 address |
| `NS` | DNS name |
| `MX` | `"<preference> <exchange>"` |
| `SRV` | `"<priority> <weight> <port> <target>"` |
| `TXT` | String or list of strings |
| `PTR` | DNS name |

SOA records come only from the zone `soa` map. `SOA`, `CNAME`, `HINFO`, and unknown types are rejected in `records`.

## Names and records

`canonical::Name` is canonical by construction. `Name::parse` accepts an optional trailing dot, lower-cases labels, and returns an absolute name. Labels accept ASCII letters, digits, hyphen, and underscore. Empty labels, over-length names, and other characters are rejected.

`Name::parse_owner` additionally accepts one leftmost `*` label for a wildcard record owner. Wildcards remain invalid in zone apexes, SOA names, NS names, and RDATA targets.

`rr::RecordType` and `RData` form a closed vocabulary for `SOA`, `NS`, `A`, `AAAA`, `MX`, `SRV`, `TXT`, and `PTR`. `RecordType::parse` accepts record types valid in the `records` list; `RecordType::from_hickory` and `to_hickory` map query and wire types. `RData::to_hickory_record` performs model-to-wire conversion. Long TXT strings are split into DNS character strings of at most 255 bytes on UTF-8 boundaries.

## Candidate pipeline

The reload pipeline is:

```text
parse -> canonicalise -> build -> validate -> adopt
```

Canonicalisation occurs during parsing. The public candidate functions are:

- `build_candidate(ParsedZones, &dyn Persistence)` applies per-owner replay floors and produces an owner-aware `CandidateSnapshot`.
- `validate_candidate(&CandidateSnapshot)` rejects cross-owner RRset collisions and duplicate canonical RDATA.
- `adopt_candidate(current, CandidateSnapshot, &mut dyn Persistence)` flattens the candidate, ticks emitted SOA serials, persists advancing floors, and returns `Arc<ZoneSnapshot>`.

One owner must use one serial across all zones in a candidate. A serial older than its persisted owner floor is rejected. An equal serial is accepted idempotently without advancing the floor. A newer serial advances it.

Within one zone, two owners cannot assert the same `(name, type)` RRset. One owner may contribute multiple distinct RDATA values to one RRset; the resulting RRset uses the minimum asserted TTL. Duplicate canonical RDATA is rejected.

`RejectReason` distinguishes stale replay, undefined serial comparison, cross-owner collision, duplicate RDATA, cross-zone owner serial conflict, malformed input, and persistence failure.

## Snapshots and persistence

`snapshot::SnapshotHash` is a deterministic FNV-1a hash over canonical, sorted flattened configuration. It includes zones, SOA data, NS data, configured serial floors, RRset keys, TTLs, and RDATA. It excludes daemon-owned emitted serials. `snapshot::compute_config_hash` accepts borrowed `ZoneConfigView` values.

`Persistence` stores two independent monotone high-water maps: a global per-owner replay floor and a per-zone emitted-SOA floor. Its readers are infallible and return `None` when no prior floor exists. Its mutators return `PersistenceError` and must persist and synchronise before returning success.

`InMemoryPersistence` provides an in-memory implementation and a write-failure switch for tests. `FilePersistence` rewrites a versioned state file through a temporary file, file `fsync`, rename, and best-effort directory `fsync`. `FilePersistence::open` returns `StateLoad::Ok` for a valid or absent file and `StateLoad::Corrupt` with empty floors for unreadable, unparseable, or wrong-version state.

The two floor kinds are not updated as a two-phase transaction. Each floor is independently monotone and does not regress.

## Store

`ZoneStore` exposes `snapshot() -> Arc<ZoneSnapshot>`.

`StaticZoneStore` holds the current snapshot in `ArcSwap`. `load_initial` parses, builds, validates, adopts, and returns `StoreInitError` when no initial snapshot can be established. `reload` repeats the pipeline and replaces the snapshot only after successful adoption. Parse, validation, or persistence failure leaves the last-known-good snapshot in service.

Each query takes one snapshot `Arc`, so a response observes one consistent generation.

## Resolver

`resolve(&ZoneSnapshot, &hickory_proto::op::Message) -> Message` is pure and deterministic. It performs longest-suffix authoritative zone selection and never performs I/O, recursion, or forwarding.

| Request or lookup result | Response |
|---|---|
| Opcode other than `QUERY` | `NOTIMP`, not authoritative |
| Question count other than one | `FORMERR`, not authoritative |
| Class other than `IN` or `ANY` | `REFUSED`, not authoritative |
| Name outside all served zones | `REFUSED`, not authoritative |
| Existing name with no requested type | `NOERROR` with SOA in authority |
| Missing name without wildcard coverage | `NXDOMAIN` with SOA in authority |
| Existing or wildcard-covered name queried as `ANY` | Authoritative `NOERROR` with an empty answer |

Apex SOA and NS answers come from dedicated zone fields. Negative answers carry the emitted SOA serial. Wildcard synthesis follows the closest existing encloser and does not override an existing node or empty non-terminal. In-zone MX exchanges and SRV targets receive available A and AAAA glue in the additional section.

When a request contains EDNS, the response advertises a 1232-byte payload. Resolution remains transport-blind; truncation occurs during UDP encoding.

## Wire and serve APIs

`wire::decode` parses a bare DNS message. `encode_udp` serialises against a supplied payload limit, drops additional and then answer data as needed, and sets `TC`. `encode_tcp` returns a two-byte big-endian length prefix followed by an untruncated DNS message.

`serve_udp` and `serve_tcp` run Tokio serve loops over supplied sockets and a shared `ZoneStore`. Fatal socket or listener errors are returned. Malformed queries and per-query I/O errors are logged and do not stop the loop.

`serve_udp_observed` and `serve_tcp_observed` additionally call a `ResponseObserver` exactly once with the resolver-built response before encoding and sending. Observers run inline, receive an immutable message reference, and must not block.

## Cargo features

| Feature | Default | Effect |
|---|---:|---|
| `default` | Yes | Empty feature set; the default build and `--no-default-features` build are equivalent |

There is no `cosmix` feature in this library.

## Dependencies

| Dependency | Use |
|---|---|
| `hickory-proto` | DNS messages, record types, and binary codec |
| `cosmix-lib-mix` | Strict-data parsing for `zones.mix` |
| `indexmap` | Map type exposed by Mix strict-data values |
| `tokio` | UDP and TCP asynchronous serve loops |
| `arc-swap` | Lock-free replacement of immutable snapshots |
| `tracing` | Reload and serve-path diagnostics |
| `tempfile` | Development-only persistence and store tests |

## Testing

The crate has an empty default feature set, so its documented core gate is:

```sh
cargo test -p cosmix-lib-dns --no-default-features
```

The test suite covers canonical names, RFC 1982 arithmetic, strict-data parsing, candidate rejection and adoption, persistence monotonicity, last-known-good reloads, resolver response codes, wildcard and empty-non-terminal behaviour, glue, EDNS and truncation, TCP framing, record round trips, and observed/unobserved serve-loop parity.
