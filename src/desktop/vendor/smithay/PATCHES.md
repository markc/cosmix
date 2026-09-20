# Local patches to vendored smithay

This tree is a **patched** vendor copy, not a pristine one. Anything that
re-vendors or rebases smithay must reapply the patches below, or silently lose
behaviour cosmix depends on.

There is no `.cargo-checksum.json` here, so nothing detects local edits
automatically: `cargo` will not warn you, and a fresh vendor drop will look
clean while having thrown work away. This file is the only index that exists.

## How to find what is patched

Every deliberate local hunk is marked with a `cosmix patch:` comment. To list
them:

```sh
grep -rn "cosmix patch" src/
```

Anything modified WITHOUT such a marker predates this convention — see
"Unindexed history" below.

---

## 2026-09-20 — DnD: `unset` must not mean "drop"

**Files:** `src/wayland/selection/data_device/{dnd_grab.rs, server_dnd_grab.rs,
source.rs}` (+214 / −44)

**The bug.** `unset` called `drop()` in all four DnD grabs — `DnDGrab`
pointer/touch and `ServerDnDGrab` pointer/touch. comp reaches `unset` from at
least six teardown paths (session lock, popup grab install, start-surface
destroy, start-surface unmap, X11 surface swap, focus-policy teardown), so
**every one of them delivered the drag payload instead of cancelling it**. On
the live desktop: lock the screen mid-drag and the file drops wherever the
pointer happens to be, with no button release. Separately, lifting a *second*
finger during a touch drag delivered the drop.

**Why the obvious fix is wrong.** The legitimate drop goes through `unset` too
— the release handlers call `handle.unset_grab(...)`. So `unset` must know who
initiated it; it cannot simply be made to cancel.

**The patch.**

1. `pending_drop` on each grab, set **only** by the grab's own release handler
   immediately before it unsets. `unset()` drops when set, cancels otherwise.
   The two release sites per grab go through
   `conclude_pointer_release_as_drop()` / `conclude_touch_release_as_drop()`,
   which bundle the flag-set and the unset into one step so they cannot be got
   half-right. **Any future "commit this drag's drop" API must route through
   those helpers** rather than writing a second `pending_drop = true;
   unset_grab(...)` pair — the contract comment at each `pending_drop`
   declaration names the six external `unset` call sites this depends on.
2. `cancel()` revokes pending offers, deactivates the offer, sends `leave`,
   fires `source.cancelled()`, clears the icon, and signals
   `ClientDndGrabHandler::dropped(..., validated = false, ...)` — the trait's
   only end-of-session hook, which comp will need the moment it composites the
   drag icon, or the icon leaks on every cancel.
3. `finished` on each grab makes the session-ending callback fire exactly once,
   and `drop()` now `take()`s its fields like `cancel()` does.
4. Touch `cancel()` calls `handle.cancel(data, seq)`, so `wl_touch.cancel` is
   actually sent. Without it the existing cosmix patch at
   `src/input/touch/mod.rs:637` was dead whenever a DnD grab was installed.
5. `source.rs`: destroying a `wl_data_source` now terminates a matching drag.
   This is how a client cancels per the Wayland spec — ctk does exactly this on
   Escape — and the handler was a no-op, leaving a zombie grab that kept
   entering surfaces and delivered on release to a destination whose `receive`
   was then refused because `source.alive()` was false: a delivered drop that
   could never transfer.

**Deliberate non-changes**, each with a comment in the code explaining why, so
a future reader does not "fix" them:

- `ServerDnDGrab::cancel` sends `leave` **unconditionally**, unlike its client
  twin. Not an oversight: `ServerDnDGrab` has neither an `origin` nor a
  `data_source` field, so the client guard has nothing to compare against and
  could only compile as a hardcoded always-true. Its `update_focus` enters
  unconditionally, so leaving unconditionally is self-consistent.
- No liveness guard before `source.cancelled()`. Sends to a dead resource are
  no-ops (`send_event` returns `Result<(), InvalidId>`, discarded by the
  generated senders), and upstream's own `drop()` calls it unguarded.
- `destroyed()` downcasts to a **bare** `DnDGrab`. A future wrapper grab
  (logging, a11y, gesture arbitration) would make the downcast silently miss and
  bring the zombie-drag bug back with no error signal. A wrapper author must
  forward `as_any()`/`has_source()` to the inner grab or extend the match.

**Known gap:** `source.rs`'s `destroyed()` uses `unset_grab` with focus restore,
which re-enters comp's `SeatHandler`/`PointerTarget` callbacks from inside a
resource destructor while `PointerHandle`'s non-reentrant mutex is held. Audited
2026-09-20 against comp's actual handlers — neither `set_cursor_image` nor the
`PointerTarget` enter/leave delegates re-enter `PointerHandle`, so it is not
reachable today. **A future comp handler that touches `PointerHandle` from those
callbacks would deadlock.**

**Verification.** 10 regression tests in
`crates/cosmix-comp/src/protocol/dnd_cancel_tests.rs`, proven to discriminate:
with these three files reverted to their pre-patch state, all six cancel-path
tests fail and all four "release still delivers drop" guards pass — the latter
must be green in both states or they are measuring the wrong thing. With the
patch: 1768 passed, 0 failed, clippy clean.

---

## Unindexed history (pre-2026-09-20)

**20 commits touched this vendored tree before this file existed, and they are
NOT audited here.** Reconstructing which hunks are local would require diffing
against upstream smithay at the vendored revision; nobody has done that. What
follows is the file-level footprint from `git log`, as a starting point for
whoever next rebases — not a patch list:

| file | commits touching it |
|---|---|
| `src/xwayland/xwm/surface.rs` | 8 |
| `src/xwayland/xwm/mod.rs` | 6 |
| `src/xwayland/xserver.rs` | 4 |
| `src/wayland/session_lock/{surface,mod,lock}.rs` | 3 each |
| `src/wayland/shell/wlr_layer/mod.rs` | 2 |
| `src/wayland/foreign_toplevel_list/mod.rs` | 2 |
| `src/wayland/compositor/{transaction,mod}.rs` | 2 each |
| `src/utils/x11rb.rs` | 2 |
| `src/input/pointer/mod.rs` | 2 |
| `src/backend/libinput/mod.rs` | 2 |
| `Cargo.toml` | 2 |
| others | 1 each |

`src/input/pointer/mod.rs` and `src/input/touch/mod.rs` matter most to the
patch above: the DnD fix reasons about `set_grab`/`unset_grab`/`with_grab`
semantics in those files, and at least one of them already carries local
changes. Verify their behaviour against the source rather than against upstream
documentation.

To find the full diff for a given file:

```sh
git log --oneline -- src/desktop/vendor/smithay/<path>
```
