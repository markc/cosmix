# Owned interactive editor (preview)

Set `MIX_EDITOR=owned` in the environment before starting an interactive Mix
session to select the owned editor. The default remains rustyline. Redirected
input or output uses the existing fallback. This preview does not change Mix's
command classification, evaluator, continuation rules or job-control policy.

The evaluator builds the prompt and an owned completion snapshot; one editor
thread owns terminal input, incremental UTF-8 decoding, bracketed paste, editing
state and raw modes. Its `poll()` includes a control socket and a signal wake
socket. Only escape disambiguation uses a timer. Completion filesystem work runs
off the input thread, with bounded candidates and generation/revision checks.

Enter restores terminal modes and disables bracketed paste before returning the
line for evaluation. No next prompt starts speculatively. The job controller
continues to own process groups and foreground-terminal transfer. Ctrl+Z at an
editor prompt restores modes before invoking the controller's existing shell
stop behaviour; resume preserves the draft. HUP asks the editor to restore its
protocols before the controller exits.

External SIGTSTP uses the same cooperative cooked-mode stop path. After `bg`,
the editor stays cooked without tty reads until a later SIGCONT finds it in the
foreground. Output uses a separate nonblocking tty description with bounded
writes between control polls. Termios restoration precedes best-effort protocol
cleanup; output backpressure never delays a suspension/shutdown acknowledgement.
HUP waits for completed cleanup, including after an input-loop failure.

The internal generation-tagged protocol supports prompt activation, empty-primary
prompt reservation, resume and shutdown. Admission refuses drafts, continuation,
paste, search and completion. A separate local lifecycle pause preserves drafts
and partial decoder bytes; it grants no evaluation permission. Both paths restore
modes before acknowledgement, and restoration errors never grant terminal use.
Restricted profiles accept only job-management syntax and discard completion and
history supplied for an ordinary prompt. This round does not expose remote
evaluation or provide the parked-evaluator host.

Implemented keys include grapheme-aware left/right/delete/backspace, Home/End,
Ctrl+A/E/B/F, Alt+B/F, history Up/Down and Ctrl+P/N, Ctrl+R reverse search,
Ctrl+K/U/W kill, Ctrl+Y yank, Ctrl+_ undo, Tab completion/cycling, Ctrl+C and
Ctrl+D. Enter leaves search with its selected match; Escape or Ctrl+G restores
the previous draft. Bracketed paste is a single undoable edit; paste payloads
over 64 KiB are rejected after consuming their closing marker. Input buffers,
snapshots, prompt text and render layouts have explicit limits.

History continues to use `.mix_history` and rustyline's `#V2` multiline encoding,
with the last 100 entries and consecutive duplicate suppression. History files
are loaded with a 16 MiB limit; incomplete/oversized loads warn and disable saving
so the original file cannot be overwritten by a loaded prefix. Completion uses the same variables, aliases,
command names and subcommand sources as the legacy path, plus the captured cwd
and home for paths. Prefix filtering precedes the 4096-result/1 MiB result cap,
including directory enumeration. Command names share a cached owned snapshot
using the legacy PATH-scan lifetime, invalidated when aliases change.

Prompt SGR colour is retained; cursor-control and terminal-protocol escapes are
stripped. Resize reflows the logical buffer; buffers taller than the terminal
use a viewport containing the cursor. Full editing/search/completion UI parity,
terminal-emulator visual acceptance and real SSH coverage remain promotion gates
before changing the default or removing rustyline.

The `owned_editor_pty` integration family covers actual PTY input and handoff,
Unicode editing, multiline history, completion, silent resize, EOF/HUP and nested
shell stop/fg. The legacy job-control family remains an independent regression
gate. No production deployment is part of this preview.
