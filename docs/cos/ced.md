# ced — the CosMix Editor

**`ced` is the desktop text editor: an iced app whose buffers live in the
[`edit`](edit.md) Bus service.** You type into a local copy that echoes at
once; the edit service holds the authoritative text, so agents edit the same
buffer at the same time, their changes appear live and marked with who made
them, and every origin (you, each agent) has its own undo. Unsaved text
survives a crash of ced *and* of the edit service. Every menu action is also
a Bus verb.

It is a Wayland client (`app_id` `dev.cosmix.ced`) in its own process, drawn
with iced's tiny-skia renderer, themed from the desktop's design tokens.

## Running it

```sh
ced                           # open the window (reattaches last session's tabs)
ced notes.md src/main.rs:120  # open files; PATH:LINE[:COL] jumps there
ced --headless                # no window: the controller and the Bus port only
ced --service NAME            # register as NAME instead of `ced` (tests, gates)
ced --print-config            # the resolved configuration, then exit
ced --version                 # cosmix-ced <semver> (<sha12>, built …)
```

**One instance.** `ced PATH…` first asks a running ced (`ced.ping`, 500 ms);
if one answers, the paths go to it as `ced.open` and the command exits.
Relative paths are resolved against the *calling* shell's directory. The
launcher entry is `dev.cosmix.ced.desktop` (Name *CosMix Editor*); it does not
make ced the default for text files.

**Quitting detaches.** Closing the window (or `Ctrl+Q`, `app.quit`) never
asks to save: the buffers stay in the edit service, where recovery files
protect them, and `session.json` remembers the tabs, carets and scroll
positions for the next start. You are asked only when you **close a tab**
that is dirty and that no one else holds.

## Keys

Notepad++ conventions. Every chord runs an action listed by `ced.actions`.

| Area | Chords |
|---|---|
| File | New `Ctrl+N` · Open `Ctrl+O` · Save `Ctrl+S` · Save As `Ctrl+Alt+S` · Save All `Ctrl+Shift+S` · Close tab `Ctrl+W` · Exit `Ctrl+Q` · Reload from Disk (menu) |
| Edit | Undo `Ctrl+Z` · Redo `Ctrl+Y`, `Ctrl+Shift+Z` · Undo another origin's last edit `Ctrl+Alt+Z` · Cut/Copy/Paste `Ctrl+X/C/V`, `Shift+Del`/`Ctrl+Ins`/`Shift+Ins` · Select All `Ctrl+A` · Duplicate line `Ctrl+D` · Delete line `Ctrl+Shift+L` · Move line `Ctrl+Shift+Up/Down` · Toggle comment `Ctrl+/` · Indent `Tab` / Outdent `Shift+Tab` · Delete word `Ctrl+Backspace` / `Ctrl+Del` · Overwrite `Insert` |
| Search | Find `Ctrl+F` · Next `F3` · Previous `Shift+F3` · Replace `Ctrl+H` · Go to Line `Ctrl+G` |
| View | Zoom `Ctrl+=` / `Ctrl+-` / `Ctrl+0` (and `Ctrl`+wheel) · Problems `Ctrl+Shift+M` · Show Whitespace, Line Numbers, Other Origins' Carets, Output, Clear Change Markers, Reload Settings (menu) |
| Tabs | `Ctrl+Tab` / `Ctrl+Shift+Tab` · `Ctrl+PageDown` / `Ctrl+PageUp` · `Ctrl+1`…`Ctrl+9` |
| Menus | `F10`, `Alt+F/E/S/V/M/T/H` |

Cursor movement is grapheme-correct: an emoji with modifiers, a flag, a
letter with combining marks and a CRLF are each one step. Up/Down keep the
visual column across wide (2-cell) characters and tabs. With a multi-line
selection, `Tab` and `Shift+Tab` indent and outdent every line as **one**
edit and one undo step. `Enter` auto-indents and uses the file's line ending.
Toggle Comment uses `--` for Mix, `//` for Rust, C, C++, Go and JavaScript,
and `#` for shell, Python, TOML and YAML.

## Who edited what: origins and lanes

The edit service records an **origin** on every edit, and each origin has its
own undo lane. ced uses two kinds:

