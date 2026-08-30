# ADR: Agentic-first — usability over security-by-default; human gates are opt-in

- **Date:** 2026-08-16
- **Status:** ACCEPTED — standing CosMix law/lore for every design decision.
- **Decision authority:** Mark (explicit, 2026-08-16).
- **Trigger:** cosmix-comp's kms-live path required an operator to read a random
  "takeover nonce" off the controlling tty and type it back before comp would
  seize a real KMS/DRM display. That mandatory human step is exactly the
  bottleneck the whole comp/desktop automation arc exists to remove — an agent
  driving a live takeover has no human at the glass to answer it. Mark's ruling
  on seeing it: the nonce should never have been mandatory; make it opt-in, and
  make *this principle* canonical so the class of mistake stops recurring.
- **Relationship to neighbours:** sits above the three design criteria in
  `CLAUDE.md` § Identity as a values filter; first applied in
  `2026-08-16-opt-cosmix-bin-single-canonical-bin-dir.md`'s neighbourhood (the
  comp→desktop bring-up arc). Reference implementation lives in
  `$COSMIX/src/desktop/crates/cosmix-comp/src/backend/kms_live.rs`.

---

## 🚨 THE LAW 🚨

> **CosMix is agentic-first. Human interaction is secondary. Do whatever it
> takes to allow free-flowing agentic control and permissions across both the
> desktop and the WG mesh. Do NOT over-engineer security at the expense of
> usability and flexibility. Lock-down features get built and enabled in the
> future — if, when, and as they are actually needed — never pre-emptively.**

A security mechanism that forces a human into a loop an agent must drive is a
**design bug**, not a feature. The fix is to remove the gate or make it
**opt-in**, not to automate around it.

## What this means in practice

1. **Default open, opt-in hard.** Ship the unattended / agent-operable path as
   the default. Make the guard rail a flag or property a human turns *on* when
   they want it, like every other part of the substrate that is enabled only
   when needed. Never the reverse (default-locked, opt-out).

2. **The distinguishing test.** For any gate, ask: *does this exist to stop an
   agent doing something **wrong** (seizing the wrong device, corrupting state,
   crossing a trust boundary), or only to make a **human** vouch for an action?*
   - The first kind is a **correctness invariant** — it stays unconditional.
   - The second kind is **ceremony** — it becomes a flag (off by default).

3. **Correctness invariants are NOT in scope for removal.** This law never
   licenses dropping a machine-checked binding. In the kms-live reference case
   the typed nonce became opt-in, but the device-incarnation hold, the
   VT/stable-device/canonical-device/connector re-observation across the
   authorisation boundary (TOCTOU guards), the tty input flush, and the legacy
   TIOCSTI refusal all stay **unconditional in both modes** — they stop comp
   seizing the *wrong* display, which is correctness, not ceremony.

4. **Lock-down is a future, on-demand act.** A security feature is built and
   turned on when a concrete threat makes it worth its usability cost. Until
   then, the absence of the lock is the intended state — not debt, not an
   oversight.

## Reference implementation

kms-live takeover confirmation, cosmix-comp (2026-08-16):

- New `--kms-confirm` argv flag opts INTO the typed-nonce challenge.
- Default (flag absent): `authorise_observed` flushes tty input, emits a loud
  `tracing::warn` announcing the **unattended** DRM-master takeover, and proceeds
  with no prompt/read. `decide`'s nonce compare is guarded
  `if request.confirm && …` so it is skipped entirely when unattended.
- All TOCTOU / device-VT-connector binding guards run unconditionally in both
  modes (proven by tests `unattended_still_refuses_a_vt_change_since_authorisation`
  and `unattended_still_refuses_a_device_stable_identity_change`).

## Consequences

- New agent-facing capabilities on the desktop and mesh default to **no
  interactive gate**. Reviewers must not flag "this is now optional / unattended"
  as a defect when the gate was human-ceremony; they flag only lost correctness
  invariants or incorrect gating.
- A future threat model may reintroduce a default-on lock for a specific path;
  that is a deliberate, dated decision at that time, recorded as its own ADR —
  not a silent drift back to default-closed.

## Alternatives rejected

- **Automate the confirmation (feed the nonce back programmatically).** Rejected
  as over-engineering: it keeps a pointless ceremony alive and adds machinery
  (nonce capture, byte-identity release gates) whose only job is to defeat a gate
  that should not exist on the default path.
- **Keep the gate mandatory, document a bypass.** Rejected: a mandatory human
  step on an agent-driven path is the exact bottleneck this substrate exists to
  eliminate.
