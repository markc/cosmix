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
`rebind refused: InvalidService`. A keymap file row with a malformed `service`
is dropped at load with a log line; the other rows still load. Rows without the
field serialize without it, so older files and callers are unaffected.

The shipped default keymap binds these right-Ctrl rows:

| Chord | Code | Action | Service | Repeat |
|---|---|---|---|---|
| RightCtrl+Left | 105 | `desktop.workspace.prev` | first segment | allow |
| RightCtrl+Right | 106 | `desktop.workspace.next` | first segment | allow |
| RightCtrl+Down | 108 | `desktop.clipboard.menu` | `desktop-vt1` | ignore |
| RightCtrl+Up | 103 | `desktop.clipboard.rotate` | `desktop-vt1` | ignore |

The default keymap only seeds a missing keymap file. A host with an existing
file keeps its rows until they are rebound with `input.bind` or the file is
edited and `input.reload` is sent.
