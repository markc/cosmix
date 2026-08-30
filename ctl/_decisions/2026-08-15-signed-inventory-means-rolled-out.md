# The signed inventory means "rolled out", and every payload carries the recovery generation

**Date:** 2026-08-15 · **Decider:** Mark (explicit, both halves) · **Status:** accepted, implementation arc started same night

Closes the nine escalations in
`_doc/2026-08-15-recovery-generation-propagation-gap.md`, which had converged
on one question: *does `_etc/mesh/inventory.signed` mean "authorised" or
"rolled out"?* Every consumer assumed the second while the toolchain only
guaranteed the first.

## Decision 1 — normal payloads carry `recovery_generation`

`cosmix-mesh-sign sign` gains `--recovery-generation <N>` (stamps the field
WITHOUT `recovery: true`), and the distributors (`mesh_sync`, `mesh_push`,
`mesh_join`) pass this control host's current floor generation automatically
on every normal payload. "Absent" then stops meaning "inherit" and starts
meaning "older than the recovery era", on both the node fold
(`cosmix-lib-mesh-trust`) and the control-host floor
(`mesh_lib.mix`) — refusal only once the local floor's generation is above
zero, so today's fleet (generation 0 everywhere, no recovery ever issued) is
the clean migration window and nothing in flight is invalidated.

Closes escalations **1** (fresh node never learns the generation), **7**
(a lower-epoch recovery undone by replaying an older generation-silent
normal payload), **8** (rebuilt node accepts a superseded recovery). The
generation-raise stays genesis-gated — the recovery-lockout hazard the
library comment warns about is unchanged.

## Decision 2 — `inventory.signed` means "rolled out"

A signature proves authorisation; the canonical cache asserts acceptance.
Therefore:

- the freshness floor advances on the **first** node that verifies, not
  after the last — the moment one node runs payload N, the older payload is
  a rollback (this matches the policy `deploy_wg`/`deploy_dnsd` already
  chose; the recorded asymmetry in `epoch.baseline.mix` is resolved in their
  favour);
- `mesh_join` signs into **proposal storage**; only a successful distributor
  publishes the canonical cache (`mesh_push`'s publish-after-local-accept is
  thereby *correct by definition* — the local node is a node);
- this ordering must NOT be copied into a future recovery distributor: an
  interrupted recovery retry presents the same generation, which the floor
  correctly refuses as non-advancing, so recovery distribution needs its own
  resume semantics (escalation 3's caveat, preserved here deliberately).

Settles escalations **3** (raise-before-or-after-fan-out), **4** (join
publishes a proposal as canonical), **9** (what a subset sync may claim).

## Consequences

- Escalation **2** (post-recovery epoch re-baseline) largely dissolves: with
  the generation on every payload, the replay-undo is refused on the
  generation axis; what remains is signer warning + spec prose, not ceremony
  enforcement.
- Escalation **6**: `mesh_alloc` allocates from the union of signed +
  authored coverage (folded into the arc).
- Escalation **5** (`mesh_baseline_reseed` escape hatch) stays undecided —
  documented hazard, root can already hand-edit both witnesses.
- SPEC 13 §6.4's "exact `recovery_generation` form and ceremony" is ratified
  by this arc's implementation.
- Reversibility: high on decision 1 (optional field; omitting it restores
  today's behaviour; every deployed noded 0.9.0 already parses it). Medium
  on decision 2 (ordering + join-flow changes, all in cmctl scripts).
