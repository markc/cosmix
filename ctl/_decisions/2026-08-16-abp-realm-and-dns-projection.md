# ABP realm + DNS projection — the four §10 decisions

**Date:** 2026-08-16 · **Decided by:** Mark · **Status:** binding
**Context:** `_plan/2026-08-14-abp-namespace-unification.md` §10 posed four
open decisions. All four are now resolved as recommended there. The plan
remains the working document for execution (§7 sequencing); this record is the
authority for *what* was decided.

## 1. Sequencing — harden trust paths before unifying names: APPROVED

Approved as executed: the §5 prerequisites were the D1 signed-inventory arc,
complete and deployed fleet-wide as of 2026-08-16 (noded 0.13.0, epoch 16 —
`_journal/2026-08-16-d14-slice4-hot-reload-contract.md`) — **except §5.3
(validate `payload.mesh` as an FQDN at verify time), which by the plan's own
sequencing lands *with* the realm change, not before** (`payload.mesh` is
still an unvalidated string, live value `"bus"`). Routing and DNS generation
now derive from the verified signed inventory; the naming migration builds on
that, not the other way around, and must carry §5.3 with it.

## 2. Realm value: `markc.internal`; no installed default, ever

- This mesh's realm is **`markc.internal`**.
- `cosmix.internal` is **documentation text only** — an illustration that must
  never become an installed value.
- **Install refuses to bind a realm without an explicit choice.** Prompting is
  not enough; an unattended install takes a generated entropy label
  (`m-<id>.internal`), never a shared default. This is what kills the
  cloned-image / restored-backup / VPN-join collision class.
- `.internal` is the right suffix because a leaked off-mesh lookup returns
  NXDOMAIN — the fail-safe outweighs global uniqueness. Uniqueness lives in
  the second label; mesh identity lives in signatures, not DNS.
- Public zones (`example.org`, `example.net`) are untouched.

Accepted cost: the realm is bound into admission, so a future rename is a
coordinated fleet re-admission. That cost is why the value is chosen
explicitly and once.

## 3. Phase-one scope: nodes only (A/AAAA); SRV deferred

Project **nodes only** — `A`/`AAAA` at `<node>.<realm>` from the verified
inventory. Service `SRV` waits until a **signed placement field** exists
(schema v2, its own migration). Deciding factor: SRV today would either lie
or read unsigned placement data, which violates the D1 principle the whole
arc just enforced. ABP routing does not need DNS service discovery — it
routes off the signed inventory.

## 4. Cross-mesh spelling: `node@realm` stays outside the DNS promise

No `service.node.foreign-realm` projection is designed now. Zero federated
meshes exist and federation requires an explicit trust grant regardless, so
nothing is blocked; designing federation addressing with a sample size of
zero is spec rot. Revisit when a second mesh is real.

## Rejected alternatives

Shipping a default realm and warning on collision (the collision has already
happened by then); generated-unique-with-rename (defers the collision to an
unguarded rename step); designing the placement field now (speculative);
redesigning cross-mesh spelling now (same).
