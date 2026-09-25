# Scenes loader

`mix --serve scenes.mix --name scenes` owns file-backed scene lifecycles.
This is Stage A: it runs beside `quoin-panel.mix`, starts with no enabled
scenes, and reports `needs_setup:true` until scenes are explicitly enabled.
The Scene Editor gallery and recovery chord are later work; this loader does
not claim to implement them.

## Files and installation

```
~/.config/cosmix/scenes/
  state.conf.mix
  <name>/scene.mix
  <name>/template.conf.mix
  <name>/behaviour.mix       # optional
```

`scene.mix` is an AMP scene document, never executable Mix. Metadata and
state are strict data, parsed with `data_parse`. Only `behaviour.mix` runs.
The document name must match its installation directory. Names contain
lower-case ASCII letters, digits, `-` and `_`, at most 64 characters; leading
`-` and `_` are refused. The scene/metadata/behaviour files and installation
directory cannot be symlinks. Shipped templates live at `$COSMIX/share/scenes`.

State has `{schema_version:1,enabled:[],origins:{},recovery:{}}`. Writes use
`write_atomic` with full durability and mode `0600`. Enablement is separate
from template metadata. A directory discovered through the filesystem remains
disabled. A loader lock prevents two loaders sharing one directory.

`setup.mix --desktop` installs `bin/scenes.mix` and its helpers; the checkout's
`share/scenes` is the canonical template installation. Adding `--system`
copies the loader/helpers and templates under `/opt/cosmix` too. No unit is
enabled, no user scenes are seeded and Quoin configuration is not rewritten.
The example `src/_etc/systemd/cosmix-desk-scenes.service.in` must be rendered by
the session installer, with a desktop user, session environment and paths.

Environment overrides:

| Variable | Meaning |
| --- | --- |
| `SCENES_DIR` | User scene root; default `$XDG_CONFIG_HOME/cosmix/scenes` |
| `SCENES_TEMPLATES` | Shipped template root; otherwise `$COSMIX/share/scenes`, or discover the checkout from the script |
| `SCENE_HOST` | Quoin service; default `shell` |

The native Mix binary is resolved from the running interpreter. Behaviour
children inherit the session and existing `COMP_SERVICE`, `APPS_SERVICE`,
`TRAY_SERVICE`, `NOTIFY_SERVICE` and other overrides. They additionally receive
`SCENE_NAME`, `SCENE_HOST`, `SCENES_SERVICE`, `SCENE_GENERATION`, and `SCENE_DIR`;
their working directory is their scene directory. Each runs as `scene-<name>`.
`SCENES_LIB` defaults to the shipped catalogue's `lib` directory, matching the
shared behaviour helpers, and can be overridden explicitly. Generations are
positive exact integers, allocated from an atomically persisted `.generation`
counter before spawning. Disabling invalidates the active generation to zero.
The document's citizen must match that routing name when it has a behaviour.
Behaviours should use `serve_name()` for their actual identity.
Without a `window` header, the loader follows Quoin's first `window` node
for popup routing; an absent edge defaults to `right`.
Managed popup page and edge values must be static; the loader refuses bindings
on those routing fields. Host model patches also refuse a changed mount address
without advancing the revision; unload and load to move a mount explicitly.

## Public Bus contract

Verbs are mesh-open, with the same implementation for local and attested mesh
requests. A refusal has nonzero RC and `{error_code,message,context?}`. Upstream
RC, diagnostics and original payload remain under `context`; filesystem
diagnostics also report file, attempted digest and last-good revision.

| Verb | Arguments and result |
| --- | --- |
| `scenes.list` | `{}` → `{needs_setup,scenes,diagnostic}`; each scene reports installation, enablement, mounted state, revision, generation, behaviour status, ready/open/pending, origin, digests, diagnostics, last exit and journal location |
| `scenes.install` | `{template,name?,enable?}` → installation receipt; stage, validate through `shell.scene.validate`, atomically rename, record origin; default disabled; refuse collisions |
| `scenes.remove` | `{name}` → `{name,recovery}`; disable, stop, unload and move the installation to `.recovery/` |
| `scenes.enable` / `scenes.disable` | `{name}` → acceptance/revision; persist desire and reconcile idempotently |
| `scenes.reload` | `{name}` → acceptance/revision; retry files and failed behaviour, retaining runtime model |
| `scenes.reset` | `{name}` → `{name,recovery}`; stage recorded shipped origin, validate, retain old copy, restore defaults; refuse unknown origin |
| `scenes.ready` | `{name,generation}` → `{name,generation,publish_model:true}`; the behaviour then publishes its initial complete model |
| `scenes.model` | `{name,generation,value}` → acceptance/revision; `value` is a complete map, submitted as `shell.scene.patch {scene,path:"model",value}` |
| `scenes.open` / `scenes.close` / `scenes.toggle` | `{name}` → `{name,open,pending}`; desired popup state, with applied completion reported through `scenes.changed` |

Stage A refuses the reserved pages `scene-panel`, `scene-launcher`,
`scene-calendar`, `scene-notes` and `settings.appearance`. For example,
install `{template:"launcher",name:"preview-launcher"}`. Installation
rewrites the scene name, behaviour routing citizen and an explicitly authored
panel name to the distinct installation name. This does not migrate existing
legacy registrations.

