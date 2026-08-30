# Node configuration

`node.conf.mix` defines one node's identity and the service configuration used
by noded, maild, webd, and simple service toggles. It is strict-data Mix, not
executable Mix. TOML is not accepted.

## Loading

Use `node::load_from(path)` when a caller already has an explicit path. The
built-in discovery functions use this order:

1. The path in `COSMIX_NODE_CONFIG`.
2. `node.conf.mix` in the resolved `CosmixDir::Etc` directory.
3. `/etc/cosmix/node.conf.mix` when `COSMIX_ETC` is not set and that path is
   distinct from the resolved configuration directory.

`load_node_config()` returns `Ok(None)` if no candidate exists.
`require_node_config()` reports absence as an error. A selected file that
cannot be read or parsed is a hard error; discovery does not continue to a
later candidate.

Setting `COSMIX_ETC` is an isolation boundary. Discovery does not fall through
from that directory to `/etc/cosmix`.

## Minimal form

`NodeConfig` and its main service sections use serde defaults, so a file may
state only the values that differ from defaults.

```text
node: "alpha"
wg_ip: "192.0.2.10"
mesh: "example.com"
noded: {
  port: 4200
  admission: "off"
}
```

The `node` value is a human-readable node name. `wg_ip` is the address used to
derive the default service binds. `mesh` is an optional mesh FQDN used to
recognise cross-mesh addresses.

## Top-level sections

| Field | Type | Purpose |
|---|---|---|
| `node` | string | Human-readable node name. |
| `wg_ip` | string | Node address used by derived listener and upstream values. |
| `mesh` | optional string | Mesh FQDN for cross-mesh address recognition. |
| `noded` | `NodedConfig` | Broker port, mesh file, and admission posture. |
| `maild` | `MaildConfig` | Mail listeners, storage, TLS, and spam settings. |
| `webd` | `WebdConfig` | Web listeners, vhosts, TLS, ACME, and routing settings. |
| `deskd` | `ServiceToggle` | Enables or disables deskd. |
| `indexd` | `ServiceToggle` | Enables or disables indexd. |
| `claud` | `ServiceToggle` | Enables or disables claud. |
| `mcp` | `ServiceToggle` | Enables or disables MCP. |

`ServiceToggle.enabled` defaults to `true`.

## Noded

`noded.port` defaults to `4200`. `noded.mesh_config` is an optional mesh
configuration path.

`noded.admission` accepts:

| Value | Behaviour |
|---|---|
| `off` | Disables admission challenges. This is the default. |
| `observe` | Runs challenges, verdicts, and logging without refusing a session. |
| `enforce` | Refuses a failed gated session. |

`NodeConfig::noded_listen()` returns `wg_ip:noded.port`.
`NodeConfig::noded_url()` returns `ws://wg_ip:noded.port/ws`; an empty `wg_ip`
uses `127.0.0.1`.

## Maild

The mail section provides:

| Field group | Fields |
|---|---|
| Service and network | `enabled`, `jmap_port`, `smtp_port`, `smtps_port`, `hostname` |
| Storage | `database`, `blob_dir`, `mds_dir` |
| Legacy TLS | `tls_cert`, `tls_key` |
| SNI TLS | `tls.identity`, `tls.strict_sni` |
| Spam filtering | `spam_enabled`, `spam_db_dir` |

Each `maild.tls.identity` row requires `server_name`, `cert`, and `key`.
`default` selects the greeting and SNI-mismatch fallback identity.
`no_sni_fallback` selects the identity for implicit TLS without SNI. Both flags
default to `false`.

When `tls.identity` is empty, maild may collapse the legacy `tls_cert` and
`tls_key` pair into one fallback identity. `strict_sni` defaults to `false`.

The helper methods `jmap_listen`, `smtp_inbound_listen`, and `smtps_listen`
combine `wg_ip` with their corresponding ports. `jmap_upstream` produces an
HTTPS upstream URL for webd.

## Webd

The web section provides:

