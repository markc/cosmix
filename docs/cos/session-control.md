# Session control: restart the session, leave the seat

`src/desktop/scripts/session-control.mix` is the session-control citizen. It
registers the Bus service `desktop-session` and answers two verbs:

| verb | what it does |
|---|---|
| `desktop.session.restart` | restarts the desktop unit (`cosmix-desktop.service`), then types a resume line into a terminal pane for each agent (Claude Code) session that was running |
| `desktop.session.leave` | switches to another VT (`chvt 1` by default); the desktop keeps running on its own VT, so you can switch back to it |

Both verbs are open to mesh callers and need no confirmation: agents drive
them directly. The human confirm step belongs to Quoin: its "Restart session…"
and "Leave seat…" corner-menu entries (`menu_items` with `confirm`) and the
`shell.session.confirm {action}` verb that global chords bind to both open a
Confirm/Cancel step first. See [Quoin](quoin.md).

## Requests and replies

The body is empty, or a JSON object with optional fields:

- `args`: a list of strings. This is the shape a Quoin corner-menu extra sends
  (`{"args":[]}`). The only string accepted is `--dry-run`.
- `dry_run`: `true` or `false`.

A dry run replies with the plan (the argv it would run, the captured
sessions, the state file) and runs nothing. `COSMIX_SESSION_DRY_RUN=1` in
the citizen's environment makes every request a dry run.

```mix
send "desktop-session" desktop.session.restart body='{"dry_run":true}'
send "desktop-session" desktop.session.leave
```

- **restart** replies at once:
  `{accepted:true, action:"restart", dry_run, unit, state_file, sessions}`.
  `sessions` lists the captured session ids; a dry run adds `argv`. The
  restart itself happens after the reply (see below).
- **leave** replies `{accepted:true, action:"leave", dry_run, vt}` after
  `chvt` returns; a dry run adds `argv`.
- **Refusals** are rc 10 `{error_code, message}`:
  - `INVALID_ARGUMENT`: a malformed body, an unknown field, or an unknown arg.
  - `SESSION_CONFIG`: a missing `COSMIX_SESSION_USER`, an unknown user or
    home, or a bad `COSMIX_LEAVE_VT`.
  - `RESTART_IN_PROGRESS`: the restart unit is already running.
  - `STATE_WRITE`: the state file could not be written.
  - `RESTART_START`: `systemd-run` failed.
  - `CHVT_FAILED`: `chvt` failed.

## How a restart survives itself

A restart closes every desktop window, including the terminal the request
came from. So nothing that the desktop owns can carry the restart out.

1. The **citizen** runs as root from its own system unit. The unit is not
   `PartOf=` or `BindsTo=` the desktop, so the restart does not stop it. The
   citizen reads the desktop user's live interactive sessions from
   `~/.claude/sessions/<pid>.json` (live means `/proc/<pid>` exists). It
   writes them to a state file owned by that user,
   `~/.cache/cosmix/session-resume/sessions-<time>.json`. It then starts the
   worker with `systemd-run --unit=cosmix-session-restart --collect` and
   replies.
2. The **worker** (`session-resume.mix --phase2`) runs in that transient
   unit, outside both the desktop's cgroup and the citizen's, so neither
   going down can stop it. It:
   - restarts the desktop unit;
   - waits until the unit is `active` AND its boot terminal
     (`cosmix-boot-term.service`) is a NEW process. An old terminal still on
     the Bus would take the keys into whatever runs in its active tab;
   - runs `--resume-fresh` as the desktop user.
3. **`--resume-fresh`** waits for the terminal on the Bus. For each session it
   opens a pane: the boot tab's for the first session, a new tab for each
   later one. It then sends a `term.type` with `{pane, instance, text,
   request_id}` of
   `cd <cwd> && claude [--dangerously-skip-permissions] --resume <id>`.
   - Sessions resume by id, because `claude -c` reopens the most recent
     conversation of a directory, and two sessions in one directory would
     both reopen the same one.
   - The `request_id`s are stable (keyed on the state file and the session)
     and every resumed id is appended to `<state>.done`. A rerun after a lost
     reply replays; it never types the same keys twice.
   - A directory or id that would need shell quoting is not typed. The worker
     logs it for a by-hand resume.

