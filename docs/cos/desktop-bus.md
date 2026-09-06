# Desktop Bus capabilities

Status: API 1, implementation 0.2.0. Explicit clipboard grants use native ABP
between noded instances, requiring noded 0.15.0 or newer at both ends.
Automatic clipboard synchronisation is not implemented.

The reusable scripts live in `src/desktop/scripts/`. One supervised Mix citizen
belongs to one desktop session. It uses the Wayland display, runtime directory
and session D-Bus inherited at launch; requests cannot select another session
or executable. Use separate service names for simultaneous desktops.

## Start a provider

Create a private strict-data session configuration containing:

```mix
{session:"desktop-a", opener:"/usr/bin/xdg-open"}
```

Set `DESKTOP_SESSION_CONFIG` to its absolute path and `COSMIX_NODE_CONFIG` to
the intended local broker configuration. Launch from the intended desktop
environment, under its user:

```text
mix --serve /path/to/cosmix/src/desktop/scripts/desktop-session.mix --name desktop-a
```

The session needs `wl-copy`, `wl-paste` and the configured opener. The service
reports whether these prerequisites are configured; actual operations can
still fail if the compositor or session bus is unavailable. Run it in a
systemd unit bound to the desktop lifetime, with `KillMode=control-group`, so
clipboard-owner children exit with that session. Set `MIX_STATS=off`.

## Commands

Set `COSMIX` to the checkout root (or `DESKTOP_REQUEST_WORKER` to the installed
`desktop-request.mix`). The CLI registers a temporary citizen and deregisters
when finished. It prints metadata and outcome only, never clipboard text or
the opened URL.

```text
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix capabilities desktop-a
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix copy desktop-a desktop-b
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix open desktop-b https://example.org/
```

Targets may be local services or `service.node.bus` addresses. Cross-node
messages use the existing noded ABP transport, without an alternative relay.
To grant clipboard access to registered local citizens of node `alpha`, add
`mesh_clipboard_nodes:["alpha"]` to the trusted provider configuration and
restart it. The default grant list is empty. This grants capabilities/read/write;
HTTP(S) opening remains local-only. Cross-mesh `@` addresses are refused.

Both nodes need protected WireGuard endpoints, verified signed membership and
D2 identities. The receiving noded must enforce admission. A provider grant
never substitutes for broker admission. Example:

```text
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix copy desktop-a desktop-b.beta.bus
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix copy desktop-b.beta.bus desktop-a
```

| Verb | JSON request | Successful response |
|---|---|---|
| `desktop.capabilities` | `{}` | API/implementation version, session/instance, configured operations and limits |
| `desktop.clipboard.read` | `{instance}` | `{instance,mime,text,bytes}` |
| `desktop.clipboard.write` | `{instance,text}` | `{instance,accepted:true,bytes}` |
| `desktop.open` | `{instance,url}` | `{instance,accepted:true}` |

Discover the current instance before read/write/open. Each process start gets
a new UUID; broker reconnect preserves it. Unknown fields, old instances,
non-HTTP(S) URLs, NUL text, invalid UTF-8 and oversized text are rejected.
Text whitespace and trailing newlines are preserved. Empty text is valid;
unavailable clipboard data returns an error rather than inventing empty text.

`accepted` means the fixed helper exited successfully. It does not prove a
browser page loaded or that another application pasted the text. A helper
timeout returns `ACTION_OUTCOME_UNKNOWN`; callers must not automatically retry.
There is no idempotency/replay guarantee in API 1.

Errors use a nonzero application rc and `{error:STABLE_CODE}`: rc 10 invalid
request/data; 11 unavailable/helper failure; 12 stale session; 13 caller
rejected; 20 timeout with ambiguous outcome. Transport errors remain separate.

## Trust and privacy boundary

Local calls require broker-stamped `broker_origin=local` and a canonical
registered `from`. An opted-in mesh clipboard call requires `broker_origin=mesh`,
an allowed `broker_peer`, a canonical `broker_service`, and `from=bridge-<peer>`.
noded supplies these only for a direct registered source received on a proven,
currently authorised bridge connection. Anonymous sources and multi-hop relays
receive no such authority. Callers cannot supply their own identity stamps.

A node grant trusts that node's registered local citizens; it is not per-app
consent or Unix UID isolation. Revocation is checked at broker enqueue; work
already queued or executing cannot be recalled. Session admission retains the
existing inventory policy for overlapping D2 credentials. See [noded](noded.md).

Broker taps can expose message bodies. Do not mistake suppressed helper logs
for end-to-end clipboard confidentiality. Use an isolated/trusted broker for
real clipboard data. No payload is deliberately written to disk, retained on
topics or included in notifications by these scripts. Runtime-reserved verbs
(including `QUIT`) and lifecycle properties are provided by Mix and do not
pass through the desktop handler checks.

## Verification

`tests/desktop-test.mix` exercises production request validation and result
handling without desktop effects. `tests/desktop-bus-test.mix` uses a real,
isolated noded and two production citizens with synthetic helper programs;
it requires a user systemd manager and the installed Mix/noded binaries.
It does not read or replace the user's clipboard or open a real browser.
The noded suite covers reply connection ownership, spoofed identity stripping,
proof/registration/membership gates and reload while delivery is waiting.
Deployment acceptance still requires bidirectional transfer over the actual
admitted nodes and their Wayland sessions; unit gates do not prove that result.
