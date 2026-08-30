# Same-UID agents are inside the foreman trust boundary

- **Date:** 2026-08-27
- **Status:** Accepted
- **Decider:** Mark (delegated: "do both of your recommendations", after the
  recommendation of 2026-08-26 20:50)
- **Scope:** `cosmix-foreman` — task 82, and task 76's folded fencing
  findings (ledger finding 777)

## The question

Task 82 asked one thing plainly: **is a same-UID agent inside the foreman
trust boundary, or outside it?** Three pre-existing weaknesses turn on the
answer — the ledger being reopened by path (swappable between opens), lane
delegation suppressed by the mere presence of `FOREMAN_*_LANE_HELD=1` in
the environment, and an agent's ability to edit the ledger or the lock
files when the sandbox is off (the agentic-first default). Task 76's
review rounds then surfaced six more of the same family: agent-authored
repository content that dodges refinery validation.

## Decision

**Inside.** A same-UID agent is trusted. We do not build pre-emptive
defences against a hostile local agent.

The reasoning is the canonical law in `CLAUDE.md`: *do not over-engineer
security at the expense of usability and flexibility; lock-down is a
future, on-demand act, built when a concrete threat makes it worth its
usability cost — never pre-emptively.* And the threat model makes the
fences hollow anyway: **an agent running as this UID already has full
filesystem access.** It does not need to smuggle a symlinked `Cargo.lock`
past the refinery to do damage — it can write the ledger directly. A fence
that a five-line script walks around is not a boundary; it is a tax on
every future change to the code it guards.

The evidence for the tax is on the record: task 76 spent **nine review
rounds** on this surface, and each round produced fresh instances, because
incremental fencing has no natural stopping point without a designed
boundary.

## What this does NOT license

The law's own carve-out applies and is the sharp edge of this decision:
*guarantees that bind correctness — TOCTOU re-checks, the hygiene that
stops the system operating on the wrong thing — stay unconditional.*

The test is **not** "could an agent exploit this?" but **"can this go
wrong with no adversary at all?"** A misbehaving agent, a nested process
inheriting an environment variable, an operator moving a file — none of
these require malice, and the system must still be correct. So task 82's
three mitigations SHIP, as correctness hardening rather than as security:

1. **Reopen the ledger by held fd** (dev/inode verified), never by path. A
   replaced or moved database must be detected, whoever moved it.
2. **Bind lane delegation to an inherited fd + owner stamp** instead of a
   bare env flag, and scrub `FOREMAN_*_LANE_HELD` from agent environments.
   Environment variables are inherited by every child by default: a nested
   `foreman` or `cargo` can silently suppress a lock it never held, with
   no malice anywhere in the picture.
3. **Keep the ledger and lock files under a state directory the sandbox
   manifest denies** — but only *when the sandbox is on*, since the
   sandbox is opt-in hardening, not the default.

Each costs nothing in usability, which is precisely why the law does not
protect them from being built.

## Task 76's six folded findings (finding 777)

Dispositioned by the same test, not waved through:

- **Not built** (defend only against hostile content): `[package].workspace`
  non-string values treated as absent; cargo stderr phrase matching being
  spoofable in the infrastructure direction.
- **Have a real no-adversary case and should be fixed** as ordinary
  correctness bugs, not fences: an added workspace `Cargo.lock` is
  validated but never relocked, so the refinery's version bump lands
  against a lockfile it did not update — a task that legitimately adds a
  lockfile lands an inconsistent tree; `[workspace].default-members` is not
  compared against the base, so a legitimate edit silently narrows what
  tier-0 tests; and an added nested `Cargo.toml` (a task adding a new
  crate) is skipped by discovery and therefore never versioned.
- **The general case** — the refinery reads repository content the verifier
  does not, with no written statement of what it reads and trusts — is
  answered by this ADR rather than by code.

## Reversal triggers

This is a dated judgement, not a permanent property. Revisit when any of
these becomes true:

- foreman runs agents under a **different UID**, or on a **multi-tenant**
  host, or accepts task branches from an author who is not the operator;
- an agent is observed reaching a routing decision through repository
  content in a way that was not intended (as opposed to merely being able
  to);
- the sandbox stops being opt-in and becomes the default, at which point
  the boundary it draws becomes load-bearing and items 3 and the "not
  built" list above should be re-scored.

## Consequences

- Task 82 is unblocked and scoped down: three correctness fixes, no
  security project.
- Task 76's fencing residue does not reopen; two of its findings become
  ordinary bug work.
- Reviewers should stop reporting same-UID content-fencing gaps as
  BLOCKERs against this codebase; cite this ADR instead.