The worker logs to `journalctl -u cosmix-session-restart`. Its first line
names the recovery command:

```
mix /opt/cosmix/share/desktop/session-resume.mix --resume-only STATE
```

Run it as the desktop user. It skips `<state>.done` and opens a new tab for
each remaining session.

## Chords

Global chords bind to Quoin's `shell.session.confirm {action}` (through
inputd), never to the session verbs, so a keypress only ever opens the
question. The proposed chords are Ctrl+Alt+Backspace (restart) and
Ctrl+Alt+Delete (leave).

**Ctrl+Alt+Delete is also the kernel's reboot key.** Holding a VT does not
stop it:

- seatd and logind put the desktop's VT in keyboard mode `K_OFF`.
- The kernel still handles `KT_SPEC` keys in that mode
  (`drivers/tty/vt/keyboard.c`, `kbd_keycode()`: the early return is
  `(raw_mode || kbd->kbdmode == VC_OFF) && type != KT_SPEC && type != KT_SHIFT`).
- Ctrl+Alt+Delete's Boot keysym is `KT_SPEC`: `ctrl_alt_del()` sends SIGINT
  to PID 1, which starts `ctrl-alt-del.target`, an alias of `reboot.target`.

It is safe only while the kernel never sees the keystroke. That holds when
inputd holds an exclusive grab on the keyboard it reads and the chord is a
bound row: the bound stroke is swallowed and not re-emitted. If inputd is not
running or not grabbing, the kernel sees the chord and the machine reboots.
Either choose another chord or run `systemctl mask ctrl-alt-del.target` on the
host, which makes PID 1 ignore the key.

## Settings (environment)

| variable | default | meaning |
|---|---|---|
| `COSMIX_SESSION_USER` | (required for restart) | the desktop user whose sessions are resumed; never guessed |
| `COSMIX_SESSION_HOME` | from `getent passwd` | that user's home |
| `COSMIX_DESKTOP_UNIT` | `cosmix-desktop.service` | the unit a restart restarts |
| `COSMIX_BOOT_TERM_UNIT` | `cosmix-boot-term.service` | the boot terminal that must come back as a new process |
| `COSMIX_SESSION_TERM_SERVICE` | `term` | the terminal's Bus service |
| `COSMIX_LEAVE_VT` | `1` | the VT `leave` switches to (1–63) |
| `COSMIX_SESSION_WORKER_UNIT` | `cosmix-session-restart` | the transient unit's name |
| `COSMIX_SESSION_DRY_RUN` | unset | `1`: every request is a dry run |

## Install

The citizen needs `lib/session.mix` and `session-resume.mix` beside it:

```
install -D -m 0644 src/desktop/scripts/session-control.mix /opt/cosmix/share/desktop/session-control.mix
install -D -m 0644 src/desktop/scripts/session-resume.mix /opt/cosmix/share/desktop/session-resume.mix
install -D -m 0644 src/desktop/scripts/lib/session.mix /opt/cosmix/share/desktop/lib/session.mix
```

Copy `src/desktop/scripts/cosmix-session-control.service` to
`/etc/systemd/system/` and set `COSMIX_SESSION_USER`. Then run:

```
systemctl daemon-reload
systemctl enable --now cosmix-session-control.service
```

Check it with a dry run before trusting it with the live desktop.

## Tests

The tests never restart or switch anything:

- `mix src/desktop/scripts/tests/session-test.mix`: request handling, plans,
  session capture and resume lines, using pure functions against fixtures.
- `COSMIX=$PWD mix src/desktop/scripts/tests/session-bus-test.mix`: the
  production citizen over a private broker in forced dry-run mode.
