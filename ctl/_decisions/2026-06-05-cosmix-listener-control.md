---
title: Listener Control — webd kill switch + runtime guard (SPEC-12 webd.listeners)
date: 2026-06-05
status: directional — draft, not yet bound in CLAUDE.md
next_review: 2026-09-05
draws_from:
  - "CLAUDE.md"
  - "_spec/2026-05-11-12-property-substrate.md"
  - "_decisions/2026-05-20-substrate-first-service-pattern.md"
  - "_plan/2026-05-20-cosmix-cross-mesh-authz.md"
  - "~/.claude/plans/proud-snuggling-barto.md (P3 plan)"
tags: ["architecture", "amp", "spec-12", "webd", "tls", "security", "decision-record"]
---

# Listener Control — webd kill switch + runtime guard

The per-interface listener work (P1–P2) gave webd a
`cosmix_daemon::listen::ListenerSet` — one listener bound per explicit
interface, isolated at the socket layer. P3 adds **runtime control**: an
operator can cut external traffic on demand (durably) and tune a public
listener's guards live, all through the substrate. This record captures the
decisions that diverge from the existing namespace precedents.

The implementation: a new SPEC-12 `webd.listeners` property namespace (keyed by
listener id), an operator-tier write AuthPolicy, an L0-seed/L1-authoritative
bootstrap, a runtime reaction loop that drives the live `ListenerSetControl`,
and `webd.listener.{enable,disable,status}` ergonomic verbs (+ the
`$COSMIX/docs/_bin/webd_listener.mix` operator script). Guard hot-swap is a new
`ListenerSetControl::swap_guard` in `cosmix-lib-daemon` (0.3.0).

## Decisions

### 1. The substrate is the system of record; a reaction loop executes

`webd.listeners[id].enabled` (the kill switch) and the guard fields are L1
state. An operator flips them via `props.set` (or the ergonomic verbs, which
are thin `props.set` wrappers — they do **not** call `ListenerSetControl`
directly). A daemon-lifetime reaction loop consumes the namespace's change
events, re-reads the **current** row, and reconciles the live listener set to
match (`enable`/`disable`/`swap_guard`), then writes the observed
`bound`/`bound_addr`/`active_conns`/`last_transition`/`last_error` back into the
row's daemon-owned columns via `WriteOrigin::backend()`.

Driving off *current row state* (not the event payload) makes a dropped or
duplicated event self-healing; the startup pass reconciles every row once,
recovering any change missed while the daemon was down. This is the
substrate-first pattern (`2026-05-20-substrate-first-service-pattern.md`): L1 is truth, L2
composes verbs, the daemon reconciles.

### 2. Operator-tier write AuthPolicy — a deliberate divergence

`vhosts` and `maild.accounts` grant every WG `/24` peer the full capability set
(the permissive Phase-1 posture). `webd.listeners` does **not**: `props.read` /
`describe` go to every peer (status is benign), but `props.write` — the cap that
flips `enabled` and so can **cut external traffic, a DoS primitive** — is
granted only to a sender whose ABP name (`cmd.from` → `PeerIdentity.service_name`,
the only identity noded surfaces today) is in an L0 allowlist:

```
[webd.listeners]
operators = ["..."]    # node.conf.mix
```

Empty/unset (the default) ⇒ **no remote peer may write** (the daemon's own
backend-origin writes — bootstrap + the reaction loop — are unaffected; they
don't flow through `AuthPolicy`). The capability name (`props.write:webd.listeners`)
is stable now and the gate tightens automatically when the cross-mesh principal
model lands (`cosmix-cross-mesh-authz.md`) — same "narrow later without renaming
caps" rule the other namespaces follow.

**Operating the kill switch therefore requires naming the operator's ABP sender
identity in `operators`.** This is a pragmatic gate keyed on the sender name
noded forwards; it is not yet a cryptographic principal. The
`webd_listener.mix` script surfaces an `auth_denied` body plainly when the
caller isn't listed.

### 3. L0 seeds, L1 wins (the reboot footgun)

`[[webd.listener]]` config rows seed the namespace **upsert-if-absent**: config
sets a listener's `enabled` (+ the daemon-owned `external` flag) **once**;
thereafter L1 is authoritative. A listener an operator killed (`enabled=false`)
in a prior session **stays killed across `systemctl restart`** — the bootstrap
does not overwrite an existing row's caller-owned fields, and the `ListenerSet`
reads each listener's `enabled` + guard from the **namespace snapshot**, not raw
config, when binding at startup. (This is the opposite of `vhosts_bootstrap`,
which re-asserts config every restart — vhost config is authoritative; listener
kill state is operator-authoritative.)

Rejected: L0-always-overwrites, which would silently undo a kill on reboot.

### 4. The lockout guard — you can't kill the mesh control path

The namespace's `before_set` hook **refuses `enabled=false` on a non-`external`
listener**. The kill switch governs external (public-facing) listeners only; an
internal WG/mesh listener is the channel an operator uses to *re-enable* a
killed listener over the mesh, so disabling it would lock them out. `external`
is daemon-owned (seeded at bootstrap, caller-immutable), so a caller cannot flip
it to bypass the guard. Re-enabling is always allowed.

### 5. Drain semantics

`disable` drops the listener's accept sockets (the kernel frees the port
immediately) and waits up to a drain deadline (5s) for in-flight connections to
finish before abandoning stragglers to complete in the background. No forced
abort — a clean cut for new traffic, a graceful tail for existing connections.

### 6. Guard hot-swap mechanism

`GuardPolicy` (rate limit / conn caps / strict-SNI / CIDR ACL) lives behind an
`arc_swap::ArcSwap` inside the listener's shared `Guards`. `swap_guard` stores a
new policy wait-free; the accept loop's hot path takes one `load()` snapshot per
connection. The **live counters survive a swap** (active-conn count, per-IP
counts, rate buckets are separate fields), so tightening `max_conns` doesn't
zero the active count and raising a rate limit doesn't refill buckets. Per-IP
counts are tracked for every admitted connection (enforced only when a cap is
set) so a swap that *introduces* a per-IP cap counts pre-existing connections.

`strict_sni` is realised at the TLS resolver (rejecting no-SNI / unknown-SNI
handshakes); it is a no-op on a plain listener. It is seeded from the row at
listener build; a runtime `strict_sni` flip applies on the next ACME renewal
(the resolver rebuild path) rather than instantly — documented limitation.

### 7. nftables is secondary and illegible

A documented nftables drop on the external interface is the belt-and-suspenders
"the daemon is wedged, cut the port *now*" escape hatch. It is **explicitly
secondary** — the substrate `enabled` flag is the system of record and the
legible control surface; nftables state is invisible to the substrate and must
be reconciled by hand. Reach for it only when the daemon can't act.

## Scope notes

- **Runtime `vhost.add` on a multi-listener node** has no interface assignment
  until config names the host in a listener's allowlist — an accepted
  limitation of the explicit-partition mode (the implicit single-listener case
  serves every vhost, so it is unaffected).
- **Formal `_spec/12` registration:** the spec uses `maild.accounts` as its
  single worked example and does not enumerate every namespace (neither `vhosts`
  nor `handlers` is listed there); `webd.listeners` follows that precedent —
  its authoritative shape is the schema in `listeners_namespace.rs` + this
  record. A spec amendment enumerating all webd namespaces is a separate
  follow-up.

## Validation

See the P3 plan's verification section: a two-listener alpha config, toggle
the LAN listener off/on and confirm `ss` + `bound`, reboot-survives-kill (L1
durability), the lockout rejection on the WG listener, and operator-vs-non-operator
write authorization.
