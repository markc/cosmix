# Desktop Bus capabilities

Status: initial local-session provider, API 1, implementation 0.1.0. This is the
first layer for desktop automation through noded. Mesh grants, cross-broker
pairing and automatic clipboard synchronisation are not implemented yet.

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

Both targets currently reside on the same broker. Remote `.bus` targets are
deliberately rejected by this initial CLI. The commands themselves are normal
Bus RPCs; the provider contains no replacement mesh transport.

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

All desktop verbs require broker-stamped `broker_origin=local` and a canonical
registered `from`. noded removes anonymous claims and rewrites client-supplied
origin headers. Mesh ingress does not yet supply verified remote-service
identity, so the provider rejects it. Registration is not Unix UID identity:
this is a trusted local broker boundary, not isolation from hostile local apps.

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
Live cross-desktop Wayland and paired mesh tests belong to the next stage.