- **`human:ced` is the UI-input lane.** Everything that arrives through ced's
  window (keyboard, input method, mouse, drops) claims it. That includes
  input a compositor injects (`comp.input.*`), which ced cannot tell from a
  physical keyboard: the label says "came in through the window", not "a
  person typed this".
- **`agent:ced.<caller>` for Bus callers.** An edit made through ced's own
  verbs (`ced.type`, `ced.action`) claims a lane named after the Bus caller,
  derived from the envelope the way editd does it: `local:<service>` for a
  registered local caller, `mesh:<service>@<peer>` for a mesh caller, `anon`
  for an anonymous one-shot. So an anonymous `send ced ced.type …` edits as
  `agent:ced.anon`. Labels longer than editd's 64 characters are cut to 43
  characters plus `+` and 16 hex digits of a blake3 hash, so two callers never
  share a lane.

Agents that talk to the edit service directly (`send edit edit.insert …`)
keep their own origin, such as `agent:ctl-90`.

**Undo.** `Ctrl+Z` undoes your lane only; an agent's edit is never undone by
it, even when it is newer. `Ctrl+Alt+Z` undoes the most recent edit by
another origin, and the menu item names that origin. *Undo Anyone's Last*
undoes whichever lane holds the newest change. `ced.action id="edit.undo"`
from the Bus undoes the **caller's** lane. Undo waits until your typing has
reached the service, so `Ctrl+Z` straight after typing undoes that typing.

**Seeing agents' edits.**

- Text an agent inserts is tinted for 2 s.
- A 4 px strip in the gutter marks lines another origin changed since you
  last focused the tab; hovering it shows `agent:ctl-90 · rev 44 · 12:03:11`.
  Markers clear 2 s after you focus the tab, or with View > Clear Change
  Markers.
- A tab with such changes shows `◆`. Other tab badges: `●` unsaved, `!` the
  file changed or vanished on disk, `⟳` reattaching.
- Other origins' carets and selections are drawn (View > Other Origins'
  Carets turns them off).
- The status bar names the last remote edit; click it to jump there.
- Colours come from the design tokens: `accent` for `agent:*`, `primary` for
  other `human:*` origins.

## Conflicts

The edit service is the single authority, and ced's optimistic local echo is
rebased onto whatever the service applied first. When an agent's edit and
your not-yet-acknowledged typing touch **overlapping** text, the agent's edit
wins and yours is taken back. Nothing is lost silently: the infobar says so,
for example *"agent:ctl-90 edited lines 12–18 while you were typing there;
your 7 characters were not applied"*, with **Show** (who, which lines, the
exact text), **Copy** and **Re-insert at Caret**. Edits that do not overlap are
never taken back, and remote edits are never reverted. `ced.stats` counts
conflicts.

## Recovery and reattach

Unsaved text is protected by the edit service's [recovery files](edit.md#recovery),
not by ced: ced writes none. What ced adds is its own full copy of each text.

- **The service restarts** (a new epoch; ced notices from noded's
  `services.registered`, from any event, or from a refused request). Every tab
  reopens its buffer: a path tab by path, a scratch tab by its `recovery_id`.
  ced compares its copy with the restored text by blake3:
  - **equal** — the tab goes live with no prompt;
  - **different** — the tab goes live on the service's text and keeps yours
    as a *detached copy*. The infobar offers **Keep mine** (replace the
    difference in one undoable edit, or in 1 MiB steps for larger
    differences, which other clients see as they land), **Take the
    service's**, and **Save mine as…**.
- **ced starts** with tabs from `session.json`, reattached the same way.
  Buffers the service restored that no one holds are offered once in a
  *Recovered buffers* dialog: Open, or Discard.
- **The status bar shows a red `UNPROTECTED`** whenever the service reports
  `volatile: true` (recovery off or degraded). While it shows, a crash of the
  service can lose unsaved text.

## Search, lint and macros

- **Find / Replace** run in the service: Find Next/Previous are `edit.find`
  from the caret with one wrap; matches in view are highlighted. Replace All
  finds every match and applies them as **one** transaction, so it is one
  undo step, and refuses up front beyond 10,000 matches or 1 MiB of inserted
  text. `$1`-style group references are expanded by ced.
