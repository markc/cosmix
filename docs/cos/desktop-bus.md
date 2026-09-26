# Desktop Bus capabilities

Status: API 1, implementation 0.3.6. Every verb is open to mesh callers by
default. Explicit grants apply only under the opt-in lock described below.
Cross-node calls use native ABP between noded instances, requiring noded
0.15.0 or newer at both ends. Automatic clipboard synchronisation is not
implemented.

The implementation version is the `-- version:` header of
`src/desktop/scripts/desktop-session.mix`, which is what
`mix desktop-session.mix --version` prints and what `desktop.capabilities`
reports as `implementation_version` (read through `script_version()`). Bump
the header. `lib/desktop.mix` keeps the same number as a literal fallback for
a mix older than 0.95.0, and `tests/desktop-test.mix` fails when the two
disagree.

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

`apps.launch` sets `DISPLAY` for each child from comp's live XWayland,
not from the citizen's own environment. A static `DISPLAY` in the
citizen's unit is ignored. At every launch it reads comp's XWayland
descriptor, `$XDG_RUNTIME_DIR/cosmix-comp/$WAYLAND_DISPLAY.xwayland.env`,
and passes its `DISPLAY=:N` to the child on top of the inherited
environment. Because it is read per launch, an XWayland restart on another
display number is seen by the next launch.

With no usable descriptor, the child is launched with `DISPLAY` removed,
never inherited. That covers XWayland not ready yet, restarting or
disabled, and a nested comp without it. An X11 app then fails at once
instead of drawing on another X server. The citizen writes one stderr line
per such launch naming the reason, including an unreadable descriptor. The
file is mode 0600, so the citizen must run as comp's uid. The unset goes
through `/usr/bin/env -u DISPLAY`, because Mix's spawn `env` option cannot
remove a variable. The program is resolved first, on the same `PATH` the
child gets, and env is handed the absolute path. A path containing `=` goes
through `/bin/sh -c 'exec "$@"'`, because GNU env reads any such operand
as an assignment. On this path the app's `argv[0]` is therefore its
absolute path. A missing or non-executable program is still refused with
rc 11 `spawn_failed`, never answered with the wrapper's pid.

A descriptor left behind by a comp killed with SIGKILL is not checked
against its `GENERATION` or against a live XWayland. It is trusted until
comp's next start republishes or removes it.

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

## Workspace citizen (`desktop`)

