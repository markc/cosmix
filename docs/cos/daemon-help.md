# Daemon command discovery

Send `HELP` to a daemon's registered Bus service to list its accepted verbs,
argument names, short descriptions and `read_only` flags. The shared client
answers HELP from the daemon's verb manifest, with HELP itself listed first.
Argument names are discovery hints; structured requests still use the JSON
schema accepted by the corresponding verb. Existing authorisation applies.

Alongside dnsd, this surface is available on indexd, maild, filesd, inputd,
interactd, nspawnd and webd in the main Cargo workspace. Manifests are installed
on every connection, including reconnects. Filesd advertises corpus verbs in
corpus mode and `fs.*` verbs in filesystem mode. Nspawnd advertises executor
verbs in executor mode and `nspawnd.ct.*` plus props verbs in controller mode.

Indexd and filesd also list their accepted unqualified aliases. Broker control
frames such as `noded.admit.challenge`, which indexd ignores, are not request
verbs and are not advertised. Props reads and watches are read-only; props
set/delete and other mutations are marked writable. A read-only flag describes
the command's operation, not an authorisation grant or a promise that diagnostic
counters will remain unchanged.

## Inputd key and pointer injection

`cosmix-inputd` 0.4.0 serves these writable verbs on the `inputd` Bus service.
All accept JSON bodies and are reachable by local and mesh callers, with no
node-local gate. The existing keymap mutation gates are unchanged.

| Verb | Body | Effect |
|---|---|---|
| `input.key` | `{"key":"F9","action":"tap"}` | Key press (value 1), release (value 0), or tap (press then release); SYN_REPORT after each state change |
| `input.pointer.move` | `{"dx":12,"dy":-4}` | Relative X/Y motion, then SYN_REPORT |
| `input.pointer.button` | `{"button":"left","action":"click"}` | Left/right/middle press, release or click; SYN_REPORT after each state change |
| `input.pointer.scroll` | `{"dy":1,"dx":-1}` | Vertical wheel steps and optional horizontal wheel steps, then SYN_REPORT |

Key names are case-insensitive: F1–F12, A–Z, Enter, Escape, Space, Tab,
Backspace, Delete, Left/Right/Up/Down, Home/End/PageUp/PageDown, and
Left/Right Ctrl, Shift, Alt and Meta (without spaces, e.g. `LeftCtrl`).
An optional `KEY_` prefix is accepted. Bare decimal strings always mean raw
Linux input-event codes in 0–767 (`0x2ff`), e.g. `"67"` is F9; use `KEY_0`
through `KEY_9` to name the physical digit keys unambiguously. These are physical
codes; the compositor's keyboard layout determines the resulting character.
Unknown names and out-of-range codes return rc 10 naming the bad key.
Actions are exactly `press`, `release` or `tap`; tap emits two SYN frames.

Motion and wheel deltas are signed 32-bit integers. Positive wheel `dy` means
up and positive `dx` means right (desktop natural-scroll settings may reverse
the displayed result). Success returns rc 0 with `{"ok":true}`; malformed
requests, unavailable `/dev/uinput` or failed writes return rc 10 with an
`error` message. Success acknowledges the kernel write, not compositor delivery.

The service lazily creates and retains a separate `cosmix-inputd virtual pointer`
uinput device, independently of `--grab` or `--observe`, and keeps it across
broker reconnects. It advertises EV_SYN, the full EV_KEY range 0–767 (including
the pointer buttons), and EV_REL
for X/Y and both wheel axes. First use allows 300 ms for seat discovery before
writing. The daemon needs permission to open `/dev/uinput`; failed creation is
retried on the next valid request. Drop or process exit destroys the device.
The keyboard grab device retains its existing keyboard capabilities.

Injection enters the kernel/libinput path for the compositor owning the seat,
including cosmix-comp and Plasma. Absolute pointer warp is a follow-up requiring
an EV_ABS device or a compositor verb with output geometry.

## Inputd keymap rows and their target service

A physical keymap row, as `input.bind` accepts it (with `"layer":"physical"`),
as `input.query` returns it under `physical`, and as the keymap file stores it:

| Field | Required | Meaning |
|---|---|---|
| `stroke` | yes | `{"code":108,"modifiers":{"right_ctrl":true}}`: an evdev code plus exact side-specific modifiers |
| `action` | yes | The Bus verb sent when the row fires, e.g. `desktop.clipboard.menu` |
| `service` | no | The registered Bus service the verb is sent to |
| `args` | no | A JSON object sent as the verb's body; absent means an empty body |
| `scope` | no | `global` (default) |
| `repeat` | no | `ignore` (default) or `allow`: whether auto-repeat fires again |
| `passthrough` | no | `true` reserves the chord but re-emits it and fires nothing |

When a row has no `service`, inputd sends the verb to the service named by its
first dot-segment: `desktop.workspace.next` goes to `desktop`. When `service` is
present, the verb goes to that service unchanged. Use `service` when the
handler's registered name differs from its verbs' first segment. For example,
the desktop-session clipboard citizen registers as `desktop-vt1` and answers
`desktop.clipboard.menu`. A row whose action is written as
`desktop-vt1.desktop.clipboard.menu` never reaches that handler, because the
citizen receives the whole string as the command and has no such verb.