`scenes.changed` publishes a change-only inventory with a loader revision.
Subscribers should take `scenes.list` after subscribing and repeat that
snapshot after reconnection/gaps. Invalid edits leave accepted content visible.
Unchanged accepted files and repeated model/exit/applied-state events are
suppressed. Temporary and backup files are ignored.

## Lifecycle and events

The loader registers filesystem watches and Bus subscriptions before reading
state and directories. It takes the broker registration snapshot after
subscribing, then mounts enabled documents as the loading citizen and starts
behaviours. Quoin directs authored click commands to the document's citizen,
with `{scene,node,kind,value?,item?}` in `$event.args`. Behaviours never load
or unload their scenes.

`fs_watch` / `on fs.changed` batches trigger one reconciliation per affected
scene; `overflow:true` triggers one rescan. The stable parent is watched too.
Layout changes use full `shell.scene.load`, carrying the last accepted runtime
model in the candidate. Model updates use the patch endpoint. Behaviour edits
pass `mix --check` and `mix lint --deny-warnings` before replacement. Generation
invalidation precedes stop; replacement waits for the retiring child's exit.

Managed spawn uses `{die_with_parent:true,exit_event:true,tag:<JSON string>}`.
The string encodes `{name,generation}`; `on proc.exited` receives
`{pid,tag,exit_code,signal}` in `$event.args`. A crash leaves the scene mounted.
Four failure-triggered restart deadlines are 1, 2, 4 and 8 seconds; the fifth
consecutive exit enters `crash_loop`. Running at least 60 seconds resets the
count at the next exit, without a health timer. Explicit reload retries the
failed scene. Disable during a deadline invalidates its generation.

The process adapter uses W1's registry-backed `kill(pid,9)` to stop the owned
child through its retained pidfd. Generation checks precede stop and exit
handling is synchronous: the numeric PID is discarded as soon as its exit
event is delivered. Replacement waits for that event; the native reaper owns
descendant cleanup. SIGKILL is deliberate so a stuck behaviour needs no stop
poll or escalation timer. Only the crash restart deadline runs in an async
self-directed handler. The installed pre-W1 interpreter cannot run this
watcher/child path; deterministic tests fake only that boundary. The integration
gate must confirm W1's PID retention and exit-delivery guarantees before
enabling the unit.

`on bus.connected` and `noded.props.changed` repair state with full service
snapshots and `shell.scenes.list` mount inventory. This also removes mounts
disabled or removed while the broker was unavailable but still present in Quoin.
A returning shell is remounted from accepted source/model even when
the current on-disk edit is broken. Behaviour service disappearance alone
never unloads a scene. The owner is the verified loading citizen.

Popup selection, temporary pinning, mutual exclusion and saved-mode recovery
advance on applied `shell.panel.changed` receipts, never select/hide polling.
Recovery is persisted before pinning. If another page takes the edge, recovery
does not hide it. The legacy panel remains in use in Stage A but consumes the
same applied-state stream; its network/audio conversion belongs to Stage C.

## Gates and installer follow-up

Run `mix src/desktop/scripts/tests/scenes-test.mix` for deterministic loader
transitions with real temporary files. `scenes-mesh-test.mix` checks list and
all mutation refusals over local and attested mesh routes, using the two target
environment variables documented in that test. Rust regression tests cover
aggregate model bounds, transactional refusals, retained bindings/ports,
existing list models and bound row refresh, authored-source round trips,
behaviour disconnect and applied panel notices.
The build cluster runs the Rust gates and native W1/Quoin integration tests.

The private session installer must install the loader, helpers and templates;
render the example unit in the desktop session; supply the matching native Mix
binary and Quoin; and leave the existing panel unit running during Stage A.
Stage B must explicitly seed the chosen set, retain popup recovery and rollback
material, stop the old citizen, and only then hand over the existing page IDs.
Readiness checks must consume service/readiness/applied-state events. No private
installer is changed by this public workstream.

Limits and remaining operational assumptions: at most 128 installed scene
records, 256 KiB per scene/behaviour file and runtime model, and 64 KiB state or
metadata. Recovery copies are retained for the operator to manage. Trusted
desktop-user edits are assumed: filesystem prechecks are not an `openat`
security boundary against a simultaneous hostile symlink swap. Reset uses two
renames with rollback on an ordinary error; power loss between those renames
leaves the preserved copy under `.recovery`. Multiple processes directly
editing state do not share the loader's advisory lock. A transport timeout can
leave the upstream mutation applied before a later event repairs local state.
The private `.generation` counter must not be removed or rolled back while
old behaviour messages can still be delivered; ordinary install/reset never
touches it.
The advisory loader lock assumes the root directory itself remains in place;
replacing the root can detach that lock inode even though native watches
recover. Stop the loader before replacing its whole root. Runtime model writes
for managed scenes must use `scenes.model`; direct `shell.scene.patch` model
writes are not imported into the loader's retained model. The local/mesh gate
assumes its two configured targets reach the same citizen over distinct routes.
