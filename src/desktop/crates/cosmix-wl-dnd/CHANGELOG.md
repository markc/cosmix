# Changelog

Backfilled 2026-07-26 from the release commits; earlier entries are
reconstructions, not contemporaneous notes.

## 0.4.2 — 2026-07-26

- Document and test the outgoing-start transaction boundary: every refusal is
  before `start_drag` except a fatal flush, which first marks the shared Wayland
  connection lost and tears the unreturned transfer down. This lets toolkit
  callers preserve their own drag until `start_outgoing` succeeds, avoiding a
  larger prepare/commit API and making ordinary in-app fallback safe.
- Pin that nonce registration failure never invokes the start request, that
  `WouldBlock` remains a successful queued flush, and that every other flush
  error is connection-terminal before it reaches the caller.

## 0.4.1 — 2026-07-26

- Accept `Ask` in an outgoing action mask. `OutgoingTransfer::new` rejected any
  mask containing it, so `ActionMask::ALL` — the value the export path passes —
  always failed, after the payload had already moved and the in-app drag had
  been suppressed. An empty mask is still rejected. On an `ask` action the
  destination presents the choice and issues the final non-`Ask` `set_actions`
  and `accept` before `finish`, so offering it puts no new obligation on the
  source.

## 0.4.0 — 2026-07-26

- Add `OutgoingIcon` and `create_drag_icon`: a CPU-owned `argb8888` drag icon
  the Wayland source can hand the compositor, dormant until a caller reaches
  it. `OutgoingIcon` is the validation boundary because every value it accepts
  reaches `wl_shm`/`wl_surface`, where a bad one is connection-fatal rather
  than a merely failed drag — buffer dimensions must be integer multiples of
  `buffer_scale`, the 64-byte-rounded pool must survive `wl_shm.create_pool`'s
  signed 32-bit size, and pixels are written little-endian as `argb8888`
  defines rather than native-endian.

## 0.3.0 — 2026-07-25

- Add the seat and lifecycle rules the ctk handoff needs: `grab_is_unambiguous`
  so a caller can decline before committing, `SendError::AmbiguousSeat` when
  the held grab is not attributable to exactly one pointer-capable seat, and a
  `PointerCapabilityLost` terminal for a press that can never be released.
- Record a vanished pointer on the transport state rather than only queueing
  it, so a terminal-class `SourceCancelled` draining first cannot relabel the
  ending as though the pointer survived.

## 0.2.0 — 2026-07-24

- Add the send half: outgoing Wayland drags correlated by a private
  `application/x-cosmix-dnd-<nonce>` MIME. `OutgoingPayload` takes real paths
  only and owns them; `NonceRegistry` registers before `start_drag` because an
  echo can arrive in the same dispatch batch as the first source callback, and
  retires both ways so a terminal with no echo cannot leak the entry.
- `dnd_drop_performed` is not terminal — the source still serves `send()` and
  awaits `dnd_finished`.

## 0.1.0 — 2026-07-23

- Wayland drag-and-drop receive bridge: a guest `wl_data_device` event queue on
  winit's existing connection (SCTK 0.19.2), turning compositor callbacks into
  a bounded, coalesced event stream. No Bevy or ctk dependency.
- Founding rule, "resolve at settle, not at capture": several callbacks arrive
  in one dispatch, so a value captured in one and consumed as a decision can be
  contradicted by the next.