`service` must match the broker's registered-name grammar
`^[a-z][a-z0-9-]{1,30}$`, so a mesh-qualified target is not accepted. An
`input.bind` with a malformed `service` returns rc 10 with
`rebind refused: InvalidService`. Rows without the field serialize without it,
so older files and callers are unaffected.

The keymap file is admitted row by row. A row that does not parse, such as one
whose `service` is a number, list or object, is dropped. The exception is
`"service": null`, which loads as an untargeted row. So is a row whose `service` fails
the grammar. The other rows still load, and the file is not reseeded. Each
dropped row is logged with its code, modifiers, action, service and reason.
`input.reload` also returns them in a `dropped` list in its reply:

```json
{"ok":true,"generation":7,"dropped":[{"code":108,"modifiers":{"right_ctrl":true},
  "action":"desktop.clipboard.menu","service":7,"reason":"malformed row: ..."}]}
```

The file on disk is left as written until the next successful `input.bind`,
or an `input.unbind` that removes a live row. Either one rewrites the file from
the live rows, so the dropped rows are gone from it. An `input.unbind` of a
stroke that is not live, including one whose row was dropped, returns
`"removed":false` and does not rewrite the file.

One legacy shape is migrated at load. A row with no `service` whose action
starts with `desktop-vt1.desktop.clipboard.` is rewritten to service
`desktop-vt1` with the rest of the action, for example
`desktop.clipboard.menu`. inputd logs each migration with the row's code,
modifiers, old action and new target. No other action is rewritten, so a row
such as `foo-bar.baz.qux` keeps first-segment routing.

The clipboard rows work with no `args` because the clipboard citizen accepts an
empty body for `desktop.clipboard.menu` and `desktop.clipboard.rotate`. Since
citizen 0.3.5, `instance` is optional on those two verbs. When a caller sends
it, a stale value is still refused with rc 12. Since citizen 0.3.6, mesh
callers reach both verbs too, like every other verb of the citizen.

Rolling back to an inputd without this field is lossy. The older binary ignores
`service` and routes by first segment, so the clipboard rows go to `desktop`,
which has no such verb, and nothing is logged. Under the older binary, the next successful `input.bind`, or an
`input.unbind` that removes a live row, rewrites the file without the field.
After rolling forward again, those rows must be rebound with `service`.

The shipped default keymap binds these right-Ctrl rows:

| Chord | Code | Action | Service | Repeat |
|---|---|---|---|---|
| RightCtrl+Left | 105 | `desktop.workspace.prev` | first segment | allow |
| RightCtrl+Right | 106 | `desktop.workspace.next` | first segment | allow |
| RightCtrl+Down | 108 | `desktop.clipboard.menu` | `desktop-vt1` | ignore |
| RightCtrl+Up | 103 | `desktop.clipboard.rotate` | `desktop-vt1` | ignore |

The default keymap only seeds a missing keymap file, or one whose whole
document is unusable. A document is unusable when its bytes are not UTF-8, are
not JSON, are not a JSON object, lack a `physical` list, or lack an unsigned
32-bit `version`. Since inputd 0.4.2, startup first renames such a file to
`keymap.json.bad-YYYYmmdd-HHMMSS` in the same directory, then seeds the
defaults. The rename never replaces an existing name. If the name is taken,
even by a file created a moment earlier, inputd tries `-1`, `-2` and so on.
The file is never deleted. inputd logs one line naming the reason and the
backup path, and `input.query` in that process carries the path:

```json
{"mode":"normal","generation":0,"physical":[...],
 "recovered_from":"/var/lib/cosmix/inputd/keymap.json.bad-20260924-101112"}
```

The field is absent when no recovery happened. It stays for the life of the
process, even after a later successful `input.reload`.

inputd checks that the file it renamed is the one that failed to parse. If the
file was replaced in between, it renames the replacement back and loads that
instead. The defaults are only created at a vacant path. If a file appears
there before they are written, that file is left untouched.

On a filesystem without an exclusive rename, inputd moves the file by linking
the backup name and then unlinking the original. If the original changed in
between, it drops the new link, leaves the file alone and does not seed.

A keymap path that is a symlink to an unusable file is never recovered. The
link and its target are left untouched, and the target must be fixed by hand.

A file that cannot be read at all is never moved, because it may be valid.
This covers a permission error, an I/O error, or a directory at the path.

In these cases inputd serves the defaults in memory and does not write the
keymap file:

- the file cannot be read;
- the unusable file cannot be renamed, or changed while being moved;
- the keymap path is a symlink to an unusable file;
- a file appears at the path while the defaults are being written.

`input.query` then carries `persist_disabled`, naming the path and the reason.
`input.bind` and `input.unbind` still change the live keymap and return rc 0,
but their reply adds `"persisted":false` and the same `persist_disabled`
reason. A save that fails while writing is enabled adds `"persisted":false`
and `persist_error` instead. A successful save leaves the reply unchanged.

`input.reload` never moves or rewrites the file. On a file that is still
unusable it returns rc 10 with the reason, plus `persist_disabled` when
writing is off, and the live keymap is unchanged. Once the file is fixed,
`input.reload` loads it and turns writing back on. Its reply then carries
`persist_reenabled` with the reason that no longer applies.

A host with a usable file keeps its rows, apart from the legacy clipboard
migration above. Other
rows change only when they are rebound with `input.bind`, or when the file is
edited and `input.reload` is sent. The `desktop-vt1` target suits a host whose
clipboard citizen runs under that name. On a host whose citizen has another
name, rebind these two rows with that host's name.
