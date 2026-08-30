# ADR: Cosmix backends are event-driven (ABP wake), never pollers

- **Date:** 2026-07-20
- **Status:** ACCEPTED — standing architecture law for every cosmix backend daemon.
- **Decision authority:** Mark (explicit, 2026-07-20).
- **Trigger:** a alpha desktop lockup investigation surfaced that
  `cosmix-sshm-worker.timer` was **polling a job queue every ~5–6 seconds**, 24/7
  — a full process spawn + cold Mix start + HTTPS-over-WireGuard round-trip to
  `mail.example.org`, ~14,000 times/day, almost always to hear "nothing queued." It
  was killed on the spot (units stopped, disabled, removed). This ADR records
  *why*, and makes the rule general so it never recurs.
- **Relationship to neighbours:** implements
  `2026-07-18-amp-as-control-plane.md` — ABP is the control plane, so backend
  work is *triggered over ABP*, not discovered by polling.

---

## 🚨 BIG NOTICE — DO NOT POLL. EVER. 🚨

> **A cosmix backend must be woken by an event, not by a clock.**
>
> The primary trigger for any queue-drain / job worker is a **delegated ABP
> `*.wake` verb** fired by the enqueuer (webd) the instant a job lands. A systemd
> timer is permitted **only** as a *lazy backstop* for a missed/failed wake, and
> it must be **minutes-scale (≥ 5 min), never seconds.**
>
> **If you catch yourself writing `OnUnitInactiveSec=` in single-digit seconds,
> or a `while true` + `sleep(N)` with N under a minute, STOP.** You are building
> the sshm mistake again. Wire a wake instead.
>
> **Seconds-scale polling of anything is banned.** It is not "near-instant
> latency" — it is a process-spawn tax, a network tax, a journal flood, and it
> masks the absence of the real design.

---

## The rule, precisely

1. **Push is primary.** Enqueue transaction → best-effort `<svc>.wake` (fired by
   webd — either the handler's `amp_call`, or, since 2026-07-20, a declarative
   `wake:<verb>` route capability webd fires itself post-response) → a tiny
   credential-free ABP **wake citizen** → the oneshot drain unit starts → it
   claims + runs until the queue drains → exits. The citizen starts the drain by
   whatever mechanism fixes the systemd job mode: `systemctl start --no-block`
   under a narrow polkit rule (provisiond/toolsd), **or** — preferred where the
   wake user must not hold any `StartUnit` grant — writing a trigger file watched
   by a `.path` unit (sshm; a polkit `StartUnit` grant cannot constrain a
   caller-supplied `--job-mode`). The wake carries **no job spec and no
   authority**; the queue is the source of truth. The ABP reply means only "wake
   accepted," never "done."
2. **The timer is a backstop, not the mechanism.** It recovers work the wake
   cannot: a job whose wake was lost (citizen down, polkit rule broken, boot
   before webd), **and** work left behind by a drain that exited early — if a
   drain caps out on run-count, wall budget, or a claim error, the wake that
   would have covered the remainder was already spent coalescing into the run
   that just ended, so only the timer re-arms it (see the toolsd exit analysis
   in `_plan/2026-07-23-consolidated-backlog.md`). Do not write "solely a lost
   wake" — that reading is what let a drain-truncation bug hide.
   **≥ 5 min.** If jobs "feel slow," fix the wake path or give the drain a
   continuation signal — do **not** shorten the timer.
3. **Boot reconcile is a third path.** `OnBootSec` (minutes) drains anything left
   queued across a reboot. Fine.
4. **Coalescing is free.** `systemctl start --no-block` of an already-running
   oneshot is a no-op, so repeated wakes collapse harmlessly. No debounce needed.

## The reference implementations (copy these)

Both already live and correct — study them before building any new worker:

| daemon | wake citizen (primary) | backstop timer | drain worker |
|---|---|---|---|
| **provisiond** | `cosmix-provisiond-wake.service` (running) — verb `provisiond.wake` | `cosmix-provisiond-drain.timer` @ 5 min | `cosmix-provisiond-drain.service` |
| **toolsd** | `cosmix-toolsd-wake.service` (running) — verb `toolsd.wake` | `cosmix-toolsd-drain.timer` @ 5 min | `cosmix-toolsd-drain.service` |