- **Lint.** Mix, scene and mix-data buffers with a path are linted with
  `mix lint --json -` on open and after each save (and 1 s after the last edit
  for buffers under 1 MiB), in the file's directory so `require()` resolves.
  Problems appear as squiggles, in the gutter and in the Problems panel
  (`Ctrl+Shift+M`). A diagnostic on a line edited since is dropped rather than
  shown in the wrong place.
- **Macros** are Mix scripts in `<config>/macros/*.mix` with a
  `-- ced-macro: <label>` header (and optional `-- ced-key: <chord>`); they
  appear in the Macros menu. ced waits for its edits to reach the service,
  then runs `/opt/cosmix/bin/mix <file>` with `CED_BUFFER`, `CED_EPOCH`,
  `CED_REV`, `CED_PATH`, `CED_LANGUAGE`, `CED_SEL_START`, `CED_SEL_END` and
  `CED_ORIGIN=agent:macro.<stem>` in the environment. A macro edits through
  `edit.*` under that origin, so its changes get their own undo lane. Output
  goes to the Output panel. There is no fallback interpreter.

## Highlighting

Syntax colours come from a vendored copy of msedit's **lsh** highlighter for
the 26 languages it defines, and from the **Mix lexer itself** for `mix`,
`scene` and `mix-data`, re-lexed 150 ms after the last change. Mix buffers
over 2 MiB stay plain. Far jumps in very large files highlight progressively
(at most 2 ms of work per frame); lines not reached yet draw plain.

## The `ced` Bus port (`ced.v1`)

Every verb is reachable by local and mesh callers with no authorization gate.
Success is `rc 0`; a refusal is `rc 10` with `{error_code, message, reason?}`
(`INVALID_ARGUMENT NOT_FOUND CONFLICT UNAVAILABLE INTERNAL UNKNOWN_VERB`).
A tab is named by `tab` (id) or `buffer`; without either, the active tab.
Positions (`POINT`, `POS`) are the edit service's forms.

| Verb | Args | Reply |
|---|---|---|
| `ced.ping` | — | `{pong, service:"ced", schema:"ced.v1", pid, headless}` |
| `ced.info` | — | `{version, git_sha, build_time, headless, tabs, edit:{epoch, version, volatile}, config_path, session_path}` |
| `ced.open` | `paths:[…]`, `line?`, `col?` | `{tabs:[{tab, buffer, path}]}` (`path:line:col` accepted) |
| `ced.new` | — | `{tab, buffer}` |
| `ced.tabs` | — | `{active, tabs:[{tab, buffer, epoch, path, name, language, rev, dirty, disk, pending, conflicts, recovered, phase}]}` |
| `ced.focus` | `tab` \| `buffer` | `{tab}` |
| `ced.state` | `tab?`, `text?` | `{tab, buffer, rev, view_gen, phase, pending, inflight, text_hash, bytes, lines, selection:{anchor, head}, first_line, last_remote, conflicts, detached_copy, text?}` — `text_hash` is blake3 of ced's view; `text` is inlined up to 4 MiB |
| `ced.type` | `text`, `tab?` | `{tab, pending}` — typed at the window's selection |
| `ced.select` | `anchor`, `head`, `tab?` | `{selection}` — sets the window's selection |
| `ced.action` | `id`, `args?`, `tab?` | `{id, ok, result?}` — any action id from `ced.actions` |
| `ced.actions` | — | `{actions:[{id, label, menu, keys, enabled}]}` |
| `ced.wait` | `tab?`, exactly one of `rev` / `idle:true` / `epoch` / `phase`, `timeout_ms` (1–30000) | `{tab, rev, phase, epoch, waited_ms}` |
| `ced.layout` | `tab?` | the last frame's geometry in logical px: `{window, menubar, tabstrip, editor, gutter_w, line_height, cell_w, first_line, visible_rows, caret, statusbar}` |
| `ced.stats` | — | `{keys, frames, model_us, view_us, next_frame_us, events, history_recoveries, snapshot_recoveries, conflicts, retries, uncertain}` (latencies as `{p50, p95, p99, max}` µs) |
| `app.describe` | — | the `ctk-app-control.v0` shape: `{contract, app:"ced", title, view, engine:"iced", version, description, controls, verbs}` |
| `app.quit` | — | `{quitting:true}`, then ced detaches and exits |

