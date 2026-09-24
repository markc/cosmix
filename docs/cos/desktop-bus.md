# Desktop Bus capabilities

Status: API 1, implementation 0.3.6. Every verb is open to mesh callers by
default. Explicit grants apply only under the opt-in lock described below.
Cross-node calls use native ABP between noded instances, requiring noded
0.15.0 or newer at both ends. Automatic clipboard synchronisation is not
implemented.

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
By default every verb is open to mesh callers (see below). The grant lists
apply only when the provider runs with `COSMIX_MESH_OPEN=0`. Both lists are
empty by default. In that mode:

- `mesh_clipboard_nodes:["alpha"]` grants capabilities, read and write to node
  `alpha`'s registered local citizens.
- `mesh_history_nodes:["alpha"]` also grants `desktop.clipboard.history` and
  `desktop.clipboard.entry`. It must be a subset of `mesh_clipboard_nodes`.
- Every other verb is local-only.

Add the lists to the trusted provider configuration and restart it.
Cross-mesh `@` addresses are refused.

Under the open default, the citizen checks only that noded stamped the call
`broker_origin=mesh`. Whatever noded itself requires to deliver a mesh call
still applies; see [noded](noded.md). The lock needs more, because it relies on
the attested `broker_peer` and `broker_service` stamps. noded supplies those
only when both nodes have protected WireGuard endpoints, verified signed
membership and D2 identities, and the receiving noded enforces admission. A
provider grant never substitutes for broker admission. Example:

```text
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix copy desktop-a desktop-b.beta.bus
mix /path/to/cosmix/src/desktop/scripts/desktop-cli.mix copy desktop-b.beta.bus desktop-a
```

| Verb | JSON request | Successful response |
|---|---|---|
| `desktop.capabilities` | `{}` | API/implementation version, session/instance, access posture, configured operations and limits |
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
registered `from`. A call stamped `broker_origin=mesh` reaches every verb,
including `desktop.open`, `desktop.clipboard.menu` and
`desktop.clipboard.rotate`, with no per-peer grant (citizen 0.3.6 and later).
noded sets `broker_origin` from the source address, so the WireGuard mesh is
the trust boundary. The `instance` fence still applies to mesh callers: a stale
`instance` is refused with rc 12. Refusal with rc 13 is then reserved for two
cases. The first is a call with no recognised broker origin. The second is a
local policy gate, not a well-formedness check: a local call without a
canonical registered `from` is refused. The same anonymous send from a mesh
node is admitted, so locally the citizen is stricter than over the mesh.
Local callers use a registered one-shot citizen instead, as `desktop-cli.mix`
does. Whether to keep this gate is an open decision.

`desktop.capabilities` reports the posture it enforces. Under the open default
it returns `access:"mesh-open"` and `mesh_open:true`. Under the lock it returns
`access:"registered-local-with-explicit-mesh-clipboard-grants"` and
`mesh_open:false`. Before citizen 0.3.6, `access` always carried the second
string, and `mesh_open` was absent. `history_mesh.granted` counts the
configured history grants in either posture, although only the lock uses them.

Setting `COSMIX_MESH_OPEN=0` in the provider's environment re-arms the strict
opt-in lock. A mesh call then requires a verb covered by a grant list,
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
real clipboard data. Clipboard history IS written to disk: captured text is
kept in `history.path` if configured, otherwise in
`$XDG_STATE_HOME/cosmix/clipboard/history-<session>.json`, with
`~/.local/state` used when `XDG_STATE_HOME` is unset. Pause capture with
`desktop.clipboard.pause` and remove entries with `desktop.clipboard.clear`.
No payload is retained on topics or included in notifications by these
scripts. Runtime-reserved verbs
(including `QUIT`) and lifecycle properties are provided by Mix and do not
pass through the desktop handler checks.

## Application registry citizen (`apps`)