| Field | Purpose |
|---|---|
| `enabled` | Enables webd. |
| `port` | Port used by the implicit node-address listener. |
| `www_dir` | Legacy document root. |
| `tls_cert`, `tls_key` | Legacy manual certificate and key paths. |
| `tls_server_name` | Hostnames covered by the legacy certificate pair. |
| `http_listen` | Optional explicit plain-HTTP bind used by HTTP-01 and redirects. |
| `acme_tos_accepted` | Accepted Let's Encrypt Subscriber-Agreement URL for production ACME. |
| `served_mail_domains` | Allowlist for mail autoconfiguration requests. |
| `autoconfig_mail_host` | Optional advertised mail host override. |
| `vhost` | Per-vhost routing, content, TLS, ACME, and upstream settings. |
| `listener` | Explicit per-interface listener rows and vhost allowlists. |
| `listeners.operators` | Bus sender names allowed to write listener-control properties. |

An empty `listener` array makes `synthesize_listeners` produce one internal
listener named `wg`, bound to `wg_ip:webd.port`, serving every resolved host.

With explicit listeners, every row contains:

| Field | Meaning |
|---|---|
| `id` | Non-empty stable listener identifier. |
| `bind` | Explicit `ip:port` socket address. |
| `external` | Marks the connection as public or untrusted-facing. |
| `enabled` | Bootstrap state; defaults to `true`. |
| `vhosts` | Hostname allowlist served by this listener. |

Listener IDs and parsed binds must be unique. A wildcard bind cannot share its
port with another bind. Every served hostname must belong to exactly one
enabled listener. A hostname disabled during vhost resolution may be skipped;
an unknown hostname remains an error.

## Vhosts

Each `webd.vhost` row may set:

| Field group | Fields |
|---|---|
| Routing and files | `host`, `aliases`, `www_dir`, `docs_dir` |
| TLS and ACME | `tls_cert`, `tls_key`, `acme` |
| Data | `cms_db_path`, `aux_dbs` |
| Upstreams | `jmap_upstream`, `noded_ws` |
| Development session | `dev_session_email`, `dev_session_password` |
| Public read identity | `public_read_email`, `public_read_password` |
| System sender | `system_sender_email`, `system_sender_password` |
| Emergency policy | `mfa_break_glass` |

`aux_dbs` rows require `name` and `path`. Validation of the schema name and
path occurs when webd opens the database.

The three password fields are deserialised but skipped during serialisation.
The `Debug` implementation prints a redaction marker instead of their values.

`WebdVhostConfig` tolerates unknown fields so older daemon builds can still
read the shared node file. Core listener, auxiliary-database, and ACME
structures reject unknown fields.

## ACME

A vhost selects manual PEM with `tls_cert` and `tls_key`, or ACME with an
`acme` block. It cannot select both.

```text
webd: {
  http_listen: "192.0.2.10:80"
  vhost: [
    {
      host: "www.example.com"
      aliases: ["example.com"]
      www_dir: "/srv/www/example"
      acme: {
        provider: "letsencrypt_staging"
        challenge: "http01"
        contact_email: "admin@example.com"
      }
    }
  ]
}
```

`provider` accepts `letsencrypt_prod` and `letsencrypt_staging`.
`challenge` defaults to `http01`. `dns01` parses but
`resolve_webd_acme` rejects it as unsupported.

Every ACME vhost requires `webd.http_listen`. Contact addresses must contain
non-empty local and domain parts, contain no carriage return or line feed, and
be at most 253 bytes.

Any production ACME vhost requires `acme_tos_accepted`. The resolver trims the
value and requires a non-empty URL below `https://letsencrypt.org/`. Staging
does not mint a ToS proof.

`resolve_webd_acme` returns one `AcmeVhostPlan` per ACME vhost and one optional
node-level `AcmeTosAcceptance`. The plan retains the source vhost index, primary
name, aliases, provider, challenge, and contact address. Hostname
normalisation remains the caller's responsibility.
