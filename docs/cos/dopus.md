# dopus — the CosMix twin-pane file manager

**dopus is an iced file manager over the headless `cosmix-dopus-core`.**
It replaces the deprecated Bevy filemgr; retirement of that app is separate.
The window is a Wayland client (`dev.cosmix.dopus`) drawn with tiny-skia and
the shared design tokens. Only dopus consumes the extracted core.

Each pane has its own directory, history, sort, hidden-file setting, lazy
directory tree and single selection. F6 switches the active pane. Places is
a plain sidebar, with Home, Filesystem and existing user directories.
Click its heading to refresh Places; pane relists also invalidate its cached
directory checks. The divider persists its position; double-click centres it.

File operations are local keyboard/dialog actions: new folder, rename,
copy or move to the other pane, and permanent delete with confirmation.
Operations run one at a time, never overwrite a destination, and relist both
panes exactly once after every operation reply, including failures.
Opening a directory navigates; opening a file spawns `xdg-open`.

## Running it

From Mix:

```mix
run_argv(["cosmix-dopus"])
run_argv(["cosmix-dopus", "/tmp/source", "/tmp/destination"])
run_argv(["cosmix-dopus", "--headless", "--service", "dopus-test"])
run_argv(["cosmix-dopus", "--print-config"])
run_argv(["cosmix-dopus", "--version"])
```

The first path goes left, the second right; extras are ignored. A file path
navigates to its parent. A second windowed launch forwards its paths to the
running service and exits; argv paths are made absolute in the calling
process. Use `--` before paths that resemble flags.

`--service NAME` changes the default Bus name `dopus`.
`--noded-url ws://127.0.0.1:4200/ws` overrides broker discovery.
Windowed dopus can run without a broker. `--headless` requires a broker,
serves the same core and verbs, cancels any dialog requests, refuses file
opening and has no theme painter. A duplicate headless service name fails.
`--version` / `-V` report build provenance before other side effects;
`--version --json` gives the structured form.

## The seven-law app contract

The frontend must honour the contract in `cosmix-dopus-core/src/lib.rs`:

1. Call `tick(now)` every frame with a monotonic clock. This advances count
   dispatch and config settling. Format relative modification times on the
   frontend's own clock.
2. Drain worker replies and feed every event through `on_event` exactly
   once, on one thread.
3. Answer every confirmation and prompt token, including cancellation.
   Recover lost dialogs using `outstanding_reservations` and `withdraw`.
4. Spawn the handler for `OpenFile` and show spawn failures in the status line.
5. Pass `ascending: true` on a sort-column change. Repeating the same column
   toggles direction.
6. Pre-validate prompt fields with `validate_filename` for immediate
   feedback; the core validates again before accepting a name.
7. Drive `set_split_ratio` from divider drags, because persistence derives
   from core state.

The core also rejects stale listing/count replies by pane generation and
path, limits count concurrency, and owns single-flight operations and
token-correlated reservations. Headless mode applies the same contract,
with timed ticks, fail-closed dialog answers and refused file launches.

## The Bus port: `dopus.v1`

The app serves local and mesh callers through noded. Success is `rc 0`;
refusal is `rc 10` with `{error_code, message, reason}` (`reason` may be null).
**Every `file.*` action is permanently Bus-forbidden**, including
`file.open`. The action list describes keyboard availability, not Bus
permission. No verb performs file operations or opens a confirmation dialog.

| Verb | Args | Reply | Refusals |
|---|---|---|---|
| `dopus.ping` | — | `{pong, service, schema:"dopus.v1", pid, headless}` | — |
| `dopus.describe` | — | `{contract:"ctk-app-control.v0", app:"dopus", title, view, engine:"iced", version, description, controls, verbs}` | — |
| `dopus.info` | — | `{version, git_sha, build_time, headless, panes:2, pane_states:[PaneState, PaneState], config_path}` | — |
| `dopus.state` | — | `{panes:[PaneState, PaneState], theme_scheme, theme_mode}` | — |
| `dopus.action` | `id`, optional `pane`, optional `args` (currently unused) | `{id, ok:true, result:null}`; quit instead returns `{quitting:true}` | `INVALID_ARGUMENT`, `FORBIDDEN`, `UNAVAILABLE` |
| `dopus.actions.list` | — | `{actions:[{id, label, keys, enabled}]}` | — |
| `dopus.theme.set` | optional `scheme`, `mode`; null leaves unchanged | `{scheme, mode}` | `INVALID_ARGUMENT`; `UNAVAILABLE` headless |
| `dopus.open` | `paths:[…]`, optional `pane` | `{accepted, opened}` | `INVALID_ARGUMENT` |
| `dopus.quit` | — | `{quitting:true}`, then exit after replying | — |

Unknown verbs return `UNKNOWN_VERB`. `NOT_FOUND`, `CONFLICT` and
`INTERNAL` are reserved refusal codes; filesystem listing errors currently
appear in pane status after an asynchronous navigation reply.

`PaneState` is identical in `info.pane_states` and `state.panes`:
`{pane, path, active, show_hidden, sort, ascending, selected, rows, status}`.
Rows are ordered left then right; `pane` is 0 or 1, `selected` is a path
or null, `sort` is `name`, `size` or `modified`, and paths are sanitised
for display. `info.panes` remains a count for existing callers.
Theme names are empty strings headless.

