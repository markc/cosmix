# webmix — standalone web-hosting distribution (vision)

**Status: VISION / direction-open (C2). Not a build plan. Gated on webd + mix nearing
1.0.** This note captures an idea and pressure-tests it; it does *not* authorize work.
Promote to a dated `_plan/` only after the "should we" call below is made and the
prerequisites are met.

Drafted 2026-06-15 (Mark + Claude).

## The idea

If `cosmix-webd` + `mix` ever ship as a homogeneous, self-contained pair — outside
the cosmix mesh, just for hosting websites — the brand is **webmix**. The analogy is
FrankenPHP (server + language runtime as one distribution, no assembly required):

- A `~/.webmix` meta-repo, mirroring how `$COSMIX` orchestrates the public siblings
  without being depended on — but scoped to *just* the webd+mix pair.
- Ships a **single daemon that "does everything web"** (SSR, routing, static, media,
  JMAP proxy, Datastar transport) with the Mix evaluator embedded.
- Comes with a **curated set of ready-to-run web apps** (CMS, PIM, …) as the default
  mega-app — the anti-WordPress: no plugin/theme zoo, one maintained, coherent set.
- Add **only `cosmix-noded`** and the daemon takes on ABP mesh characteristics
  automatically.

## Feasibility — the merge is mostly already done

This is the strong part. `cosmix-webd/Cargo.toml` already depends on `cosmix-lib-mix`
(full features: json/crypto/url/markdown/datetime/datastar) **plus** `cosmix-lib-bus`
and `cosmix-lib-client`. webd executes the SSR CMS/PIM suite as **Mix run in-process**
(the trusted in-proc half of the maild↔webd trust split). So:

- **"A single daemon that does everything web" is the current webd**, not a future
  fusion. There is no two-binaries-to-glue problem — webd *is* webd+mix.
- The `mix` CLI (`cosmix-mix` binary) stays separate; it's an ops/admin tool. webmix
  may bundle it alongside, but the daemon already holds the evaluator.
- **ABP-via-noded already works the way the idea wants.** webd links the ABP client
  and embeds the Mix evaluator, whose ABP forms (`send`/`emit`) degrade to silent
  no-ops when no broker is present and become mesh-viable the instant one appears — no
  recompile (the auto-upgrade story; AGENTS.md §8). (The `mix --serve` *binary* gets
  this via `MixAmpHandler`'s runtime probe; webmix relies on the evaluator-level
  degradation, which holds wherever the evaluator is embedded.) So: ship webmix as
  `webmix` daemon **+** `cosmix-noded`, and ABP lights up with no recompile and no
  script-author conditionals. Keep noded a *separate* process — it's the broker other
  daemons attach to; folding it into webd would break that role.

### The genuinely new work (the real cost)

Not the merge — the **de-mesh-ification** for out-of-cosmix life:

1. **Single-host bootstrap config.** webd today assumes mesh-private inputs (real
   domains, `$ADDR` tables, SPEC-13 inventory). webmix needs a config story that
   stands alone with no mesh.
2. **Degenerate single-node noded.** A standalone mode without the mesh-trust /
   admission / inventory apparatus (SPEC-13), so a one-server install isn't dragging
   the whole mesh machine.
3. **Sanitization.** Same git-history-is-forever risk as the cos extraction. The
   shared vhost tree bakes real domains into `site.conf.mix`. A public webmix needs
   the RFC 5737 / `example.com` discipline applied to the curated-app templates —
   real work, not a packaging afterthought.
4. **Curated-app bundle + installer.** Package the shared-vhost CMS/PIM tree as a
   starter template; an install/upgrade path.
5. **Version-lockstep policy** between the bundled webd and mix.

## The open question (why this is C2, Mark's call)

The cosmix mandate frames the project as an **agent-operable substrate, AI-first**. A
web-hosting distro for *humans* is the web cousin of "normal-desktop drift." It can be
reconciled — a minimal, legible, reconstructible, agent-operable web stack is arguably
the cleanest external demo of the three criteria — **but only if the curated apps stay
agent-operable** (Mix-scriptable, ABP-addressable), not if webmix quietly becomes "a
nicer WordPress." That reconciliation is a direction call, not an information lookup,
so it stays open until Mark makes it.

- **Pick webmix** if: an external, legible proof-surface for the substrate thesis is
  worth the maintenance surface, and the curated apps are held to agent-operability.
- **Skip / defer** if: it splits focus from the substrate before webd/mix are stable,
  or risks becoming a human-first product that drifts from the AI-first thesis.

Reversibility: starting a `~/.webmix` meta-repo is cheap and `git revert`-able; a
*public release* with real users is a one-way door (support expectations, API
stability). Don't cross that until the standalone work above is done and the direction
call is explicitly yes.

## Prerequisites before this becomes a `_plan/`

- webd and mix near 1.0 (both churn weekly today — webd 0.8.x, mix 0.18.1).
- The standalone config + single-node noded modes scoped.
- The "should we" call made (C2).

## Pointers

- `_decisions/2026-06-04-maild-webd-trust-split.md` — why Mix runs in-proc in webd (the merge
  is already here).
- `project_webd_shared_vhost_tree` (memory) — the shared `/opt/cosmix/vhosts/shared`
  tree + per-vhost `site.conf.mix` = the curated-app seed.
- `project_datastar_webd_foundation`, `project_ssr_pim_suite`,
  `project_ssr_datatable_widget_plan` (memory) — what the default mega-app already is.
- `_decisions/2026-04-27-core-and-citizen-crate-pattern.md` — the core/citizen split keeps
  *library* crates mesh-free-testable; note the `cosmix-mix` **binary's** bare/mesh
  split is now a **runtime** probe (`MixAmpHandler`), not a build feature — webmix
  inherits ABP at runtime via noded, not via a compile flag.