A second desktop citizen, `src/desktop/scripts/apps.mix`, is the data
foundation for the desktop launcher and the taskbar's app icons. It scans
the freedesktop desktop-entry directories (`$XDG_DATA_HOME/applications`,
then each `$XDG_DATA_DIRS` entry's `applications`, subdirectories folded
into the id with `-`), keeps `[Desktop Entry]`-group `Type=Application`
entries that pass `OnlyShowIn`/`NotShowIn` (against the desktop name
`COSMIX`) and `TryExec`; `Hidden=true` erases an entry (the spec treats it
as deleted) while `NoDisplay=true` only hides it from `apps.list` — the
entry stays in the index with `no_display` set, so `apps.get` and
`apps.launch` still find it. Localised keys resolve per the spec's
`lang_COUNTRY@MODIFIER` order from `LC_ALL`, then `LC_MESSAGES`, then
`LANG` (encoding stripped). Icons resolve through the freedesktop
icon-theme spec (exact size match, then closest by
`DirectorySizeDistance`, `Inherits` depth-first, `hicolor`, then
`/usr/share/pixmaps`; parsed `index.theme` files are cached in memory).
Start it like the session provider:

```text
mix --serve /path/to/cosmix/src/desktop/scripts/apps.mix
```

It is event-driven: the tree is scanned once at start and again only on
`apps.reload`. There is no timer or poll loop; a future inotify wake is the
intended rescan trigger. `src/desktop/scripts/cosmix-desk-apps.service` is
an example user unit bound to `graphical-session.target`.

| Verb | JSON request | Successful response |
|---|---|---|
| `apps.list` | `{category?, query?}` | array of `{id,name,generic_name,comment,icon,categories,exec,terminal,path}` sorted by name |
| `apps.get` | `{id}` | the entry with all parsed fields (adds `keywords`, `try_exec`, `no_display`, `workdir`, `only_show_in`, `not_show_in`) |
| `apps.icon` | `{name, size?, scale?, theme?}` | `{path,size,scale,theme,kind}` — `kind` is `svg`, `png` or `xpm`; `name` may be an absolute path |
| `apps.launch` | `{id, uris?}` | `{id,pid,argv}` — spawned detached via argv, never a shell |
| `apps.reload` | `{}` | `{count}` |

`query` is a case-insensitive substring over name, generic name, comment and
keywords. `uris` feed the Exec field codes: `%f`/`%F` take `file://` URIs
decoded to local paths (an empty or `localhost` authority is local per
RFC 8089; any other authority keeps the URI as-is), `%u`/`%U` take the
URIs, `%i` becomes `--icon ICON`, `%c` the name, `%k` the desktop-file
path; deprecated `%d %D %n %N %v %m` are dropped, unrecognised `%X`
sequences stay literal, and Exec quoting is parsed into argv per the spec
(`%f`/`%F`/`%u`/`%U` are only meaningful as a standalone argument).
`Terminal=true` entries are prefixed with the terminal property's argv —
`apps.terminal` may carry arguments (e.g. `kitty -e`) and is parsed with
the same Exec rules. Unknown ids answer `{error:"not_found"}` with rc 14;
malformed requests rc 10.

Properties publish through `apps.props.watch` (snapshot read; optionally
`{path}` for one leaf) and `apps.props.set` (`{path, value}`): `apps.count`
(read-only), `apps.icon_theme` (default: `[Icons] Theme` from
`~/.config/kdeglobals`, else `breeze` when installed, else `hicolor`) and
`apps.terminal` (default `foot`). Mix 0.89.0 reserves
`apps.props.{get,list,describe}` for the runtime-owned lifecycle tree, so
the author properties ride the SPEC-12 fall-through verbs instead.

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

`tests/apps-test.mix` exercises the apps citizen's desktop-entry parser and
icon resolver against a fixture tree it creates under a temp dir (no Bus,
nothing launched). The pure functions live in `src/desktop/scripts/lib/apps.mix`
so the test can require them directly. `tests/apps-bus-test.mix` serves the
production apps citizen against an isolated noded (private TCP port plus
private verified-lane unix socket, so it needs no user systemd manager and
cannot reach the host Bus) and covers list/get/launch/reload/props over the
real Bus with recorder-script fixture "applications" — no real application
is ever launched.
