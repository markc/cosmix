# Ledger and lock identity are correctness checks, not security hardening

**Decided by Mark, 2026-08-29.** Resolves the decision fleet task 82 was blocked
on, which its own spec said must be recorded before implementation could start.

## The question task 82 asked

Task 82 collected three findings from the task-69 cold reviews and asked Mark to
rule on one thing: *is a same-UID agent inside the trust boundary or outside it?*
If outside, all three become correctness invariants that ship unconditionally. If
inside, all three are hardening and go opt-in behind the sandbox manifest.

It had bounced four times with three review rejections, with agents
re-litigating the framing rather than converging on an implementation.

## The decision: reframe, don't answer

The question was the wrong one, and answering it either way was going to keep
producing arguments.

**Same-UID agents remain inside the trust boundary** — that is settled by
`2026-08-16-agentic-first-security-is-opt-in.md` and is not reopened here.

But two of the three findings are **not security gates at all**. They are the
system checking that it has bound to the *right object*, which the agentic-first
law explicitly keeps unconditional:

> This does not license removing real, non-optional invariants. Guarantees that
> bind *correctness* — device/VT identity continuity, TOCTOU re-checks, the
> hygiene that stops the system seizing the *wrong* thing — stay unconditional.

The ADR's own test is *"does this gate exist to stop an agent doing something
wrong, or only to make a **human** vouch for it?"* An inode check makes nobody
vouch for anything. There is no prompt, no flag, no human in the loop, and no
agent is impeded. It is invisible unless it fires, and if it fires the system was
about to write to the wrong database.

So they are built as **identity checks**, and the trust-boundary question simply
does not arise.

## What that means per finding

**1. `ledger.rs` ~873 — reopen by path → BUILD, as an identity check.**
A reopen by pathname can bind to a different valid database carrying the same
project identity, giving two halves of the fleet different ledgers while both
believe they are correct. Verify dev/inode (or reopen the held fd) so the reopen
provably reaches the same object the primary opened. Cheap, invisible, no
ceremony. The realistic trigger is not an attacker but an accident — a stray
symlink, a worktree cleanup racing a reopen — and split-brain is silent and
unrecoverable when it happens.

**2. `clone_lock.rs` ~176 / verify lane — env-flag delegation → BUILD, same
framing.** The presence of `FOREMAN_CLONE_LANE_HELD=1` in the environment alone
suppresses the flock. That is a lock that **fails open on an inherited string**:
any nested foreman or cargo started with that variable set silently proceeds
unlocked, and nothing reports it. Bind the delegation to an inherited fd plus an
owner stamp (pid/pid_start) so the claim "an ancestor holds this lock" is
*provable* rather than *asserted*, and scrub the variables from agent
environments. Note the operational hazard recorded elsewhere: the flock wrapper
and the marker must be dropped together, never separately.

**3. `sandbox.rs` ~59 — sandbox default-off → DECLINED, and recorded as declined.**
This one genuinely is the law's territory: with the default sandbox off, a
same-UID agent can edit the ledger and lock files that drive routing. The
mitigations on offer — a denied state dir in the bwrap manifest, or running
agents as a separate UID — are exactly the "lock-down is a future, on-demand act"
the law defers. Same-UID agents are inside the boundary; the sandbox stays
opt-in. **This is written down so it stops being rediscovered.** Three review
rounds were spent re-arguing it, and an undocumented non-decision is
indistinguishable from an oversight.

## Why the reframe was the useful move

The trust-boundary framing made two cheap, uncontroversial fixes hostage to a
philosophical question they did not depend on. Separating them let (1) and (2)
proceed on their own merits and let (3) be declined explicitly rather than
argued repeatedly.

The general lesson worth keeping: **when a task keeps bouncing on its framing
rather than its implementation, the framing is the defect.** Task 67 had the same
shape and was resolved the same way — by slicing rather than by another attempt.
