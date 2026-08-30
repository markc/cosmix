# cosmix-dnsd configuration

`cosmix-dnsd` takes its service inputs from command-line options. The citizen
build can additionally read `wg_ip` from the standard `node.conf.mix` search
path.

## Command line

```text
cosmix-dnsd --zones <zones.mix> --state <state-file> \
  --listen <ip:port> [--listen <ip:port> ...] \
  [--allow-non-loopback-listen]
```

| Option | Required | Meaning |
|---|---:|---|
| `--zones <path>` | yes | Strict-data Mix file containing the static zones. |
| `--state <path>` | yes | Persistent daemon state used by the zone store. |
| `--listen <ip:port>` | yes | UDP and TCP listen address. Repeat for multiple addresses. |
| `--allow-non-loopback-listen` | no | Allows a non-loopback address in standalone mode or when trusted node configuration is unavailable. |
| `-h`, `--help` | no | Prints the usage string and exits with status `2`. |
| `-V`, `--version` | no | Prints the package name and version, then exits successfully. |

Hostnames are not accepted by `--listen`; the value parses as a socket address.
Use brackets around IPv6 addresses, for example `[::1]:5353`.

## Zone file

The crate fixture uses Mix strict-data syntax. The top level contains a
`zones` map. Each zone supplies an SOA description, an NS list, a configured
serial floor, and a list of owner bundles.

```text
zones: {
  "example.com": {
    soa: {
      primary: "beta.example.com",
      mbox: "hostmaster.example.com",
      ttl: 300,
      minimum: 60
    },
    ns: [ "beta.example.com" ],
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
            data: "192.0.2.5"
          }
        ]
      }
    ]
  }
}
```

Entries inside maps and lists are comma-separated. The crate fixture exercises
`A`, `MX`, `SRV`, and `PTR` records plus generated zone-apex `SOA` and `NS`
answers.

The daemon treats `--zones` as read-only input. Send `SIGHUP` after replacing
the file to request an adopt-or-keep-last-good reload.

## State file

`--state` names the file used by `FilePersistence`. The zone store uses it to
retain its floor state.

At startup:

- a readable state file is loaded;
- a corrupt or unreadable state file is logged and treated as absent; and
- an unusable initial zone candidate with no last-known-good state stops the
  daemon.

The state file format is internal to the zone store. Do not use it as the zone
configuration source.

## Listen addresses

Every explicit `--listen` value is mandatory: failure to bind either UDP or TCP
stops startup. Each listener serves the same zone snapshot.

| Address | Standalone build | Citizen with usable `wg_ip` | Citizen without usable `wg_ip` |
|---|---|---|---|
| Loopback | accepted | accepted | accepted |
| Configured `wg_ip` | requires override flag | accepted | requires override flag |
| Other non-loopback | requires override flag | rejected | requires override flag |
| Wildcard or unspecified | rejected | rejected | rejected |

`--allow-non-loopback-listen` never permits a wildcard address. In a citizen
with a usable `wg_ip`, it also never permits an address other than that
configured value.

IPv4-mapped IPv6 addresses are canonicalised before classification and
comparison. For example, `[::ffff:192.0.2.5]:53` matches a configured
`192.0.2.5`.

## Citizen node configuration

The default build asks `cosmix-lib-config` to load `node.conf.mix` using its
standard environment, XDG, and system configuration search. Only the `wg_ip`
field affects this crate's bind decision.

The configured value must parse as an IP unicast address. Unspecified,
loopback, multicast, and IPv4 broadcast values are not usable mesh identities.
An absent, unreadable, malformed, or unusable value produces a warning and
selects the degraded standalone bind rule.

When the configured address is not yet present on an interface, the citizen
retries the UDP and TCP bind pair for up to 30 seconds at one-second intervals.
`AddrInUse`, permission errors, and other bind failures are returned
immediately.

## Citizen loopback listener

After binding every explicit address, the citizen attempts
`127.0.0.1:53` unless it was already requested. This implicit listener is
best-effort: failure is logged but does not stop service on explicit
listeners.

Passing `--listen 127.0.0.1:53` makes that listener explicit and restores the
normal fatal-on-bind-failure rule.

The standalone build creates no implicit listener.

## Signals

| Signal | Effect |
|---|---|
| `SIGHUP` | Reloads the zone candidate; keeps the current snapshot on rejection. |
| Interrupt (`Ctrl-C`) | Aborts service tasks and exits successfully. |

Return to the [crate overview](README.md).