Current implementations:
`_provisiond/{provisiond.mix,drain.mix,49-cosmix-provisiond-wake.rules}` and
`_toolsd/{toolsd.mix,toolsd_drain.mix,49-cosmix-toolsd-wake.rules}`.

Both backstops are lazy, and toolsd was relaxed from 1 min to 5 min on 2026-07-25 —
1 min violated the ≥5-min rule below. It had been set tight back when the interval was
mistaken for the interactive-latency mechanism; the wake is.

**Do not read either drain as "loops until the queue is empty."** An earlier revision of
this paragraph claimed that; cold review refuted it 2026-07-25. Both drains are bounded
and both can exit with work still queued:

- **provisiond** classifies each claim as `empty` / `more` / `job` / `infra`, needs **two
  consecutive empties** to call the queue drained, and retries infrastructure failures up
  to `$MAX_INFRA = 3` — but still exits on `$MAX_JOBS = 50` or `$MAX_WALL = 1800`s. Its own
  log line for those says *"exiting, timer/next wake resumes"*.
- **toolsd** is weaker: `$MAX_RUNS = 8`, a 525s wall budget, and a claim HTTP/JSON error
  that returns the same `nil` as a 204 and breaks the loop. Worse, a 204 does not even mean
  empty — `tools_claim_next` answers 204 whenever another run is already claimed, since
  global concurrency is 1.

So for a bounded drain the timer is **not purely a lost-wake backstop — it is also the
resume path**, because the enqueuer's wake was already spent coalescing into the drain that
was running. That is by design in provisiond (50 jobs is a generous bound and its jobs are
slow builds) and an accepted rough edge in toolsd (interactive spinner, up to a 5-min stall
on a >8-job burst; tracked in `_plan/2026-07-23-consolidated-backlog.md`). Either way the
≥5-min floor stands: the fix for a stall is a continuation signal, never a faster timer.

## The offender

**sshm** was the *only* drain worker with **no wake citizen** — the `sshm.wake`
verb was explicitly deferred ("polling worker suffices" — sshm admin-panel
plan §wake, retired 2026-07-23, git history) and the 5 s poll shipped as
its sole trigger. Killed 2026-07-20. **Rearchitecture required before the sshm
panel's action buttons work again:** add `sshm.wake` to the delegated-verb
allowlist + route (`$COSMIX`), stand up a `cosmix-sshm-wake.service` citizen,
have webd fire it on enqueue, and re-add the drain timer as a **5-min backstop**
(not the 5 s poller). **Current implementation:** `_bin/deploy_sshm.mix`,
`_bin/sshm.mix`, and
`_etc/systemd/cosmix-sshm-wake.service` + `_etc/systemd/cosmix-sshm-drain.{service,path,timer}`
(the wake half is a service only — it is the `.path`-mediated variant, so there is no
`cosmix-sshm-wake.path` and no `cosmix-sshm-wake.timer`). The killed units'
reference copy still sits at
`/opt/cosmix/share/cosmix/sshm_worker.mix` (+ `sshm_worker_lib.mix`); the drain
logic is fine — only its *trigger* was wrong.

## Corollary — fix the binary, never a workaround

This is the same law as **"extend Mix, don't work around it."** If doing it the
right way (an ABP wake) needs a capability the cosmix backend doesn't yet
have — a new delegated verb, a polkit scope, a webd accelerator, an ABP citizen
primitive — **add it to the relevant cosmix binary/service** (`$COSMIX`, webd,
noded), rebuild, deploy. **Never** paper over a missing wake with a faster
timer. A missing wake path is a *cosmix bug*, not a licence to poll.

## Checklist before shipping any new backend worker

- [ ] Is there a delegated `*.wake` verb + wake citizen? (If no → **not done.**)
- [ ] Does webd (or the enqueuer) fire the wake on enqueue, best-effort?
- [ ] Is the enqueue durable independent of the wake succeeding?
- [ ] Is the *only* timer a ≥ 5-min backstop, clearly commented as such?
- [ ] No `OnUnitInactiveSec` < 5 min anywhere. No sub-minute `sleep()` loop.
- [ ] Does `systemctl start --no-block` coalescing cover concurrent wakes?
