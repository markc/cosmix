# Owned editor — stage C preview

`MIX_EDITOR=owned` selects this editor through `repl_editor.rs`; rustyline stays
the default. `runtime.rs` starts one owner thread. It owns buffer, history,
decoder, interaction state and terminal modes. The evaluator supplies evaluated
prompts and owned completion snapshots; no evaluator `Rc` crosses threads.

- `mod.rs` is the generation/revision reducer. Mode effects require completion
  before acknowledgement. Empty-primary admission and draft-preserving local
  pause are distinct: only an admission reservation can be consumed.
- `input.rs` incrementally decodes UTF-8, keys and bounded bracketed paste.
  Its poll includes tty input, control wake, signal wake and writable output.
  Escape disambiguation is the only timer; requests never depend on that timer.
- `terminal.rs` owns a separate nonblocking output description and bounded
  writes. Restore checks foreground, restores termios first, then attempts
  protocol cleanup without waiting for output drain. Finish moves below the
  logical tail even when the cursor is on an earlier row.
- `signals.rs` provides async-signal-safe SIGTSTP ingress for the job controller.
  External and typed stops restore cooked mode first. A background SIGCONT
  leaves the editor suspended until a later signal finds it foreground.
- `runtime.rs` handles prompt, admission, pause/resume and shutdown control;
  shutdown waits for a cleanup latch even after input failure. Completion uses
  cached owned command names, prefix filtering before result caps and tagged
  results. Begin resets interaction/completion state and rejects stale results.
- `buffer.rs` keeps grapheme boundaries, undo/redo and kill/yank state. Prompt
  bytes are reserved in its input budget so rejected paste/edit operations
  preserve the draft instead of failing in layout afterwards.
- `history.rs` is the rustyline-compatible V2 codec. The owner maintains history;
  the REPL adapter performs file I/O. Incomplete/oversized ingestion warns and
  disables saving, preserving the original file.
- `render.rs` computes cell runs and cursor/end positions for Unicode and
  wrapping. Only prompt SGR styles are replayed, never cursor/protocol escapes.

The controller owns foreground process groups and its job-mode records; the
editor owns prompt-mode records. Correct ordering remains restore-before-line
return and foreground-before-mode-entry. Neither record grants ownership.

Per-keystroke O(buffer) relayout is accepted and bounded; optimisation is deferred.
Search Enter selects without submitting; this deliberate divergence remains a promotion gate.
Partial trailing UTF-8 keeps admission Busy until completed, a cost of the one-timer rule.
`Generation.session = 1` is a placeholder; attachment identity integration belongs to stage D.
Full visual/SSH parity and switching the default remain promotion work.

Unit and real-PTY tests cover decoder boundaries, history round trips, Unicode,
paste/search/draft preservation, completion caps/cycling, external stop/bg/fg,
output backpressure, mode-entry refusal, input failure cleanup, HUP, wrapped
submission, oversized history protection and legacy-path regression. The PTY
helpers use separate processes and retain only CLOEXEC descriptors.