**`ced.type` and `ced.select` drive the window's selection**, ARexx-style: a
Bus-driven insert lands at your caret and moves it. That is by design; use
`edit.*` directly to edit elsewhere without touching the view.

**`ced.wait`** is event-driven. The condition is checked when the request
arrives (an immediate reply when it already holds) and again after every
change in ced; it never polls. `rev:N` holds once the tab has folded rev N and
is live; `idle:true` once every local edit is acknowledged and nothing is in
flight; `epoch` once the tab is attached to that edit-service session; `phase`
is `bootstrapping`, `live`, `recovering` or `detached`. Past `timeout_ms` it
refuses `CONFLICT` `timeout`; if the tab closes or ced loses the Bus, `CONFLICT`
`cancelled`.

**Window-only actions** (dialogs, the find bar, zoom, panels, help, and
actions that need arguments when called without them) are performed by the
window, and the Bus reply waits until the window has actually done them. If
the window has not answered within 10 s the caller gets `TIMEOUT`
`timeout`. A `--headless` ced has no window, so it refuses them with
`UNAVAILABLE`, as it does `ced.layout` (`reason:"headless"`). Arguments that
make an action non-interactive run it anywhere, for example
`ced.action id="file.save_as" args={path: "/tmp/x.md"}`,
`id="search.goto_line" args={line: 40}`, `id="search.replace_all" args={pattern: "foo", replacement: "bar"}`,
`id="file.close" args={force: true}`.

```mix
send ced ced.open paths=["~/notes/todo.md"]
$t = $result.tabs[0].tab
send ced ced.wait tab=$t phase="live" timeout_ms=5000
send ced ced.type tab=$t text="-- reviewed\n"
send ced ced.wait tab=$t idle=true timeout_ms=5000
send ced ced.action tab=$t id="edit.undo"          -- undoes only this caller's lane
send ced ced.state tab=$t
print($result.text_hash)
```

The headless end-to-end test drives exactly this surface against a private
broker and a real edit service, including two kills of the service with the
tab reattaching each time:

```sh
COSMIX=$PWD mix src/desktop/apps/ced/tests/ced-bus-test.mix \
  --editd src/target/release/cosmix-editd --ced src/desktop/target/release/ced
```

## Files, configuration and theme

ced keeps its files under one app directory, resolved in this order:
`$COSMIX_APP_HOME`, `$COSMIX_APPS_HOME/ced`, `$XDG_STATE_HOME/cosmix/apps/ced`,
`~/.local/state/cosmix/apps/ced`.

| Path | Contents |
|---|---|
| `config/ced.conf.mix` | settings (below); read at start and on View > Reload Settings |
| `config/theme.conf.mix` | optional per-app theme override |
| `config/macros/*.mix` | macros |
| `state/session.json` | open tabs, carets, scroll, recent files; written atomically 1 s after a change and on exit |

`ced.conf.mix` keys: `tab_size` (default 4), `insert_spaces` (a map from
language to bool; unlisted languages insert spaces except `text` and
makefiles; Mix-family languages indent by 2), `font_px`, `show_whitespace`,
`line_numbers`, `remote_carets`, `lint_on_save`, `ambiguous_wide` (East Asian
ambiguous-width characters take 2 cells). A bad value is reported and
ignored, never fatal. `ced --print-config` prints the result.

**Theme.** ced compiles the shared `theme.conf.mix` in the cosmix config
directory, overlaid by the per-app override, with `cosmix-design` in your
light/dark mode, and follows `theme.changed` live. The text uses the `Mono`
typography role and the chrome the `Ui` role.

## Limits (E1)

- No soft wrap: long lines scroll horizontally.
- No bidi reordering: right-to-left text shows in logical order.
- One selection per view (other origins' selections are shown).
- One window; no split views or tab dragging.
- File dialogs are ced's own (a path field with Tab completion, a directory
  list, recent files, hidden files toggle); there is no portal picker.
- End-to-end key-to-photon latency is not measured yet; `ced.stats` reports
  ced's own stages (`model_us`, `view_us`, `next_frame_us`).
