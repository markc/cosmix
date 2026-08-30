# Decision: `mix.eval` mesh-native remote-eval verb — REJECTED (do not add)

**Date:** 2026-07-03 · **Status:** REJECTED — do NOT add (not a pending/deferred
feature) · **Decided by:** Codex as delegated decision authority (thread 019f233f);
**Mark concurs** (2026-07-03: "mix.eval should not be added to mix … too dangerous to
allow unless seriously mitigated (not likely in the near future)") · **Class:** C2
(trust/auth boundary — a new mesh-native RCE capability).

> **Standing answer: NO.** `mix.eval` is full-host remote code execution over the
> mesh. It is too dangerous to allow absent *serious* mitigation, which is not
> expected near-term. Do not resurface it as a suggestion or roadmap item. The
> "required future contract" below is the bar that mitigation would have to clear —
> it documents *why the answer stays no*, not a plan to build it.

## The proposal (from the 2026-07-02 Mix audit)

Add a `mix.eval` reserved ABP verb: a remote peer sends Mix **source** to a
`mix --serve` citizen over the ABP mesh; the citizen runs it and returns the
result. Flagged in the audit as "needs AuthPolicy". This is **arbitrary remote
code execution over the mesh by design**.

## Verdict: REJECTED — do not add

Do **not** add `mix.eval` — reserved or otherwise. **`ssh_mix` remains the
supported authenticated remote-Mix-execution path** (it runs Mix source on a remote
node over ssh, which carries its own auth) and covers the real need. A mesh-native
eval verb is **too dangerous to allow absent serious mitigation** (a real
`PeerIdentity → CapabilitySet` auth spine in the serve runtime, plus everything in the
required-future-contract below) — mitigation that is **not expected near-term**. Treat
the standing answer as NO; do not carry `mix.eval` as a roadmap or "someday" item.

### Reasoning (Codex)

- `mix.eval` is deliberate **full host RCE**. Mix cannot offer a coherent
  "restricted eval" without gutting the language's operational purpose — `run`,
  `ssh_run`, file IO, ABP calls, `include`, env, and process control are core.
- **The current reserved path is identity-blind.** `ServeRuntime::handle_reserved()`
  (`crates/cosmix-mix/src/serve_runtime.rs`) receives only `command`, `args_header`,
  `req_body`, `handler_commands` — no peer identity. Its identity-blind reserved
  surface today is `HELP` / `INFO` / `QUIT` / `<svc>.props.{get,list,describe}`
  (props.get is read-only + redacted; unauthenticated is acceptable there, NOT for
  eval).
- **Headers are not auth.** `IncomingEvent.headers["from"]` and any
  signed-identity-looking header are transport data until the citizen runs the same
  **verified** `PeerIdentity` pipeline the cos daemons use (SPEC 12 `AuthPolicy`,
  SPEC-10 `signed_ident` via cosmix-mesh-sign) — which the lightweight mix serve
  runtime does not have.
- A **global reserved `mix.eval`** would be unshadowable and pre-dispatch — exactly
  the wrong place for unauthenticated RCE.
- An author-written `on mix.eval … end` is not a language feature; the core must not
  **mint** the dangerous verb before policy exists. An operator who accepts the risk
  can already write an explicit domain handler with their own allowlist — the core
  simply doesn't bless it.
- Marginal value is low: `ssh_mix` already gives authenticated remote Mix execution.
  `mix.eval`'s only extra is "remote eval without an ssh account, on mesh identity" —
  not worth a new RCE surface before the auth spine exists.

## Required future contract (preconditions to reconsider)

Before `mix.eval` (or `<svc>.eval`) is built, ALL of these must hold:

1. **Default OFF, always** — armed only by an explicit `--serve` flag / config, never
   on by default.
2. **Verified identity required** — a SPEC-10 `signed_ident` allowlist, or an
   `AuthPolicy` capability derived from a **verified** `PeerIdentity`. An unsigned,
   unknown, or unverifiable peer is a **hard deny** (no eval).
3. **Gate location** — inside the serve/runtime dispatch path, evaluated only **after**
   `PeerIdentity` is plumbed into the event/reserved call. Never inferred from raw
   headers.
4. **Scope** — full Mix only. **No fake sandbox subset** (it would be security
   theatre given Mix's purpose).
5. **Namespace** — service-scoped **`<svc>.eval`**, not a global `mix.eval`, unless a
   later explicit platform-wide eval-authority model exists.
6. **Audit** — every attempt, allowed OR denied, logged with service, verified
   identity fields, command, rc, source **SHA-256** + byte length, and timestamp.
   **Never log raw source by default.**

## Consequence

The 2026-07 audit's three C2 items are now all resolved: **env-transport** and the
**send/emit rc contract** shipped (mix 0.21.1, see
`_journal/2026-07-03-mix-c2-items-shipped.md`); **`mix.eval` is REJECTED** — Codex and
Mark agree it should not be added. The required-future-contract above is the bar any
future mitigation would have to clear (auth spine first, then the six conditions); it
exists to document *why the answer stays no*, not to schedule the work. Absent serious
mitigation — not expected near-term — the standing answer is NO.