**Pane targeting.** `dopus.action` accepts `pane:"left"|"right"|"active"`,
defaulting to active. Navigation (`nav.back/forward/parent/home`), refresh,
hidden toggle, sort and selection actions affect only that pane, without
changing the active pane. Global actions (`nav.switch-pane`, theme and quit)
remain global. Unknown pane values are invalid.

`dopus.open` preserves positional routing when `pane` is omitted:
first path left, second right, extras ignored. With an explicit pane,
exactly one path is required and focus stays where it was.
`accepted` counts supplied paths, including ignored extras; `opened`
means at least one navigation was requested, not that its listing succeeded.
Leading `~` is expanded. Relative Bus paths resolve against dopus's
working directory, so callers should send absolute paths.

```mix
send dopus dopus.open paths=["/tmp"] pane="right"
send dopus dopus.action id="nav.parent" pane="right"
send dopus dopus.action id="view.sort-size" pane="right"
send dopus dopus.state
print($reply)
```

The action reply acknowledges the state change or listing request; listing
workers finish later. There is no wait or tree-expansion verb in v1.
Theme actions work windowed and return `UNAVAILABLE` headless.
`location.focus` is a local UI action and returns `FORBIDDEN` over the Bus.
`app.quit` is an action id; the direct quit verb is `dopus.quit`.

## Keymap and location editing

| Binding | Action |
|---|---|
| Ctrl+L | `location.focus`: select the active location bar's text |
| F6 | `nav.switch-pane` |
| Alt+Left / Alt+Right | `nav.back` / `nav.forward` |
| Backspace / Alt+Home | `nav.parent` / `nav.home` |
| F5 / Ctrl+H | `view.refresh` / `view.toggle-hidden` |
| Ctrl+1 / Ctrl+2 / Ctrl+3 | `view.sort-name` / `view.sort-size` / `view.sort-modified` |
| Down / Up / Home / End | `selection.next` / `previous` / `first` / `last` |
| Enter | `file.open` |
| Ctrl+Shift+N / F2 | `file.new-folder` / `file.rename` |
| Ctrl+C / Ctrl+X | `file.copy-other-pane` / `file.move-other-pane` |
| Delete | `file.delete` (permanent deletion after confirmation) |
| Ctrl+Q | `app.quit` |
| Ctrl+Alt+D | `theme.mode-toggle` |
| Ctrl+Alt+1 through 6 | `theme.scheme-ocean/crimson/stone/forest/sunset/mono` |

Clicking a location bar also edits it. Enter submits the real path text,
Escape cancels, and clicking a listing or another control dismisses the
edit. While editing, default browse/file shortcuts are suppressed by
`FocusContext`; clipboard and undo keys belong to the field.
Dialogs own Enter/Escape and suppress browse actions.

Defaults live in `cosmix-actions`' `DOPUS_DEFAULT_KEYMAP_MIX`.
The per-app `config/keymap.conf.mix` supplies `custom` overrides and
`chord_timeout_ms` (default 1000). It reloads on window focus; invalid
reloads retain the current map. P2 had click-to-edit only; Ctrl+L is new.

## Configuration: schema 1

The app root is the first absolute path available from `$COSMIX_APP_HOME`,
`$COSMIX_APPS_HOME/dopus`, `$XDG_STATE_HOME/cosmix/apps/dopus`, or
`$HOME/.local/state/cosmix/apps/dopus`.

| File under the app root | Purpose |
|---|---|
| `config/config.conf.mix` | Pane and split state, schema 1 |
| `config/keymap.conf.mix` | Optional keymap overlay |
| `config/theme.conf.mix` | Optional theme override over shared design settings |

```mix
{
  schema_version: 1,
  left: {path: "/tmp", show_hidden: false, sort: "name", ascending: true},
  right: {path: "/tmp", show_hidden: false, sort: "name", ascending: true},
  active_pane: "left",
  split_ratio: 0.5
}
```

Defaults: left at Home, right at Downloads if it exists (otherwise Home),
hidden files off, name ascending, left active, equal split. The window clamps
the split to 0.1–0.9. Invalid startup directories fall back to Home.
Config snapshots come from core state and settle for 0.35 seconds before an
atomic write. A malformed, unreadable or unsupported-schema config loads
defaults and disables saving, preserving the original file.
`--print-config` prints the resolved config as JSON.
Theme changes are session selections, not persisted config fields.

## Not in v1

- Drag and drop, internal or OS.
- Multi-selection.
- Previews.
- Listing virtualisation (drawing is limited to visible rows).
- Filemgr config import.

DCS sidebars are excluded by design, including future versions.

## Verification

`src/desktop/apps/dopus/tests/dopus-bus-test.mix` starts an isolated noded
and headless app. It covers pane targeting, navigation, sort, hidden files,
selection, availability, refusals, single-instance registration and
reply-before-exit. Core tests cover operations and their relist law.
File-operation dialogs and keyboard focus need a windowed gate: the Bus
test cannot drive operations through the deliberately forbidden `file.*` ids.