`src/desktop/scripts/desktop-workspace.mix` registers the Bus service
`desktop` and switches cosmix-comp's workspaces. inputd's default keymap fires
two of its verbs with an empty body: RightCtrl+Left (code 105) sends
`desktop.workspace.prev` and RightCtrl+Right (code 106) sends
`desktop.workspace.next`. Both rows route by first segment, so they reach this
citizen with no `service` field (see
[Inputd keymap rows](daemon-help.md#inputd-keymap-rows-and-their-target-service)).
Start it under that name:

```text
mix --serve /path/to/cosmix/src/desktop/scripts/desktop-workspace.mix --name desktop
```

Each verb makes exactly one call to comp, with a 3-second budget.
`COSMIX_DESKTOP_COMP_SERVICE` names comp's Bus service and defaults to `comp`.

| Verb | JSON request | Forwards to comp | Reply |
|---|---|---|---|
| `desktop.workspace.next` | empty or `{}` | `comp.workspace.switch {"index":"next","wrap":true}` | `{ok,from,to}` |
| `desktop.workspace.prev` | empty or `{}` | `comp.workspace.switch {"index":"prev","wrap":true}` | `{ok,from,to}` |
| `desktop.workspace.set` | `{"n":N}`, N an integer 1–16 | `comp.workspace.switch {"index":N,"wrap":true}` | `{ok,from,to}` |
| `desktop.workspace.current` | empty or `{}` | `comp.props.get {"path":"workspaces"}` | `{ok,desktop,count}` |

comp does the wrap: `next` on the last workspace lands on the first, and
`prev` on the first lands on the last. The citizen sends `wrap` explicitly, so
it does not depend on comp's default. `from` and `to` are comp's own numbers.
`current` returns `count` too, so a caller knows where `next` will wrap. comp
still range-checks `set`, so an `N` above the live workspace count is comp's
`invalid_value`.

```json
{"ok":true,"from":4,"to":1}
{"ok":true,"desktop":3,"count":4}
```

Every failure has an rc of 10 or more, so the `$rc >= 10` test reads it as a
failure:

| rc | Meaning | Body | What to do |
|---|---|---|---|
| 10 | Malformed request, refused locally and never sent to comp | `{ok:false,error}` | Fix the request body |
| 11 | comp unreachable: no reply, a broker refusal such as an unregistered service, or the 3-second timeout | `{ok:false,error}` | Check that comp is registered and running |
| 12 | comp replied outside its contract | `{ok:false,error:"malformed comp reply",comp}` | Likely a comp version mismatch, so file it |
| comp's rc | comp refused, for example `locked` under a session lock | `{ok:false,error,comp}` | Read `comp.error_code` |

A malformed request is a body that is not a JSON object, any field on `next`,
`prev` or `current`, or a `set` without exactly an integer `n` in range. On a
comp refusal, `comp` is comp's whole reply. comp stamps `error_code` beside
`error` on every refusal and adds detail fields where it has them. A `set`
above the live count, for example, is rc 10 with this `comp` field:

```json
{"error":"invalid_value","path":"index","expected":"unsigned integer","range":"1..=4","error_code":"invalid_value"}
```

`error` in the reply is comp's `error`, or its `error_code` when `error` is
absent. On a comp whose `workspaces` subtree has no `count`, which is true of
releases before workspace counts, `current` answers rc 12 with the subtree
under `comp`. There is no retry and no second backend. Mesh callers reach every
verb with no authorization gate, and only well-formedness is checked.

`next` is a Mix keyword, so quote the verb in a Mix `send` until the parser
fix lands:

```mix
send desktop "desktop.workspace.next"
$b = json_encode({n: 2})
send desktop desktop.workspace.set body=$b
```

Install: the citizen requires `lib/workspace.mix` beside it, so install both
files into `/opt/cosmix/share/desktop/`. Stop any running `desktop` citizen
first, because two registrations of the same service name are undefined.

```text
install -D -m 0644 src/desktop/scripts/desktop-workspace.mix /opt/cosmix/share/desktop/desktop-workspace.mix
install -D -m 0644 src/desktop/scripts/lib/workspace.mix /opt/cosmix/share/desktop/lib/workspace.mix
```

`src/desktop/scripts/cosmix-desktop-workspace.service` is an example system
unit. Its one placeholder is `User=CHANGE-ME`: set it to the desktop user
before `systemctl enable`. systemd derives `HOME` from it. The unit's comment
describes the user-unit form for hosts that run a user session manager. Validation and reply shaping live in
`src/desktop/scripts/lib/workspace.mix`.

Earlier deployments ran a private copy of this citizen with a KWin fallback
over the session D-Bus. This script replaces that copy. The KWin path is not
carried over, because cosmix components reach D-Bus only through
cosmix-dbusd adapters.

## Scene Editor chord (`scenes`)

inputd's default keymap binds Ctrl+Alt+P twice on `KEY_P` (code 25): once with
Left Ctrl and Left Alt, once with Right Ctrl and Left Alt. Both rows send
`scenes.editor.open` with body `{"safe":true}` to the scenes loader, which is
registered as `scenes`. Right Alt is not bound, because it is AltGr on some
layouts. The rows swallow the chord, so applications never receive it, and they
fire on the press only, not on auto-repeat. A safe open while the shipped
editor is visible hides it, so the same chord opens and closes the editor. The
rows and how to add them to an existing keymap file are in
[Inputd keymap rows](daemon-help.md#inputd-keymap-rows-and-their-target-service).

The same request from a terminal or an agent needs no key:

```mix
$b = json_encode({safe: true})
send scenes "scenes.editor.open" body=$b
```

## Verification

`tests/desktop-test.mix` exercises production request validation and result
handling without desktop effects. Run it with `COSMIX_MESH_OPEN=0` in the
environment:

```text
env COSMIX_MESH_OPEN=0 mix src/desktop/scripts/tests/desktop-test.mix
```

Most of the file tests the locked posture, and without that setting it fails
early, at the first grant check. The test opens the default posture itself for
its full-mesh-access block and restores the setting afterwards. The locked
checks for menu, rotate and capabilities run only when the setting is `0`.
`tests/desktop-bus-test.mix` uses a real,
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

`tests/workspace-test.mix` exercises the workspace citizen's request
validation and reply shaping with no Bus and no compositor:

```text
mix src/desktop/scripts/tests/workspace-test.mix
```
