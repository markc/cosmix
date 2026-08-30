# Audio idle-silence fast-path: killed, not re-implemented

**Decision (2026-07-28, Mark-directed):** the parked `$COSMIX` branch
`wip/audio-idle-fast-path` (tip `daebce5`, off `main` @ `1c51723`) is deleted
without re-implementation. This resolves the A/B/C decision that had been
pending since 2026-07-22 as **C: revert/drop the fast path**. Do not re-propose
an idle-silence fast-path for cosmix-musicd without new evidence.

## What it was

cosmix-musicd idle-silence fast-path (stopped + settled + SILENCE_HOLD of
pre-master silence → memset silence instead of 32-voice/sample synth) plus a
ctk `state_tick` backstop poll relaxation 50ms→250ms. Touched
`gui/ctk/src/amp.rs` (a path that no longer exists — desktop workspace moved to
`desktop/ctk/` on 2026-07-24) and cosmix-musicd `mixer.rs`/`mixer_host.rs`
(+229 lines).

## Why killed

- **Benefit measured at ~0.2% of a core.** The real idle win (reactive
  rendering, 55%→23%) was separate work, already on `main`.
- Cold codex review left a **structurally unfixable residual**: the fast path
  cannot see synth voice quiescence (channel mute/fader masking, cross-strip
  cancellation, `ampeg_delay` voices up to ~102s). Fixing it fully meant
  vendoring/patching rustysynth for an `active_voice_count()` getter (option A)
  — a maintained fork for 0.2%.
- The branch was also 91 commits behind with its ctk half pointing at a moved
  path; a cherry-pick would have fought the workspace relocation.
- The branch's genuinely-fixed findings (RMS window accounting on fast-path
  exit, `live_note()` disarm) only matter *when the fast path exists* — nothing
  worth salvaging onto `main`.

## Recovery (while it lasts)

The commit object `daebce5` remains in the local `$COSMIX` object store until
git gc prunes it (~2 weeks–30 days); it was never pushed to origin. A
format-patch export existed only in the 2026-07-28 session scratchpad. After
gc, the reference implementation is this record plus the review thread
`019f89dc-d5f3-7391-8256-2d5dab67386b`.

Same sweep also dropped the two stale cmctl stashes (2026-05-22 VirtualList
for disp-html — superseded by the Datastar re-founding; 2026-05-28 lib-log
stats wiring against the pre-split cmctl tree that no longer holds Rust).
