# Scenes loader

`mix --serve scenes.mix --name scenes` owns file-backed scene lifecycles.
It starts with no enabled scenes and reports `needs_setup:true` until scenes
are explicitly enabled. From Stage B it owns the bottom panel, launcher,
calendar and notes pages once they are seeded and enabled, replacing
`quoin-panel.mix` (see [the legacy handover](#stage-b-legacy-handover)).
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

Without `name`, `scenes.install` uses the scene name the template authors:
the directory name for every template except `settings`, which authors
`quoin-settings`. An authored `window.panel` is kept when installing under
the authored name and rewritten to `scene-<name>` on a rename. So
`{template:"settings"}` installs `quoin-settings` on Quoin's declared page
`settings.appearance`. While Quoin's built-in fallback holds that page, the
loader's load takes it over (see [Quoin](quoin)). Disabling or removing the
scene gives the page back to the fallback.

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
| `SCENES_LEGACY_SERVICE` | Legacy panel citizen whose registration holds the four legacy pages; default `quoin-panel` |

The native Mix binary is resolved from the running interpreter. Behaviour
children inherit the session and existing `COMP_SERVICE`, `APPS_SERVICE`,
`TRAY_SERVICE`, `NOTIFY_SERVICE` and other overrides. They additionally receive
`SCENE_NAME`, `SCENE_HOST`, `SCENES_SERVICE`, `SCENE_GENERATION`, and `SCENE_DIR`;
their working directory is their scene directory. Each runs as `scene-<name>`.
`SCENE_DIR` is the directory the behaviour runs from. For the Scene Editor's
reserved entry that is either the shipped template or the user copy, so
helpers kept beside a template load with `require($SCENE_DIR .. "/lib.mix")`.
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
| `scenes.model` | `{name,generation,value}` → acceptance/revision; `value` is a complete map, submitted by the loader as `shell.scene.patch {scene,path:"model",generation,value}` |
| `scenes.open` / `scenes.close` / `scenes.toggle` | `{name}` → `{name,open,pending}`; desired popup state, with applied completion reported through `scenes.changed` |

Installation rewrites the scene name, behaviour routing citizen and an
explicitly authored panel name to the installation name, so
`{template:"launcher",name:"preview-launcher"}` mounts a second, distinct
page.

### Stage B legacy handover

`scene-panel`, `scene-launcher`, `scene-calendar` and `scene-notes` were
mounted by `quoin-panel.mix`. Quoin's seats are owner-exclusive: whichever
citizen holds a page keeps it, and the other one's load of that page is
refused (`SUBPANEL_COLLISION`) until the holder unloads or disconnects.
Neither owner can take a page from the other. While the `SCENES_LEGACY_SERVICE` registration (default
`quoin-panel`) is present, a scene on one of those pages installs and enables
normally but is not mounted: its diagnostic is `SCENES_LEGACY_HELD` with the
holder name, and `enable`/`reload` reply rc 10 with it. That registration
disappearing from the broker snapshot triggers one rescan, which mounts the
held scenes and starts their behaviours. The handover is state, not that one
edge: while the legacy citizen stays absent, an enabled, installed scene on a
legacy page that is still unmounted (a load that timed out, or a
`SUBPANEL_COLLISION` because Quoin had not yet applied the old citizen's
disconnect) is retried on every later broker snapshot and `shell.panel.changed`
notice. For a failure no later event would retry, the loader starts one local
`task_start` backoff (1, 2 … 32 s, then it stops and leaves the diagnostic until
nothing is owed); it never self-emits over the Bus.

The guard only stops the loader racing a live legacy citizen. It does not
stop the reverse. If `quoin-panel` starts again while the loader holds the
pages, the loader keeps them mounted (it never unloads for the legacy
citizen), and the legacy citizen's own loads are refused by the seat rule, so
it runs with no pages. While it stays registered the loader also will not
reload or remount those pages, so their accepted content stays as it was.
**Operator rule: do not run both.** Stop and disable the legacy unit before
enabling the templates; starting it again is not a rollback until the
loader's four scenes are disabled.

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
The retiring record survives removal and reinstallation of the same name.
Before spawning, the loader also reads the broker registry and waits for any
old `scene-<name>` registration to disappear. Service snapshots resume that
handover. A held `scene-<name>` registration reports `SCENES_NAME_HELD` with
the holder name in `scenes.list` and `scenes.changed`; it waits for the
name-release event without a timeout loop.
Each name in a filesystem batch or rescan has its own error boundary,
so one refused unload does not discard the rest of the batch.

Loader `RELOAD` is refused during candidate initialisation, before filesystem
or process side effects; the old evaluator continues serving. Restart the
loader process to update its script. Native Mix currently has no post-commit
hook for starting replacement children after the old registry retires.
`scenes.reload {name}` still replaces an individual behaviour normally.

Managed spawn uses `{die_with_parent:true,exit_event:true,tag:<JSON string>}`.
The string encodes `{name,generation}`; `on proc.exited` receives
`{pid,tag,exit_code,signal}` in `$event.args`. A crash leaves the scene mounted.
Four failure-triggered restart deadlines are 1, 2, 4 and 8 seconds; the fifth
consecutive exit enters `crash_loop`. Running at least 60 seconds resets the
count at the next exit, without a health timer. Explicit reload retries the
failed scene. Disable during a deadline invalidates its generation. Deadlines
use a separate in-process `task_start` task, never a Bus self-emit. The native
exit handler performs synchronous bookkeeping only. The task's async phase
waits without carrying writable state; its synchronous phase re-reads current
state under the writer lock and performs the fenced restart. Concurrent exits
retain both successors, and disable during the wait stays disabled.
An elapsed deadline stays eligible when Quoin is absent and resumes on remount;
reconnect reconciliation also checks the computed deadline directly.

The process adapter uses W1's registry-backed `kill(pid,9)` to stop the owned
child through its retained pidfd. Generation checks precede stop and exit
bookkeeping precedes the async deadline: the numeric PID is discarded as soon
as its exit event is handled. Replacement waits for that event; the native reaper owns
descendant cleanup. SIGKILL is deliberate so a stuck behaviour needs no stop
poll or escalation timer. The installed pre-W1 interpreter cannot run this
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
advance from command replies followed by `shell.panel.state` reads. Already
satisfied phases complete immediately. `shell.panel.changed` is a hint to read
state, and every subscribed topic's gap causes a full resynchronisation.
Recovery requires both the saved page and mode to be restored before its
record is removed, including when both pages were already pinned.
Hidden mode can remain visible because the pointer or a holder reveals it;
matching page and mode completes restoration without cancelling that reveal.
Quoin replies `applied:true` with the actual `visible:true` snapshot in this
case. Refused popup commands settle with `pending:false` and a diagnostic in
`scenes.list`; failed recovery records wait for an explicit open/close retry,
not another hint. The legacy panel logs refusals and clears the pending attempt,
retaining its pin recovery record for a later explicit retry or resynchronisation.
Recovery is persisted before pinning. If another page takes the edge, recovery
does not hide it. The legacy panel remains in use in Stage A but consumes the
same applied-state stream; its network/audio conversion belongs to Stage C.
Its minute clock and the calendar template use local async deadline tasks;
`bus.connected` starts a clock only when it is not running. Gaps received
during a resynchronisation coalesce into one further pass.

Render failures retain readable diagnostics and the last applied revision.
A subsequent accepted revision or remount recreates missing renderer entities.
The initial connection notice reuses the startup snapshot without remounting.

Deferred: `.recovery` retention remains manual until the editor offers a
reviewable prune policy; automatic deletion could destroy the only saved edit.
Deferred: the parent filesystem watch is retained for root replacement and
shares the evaluator's inotify fd and worker.
Deferred: per-expression `Instant::now()` budget checks remain until measured
optimisation preserves the deadline guarantee for always-ready expressions.

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
render the example unit in the desktop session; and supply the matching native
Mix binary and Quoin. For Stage B it explicitly seeds the chosen set (never a
fresh install's default), retains popup recovery and rollback material, and
stops the old citizen; the loader's handover rule above then mounts the
existing page IDs. Readiness checks must consume service/readiness/applied-state
events. No private installer is changed by this public workstream.

Limits and remaining operational assumptions: at most 128 installed scene
records, 256 KiB per scene/behaviour file and runtime model, and 64 KiB state or
metadata. Recovery copies are retained for the operator to manage. Recovery
mutations reject symlinks (including dangling links), non-directory recovery
paths and destinations outside the canonical scene root. Source and parent
checks use no-follow stat; rename does not follow its final components. Trusted
desktop-user edits are still assumed: Mix has no directory-handle-relative
rename, so these prechecks cannot exclude a simultaneous hostile parent swap.
Reset uses two
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
for managed scenes must use `scenes.model`. The loader mounts with a JSON
`shell.scene.load {source,model_generation}` envelope and updates the fence
before each spawn; zero disables model writes. The host requires both the
verified loading citizen and matching positive `generation` for `model` and
`model.*` patches. Direct behaviour/editor writes are refused without changing
the model or revision, and a raw load cannot strip a managed fence. Local and
mesh editors keep access through the loader's generation-aware verbs, so its
retained model survives remounts. Unmanaged scenes keep direct model patches.
The local/mesh gate
assumes its two configured targets reach the same citizen over distinct routes.
