---
title: Managed Daemon Identity Profile — Retained Contract
chapter: 10a
version: 0.1.1
status: draft
date: 2026-09-05
---

# Managed Daemon Identity Profile — Retained Contract

**DAEMON-PROFILE-001 — Retained identity contract.** The following numbered sections preserve the managed fixed-identity profile, including allocation rules, filesystem ownership, sysusers projection, system/user-unit distinctions, hardening directives, fail-closed verification, conformance lints and the registry. Original section numbers are local to this profile; `SPEC 10` references name the legacy identity contract, not a new distribution ID.

This is intended normative detail retained during refactoring, not a fresh attestation of deployed accounts, unit effectiveness or generator availability. Dated statements such as “present”, “shipped”, “verified”, version frontiers and rollout coverage remain historical assertions until supported by a current conformance record. The main [daemon chapter](10-daemon-agent-operation.md) explains the implementation boundary. No existing allocation is changed by publication.

Examples and verification pseudocode are descriptive legacy material, not scripts to execute. Production paths use `/opt/cosmix/bin`; where a legacy example conflicts with that convention, the discrepancy is retained for explicit correction rather than silently authorising an alternate install. Cross-references to other legacy chapter numbers resolve through the migration map. The distribution surveys and change history are retained in the private historical snapshot, not republished as current platform facts.

## 1. Introduction

**Registry drift — do not allocate or regenerate from this historical table.**
The retained registry is version 1.4.4 and lacks the normative 1.4.5/1.4.6
amendments. The [committed sysusers projection](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/_etc/sysusers/cosmix.conf)
labels itself 1.4.6 and assigns 519 to powerd and 520 to mprisd. Appendix A's
“next free 519” comments are historical, not allocation authority. Recover and
reconcile the registry amendments before generating accounts or passing a registry
equality gate; this refactor neither frees those IDs nor ratifies new allocations.

**Retained observability obligations (legacy Appendix D 1.4.0/1.4.1).**
The six observability identities require identity-profile L2 from first install,
including non-ABP-registering upstream binaries. Mask package-native units and
supervise the same binary through `cosmix-<d>.service` with its Cosmix user/group,
state directory and §5.2 hardening. A package-native POSIX user may remain but
must not appear in a Cosmix unit's `User=`/`Group=` or join `cosmix-tls` or any
future Cosmix shared-credential group. These units must declare
`After=cosmix-noded.service` under §5.4 despite not registering with the broker.
These are retained requirements, not fresh deployed-hardening evidence.

### 1.1 Purpose

This SPEC defines:

1. The numeric range Cosmix daemons occupy in the system UID/GID space.
2. The initial registry of daemon names to UID/GID assignments.
3. The rules by which the registry grows, changes, and tombstones entries.
4. The filesystem layout under which daemons own state.
5. The `sysusers.d` fragment that materializes the registry.
6. The systemd unit directives every daemon's service file MUST carry.
7. The install-time preflight that verifies identity before service start.
8. The conformance levels and CI lint shape that prevent drift.

### 1.2 Scope

This SPEC applies to every long-running Cosmix daemon that registers as a
service on the node-local Bus broker (`cosmix-noded`) or runs under
systemd as a system unit. It does
not apply to:

- **Session-scoped processes** (§7) — compositors, launchers, and other
  components bound to a logged-in human seat.
- **Build-time identities** — the UID under which `cargo build` runs is
  intentionally unspecified; see §1.3.
- **Per-tenant vhost identities** — NetServa NS 3.0 vhost users at
  UID ≥ 1000 are governed by NetServa, not by this SPEC.

Every daemon in scope is a **full Bus citizen by default**: it SHALL connect
to the node-local broker, register its service name, and expose its control
surface over ABP verbs — there is no "edge daemon, no Bus" exemption, and no
custom IPC/control-file/signal-based control surface may substitute for the
ABP surface. (Promoted 2026-07-23 from the all-daemons-amp-citizens decision;
rationale in the historical core/citizen architecture decision.)

### 1.3 Non-goals

- **A globally reserved UID range.** Cosmix does not own any portion of
  the POSIX system UID space by fiat. The 500–599 window is a *preferred*
  allocation on Cosmix-managed hosts; conflicts with non-Cosmix
  reservations are detected and resolved at install time, not assumed
  away.
- **A standard build identity.** A public clone of the Cosmix repository
  builds under whatever UID the operator chooses. This SPEC governs
  *deployed* identity, not *development* identity.
- **Universal hardening.** Per-daemon `systemd.exec(5)` hardening
  (`ProtectSystem=`, `RestrictAddressFamilies=`, `SystemCallFilter=`) is
  REQUIRED but daemon-specific; this SPEC mandates the directives every
  unit MUST carry, not the full hardening profile.

### 1.4 Terminology

| Term | Definition |
|------|------------|
| **registry** | The canonical mapping of daemon names to UID/GID values, defined in §2 and Appendix A. |
| **canonical source** | The Markdown table in this SPEC and Appendix A. The `sysusers.d` fragment is *generated* from this source (§4). |
| **daemon leaf** | A directory of the form `/etc/cosmix/<d>/`, `/var/lib/cosmix/<d>/`, etc., owned by `cosmix-<d>`. |
| **substrate-shared state** | Directories under `/var/lib/cosmix/` that are not daemon-leaves, owned by `root:root`, and accessed by daemons through explicit ABP contracts (§3.4). |
| **tombstone** | A registry entry whose UID/GID is permanently retired from allocation but kept in the spec for audit (§2.4). Applies to daemon-identity and shared-credential entries only; the citizen-identity stream uses the retire→quarantine→reclaim lifecycle (§2.5) instead of tombstoning. |
| **citizen-identity entry** | A registry entry (v1.2.0) with the *same shape* as a daemon-identity entry (POSIX `cosmix-<d>` user + same-numbered group, GID==UID, ABP service name = name minus `cosmix-`) but allocated in the dedicated **citizen band** and governed by the scoped, gated **reuse rule** (§2.3 R7/R8, §2.5) rather than the strict daemon no-reuse rule. It is the SPEC-10 identity of a *registered serve-mode Mix citizen* (SPEC 18 §2). |
| **citizen band** | The inclusive UID/GID window **600–699**, the citizen-identity stream's preferred fixed-ID window (§2.1). Disjoint from the 500–599 daemon-identity/shared-credential window. |
| **quarantine window** | The fixed interval (**30 days**, §2.3 R8) that MUST elapse after a citizen-identity entry is `retired:` before its UID is eligible to re-enter the free pool, in addition to (not instead of) purge-verification passing. |
| **purge-verification** | The automated, mesh-wide check (§2.3 R8) that a retired citizen UID owns no file, no enabled/started unit, and no leftover state-directory leaf on *any* node that ever projected it, and that the `cosmix-<d>` projection has been removed everywhere. It is R2.a's conjunctive check set, generalized and automated because the citizen class has a uniform machine-checkable footprint. |
| **preflight** | The install-time procedure (§6) that verifies every registry entry is either free or already correctly assigned, before any service is started. |
| **fail closed** | An install-time check that, on any deviation, halts installation rather than auto-correcting. The operator MUST resolve the conflict manually. |
| **session-scoped process** | A Cosmix process that runs under the logged-in human user via a user systemd unit, not as a system daemon (§7). |

---

## 2. UID/GID Registry

### 2.1 The preferred allocation window

Cosmix daemons are allocated UIDs and GIDs in the inclusive range
**500–599** on Cosmix-managed hosts. This range is the **preferred
fixed-ID window**, not a globally reserved range.

The window is a deployment-profile choice, not proof that these IDs are
available on a particular host. Check local account collisions at installation.
Historical deployment surveys and allocation rationale are not reproduced here.

UIDs in 500–599 that are not yet allocated to a Cosmix daemon are
*available for future Cosmix allocation*, subject to install-time
preflight (§6). They are not reserved against external packages. An
operator who installs an unrelated package that pins a UID in 500–599
SHALL resolve the conflict at install time (§6.1); the substrate does
not silently work around it.

The **citizen band** is the inclusive range **600–699** (100 slots),
introduced in v1.2.0 as the citizen-identity stream's preferred
fixed-ID window. It is **disjoint** from the 500–599 daemon-identity /
shared-credential window: no number is ever shared across the two
windows, which makes the daemon and citizen streams trivially
separable in `getent passwd | grep cosmix-` output (a UID ≥ 600 is a
citizen; a UID 500–599 is a daemon or a shared-credential group).
The citizen band carries the **same posture** as 500–599 — *preferred,
not globally reserved*: it is not owned by fiat, conflicts are
detected and resolved at install-time preflight (§6) and fail closed,
and an operator who installs an unrelated package pinning a UID in
600–699 SHALL resolve the conflict at install time. The 600–699
window was selected because Appendix C already records it as surveyed
clean ("600 | 600–699 | No known reservations | Clean"); a
confirmatory re-survey on 2026-05-16 (Appendix B.4) found no new
reservations. This satisfies the R5 obligation that a *new window*
rest on a fresh empirical survey rather than on the 500-anchor
survey alone. Allocation within the band is governed by R7/R8 (§2.3),
not by R1.

### 2.2 Initial registry

The initial allocation, as of this SPEC's publication, is given in
Appendix A. As a summary:

| UID/GID | POSIX name | ABP service | Role |
|--------:|------------|-------------|------|
| 500 | `cosmix-noded` | `noded` | Per-node ABP broker / mesh peer |
| 501 | `cosmix-maild` | `maild` | Mail daemon |
| 502 | `cosmix-webd` | `webd` | Web daemon |
| 503 | `cosmix-indexd` | `indexd` | Knowledge / vector index daemon |
| 504 | `cosmix-agentd` | `agentd` | Agent runtime |
| 505 | `cosmix-mcp` | `mcp` | MCP bridge |
| 506 | `cosmix-dnsd` | `dnsd` | Authoritative WG-mesh DNS daemon (R2.a-reclaimed cloudd slot) |
| 507 | `cosmix-cron` | `cron` | Scheduler |
| 508 | `cosmix-prometheus` | `prometheus` | Observability tier: Prometheus time-series store (v1.4.0) |
| 509 | `cosmix-grafana` | `grafana` | Observability tier: Grafana dashboards (v1.4.0) |
| 510 (GID-only) | `cosmix-tls` | — | Shared-credential group: TLS keypair read access (§3.3) |
| 516 (GID-only) | `cosmix-mesh` | — | Shared-credential group: signed-inventory read access (§3.3) (v1.4.2) |
| 511 | `cosmix-loki` | `loki` | Observability tier: Loki log store (v1.4.0) |
| 512 | `cosmix-alloy` | `alloy` | Observability tier: Grafana Alloy log shipper, per-node (v1.4.0) |
| 513 | `cosmix-pveexport` | `pveexport` | Observability tier: proxmox-exporter (Starttoaster, Go), PVE API scraper (v1.4.0) |
| 514 | `cosmix-nodeexport` | `nodeexport` | Observability tier: `node_exporter` (Prometheus, Go), per-host OS metrics (v1.4.1) |
| 515 | `cosmix-wgd` | `wgd` | WireGuard mesh control plane (SPEC-13 D0) (v1.4.2) |
| 517 | `cosmix-interactd` | `interact` | Reserved interaction namespace/props identity; desktop sink runs in-session (§7.3) (v1.4.3, explicit R6 override) |
| 518 | `cosmix-nspawnd` | `nspawnd` | nspawn host executor: generation-fenced CT lifecycle (nspawn cluster-lite C1) (v1.4.4) |
| 600 | `cosmix-statecache` | `statecache` | Citizen-identity: SPEC-18 reference citizen (§2.5) |

`cosmix-noded` registers under the ABP service name `noded`,
matching the binary name. The historical alias `hub` was
removed in the 2026-05-09 cosmix-noded rename — the substrate
no longer has a centralised "hub" role; every mesh node runs
its own `cosmix-noded` and peers as equals.

The registry has three entry classes:

- **Daemon-identity entries** (the `cosmix-<d>` rows in 500–599): pair
  a POSIX user with a same-numbered group. GID **MUST** equal UID for
  every daemon-identity entry. This is enforced by the CI lint defined
  in §8.2 and by `systemd-sysusers` when only a `u` line is given.
  Governed by the strict no-reuse rules R1–R6 (§2.3).
- **Shared-credential group entries** (the `cosmix-tls` row, and any
  future rows of the same shape): a POSIX group with **no associated
  user**. Shared-credential GIDs are allocated within the same
  500–599 window as a *separate append-only stream* from the
  daemon-identity stream, starting at GID 510 and continuing
  upward. The two streams SHALL NOT collide on a single number, but
  each advances independently — the daemon-identity stream's "next
  free UID" pointer is not bumped when a shared-credential entry is
  added, and vice versa. The GID==UID rule does not apply because
  there is no user. The 500–509 range is reserved for the
  daemon-identity stream so the two streams stay visually separable
  in `getent group | grep cosmix-` output (entries up to 509 are
  always daemons; entries from 510 may be either, distinguished by
  whether a same-numbered user exists). These entries exist to
  mediate credential read access shared by two or more daemons (TLS
  material, secret bundles) without coupling daemon identities to
  one another. They are **not** ABP services and have no ABP service
  name (the `bus` column is `—`).
- **Citizen-identity entries** (the `cosmix-<d>` rows in the 600–699
  citizen band, v1.2.0): pair a POSIX user with a same-numbered group,
  *exactly the same shape* as a daemon-identity entry — GID **MUST**
  equal UID, R6's two-name split applies (POSIX `cosmix-<d>` +
  prefix-stripped ABP service name), and R3's tier-neutral numbering
  applies. They differ from daemon-identity entries in **two** respects
  only: (1) they are allocated in the disjoint citizen band (§2.1),
  forming a *third separate append-only stream* whose "next free UID"
  pointer advances independently of the daemon-identity and
  shared-credential streams and never collides with either; and (2)
  their UID may be **reclaimed** under the scoped, gated reuse rule
  R7/R8 (§2.3) and the retire→quarantine→reclaim lifecycle (§2.5),
  *instead of* the strict daemon no-reuse/tombstone rule (R2, §2.4).
  A citizen-identity entry is the SPEC-10 identity of a *registered
  serve-mode Mix citizen* (SPEC 18 §2); the unregistered transient
  Mix-citizen form (SPEC 18 §2) consumes **no** registry slot and is
  out of scope for this registry. The reuse exception is confined to
  this class: it SHALL NOT be read as relaxing R2/R2.a for
  daemon-identity entries or the no-reuse posture of the
  shared-credential stream.

Shared-credential groups SHALL NOT be used to grant *write* access
across daemon identities; per §3.4 substrate-shared writable state is
mediated by the §3.4 mechanisms (ABP contract or supplemental write
group), not by general-purpose shared groups in this section.

### 2.3 Allocation rules

The following rules govern the registry over time. They are normative.
**R1–R6 govern the daemon-identity stream** (and, where stated,
shared-credential entries). **R7–R8 govern the citizen-identity stream
(§2.2, v1.2.0).** R3 (tier-neutral numbering) and R6 (the two-name
split) additionally bind citizen-identity entries by §2.2's
"same shape" clause; R1/R2/R2.a/R4/R5 do **not** apply to the citizen
stream, which has its own allocation and reuse rules (R7/R8). No rule
below relaxes any pre-v1.2.0 constraint on the daemon-identity or
shared-credential streams.

**R1.** New daemons SHALL be allocated the **next free UID** in the
500–599 daemon-identity window in append-only order. Existing
allocations SHALL NOT be reordered, renumbered, or compacted. The sole
exception is an R2.a-reclaimed UID, which is allowed to sit numerically
below the current append-only frontier (see R2.a below); such a UID
re-enters the free pool and is preferred over the next sequential UID
for the next eligible daemon allocation. R1 does not govern the
citizen band (600–699); see R7.

**R2.** UIDs SHALL NOT be reused, with one exception. When a daemon is
retired (renamed, merged into another daemon, or removed entirely), its
UID becomes a tombstone (§2.4) and SHALL NOT be assigned to any future
daemon.

*Exception (R2.a — never-functional reclamation).* A UID that was
allocated to a daemon which was retired without that daemon ever
becoming functional on any mesh node MAY be reclaimed: the allocation
is fully removed from the registry (no tombstone) and re-enters the
free pool, where per R1 it is preferred over the next sequential UID
for the next eligible daemon allocation (i.e. allowed out of
append-order).

The substantive precondition is **the absence of on-disk state owned
by the retired UID** — that is the ambiguity R2 exists to prevent.
A passwd/group projection alone (the sysusers fragment having created
the user on one or more nodes) does NOT block reclamation, but it
SHALL be cleaned up before the new fragment promotes (see remediation
below). The conditions are conjunctive — *all* of the following SHALL
hold for the operator to invoke R2.a:

- No file on any mesh node is owned by the retired UID or its group.
  This SHALL be verified by `sudo find / -uid <N> -o -gid <N>` on
  every node that ever ran `systemd-sysusers` against a fragment
  containing the retired entry. Empty output on every node is required.
- No systemd unit, ABP-registered service, on-disk daemon-state
  directory, or unit-managed `StateDirectory=`/`RuntimeDirectory=`
  ever ran in production under the retired UID. (Allocations that
  were never wired up to a unit, or whose unit was never enabled or
  started, satisfy this trivially.)
- Where a node has projected the retired `cosmix-<d>` user from a
  prior sysusers fragment, the operator SHALL `userdel cosmix-<d>`
  (which also removes the same-numbered group via `USERGROUPS_ENAB`,
  per Debian/Ubuntu default; `groupdel cosmix-<d>` is the explicit
  fallback) on every such node BEFORE promoting the new fragment.
  This is the remediation step that closes the gap between the
  registry and the live `getent passwd|group` state, restoring the
  invariant before file ownership has a chance to drift.
- The reclamation commit message records the R2.a invocation, names
  the retired daemon and its UID, and (where remediation was needed)
  lists the nodes on which `userdel` was run, so the audit trail
  survives even though the registry row does not.

The justification for R2.a is that the no-reuse invariant exists to
prevent file-ownership ambiguity after a `chown`-style sweep —
ambiguity that cannot arise when no host has files owned by the
retired UID. A sysusers projection that created the passwd entry but
never had any file, unit, or service materialise under it is *not*
the failure mode R2 guards against; the projection is removable with
`userdel` and leaves no residue. UIDs of daemons that *did* become
functional in production — that owned files, ran units, or registered
ABP services — SHALL continue to follow the strict tombstone rule.

**R3.** Tier or category (substrate, application, UI plumbing, etc.)
SHALL NOT influence the numeric allocation. Tier MAY appear as
metadata in the Markdown registry but SHALL NOT be encoded in the
numeric ordering. Architecture taxonomy is mutable; UID numbers are
not.

**R4.** When two daemons merge, the merged daemon SHOULD inherit the
UID of whichever predecessor owns more on-disk state, to avoid a
filesystem-wide `chown` sweep. The other predecessor's UID becomes
a tombstone.

**R5.** When the next free daemon-identity UID would exceed 599, the
SPEC SHALL be amended (per Constitution Article VII) to either (a)
extend the window or (b) move to a new window after a fresh empirical
survey of upstream UID reservations. The operator SHALL NOT silently
allocate beyond 599. (v1.2.0 invoked R5(b)'s "fresh empirical survey"
discipline to open the *separate* 600–699 citizen window — §2.1,
Appendix B.4 — even though the daemon stream was not exhausted; the
citizen band's own exhaustion is handled by R7, not R5.)

**R6.** Each daemon-identity entry has two distinct names. (Shared-credential
group entries per §2.2 have only the POSIX/systemd group name and no ABP
service name, because they are credential boundaries rather than process
identities.)

- The **POSIX/systemd name** SHALL begin with the prefix `cosmix-`
  and SHALL match the regex `^cosmix-[a-z][a-z0-9-]{1,30}$`. This
  name is used for `User=`, `Group=`, the systemd unit file
  (`cosmix-<d>.service`), the `sysusers.d` entry, and on-disk paths
  (`/etc/cosmix/<d>/`, `/var/lib/cosmix/<d>/`). The portion after
  the `cosmix-` prefix is the registry's `<d>` token.
- The **ABP service name** is the same `<d>` token *without* the
  `cosmix-` prefix (e.g. `noded`, `maild`, `webd`, `indexd`). It
  is the name the daemon registers with under its node-local
  `cosmix-noded` broker. The ABP service name SHALL match
  `^[a-z][a-z0-9-]{1,30}$` and SHALL be unique across the
  registry. Because a registered serve-mode Mix citizen registers
  on the same node-local broker as a daemon, ABP-name uniqueness is
  enforced across the **union** of non-tombstoned daemon-identity
  entries and non-retired/non-reclaimed citizen-identity entries: a
  citizen ABP name SHALL NOT collide with a daemon ABP name or with
  another live citizen ABP name (CI lint L14, §8.2).

Existing ABP service names that diverge from this derivation
(historical aliases, multi-service daemons) SHALL be recorded
explicitly in the registry's `bus:` field per entry; absence of an
`bus:` field means the ABP service name is the registry name minus
`cosmix-`.

**R7. (Citizen allocation — citizen-identity stream only.)** New
citizens SHALL be allocated a UID/GID in the 600–699 citizen band.
The allocation order is the **lowest free UID in the band**, where
the *free pool* is the union of (a) UIDs in 600–699 never allocated
to any citizen and (b) UIDs whose only remaining registry rows are
`reclaimed:` rows (R8). Append-only ordering does **not** bind
citizen allocation — a reclaimed UID re-enters the pool and may be
re-allocated below the current frontier. The safety mechanism is the
R8 reuse gate, **not** numeric monotonicity. Within *never-reclaimed*
citizen rows the canonical source SHALL remain append-only: a row
that has not been reclaimed SHALL NOT be reordered, renumbered, or
removed (it is `retired:` or live, never silently deleted). The
citizen stream's "next free UID" pointer advances independently of
the daemon-identity and shared-credential pointers and SHALL NOT
collide with either window (the bands are disjoint by §2.1, so the
non-collision is structural; the CI lint asserts it as
defence-in-depth).

**R8. (Citizen reuse gate — citizen-identity stream only.)** A
retired citizen UID `N` re-enters the free pool (R7) **if and only
if BOTH** of the following hold; the conditions are *conjunctive*
and neither alone is sufficient:

- **(a) Mesh-wide purge-verification passes.** On *every* mesh node
  that ever projected the entry (ran `systemd-sysusers` against a
  fragment containing `cosmix-<d>`), *all* of the following SHALL
  hold: `sudo find / -uid N -o -gid N` produces empty output; no
  enabled or started systemd unit references the retired identity;
  the per-name `StateDirectory=`/`RuntimeDirectory=`/`LogsDirectory=`
  leaves (`/var/lib/cosmix/<d>/`, `/run/cosmix/<d>/`,
  `/var/log/cosmix/<d>/`) are absent; the citizen's
  `/usr/local/lib/cosmix/<d>.mix` script is absent; and the
  `cosmix-<d>` projection has been `userdel`'d (group via
  `USERGROUPS_ENAB`, `groupdel` fallback). This is R2.a's conjunctive
  check set, **automated and generalized to functional citizens**.
- **(b) The 30-day quarantine window has elapsed.** At least 30 days
  SHALL have passed since the entry's `retired:` date before its UID
  is eligible, *even if (a) already passes*. The window covers
  eventually-consistent mesh state and nodes that were offline during
  the verification sweep.

When BOTH hold, the operator SHALL annotate the retired row with a
`reclaimed: <date>` attribute and a `verifier: <record>` token (the
per-node purge-verification run record), leaving the row in the
canonical source for audit (it materializes as a comment only — §4.3,
§2.5). The reclamation change message SHALL record the R8 invocation,
the citizen name and UID, the `retired:` and `reclaimed:` dates, and
the per-node verifier run records. R8 SHALL NOT auto-correct,
auto-allocate, or bypass either gate; a tooling-unavailable or
inconclusive purge-verification is a non-pass and the UID stays in
quarantine.

*Justification for R7/R8 (why scoped reuse is sound for citizens but
not daemons).* The no-reuse invariant (R2) exists to prevent
file-ownership ambiguity after a `chown`-style sweep. For the
daemon-identity class that ambiguity is unbounded — daemons have
heterogeneous, spec-specific on-disk footprints — so daemons stay
strictly no-reuse with only the narrow, **manual** R2.a escape for
never-functional allocations. The citizen-identity class is different
*in kind*: every registered serve-mode Mix citizen has the **same**
machine-enumerable footprint (exactly one
`/usr/local/lib/cosmix/<d>.mix`, exactly one per-name
`StateDirectory=cosmix/<d>` leaf, exactly one `cosmix-<d>.service`
unit — SPEC 18 §2, §9). "No file, unit, or state owned by `N` on any
node" is therefore an *automatable, exhaustive* predicate for citizens
in a way it is **not** for daemons, which is precisely why R8 may
generalize and automate R2.a for *functional* citizens whereas R2.a
must stay manual and never-functional-only for daemons. The 30-day
quarantine adds defence-in-depth against eventually-consistent and
offline-node state that a single sweep could miss. The pair
(automated exhaustive purge-verify) ∧ (quarantine) reconstructs
exactly the safety property R2 protects — which is why bounded scoped
reuse is sound for the citizen band specifically and is **not**
extended to the daemon-identity or shared-credential streams. This
also keeps the `506`/cloudd daemon-stream question orthogonal to the
citizen band: the SPEC-18 reference citizen is a citizen-band entry
and never touches the 500–599 stream. The `506`/cloudd question is
itself **resolved** by the v1.3.0 amendment — `cosmix-dnsd` consumes
the R2.a-reclaimed `506` slot as the next eligible daemon allocation
(R1's "preferred over the next sequential UID" rule for an
R2.a-reclaimed UID; Appendix A, Appendix D 1.3.0). No daemon-stream
gap remains; the append-only frontier is unchanged at 508 because the
R2.a slot sits *below* the frontier (R1).

### 2.4 Tombstones

Tombstoning applies to **daemon-identity and shared-credential
entries only**. Citizen-identity entries are *never* tombstoned;
their retirement is governed by the retire→quarantine→reclaim
lifecycle in §2.5 (the citizen-class analogue of a tombstone is a
permanently-`retired:`, never-`reclaimed:` row).

A tombstoned entry remains in the registry with a `tombstoned: <date>`
attribute and a `reason:` field. Tombstones are visible in the
generated `sysusers.d` fragment as comments only — they SHALL NOT
materialize as system users on installed hosts.

The CI lint (§8.2) SHALL reject any change that:

- removes a tombstone entry from the canonical source,
- assigns a tombstoned UID to a new daemon, or
- silently re-uses a tombstoned name without an explicit `successor:`
  pointer.

### 2.5 Citizen retirement, quarantine, and reclamation

A citizen-identity entry (§2.2) moves through up to three lifecycle
states. The state is carried as an attribute on the canonical
Appendix A row and determines how the entry projects into the
generated `sysusers.d` fragment (§4.3):

| State | Registry attributes | sysusers projection | UID reusable? |
|-------|---------------------|---------------------|---------------|
| **live** (default) | none (no `retired:`/`reclaimed:`) | `u cosmix-<d> <uid> …` (materializes) | No — it is in use |
| **retired** | `retired: <date>` | comment `# quarantine: …` (does **not** materialize) | No — in quarantine (R8) |
| **reclaimed** | `retired: <date>` + `reclaimed: <date>` + `verifier: <record>` | comment `# reclaimed: …` (does **not** materialize) | Yes — back in the free pool (R7) |

Transitions:

1. **live → retired.** The operator decommissions the citizen
   (removes its unit, stops/disables it, deletes its
   `/usr/local/lib/cosmix/<d>.mix`). The Appendix A row gains
   `retired: <date>`; the row is **kept** (never deleted — R7) and
   stops materializing as a `u` line. The 30-day quarantine clock
   (R8(b)) starts at `<date>`.
2. **retired → reclaimed.** Only when **both** R8 gates pass
   (automated mesh-wide purge-verification *and* the 30-day window).
   The operator adds `reclaimed: <date>` and `verifier: <record>`;
   the UID re-enters the citizen free pool (R7). The row remains in
   Appendix A as an audit record (comment-only projection).
3. **(re-)allocation.** A reclaimed UID may be assigned to a *new*
   citizen by appending a **new** live row with the same UID and a
   different name. The prior `reclaimed:` row(s) for that UID are
   retained for audit. The CI lint (§8.2 L5) enforces that **at most
   one non-reclaimed row** (live or retired) exists per citizen UID
   at any time, and that every `reclaimed:` row carries both a
   `reclaimed:` date and a `verifier:` token.

A row that is `retired:` but has never satisfied R8 stays in
quarantine indefinitely; it is the citizen-class equivalent of a
tombstone and SHALL NOT be reclaimed until both gates pass. There is
no "never-functional" citizen short-cut analogous to R2.a — the
uniform-footprint purge-verification (R8(a)) already subsumes the
never-functional case (a never-started citizen trivially passes the
`find`/unit/state checks), and the quarantine window still applies.

---

## 3. Filesystem Layout

### 3.1 Directory hierarchy

Every Cosmix daemon's on-disk presence SHALL conform to the following
hierarchy. The structure mirrors across all five trees: each daemon
owns a leaf named `<d>/` corresponding to its registry name minus the
`cosmix-` prefix.

| Path | Purpose | Created by | Owner / mode |
|------|---------|------------|--------------|
| `/etc/cosmix/` | Configuration root | Cosmix package | `root:root 0755` |
| `/etc/cosmix/<d>/` | Per-daemon configuration | Cosmix package | `root:root 0755` |
| `/etc/cosmix/<d>/config.toml` | Daemon configuration file | Cosmix package | see §3.3 |
| `/var/lib/cosmix/` | State root | systemd-sysusers / package | `root:root 0755` |
| `/var/lib/cosmix/<d>/` | Per-daemon writable state | `StateDirectory=cosmix/<d>` | `cosmix-<d>:cosmix-<d> 0750` |
| `/var/lib/cosmix/<shared>/` | Substrate-shared state (§3.4) | Cosmix package | `root:root 0755` |
| `/run/cosmix/<d>/` | Per-daemon ephemeral runtime | `RuntimeDirectory=cosmix/<d>` | `cosmix-<d>:cosmix-<d> 0750` |
| `/var/cache/cosmix/<d>/` | Per-daemon cache (OPTIONAL) | `CacheDirectory=cosmix/<d>` | `cosmix-<d>:cosmix-<d> 0750` |
| `/usr/share/cosmix/<d>/` | Per-daemon read-only data | Cosmix package | `root:root 0755` |
| `/usr/share/cosmix/spec/` | Substrate-wide read-only data (specs, schemas) | Cosmix package | `root:root 0755` |
| `/var/log/cosmix/<d>/` | Per-daemon file logs (CONDITIONAL — see §3.5) | `LogsDirectory=cosmix/<d>` | `cosmix-<d>:cosmix-<d> 0750` |

Unit files SHALL use the **nested** form (`StateDirectory=cosmix/<d>`),
not the flat form (`StateDirectory=cosmix-<d>`). The flat form is
forbidden because it produces sibling directories that defeat the
unified-parent property.

Unit files SHALL NOT mix nested-form (`StateDirectory=cosmix/<d>`) with
parent-form (`StateDirectory=cosmix`) across the daemon set. The parent
`cosmix/` is owned by `root:root`; if any unit declares `StateDirectory=cosmix`,
ownership of the parent collapses to that daemon and other daemons lose
traversal guarantees.

### 3.2 Ownership matrix

The default ownership and mode for each directory class:

| Class | Owner | Group | Mode | Daemon access |
|-------|-------|-------|------|----------------|
| Tree roots (`/etc/cosmix/`, `/var/lib/cosmix/`, `/run/cosmix/`, `/usr/share/cosmix/`) | `root` | `root` | `0755` | Traverse only |
| Daemon leaf (state, runtime, cache, logs) | `cosmix-<d>` | `cosmix-<d>` | `0750` | Read/write |
| Daemon leaf (config, share) | `root` | `root` or `cosmix-<d>` | `0755` or `0750` (see §3.3) | Read only |
| Substrate-shared state (§3.4) | `root` | `root` | `0755` | Traverse + explicit ABP-contract write only |

### 3.3 Configuration ownership

Daemons SHALL NOT own their own configuration files. Configuration is
owned by the package or the operator, not by the daemon process. The
ownership rules are:

- **Non-secret configuration:** `root:root 0644` for the file,
  `root:root 0755` for the containing directory. This is the case
  satisfied by `ConfigurationDirectory=cosmix/<d>` alone, because
  `systemd.exec(5)` documents that `ConfigurationDirectory=` does
  *not* chown the directory to the daemon's `User=`/`Group=` (unlike
  `StateDirectory=`). The `0755` mode allows world-traverse to the
  config file, which the daemon reads as a regular user.
- **Configuration containing secrets** (API tokens, TLS keys,
  database passwords, ABP credentials): `root:cosmix-<d> 0640` for
  the file, `root:cosmix-<d> 0750` for the containing directory.
  Because `ConfigurationDirectory=` does not set group ownership,
  the secret-config directory SHALL be created by the package
  installer or by a `tmpfiles.d` fragment with the correct
  `root:cosmix-<d> 0750` ownership *before* the unit starts, and
  the unit SHALL declare `ConfigurationDirectoryMode=0750`. The
  registry user (§2) SHALL exist before this directory is
  materialised: the installer (or boot sequence) SHALL run
  `systemd-sysusers` before `systemd-tmpfiles --create` invokes the
  Cosmix tmpfiles fragment, so that the `cosmix-<d>` group exists
  when the tmpfiles fragment chowns the directory. This is an
  installer/boot ordering requirement, not a property of the
  tmpfiles fragment itself.
- **TLS keypairs read by more than one daemon** (e.g. a host
  certificate consumed by both `cosmix-maild` and `cosmix-webd`):
  `root:cosmix-tls 0640` for the private key. The `cosmix-tls`
  shared-credential group (§2.2) SHALL be the access mechanism;
  per-host POSIX ACLs (`setfacl u:cosmix-<d>:r …`) SHALL NOT be used
  for this case. Each consuming daemon's `cosmix-<d>` user SHALL be
  added to `cosmix-tls` via the canonical sysusers fragment so that
  the membership is reproducible and auditable rather than a
  per-host operator action. The certificate (public material) MAY
  remain `root:root 0644`. This pattern keeps the daemons mutually
  isolated — neither daemon's primary group grants the other any
  access — while avoiding both the per-host ACL workaround and the
  `acl` package dependency on hosts (e.g. Debian 13) that do not
  ship `setfacl` by default.
- **Daemon-writable runtime state derived from config** (e.g. a
  generated cache of resolved DNS names) SHALL be written under
  `/var/lib/cosmix/<d>/` or `/var/cache/cosmix/<d>/`, never back into
  `/etc/cosmix/<d>/`.

A daemon process SHALL NOT have write access to `/etc/cosmix/<d>/`.
This is enforced by `ReadOnlyPaths=/etc/cosmix` (or the equivalent
implicit guarantee from `ProtectSystem=strict` plus a
`ReadWritePaths=` allow-list that excludes `/etc/cosmix`).

### 3.4 Substrate-shared state

Some state is owned by the substrate as a whole, not by any individual
daemon. Examples (each governed by its own SPEC or doc):

- `/var/lib/cosmix/registry/` — service-name registry (planned)
- `/var/lib/cosmix/topology/` — mesh topology snapshot (planned)
- `/var/lib/cosmix/spec/` — SPEC delivery cache (per SPEC 07)

These directories are owned by `root:root 0755`. Daemons SHALL access
them only through one of:

1. **Read-only traversal**, with no writes;
2. **An explicit ABP contract** that delegates writes to a designated
   substrate service (e.g. `cosmix-noded` may write to
   `/var/lib/cosmix/registry/` because the registry is part of its
   contract); or
3. **A supplemental group** that grants the specific daemon write
   access, with the supplemental group declared in this SPEC's
   amendment record.

Daemons SHALL NOT use file ownership of substrate-shared directories
to coordinate. Cross-daemon coordination is ABP, not filesystem.

### 3.5 Logging

`/var/log/cosmix/<d>/` SHALL exist only when a daemon emits log files
directly. Daemons that log via `journald` (the default) SHALL NOT
declare `LogsDirectory=`. The `/var/log/cosmix/` parent SHALL NOT be
created unless at least one daemon requires it, to avoid empty-tree
litter.

### 3.6 Backup, restore, and image-transfer boundary

The 500–599 window is **preferred**, not globally reserved (§2.1).
Numeric ownership of files in `/var/lib/cosmix/` is therefore only
meaningful on a host that has passed §6.2 verification. The
following SHALL hold:

- Backup, restore, snapshot, replication, container-image, and
  rsync-style transfer tooling SHALL NOT interpret numeric UID/GID
  500–599 ownership as "Cosmix-owned" on a target host before that
  host has passed §6.2 verification.
- A restore that places `/var/lib/cosmix/` content onto a host
  failing §6.2 SHALL be reported as a fail-closed condition; the
  restore tool SHALL NOT chown the tree to satisfy the registry.
- Bind-mounting `/var/lib/cosmix/<d>/` from a host into a container
  is permitted only when the container's registry projection
  (sysusers fragment baked into the container image) matches the
  host's. Mismatched projections SHALL be treated as a §6.2 failure
  inside the container. When the container uses user-namespace
  remapping (e.g. systemd-nspawn with a private user namespace, or
  rootless Podman), the
  comparison SHALL be made against the *effective* UIDs/GIDs
  visible inside the container's mount namespace — i.e. the
  remapped IDs, not the host-side IDs. A registry user whose
  remapped ID falls outside the 500–599 window inside the
  container is a fail-closed condition.

This boundary protects against a class of cross-host confused-deputy
attacks where files restored from a backup of a Cosmix host end up
owned by an unrelated UID 500–599 on the destination host.

---

## 4. sysusers.d Derivation

### 4.1 Canonical source

The Markdown table in Appendix A is the **single canonical source** for
the registry. The `/usr/lib/sysusers.d/cosmix.conf` fragment is a
**generated artifact**.

When the canonical Markdown and the generated fragment disagree, the
canonical Markdown is authoritative. CI (§8.2) SHALL reject any commit
in which they disagree.

### 4.2 Generated artifact


The generated fragment SHALL be installed at
`/usr/lib/sysusers.d/cosmix.conf`. It SHALL be machine-applied via
`systemd-sysusers` at install time (§6) and at every package update.

### 4.3 Syntax

Each registry entry SHALL produce a `sysusers.d` line of the form:

```
u <name> <uid> "<gecos>" /nonexistent /usr/sbin/nologin
```

- The `u` directive (lowercase `u`, no `!`) creates a system user.
  Cosmix does not use the `u!` form (which marks the user as
  system-only and prevents login by some tools' heuristics) because
  `nologin` shell already prevents interactive use, and `u!` adds a
  non-uniformity that complicates automation.
- The home directory SHALL be `/nonexistent`. Daemons own state under
  `/var/lib/cosmix/<d>/`, not under a traditional home.
- The shell SHALL be `/usr/sbin/nologin`. (`/sbin/nologin` is also
  acceptable on systems that use the older path; the generator SHALL
  pick the path that exists on the target host.)
- The GECOS field SHALL be the descriptive role string from the
  registry, double-quoted.
- The matching group SHALL be created implicitly by `systemd-sysusers`
  when only a `u` line is given. No separate `g` line is required;
  one MAY be added if a future daemon needs supplemental group
  membership beyond its primary group.

Tombstoned entries SHALL appear in the generated fragment as comments
of the form:

```
# tombstone: <name> <uid> retired <date> — <reason>
```

**Citizen-identity entries** project per their §2.5 lifecycle state:

- A **live** citizen row produces a normal `u` line, *identical in
  shape* to a daemon-identity `u` line (no `:gid` suffix; same
  `/nonexistent` home and `nologin` shell):

  ```
  u <name> <uid> "<gecos>" /nonexistent /usr/sbin/nologin
  ```

- A **retired** (in-quarantine) citizen row SHALL NOT materialize a
  `u` line; it appears as a comment of the form:

  ```
  # quarantine: <name> <uid> retired <date> — eligible <date+30d> (SPEC-10 R8)
  ```

- A **reclaimed** citizen row SHALL NOT materialize a `u` line; it
  appears as a comment of the form:

  ```
  # reclaimed: <name> <uid> retired <date> reclaimed <date> verifier <record>
  ```

Citizen `u` lines are emitted in their own sub-block, after the
daemon-identity `u` lines and the shared-credential `g`/`m` lines, so
the three streams stay visually separable in the fragment (§9.1).

---

## 5. systemd Unit Requirements

### 5.1 Required directives

#### 5.1.1 System-service daemons

Every system-service Cosmix daemon's `cosmix-<d>.service` unit file SHALL
include the following directives in `[Service]`:

| Directive | Value | Purpose |
|-----------|-------|---------|
| `User=` | `cosmix-<d>` | Match registry |
| `Group=` | `cosmix-<d>` | Match registry |
| `StateDirectory=` | `cosmix/<d>` | Materialize `/var/lib/cosmix/<d>/` |
| `RuntimeDirectory=` | `cosmix/<d>` | Materialize `/run/cosmix/<d>/` |
| `ConfigurationDirectory=` | `cosmix/<d>` | Materialize `/etc/cosmix/<d>/` (read-only at runtime) |
| `ProtectSystem=` | `strict` | Read-only `/usr`, `/boot`, `/etc` |
| `ProtectHome=` | `true` | No `/home`, `/root`, `/run/user` access |
| `PrivateTmp=` | `true` | Per-daemon `/tmp` namespace |
| `NoNewPrivileges=` | `true` | Block `setuid`/`setgid` escalation |

#### 5.1.2 Registered systemd-user daemons

A registry entry MAY be classified by an amendment as a **registered
systemd-user daemon** when its live service must inherit resources belonging to
the logged-in session and cannot truthfully run under its reserved POSIX row.
Version 1.4.3 classifies only `cosmix-interactd` this way (§7.3). This is a
closed class: placing a unit under the user manager does not implicitly earn
the exception.

A registered systemd-user daemon's unit SHALL:

- be installed for the user manager and run as the logged-in user;
- omit `User=`, `Group=`, `DynamicUser=`, `StateDirectory=`,
  `RuntimeDirectory=`, and `ConfigurationDirectory=`;
- declare `After=graphical-session.target` and
  `PartOf=graphical-session.target` when it consumes the session D-Bus;
- use an absolute `/opt/cosmix/bin/cosmix-<d>` `ExecStart=` and declare
  `Restart=on-failure`; and
- rely on the daemon's broker reconnect loop instead of declaring
  `After=`/`Requires=cosmix-noded.service`, because system and user managers
  do not share a dependency graph.

Session-bus access under `/run/user/<uid>/bus` is explicitly permitted for this
class. The reserved SPEC-10 POSIX row remains sysusers/preflight and future
namespace-ownership material; it is not asserted as the live process UID/GID.

### 5.2 Hardening expectations

This subsection defines the **Level 2 mandatory hardening set** for
system-service daemons. At conformance Level 2 (§8.1), every such daemon's `[Service]`
section SHALL include each of the following directives, in the
**canonical hardening order** given by the row order of the table
below, **with the exact value shown** — except for the four
directives where a named alternative is permitted:

- `RestrictAddressFamilies=` MAY be a strict subset of
  `AF_UNIX AF_INET AF_INET6` (i.e. the daemon MAY drop families it
  does not need; it MUST NOT add others).
- `SystemCallFilter=` MAY add daemon-specific deny filters
  (`SystemCallFilter=~@<group>`), but the base allow filter SHALL
  remain `@system-service`.
- `CapabilityBoundingSet=` MAY be either empty or a non-empty
  `CAP_*` allow-list. A non-empty value SHALL list only `CAP_*`
  tokens (no minus operators, no `~` complement form).
- `AmbientCapabilities=` MAY be either empty or a non-empty
  `CAP_*` allow-list. A non-empty value SHALL be a subset of
  `CapabilityBoundingSet=` (per `systemd.exec(5)` semantics) and
  SHALL list only `CAP_*` tokens.

Any other deviation is permitted only via an in-line deviation
comment placed in canonical order at the position the directive
would otherwise occupy. The comment SHALL match the exact form

```
# §5.2 deviation: <Directive>= — <reason>
```

where `<Directive>=` is the directive name with a single trailing
`=` (e.g. `MemoryDenyWriteExecute=`). Documented deviations are
permitted at Level 2; silent omissions and value drift are not.

**Boolean normalisation.** Boolean values in this table use the
canonical `true` / `false` form. Implementations and the CI lint
SHALL treat the systemd-equivalent spellings (`yes`/`on`/`1`,
`no`/`off`/`0`, per `systemd.syntax(5)`) as identical for §5.2
conformance purposes. The §9.2 example uses `yes`/`no` to match
common upstream systemd unit conventions.

| Directive | Level 2 value |
|-----------|---------------|
| `RestrictAddressFamilies=` | `AF_UNIX AF_INET AF_INET6` (or a strict subset) |
| `RestrictNamespaces=` | `true` |
| `RestrictRealtime=` | `true` |
| `RestrictSUIDSGID=` | `true` |
| `LockPersonality=` | `true` |
| `MemoryDenyWriteExecute=` | `true` |
| `SystemCallArchitectures=` | `native` |
| `SystemCallFilter=` | `@system-service` plus daemon-specific add/deny lists |
| `ProtectKernelTunables=` | `true` |
| `ProtectKernelModules=` | `true` |
| `ProtectKernelLogs=` | `true` |
| `ProtectControlGroups=` | `true` |
| `ProtectProc=` | `invisible` |
| `ProcSubset=` | `pid` |
| `CapabilityBoundingSet=` | empty, or an explicit `CAP_*` allow-list |
| `AmbientCapabilities=` | empty, or an explicit `CAP_*` allow-list |
| `UMask=` | `0027` |

CI lint L13 (§8.2) is the enforcement contract for this table.

A daemon's unit file SHOULD declare `ReadWritePaths=` only when the
state directory machinery (`StateDirectory=`, `RuntimeDirectory=`)
is insufficient. Adding a path here is a privilege escalation
relative to the default; it SHALL carry a comment explaining the
need.

### 5.3 Forbidden directives

The following directives SHALL NOT appear in any Cosmix daemon's
service unit:

- `DynamicUser=` — incompatible with the fixed registry. Conflicts
  with the append-only and never-reused invariants and produces UIDs
  in 61184–65519 that are not under registry control.
- `User=root` — Cosmix daemons do not run as root. A daemon that
  legitimately requires root capability (e.g. binding port < 1024)
  SHALL use `AmbientCapabilities=` and the relevant `CAP_*` token
  rather than running as root.
- `User=` matching a UID outside the registry — every Cosmix daemon
  SHALL run as its registered identity.

### 5.4 Unit ordering and dependencies

System-service `cosmix-<d>.service` units SHALL declare:

- `After=network-online.target` (for daemons requiring network)
- `Wants=network-online.target` (matching pair)
- `After=cosmix-noded.service` for any daemon that registers as an
  ABP service (registration requires the local broker be up first),
  with one exception (next bullet)
- `Requires=cosmix-noded.service` is OPTIONAL; daemons MAY survive
  broker bounces and reconnect via ABP's reconnection contract
  (SPEC 01 §10).

A **registered serve-mode Mix citizen** is a SPEC-10 daemon for the
purposes of this section: its `cosmix-<d>.service` unit carries the
§5.1 required directives and the §5.2 hardening set *identically* to a
Rust daemon, differing only in `ExecStart=/opt/cosmix/bin/mix --serve
/usr/local/lib/cosmix/<d>.mix` (the interpreter is the long-running
process; `MemoryDenyWriteExecute=` remains `true` because Mix is a
tree-walking interpreter with no JIT). Such a unit registers as an ABP
service and therefore declares `After=cosmix-noded.service` under the
rule above, and `Requires=cosmix-noded.service` is OPTIONAL on the same
terms (the SPEC 18 §3.3 supervised reconnect contract is the in-process
counterpart). See SPEC 18 §2 (identity delegated here, not reinvented)
and SPEC 18 §9 (the Phase 1 unit template).

**Broker-provider exception.** The unit that *provides* the
node-local ABP broker (currently `cosmix-noded.service`, registry
UID 500, ABP service `noded`) SHALL NOT declare
`After=cosmix-noded.service` or `Requires=cosmix-noded.service`
against itself. CI lint L15 (§8.2) is the enforcement contract: it
requires `After=cosmix-noded.service` on every other
ABP-registering unit and excludes the single broker-provider unit.
If a future amendment moves the broker-provider role to a different
unit, the exception SHALL move with it; only one unit holds this
exception at a time.

Registered systemd-user daemons are exempt from the noded ordering rule and
MUST NOT name the system-manager `cosmix-noded.service` in `After=` or
`Requires=`. Their mandatory reconnect loop is the cross-manager startup-race
control; `cosmix-interactd` is the v1.4.3 instance (§5.1.2, §7.3).

### 5.5 Unit installation, PATH, and the per-crate deploy slice

(Promoted 2026-07-23 from the cosmix-deployment-layout decision; full
rationale in git history.)

- **Units are linked, not copied.** The canonical unit file lives at
  `/opt/cosmix/systemd/cosmix-<d>.service`; it is activated via
  `systemctl link` (a symlink under `/etc/systemd/system/`), never by
  copying the file. The reinstallable tree stays the single source of
  truth; an image-rebuild or upgrade replaces `/opt/cosmix/` wholesale and
  the links follow.
- **PATH** is provided by a one-line `/etc/profile.d/cosmix.sh` adding
  `/opt/cosmix/bin` — no per-user shell config, no parallel installs.
- **Users and directories** are declared via `/etc/sysusers.d/cosmix.conf`
  (generated per §4) and `/etc/tmpfiles.d/cosmix.conf`; both are applied
  idempotently at boot, never by ad-hoc `useradd`/`mkdir` in scripts.
- **Each daemon crate carries its own `deploy/` slice** (unit file, config
  template — `*.conf.mix` since the fleet-wide conf.mix migration — and
  libexec scripts), mirroring the core-and-citizen pattern into deployment;
  only genuinely cross-cutting artifacts (meta-units, node-role units,
  per-host drop-ins) live outside the crate.

---

## 6. Install-Time and Startup Verification

The substrate distinguishes three verification phases. Each phase
runs at a different point in the lifecycle, against a different set
of expected states, and on a different failure surface. An
implementation SHALL implement all three.

| Phase | When | Expected state of registry entries | On absence | On wrong UID/GID | ABP availability |
|-------|------|----------------------------------|------------|------------------|------------------|
| **Install preflight** (§6.1) | Before `systemd-sysusers` materializes entries; before any unit is enabled | MAY be absent | OK to create | Fail closed | Not assumed |
| **Post-sysusers verification** (§6.2) | After `systemd-sysusers` runs; before any `cosmix-*.service` is started | MUST exist | Fail closed | Fail closed | Not assumed |
| **Startup verification** (§6.3) | At `cosmix-noded` startup, before any ABP socket is bound | MUST exist | Fail closed | Fail closed | Initial run: log locally only (ABP is not up yet by definition). Subsequent reload-time runs: MAY emit on the `preflight` topic. |

### 6.1 Install preflight

Before `systemd-sysusers` materializes new entries, the installer
SHALL perform the following preflight, **in order**, with the rules
that apply to each entry's class:

**Daemon-identity entries** (§2.2):

```
For each daemon-identity entry (name, uid):
    let user_by_uid    = getent passwd <uid>
    let user_by_name   = getent passwd <name>
    let group_by_gid   = getent group <uid>     # GID == UID per §2.2
    let group_by_name  = getent group <name>

    if (user_by_uid empty and user_by_name empty
        and group_by_gid empty and group_by_name empty):
        # UID/GID and name both free — sysusers may safely create
        OK to create

    elif (user_by_uid.name == <name> and user_by_uid.uid == <uid>
          and user_by_name.uid == <uid>
          and group_by_gid.name == <name> and group_by_gid.gid == <uid>
          and group_by_name.gid == <uid>):
        # Already correctly assigned — no-op
        OK to skip

    else:
        # Conflict — fail closed
        ABORT with diagnostic: which UID/GID is taken by which existing
        user/group, and which existing user/group holds the cosmix-*
        name with a different ID. Operator MUST resolve before
        installation proceeds.
```

**Shared-credential group entries** (§2.2):

```
For each shared-credential group entry (name, gid):
    let user_by_name   = getent passwd <name>     # MUST be empty
    let group_by_gid   = getent group  <gid>
    let group_by_name  = getent group  <name>

    if (user_by_name empty
        and group_by_gid empty and group_by_name empty):
        # GID and name both free — sysusers may safely create
        OK to create

    elif (user_by_name empty
          and group_by_gid.name == <name> and group_by_gid.gid == <gid>
          and group_by_name.gid == <gid>):
        # Already correctly assigned — no-op
        OK to skip

    else:
        # Conflict — fail closed (includes the case where a
        # same-named user has appeared, which §2.2 forbids).
        ABORT with diagnostic: which GID is taken by which existing
        group, which existing group holds the cosmix-* name with a
        different GID, or that a user with the same name exists.
        Operator MUST resolve before installation proceeds.
```

**Citizen-identity entries** (§2.2, §2.5):

```
For each citizen-identity entry (name, uid, state):
    if state == live:
        # Identical to the daemon-identity preflight: a live citizen
        # has the same passwd+same-numbered-group shape (GID == UID).
        apply the Daemon-identity preflight block above, verbatim,
        to (name, uid).

    else:   # state in { retired, reclaimed }
        # A retired/reclaimed row does NOT materialize (§4.3). Its
        # NAME MUST NOT resolve to a live user or group. The check is
        # by-NAME ONLY — never by UID: if the UID has been
        # R7-re-allocated it is held by a *different* live citizen
        # name, preflighted above on its own live row; a by-UID test
        # would false-positive that legitimate re-allocation, so the
        # by-name rule needs no R7 exception clause.
        let user_by_name  = getent passwd <name>
        let group_by_name = getent group  <name>

        if user_by_name empty and group_by_name empty:
            # The retired/reclaimed NAME is unbound — OK. A
            # R7-re-allocated UID held under a *different* live name
            # is verified by that citizen's own live row, never here.
            OK to skip
        else:
            # A retired/reclaimed citizen NAME is unexpectedly
            # live (stale projection, drift) — fail closed
            ABORT with diagnostic: retired/reclaimed citizen <name>
            (<uid>) name resolves to a live user/group; operator MUST
            run the §2.5 / R8 remediation (userdel on every
            projecting node) before installation proceeds.
```

The R8 reuse gate (purge-verification + quarantine) is an **operator
amendment-time** procedure performed when a row transitions
retired→reclaimed in the canonical source; it is **not** an
install-time check. Install preflight only enforces the *projection*
consequence: a retired/reclaimed row fails closed if it is
unexpectedly live on the host.

Membership lines (`m <user> <group>`) are not preflighted on their
own: they take effect only when `systemd-sysusers` runs Phase 3, at
which point the §6.2 post-sysusers verification re-asserts every
declared membership.

The install preflight SHALL NOT auto-correct, auto-allocate, or
rename to bypass a conflict.

### 6.2 Post-sysusers verification

After `systemd-sysusers` has been invoked, and before any
`cosmix-*.service` unit is enabled or started, the installer SHALL
re-query each registry entry and SHALL fail closed under the rules
that apply to the entry's class:

- **Daemon-identity entries** (§2.2) SHALL resolve to a user *and*
  same-numbered group with `uid == gid == registered UID` and
  `name == registered name`.
- **Shared-credential group entries** (§2.2) SHALL resolve to a
  group with `gid == registered GID` and `name == registered name`,
  and SHALL have *no* associated user with the same name. In
  addition, every membership pair declared in the canonical
  sysusers fragment for that group (each `m <user> <group>` line)
  SHALL be present in `getent group <group>`'s membership list.
- **Citizen-identity entries** (§2.2, §2.5): a **live** citizen row
  SHALL resolve to a user *and* same-numbered group with
  `uid == gid == registered UID` and `name == registered name`
  (the daemon-identity rule, applied unchanged). A **retired** or
  **reclaimed** citizen row is comment-only (§4.3) and its
  **name** SHALL NOT resolve to a live user or group. The check is
  **by name only — never by UID**: if the retired/reclaimed UID has
  been R7-re-allocated it is held by a *different* live citizen
  *name*, which is verified independently by that citizen's own live
  row above; a by-UID test would false-positive that legitimate
  re-allocation, so the by-name rule needs no R7 exception clause.

Any of the following constitutes a fail-closed condition:

- A registry user or group is absent after sysusers should have
  created it (sysusers fragment drift, install ordering bug, or a
  lock contention).
- A registry name resolves to a UID/GID outside the registry.
- A registry UID/GID resolves to a name other than the registered
  name.
- A shared-credential group has unexpectedly acquired a same-named
  user, or has lost a declared membership.
- A tombstoned UID/GID is currently held by a live user or group.
- A `retired:` or `reclaimed:` citizen **name** resolves to a live
  user or group (by-name check; the §6.1 retired/reclaimed-state
  check, re-asserted post-sysusers). A UID R7-re-allocated to a
  *different* live name is **not** a violation — that name is
  verified by its own live row, never by this audit row.
- A citizen UID has more than one non-`reclaimed` (live or
  `retired:`) registry row, or a `reclaimed:` row lacks its
  `reclaimed:` date or `verifier:` token (§2.5; CI lint L5 catches
  this in-source, this re-asserts it against the projected state).
- The `sysusers.d` fragment on disk does not match the canonical
  source.

Failures here SHALL halt installation with a structured diagnostic
and SHALL NOT be silently coerced.

### 6.3 Startup verification

At `cosmix-noded` startup, before any ABP socket is bound, the
daemon SHALL run §6.2's verification pass against its own registry
copy. The same fail-closed rules apply: a single mismatch prevents
the ABP broker socket from binding.

Because this phase runs before ABP is up, results SHALL be written
to the daemon's local log channel (systemd journal, with the
structured fields `cosmix.spec=10`, `cosmix.spec.version=1.0.0`,
`cosmix.preflight=ok|fail`, and on failure a `cosmix.preflight.errors=`
field listing affected entries). Once ABP is up, subsequent
verification runs (e.g. on broker reload) MAY additionally emit
structured events on the substrate's `preflight` topic; absence of
that topic SHALL NOT be treated as a failure.

### 6.4 Idempotency

All three phases are idempotent. Re-running any of them on a
correctly provisioned host SHALL be a no-op and SHALL exit zero.
This is the property the CI lint depends on (§8.2).

### 6.5 Fail-closed semantics

"Fail closed" in this SPEC means: the verifier exits non-zero, no
`cosmix-*.service` is started by the installer or broker, and (where
applicable) the ABP socket is not bound. There SHALL be no fallback
path that allows partial start-up under a registry mismatch.

### 6.6 Read-only-root and immutable-image hosts

On hosts with read-only root filesystems (e.g. systemd-portabled
images, OSTree-style deployments), the registry SHALL be applied at
**image build time**, not at first boot. The `sysusers.d` fragment is
baked into the image; first-boot verification is the §6.2 pass
against the baked state, and runtime verification is §6.3.

Live mutation of the registry on a read-only-root host is a SPEC
violation: the only path is to rebuild the image with an amended
SPEC.

---

## 7. Session-Scoped Exclusions

### 7.1 Test for inclusion

A Cosmix process belongs in the daemon registry (§2) if and only if it
runs **as a system service** with persistent identity across user
logins. A process is **session-scoped** and excluded from the registry
if any of the following hold:

- The process binds to a logged-in human seat (Wayland session,
  console, audio group, video group).
- The process consumes per-user XDG state under `$XDG_*_HOME`.
- The process uses the user D-Bus bus rather than the system bus.
- The process is launched by a `systemd --user` unit, not a system
  unit.
- The process holds capabilities or file descriptors that are valid
  only within an active session (e.g. a logind session ID).

If any one of the above holds, the process SHALL run as the logged-in
human user. It SHALL NOT have a registry entry, a `cosmix-*` system
UID, or a `/var/lib/cosmix/<d>/` daemon leaf.

### 7.2 Listed exclusions

The following Cosmix components are session-scoped and excluded from
the registry as of this SPEC's publication:

| Component | Reason |
|-----------|--------|
| `cosmix-comp` and native CTK applications | Desktop processes bound to the logged-in seat/session; the compositor is specified by SPEC 16 |

Future Cosmix components SHALL apply the test in §7.1 to decide
inclusion. (`cosmix-menu` was previously listed here; the crate was
removed when its assumptions — XDG-tray launcher for a desktop full
of Dioxus apps — no longer matched the post-pivot Cosmix surface.)

### 7.3 `cosmix-interactd` reserved identity and session runtime

`cosmix-interactd` is the narrow exception to §7.1's “no registry entry”
rule. UID/GID **517** reserves a stable POSIX identity for the interaction
namespace and any future system-owned persisted props projection, but the
current notify.v1 implementation is memory-backed and its freedesktop sink
requires the logged-in user's session D-Bus. The shipped process therefore
MUST run from `cosmix-interactd.service` as a `systemd --user` service under
the logged-in user and conform to §5.1.2; it MUST NOT claim that its live
process is running as `cosmix-interactd` UID/GID 517.

This reservation does not grant the session process access to a
`/var/lib/cosmix/interactd/` daemon leaf and does not weaken the general
session-scoped exclusion. A later system-owned persistence helper MAY use the
reserved identity only through a separately specified trust boundary; until
then the row is namespace/ownership reservation and preflight material only.

---

## 8. Conformance

### 8.1 Conformance levels

A Cosmix installation conforms to this SPEC at one of three levels:

**Level 0 (Pre-conformance).** Daemons run under arbitrary identities,
without registry verification. Permitted only in a dev box during
substrate bootstrap. Not permitted on any mesh node.

**Level 1 (Registered).** Every system-service daemon's running UID/GID matches
its registry entry; every registered systemd-user daemon satisfies §5.1.2 and
is not claimed to run as its reserved row. The `sysusers.d` fragment on disk
matches the canonical source. The applicable §5.1 directives are present.
Required for every mesh node.

**Level 2 (Hardened).** Level 1 plus the §5.2 hardening directives for
system-service daemons; registered systemd-user daemons remain governed by
§5.1.2. Install-time preflight (§6) is executed and logged and CI lint (§8.2)
is green at the time of last package update. Required for any internet-exposed
mesh node.

### 8.2 CI lint shape

A CI lint SHALL be runnable in the source tree and SHALL verify all of
the following invariants. The lint is part of the substrate's
self-observation surface (per the Three Design Criteria) and is itself
agent-operable.

| ID | Invariant |
|----|-----------|
| L1 | Canonical Markdown registry parses without syntax errors into up to three ordered blocks, each opened by its own header row: a **daemon-identity** block (header begins `uid`) of `(name, uid, gid, bus, gecos)` tuples (the `bus` field is either an explicit ABP service name or the placeholder `-` meaning "default-derive: name minus `cosmix-` prefix"); an OPTIONAL **shared-credential-group** block (header begins `gid`) of `(name, gid, purpose)` tuples (no UID, no ABP service); and an OPTIONAL **citizen-identity** block (header begins `cid`) of `(name, uid, gid, bus, gecos, state)` tuples where `state` is one of live (no `retired:`/`reclaimed:`), `retired:` (with a date), or `reclaimed:` (with both a `retired:` and a `reclaimed:` date and a `verifier:` token). The lint MUST distinguish the three blocks and apply the per-class rules below. |
| L2 | UID == GID for every non-tombstoned daemon-identity entry and for every citizen-identity entry (citizen entries have the daemon-identity shape, §2.2). Enforced via the projected `u <name> <uid>` form carrying no `:gid` suffix. (Shared-credential group entries have no UID; this rule does not apply to them.) |
| L3 | Allocation is monotonic *within each block*: daemon UIDs appear in append-only order; shared-credential GIDs appear in append-only order; **non-`reclaimed:` citizen rows** appear in append-only order. Tombstones (daemon/shared) and `retired:`/`reclaimed:` citizen rows appear in their original positions; a `reclaimed:` citizen UID is exempt from monotonicity because R7 permits below-frontier re-allocation. |
| L4 | Every daemon UID and every shared-credential GID is in the 500–599 window; every citizen-identity UID is in the 600–699 citizen band (§2.1). (Or in a window declared by a future amendment.) |
| L5 | No tombstoned UID or name appears as a live daemon/shared entry. For the citizen-identity block: at most **one** non-`reclaimed:` row (live or `retired:`) exists per citizen UID; every `reclaimed:` row carries both a `reclaimed:` date and a non-empty `verifier:` token; no `retired:` or `reclaimed:` citizen row projects a `u` line (§4.3); a re-allocated UID's new live row has a different `name` from every prior `reclaimed:` row for that UID. |
| L6 | `systemd-sysusers --dry-run <build-tree>/cosmix.conf` succeeds against the freshly generated fragment in the build tree (not the installed `/usr/lib/sysusers.d/` copy, which may be older or absent in CI). |
| L7 | The generated `sysusers.d` fragment matches the canonical source (regenerate, diff, fail on diff). |
| L8 | Every system-service `cosmix-*.service` unit carries §5.1.1's directives and its `User=` / `Group=` / `StateDirectory=` / `RuntimeDirectory=` references a canonical registry name. Every registered systemd-user unit explicitly listed by §5.1.2 instead carries that subsection's directives and omits `User=` / `Group=` / `DynamicUser=` / `StateDirectory=` / `RuntimeDirectory=` / `ConfigurationDirectory=`. A filename alone does not select the user-unit branch. |
| L9 | No `cosmix-*.service` unit declares `DynamicUser=` or `User=root`. |
| L10 | No daemon-writable path under `/etc/cosmix/` appears in any unit's `ReadWritePaths=`. |
| L11 | No two units declare conflicting `StateDirectory=` parents (i.e. one declaring `cosmix` while another declares `cosmix/<d>`). |
| L12 | The session-scoped exclusion list (§7.2) is consistent: every name listed there has no registry entry; every component with a registry entry is not in the exclusion list. |
| L13 | Every system-service `cosmix-*.service` unit at Level 2 declares each §5.2 directive **with the value listed in the §5.2 table, compared after systemd boolean normalisation** (per `systemd.syntax(5)`: `true`/`yes`/`on`/`1` are equivalent for a boolean `true`; `false`/`no`/`off`/`0` for `false`), in canonical hardening order (the row order in the §5.2 table). Named alternatives are accepted only for the four directives listed under §5.2 (`RestrictAddressFamilies=` strict subset; `SystemCallFilter=` base allow plus daemon-specific deny filters; `CapabilityBoundingSet=` and `AmbientCapabilities=` empty or `CAP_*` allow-list). Each missing directive is replaced by an in-line comment of the form `# §5.2 deviation: <Directive>= — <reason>` placed in the same canonical order, where `<Directive>=` is the directive name with trailing `=`. Registered systemd-user units are checked under L8's §5.1.2 branch, not this system-service hardening table. |
| L14 | The R6 split is enforced: every **daemon-identity entry**'s and every **non-`retired:`/non-`reclaimed:` citizen-identity entry**'s resolved ABP service name (the explicit `bus:` value, or the default derivation when `bus:` is `-`) matches the regex `^[a-z][a-z0-9-]{1,30}$`, does not start with `cosmix-`, and is unique across the **union** of non-tombstoned daemon-identity entries and live citizen-identity entries (a citizen ABP name SHALL NOT collide with a daemon ABP name or another live citizen ABP name — §2.3 R6). (Shared-credential group entries have no ABP service name; this rule does not apply to them.) |
| L15 | Every system-service `cosmix-*.service` unit other than the broker-provider unit that registers as an ABP service declares `After=cosmix-noded.service`. The broker-provider unit (currently `cosmix-noded.service` per §5.4) does not order against itself. Registered systemd-user daemons are explicitly exempt from this cross-manager ordering and MUST omit `After=`/`Requires=cosmix-noded.service`; the lint verifies that omission and their explicit §5.1.2 classification. |
| L16 | The shared-credential-group block in Appendix A and the corresponding `g`/`m` lines in the generated `sysusers.d` fragment agree pairwise: every `g <name> <gid>` line in the fragment has a matching shared-credential row in Appendix A with identical `name` and `gid`, and vice versa. Every `m <user> <group>` line names a `<user>` that exists as a daemon-identity entry (non-tombstoned) and a `<group>` that exists as a shared-credential-group entry (non-tombstoned). Every `m <user> <group>` line in the fragment SHALL appear after both its referenced `u <user>` line and its referenced `g <group>` line (per §9.1), so that `systemd-sysusers` cannot silently materialize a referent with defaults via its implicit-creation behavior. No shared-credential-group `<name>` collides with any daemon-identity `<name>`, and no shared-credential-group `<gid>` collides with any daemon-identity `<uid>`. |
| L17 | The citizen-identity block in Appendix A and the corresponding citizen `u` lines in the generated `sysusers.d` fragment agree pairwise: every **live** citizen row has exactly one matching `u cosmix-<name> <uid>` fragment line with identical `name` and `uid` (and no `:gid` suffix), and every citizen `u` line in the fragment has a matching **live** Appendix A citizen row; every `retired:` row projects exactly one `# quarantine:` comment and **no** `u` line; every `reclaimed:` row projects exactly one `# reclaimed:` comment and **no** `u` line (§4.3). No citizen-identity `<name>` collides with any daemon-identity or shared-credential-group `<name>`, and no citizen-identity `<uid>` collides with any daemon-identity `<uid>` or shared-credential-group `<gid>` (the 600–699 band is disjoint from 500–599 so this is structural; the lint asserts it as defence-in-depth). Citizen `u` lines appear after the daemon-identity `u` lines and after the shared-credential `g`/`m` lines. |

The lint SHALL be invoked by CI on every pull request and SHALL block
merge on any failure. It MAY also be invoked by the install-time
preflight (§6) for additional defense in depth.

---

## 9. Examples

### 9.1 Generated `sysusers.d` fragment

The fragment below is the canonical projection of Appendix A into
`systemd-sysusers` syntax. It SHALL be installed at
`/usr/lib/sysusers.d/cosmix.conf` (vendor) or
`/etc/sysusers.d/cosmix.conf` (admin override). The order of `u`
lines SHALL match the append-only order of the daemon-identity block
in the canonical registry; the order of `g` and `m` lines SHALL
match the append-only order of the shared-credential-group block.
`m <user> <group>` lines SHALL appear after the `g <group>` line
they reference and after the `u <user>` lines they name; this is
the natural order produced by emitting daemon-identity entries first.
Citizen-identity `u` lines (§2.2, v1.2.0) are emitted in their own
sub-block after the shared-credential `g`/`m` lines, and only for
**live** citizen rows; `retired:`/`reclaimed:` rows project as
comment lines (§4.3) and SHALL NOT emit a `u` line.

```sysusers
# /usr/lib/sysusers.d/cosmix.conf
# Generated from Appendix A of cosmix-daemon-identity v1.4.4.
# DO NOT EDIT — regenerate from the canonical Markdown registry.

# --- Daemon-identity entries (POSIX user + same-numbered group) ---
#Type Name              ID   GECOS                                Home           Shell
u     cosmix-noded      500  "Cosmix node daemon (ABP broker)"    /nonexistent   /usr/sbin/nologin
u     cosmix-maild      501  "Cosmix mail daemon"                 /nonexistent   /usr/sbin/nologin
u     cosmix-webd       502  "Cosmix web daemon"                  /nonexistent   /usr/sbin/nologin
u     cosmix-indexd     503  "Cosmix knowledge daemon"            /nonexistent   /usr/sbin/nologin
u     cosmix-agentd     504  "Cosmix agent runtime"               /nonexistent   /usr/sbin/nologin
u     cosmix-mcp        505  "Cosmix MCP bridge"                  /nonexistent   /usr/sbin/nologin
u     cosmix-dnsd       506  "Cosmix authoritative DNS daemon"    /nonexistent   /usr/sbin/nologin
u     cosmix-cron       507  "Cosmix scheduler"                   /nonexistent   /usr/sbin/nologin
u     cosmix-prometheus 508  "Cosmix Prometheus (obs tier)"       /nonexistent   /usr/sbin/nologin
u     cosmix-grafana    509  "Cosmix Grafana (obs tier)"          /nonexistent   /usr/sbin/nologin
u     cosmix-loki       511  "Cosmix Loki (obs tier)"             /nonexistent   /usr/sbin/nologin
u     cosmix-alloy      512  "Cosmix Grafana Alloy (obs tier)"    /nonexistent   /usr/sbin/nologin
u     cosmix-pveexport  513  "Cosmix proxmox-exporter (obs tier)" /nonexistent   /usr/sbin/nologin
u     cosmix-nodeexport 514  "Cosmix node_exporter (obs tier)"    /nonexistent   /usr/sbin/nologin
u     cosmix-wgd        515  "Cosmix WireGuard mesh control plane" /nonexistent   /usr/sbin/nologin
u     cosmix-interactd  517  "Cosmix interaction broker"          /nonexistent   /usr/sbin/nologin
u     cosmix-nspawnd    518  "Cosmix nspawn host executor"        /nonexistent   /usr/sbin/nologin

# --- Shared-credential groups (group only; no associated user) ---
# cosmix-tls mediates read access to TLS keypairs shared by ≥2 daemons
# (SPEC 10 §3.3). Membership lines below add the consuming daemons; new
# consumers add their own `m cosmix-<d> cosmix-tls` line in this fragment
# rather than via per-host setfacl.
g     cosmix-tls     510
m     cosmix-maild   cosmix-tls
m     cosmix-webd    cosmix-tls
# cosmix-mesh mediates read access to the signed mesh inventory (SPEC-13
# INV-1) under /var/lib/cosmix/noded/, shared by mesh daemons, WITHOUT
# granting cosmix-noded's group any read on the private d2 seed (§3.3).
g     cosmix-mesh    516
m     cosmix-wgd     cosmix-mesh

# --- Citizen-identity entries (POSIX user + same-numbered group) ---
# Citizen-identity entries (SPEC 10 §2.2, §2.5, v1.2.0) have the exact
# same on-disk shape as a daemon-identity entry but live in the disjoint
# 600–699 band and are governed by scoped, gated reuse (§2.3 R7/R8): a
# retired UID re-enters the free pool only after mesh-wide automated
# purge-verification AND a 30-day quarantine. Only LIVE citizen rows
# materialize a `u` line here; retired:/reclaimed: rows are comment-only
# in the canonical registry (Appendix A) and SHALL NOT appear below
# (SPEC 10 §4.3, §9.1). The consumer is SPEC 18 (Mix Citizen Runtime).
u     cosmix-statecache 600 "Cosmix SPEC-18 reference citizen" /nonexistent   /usr/sbin/nologin
```

`Home=/nonexistent` matches §4.3; daemons and citizens own state
under `/var/lib/cosmix/<d>/` created by systemd `StateDirectory=`
(§5.1, §3.4), not under a traditional home. `Shell=/usr/sbin/nologin`
satisfies the no-interactive-login rule of §4.3. A citizen `u` line
is byte-for-byte the same shape as a daemon `u` line — the difference
is purely registry governance (§2.3 R7/R8), invisible to
`systemd-sysusers`.

### 9.2 Service unit fragment

The fragment below is a minimal, normative example of a Cosmix
daemon unit at conformance Level 2 (§8.1). It demonstrates §5.1
required directives, §5.2 hardening directives, and §5.4 ordering.
Local installations MAY add directives, but SHALL NOT remove or
weaken any directive shown here.

```ini
# /usr/lib/systemd/system/cosmix-maild.service
[Unit]
Description=Cosmix mail daemon
Documentation=https://cosmix.dev/spec/10
After=network-online.target cosmix-noded.service
Wants=network-online.target
Requires=cosmix-noded.service

[Service]
Type=notify
User=cosmix-maild
Group=cosmix-maild
StateDirectory=cosmix/maild
StateDirectoryMode=0750
RuntimeDirectory=cosmix/maild
RuntimeDirectoryMode=0750

ConfigurationDirectory=cosmix/maild
ConfigurationDirectoryMode=0755
WorkingDirectory=%S/cosmix/maild
# %E/cosmix/maild expands to /etc/cosmix/maild — the per-daemon config
# leaf defined in §3.1. The daemon reads from this path. This example
# is the non-secret case (root:root 0755 dir, root:root 0644 file) —
# see §3.3. For a secret-config variant the dir is created in advance
# by the package or a tmpfiles fragment as root:cosmix-maild 0750, and
# ConfigurationDirectoryMode= is set to 0750 to match.
ExecStart=/opt/cosmix/bin/cosmix-maild --config %E/cosmix/maild/config.toml
Restart=on-failure
RestartSec=5s

# §5.1 required (Level 1)
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

# §5.2 mandatory hardening (Level 2)
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged @resources
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectProc=invisible
ProcSubset=pid
CapabilityBoundingSet=
AmbientCapabilities=
UMask=0027

# Optional belt-and-braces: device namespace lock-down
PrivateDevices=yes

[Install]
WantedBy=multi-user.target
```

Notes:

- `User=` and `Group=` match the canonical registry entry for UID
  501 (Appendix A); CI lint L8 (§8.2) verifies this.
- `StateDirectory=cosmix/maild` causes systemd to create
  `/var/lib/cosmix/maild/` owned `cosmix-maild:cosmix-maild` mode
  `0750` before the daemon starts, satisfying §3.4.
- `ConfigurationDirectory=cosmix/maild` materialises
  `/etc/cosmix/maild/` (§3.1) and exposes it as `%E/cosmix/maild`.
  The daemon reads `config.toml` from there. Per §3.3 the directory
  is read-only at runtime — the unit does not list `/etc/cosmix/`
  in `ReadWritePaths=` (lint L10).
- `Requires=cosmix-noded.service` plus `After=` realises the §5.4
  ordering invariant: the local ABP broker is up before any mesh
  citizen starts.
- `CapabilityBoundingSet=` and `AmbientCapabilities=` are emptied;
  bind-to-port-25 daemons (e.g. `cosmix-maild` SMTP inbound) SHOULD
  prefer `AmbientCapabilities=CAP_NET_BIND_SERVICE` over running as
  root, and SHALL document the deviation in an in-line comment per
  §5.2.

### 9.3 Verification — pseudocode

The pseudocode below illustrates the §6.2 post-sysusers verification
pass and the §6.3 startup verification pass. It is **not** a
language reference and is **not** a normative implementation; the
contract is the prose in §6.1–§6.5. A real implementation MAY be
written in Mix, Rust, or shell, provided it observes fail-closed
semantics (§6.5) and emits diagnostics matching §6.3.

```
# Post-sysusers verification (§6.2) and startup verification (§6.3).
# Difference: §6.2 expects sysusers has just run; §6.3 expects the
# users to have existed since the last successful install.
#
# The registry has three classes (§2.2 v1.2.0):
#   * DAEMON_IDENTITY:  rows that own a POSIX user + same-numbered group.
#   * SHARED_GROUPS:    group-only rows with declared memberships.
#   * CITIZEN_IDENTITY: live rows verify exactly like DAEMON_IDENTITY;
#                       a retired:/reclaimed: row's NAME MUST NOT
#                       resolve to a live user or group (by-name only;
#                       an R7-re-allocated UID is held under a
#                       different live name, verified by its own live
#                       row) (§2.5, §6.2).
# Each class verifies under its own rule set; failures from any
# halt installation under §6.5 fail-closed semantics.

DAEMON_IDENTITY = [
  (500, "cosmix-noded"),      (501, "cosmix-maild"),    (502, "cosmix-webd"),
  (503, "cosmix-indexd"),     (504, "cosmix-agentd"),   (505, "cosmix-mcp"),
  (506, "cosmix-dnsd"),       (507, "cosmix-cron"),     (508, "cosmix-prometheus"),
  (509, "cosmix-grafana"),    (511, "cosmix-loki"),     (512, "cosmix-alloy"),
  (513, "cosmix-pveexport"), (514, "cosmix-nodeexport"),
  (515, "cosmix-wgd"),
  (517, "cosmix-interactd"),
  (518, "cosmix-nspawnd"),
]

SHARED_GROUPS = [
  # (gid, name, members)
  (510, "cosmix-tls", ["cosmix-maild", "cosmix-webd"]),
  (516, "cosmix-mesh", ["cosmix-wgd"]),
]

CITIZEN_IDENTITY = [
  # (uid, name, state)  state ∈ {"live", "retired", "reclaimed"}
  (600, "cosmix-statecache", "live"),
]

errors = []

# --- Daemon-identity entries ---
for (want_uid, want_name) in DAEMON_IDENTITY:
    user_by_uid   = getent("passwd", want_uid)    # numeric lookup
    user_by_name  = getent("passwd", want_name)   # name lookup
    group_by_gid  = getent("group",  want_uid)
    group_by_name = getent("group",  want_name)

    # Absence — fail-closed at §6.2 and §6.3
    if user_by_uid is None or user_by_name is None:
        errors.append((want_name, "absent"))
        continue
    if group_by_gid is None or group_by_name is None:
        errors.append((want_name, "group absent"))
        continue

    # Name/UID disagree — somebody else holds the slot
    if user_by_uid.name != want_name:
        errors.append((want_name, f"uid {want_uid} held by {user_by_uid.name}"))
    if user_by_name.uid != want_uid:
        errors.append((want_name, f"name held by uid {user_by_name.uid}"))
    if group_by_gid.name != want_name:
        errors.append((want_name, f"gid {want_uid} held by {group_by_gid.name}"))
    if group_by_name.gid != want_uid:
        errors.append((want_name, f"group held by gid {group_by_name.gid}"))

    # GID must equal UID (§2.2 daemon-identity rule)
    if user_by_uid.uid != user_by_uid.gid:
        errors.append((want_name, f"uid {user_by_uid.uid} != gid {user_by_uid.gid}"))

# --- Shared-credential group entries ---
for (want_gid, want_name, want_members) in SHARED_GROUPS:
    group_by_gid  = getent("group",  want_gid)
    group_by_name = getent("group",  want_name)
    user_by_name  = getent("passwd", want_name)   # MUST be empty (§2.2)

    if group_by_gid is None or group_by_name is None:
        errors.append((want_name, "shared group absent"))
        continue
    if group_by_gid.name != want_name:
        errors.append((want_name, f"gid {want_gid} held by {group_by_gid.name}"))
    if group_by_name.gid != want_gid:
        errors.append((want_name, f"group held by gid {group_by_name.gid}"))
    if user_by_name is not None:
        # §2.2: shared-credential entries SHALL NOT have a same-named user.
        errors.append((want_name, f"shared-credential name collides with user uid {user_by_name.uid}"))

    # §6.2: every declared membership SHALL be present in getent.
    live_members = group_by_name.members  # 4th colon-field, comma-split
    for m in want_members:
        if m not in live_members:
            errors.append((want_name, f"missing membership: {m}"))

# --- Citizen-identity entries (§2.5 lifecycle) ---
for (want_uid, want_name, state) in CITIZEN_IDENTITY:
    user_by_uid   = getent("passwd", want_uid)
    user_by_name  = getent("passwd", want_name)
    group_by_gid  = getent("group",  want_uid)
    group_by_name = getent("group",  want_name)

    if state == "live":
        # A live citizen verifies under the exact daemon-identity rule
        # set (§2.2 — citizens have the daemon shape; GID==UID).
        if user_by_uid is None or user_by_name is None:
            errors.append((want_name, "citizen absent")); continue
        if group_by_gid is None or group_by_name is None:
            errors.append((want_name, "citizen group absent")); continue
        if user_by_uid.name != want_name:
            errors.append((want_name, f"uid {want_uid} held by {user_by_uid.name}"))
        if user_by_name.uid != want_uid:
            errors.append((want_name, f"name held by uid {user_by_name.uid}"))
        if group_by_gid.name != want_name:
            errors.append((want_name, f"gid {want_uid} held by {group_by_gid.name}"))
        if group_by_name.gid != want_uid:
            errors.append((want_name, f"group held by gid {group_by_name.gid}"))
        if user_by_uid.uid != user_by_uid.gid:
            errors.append((want_name, f"uid {user_by_uid.uid} != gid {user_by_uid.gid}"))
    else:
        # state in {"retired", "reclaimed"}: §2.5/§4.3 — the row is
        # comment-only and MUST NOT materialize. The check is by-NAME
        # ONLY, fail-closed: the retired/reclaimed NAME MUST NOT resolve
        # to any live user or group, regardless of UID. A by-UID check
        # would be laxer (partial-truth): a reclaimed UID may be
        # legitimately R7-re-allocated to a *different-named* live
        # citizen — that row is its own live CITIZEN_IDENTITY tuple,
        # checked above — so a by-UID test would false-NEGATIVE the
        # genuine "name still bound" violation whenever the UID happens
        # to be re-held by a differently-named live citizen. This
        # matches §6.2 and `spec10_postcheck.mix` exactly (by-name).
        if user_by_name is not None:
            errors.append((want_name,
                f"{state} citizen name still resolves to a live user "
                f"(uid {user_by_name.uid})"))
        if group_by_name is not None:
            errors.append((want_name,
                f"{state} citizen name still resolves to a live group "
                f"(gid {group_by_name.gid})"))

# Fail-closed reporting (§6.5)
if errors:
    log_local("cosmix.spec=10 cosmix.preflight=fail", errors)
    if bus_broker_is_up():
        bus_emit("preflight.failed", {spec: 10, version: "1.4.4", errors: errors})
    exit(1)

live_citizens = [c for c in CITIZEN_IDENTITY if c[2] == "live"]
log_local("cosmix.spec=10 cosmix.preflight=ok",
          daemons=len(DAEMON_IDENTITY), shared=len(SHARED_GROUPS),
          citizens=len(live_citizens))
if bus_broker_is_up():
    bus_emit("preflight.ok", {
        spec: 10, version: "1.4.4",
        daemons:  len(DAEMON_IDENTITY),
        shared:   len(SHARED_GROUPS),
        citizens: len(live_citizens),
    })
exit(0)
```

Notes on the ABP-emit branch:

- §6.3 runs *before* `cosmix-noded` binds its ABP socket. The
  verifier therefore SHALL log to the local journal and SHALL NOT
  treat ABP being down as a failure.
- §6.2 runs after `systemd-sysusers` and before any
  `cosmix-*.service` is started. ABP MAY be up if a prior
  `cosmix-noded` is already running (e.g. an in-place upgrade); if
  so, the verifier MAY emit on the `preflight` topic. If not, local
  logging is sufficient.

---

## Appendix A. Initial UID/GID Registry (1.4.4)

```
# Cosmix daemon identity registry — version 1.4.4
# Date: 2026-08-08
# Daemon/shared window: 500-599 (preferred fixed-ID window; see §2.1)
# Citizen window:        600-699 (citizen-identity stream, v1.2.0; §2.1, R7)
# Daemon/shared allocation rule: append-only, no reuse (R1, R2, R2.a)
# Citizen allocation rule:       lowest-free in 600-699; scoped gated
#   reuse only after R8 (mesh-wide purge-verification AND a 30-day
#   quarantine window) — see §2.3 R7/R8 and §2.5.
# ABP service name defaults to <name> minus the "cosmix-" prefix unless
# an explicit `bus:` field overrides it (R6). The ABP name is the
# identity the daemon registers with on the node-local broker if it
# ABP-registers; L14 uniqueness binds the name across the daemon and
# live citizen blocks whether or not the daemon currently registers.
# The v1.4.0 observability-tier entries (508/509/511/512/513) hold
# their R6 default names (`prometheus`, `grafana`, `loki`, `alloy`,
# `pveexport`) without currently ABP-registering — see Appendix D for
# the precedent and the package-native-unit masking requirement. The
# v1.4.1 addition (514 `cosmix-nodeexport`, R6 name `nodeexport`)
# follows the same deployment shape.

# --- Daemon-identity entries (POSIX user + same-numbered group) ---
uid  name              bus     gecos                                  tier         tombstoned
---  ----------------  ------  -------------------------------------  -----------  ----------
500  cosmix-noded      -       "Cosmix node daemon (ABP broker)"      substrate    -
501  cosmix-maild      -       "Cosmix mail daemon"                   application  -
502  cosmix-webd       -       "Cosmix web daemon"                    application  -
503  cosmix-indexd     -       "Cosmix knowledge daemon"              substrate    -
504  cosmix-agentd     -       "Cosmix agent runtime"                 substrate    -
505  cosmix-mcp        -       "Cosmix MCP bridge"                    substrate    -
506  cosmix-dnsd       -       "Cosmix authoritative DNS daemon"      substrate    -
507  cosmix-cron       -       "Cosmix scheduler"                     substrate    -
508  cosmix-prometheus -       "Cosmix Prometheus (obs tier)"         substrate    -
509  cosmix-grafana    -       "Cosmix Grafana (obs tier)"            substrate    -
511  cosmix-loki       -       "Cosmix Loki (obs tier)"               substrate    -
512  cosmix-alloy      -       "Cosmix Grafana Alloy (obs tier)"      substrate    -
513  cosmix-pveexport  -       "Cosmix proxmox-exporter (obs tier)"   substrate    -
514  cosmix-nodeexport -       "Cosmix node_exporter (obs tier)"      substrate    -
515  cosmix-wgd        -       "Cosmix WireGuard mesh control plane"  substrate    -
517  cosmix-interactd  interact "Cosmix interaction broker"            session-reserved -
518  cosmix-nspawnd    -       "Cosmix nspawn host executor"          substrate    -

# --- Shared-credential group entries (group only; no associated user) ---
gid  name        purpose                                                   tombstoned
---  ----------  --------------------------------------------------------  ----------
510  cosmix-tls  Read access to TLS keypairs shared by ≥2 daemons (§3.3)   -
516  cosmix-mesh Read access to the signed mesh inventory (SPEC-13 INV-1) shared by mesh daemons (§3.3)  -

# --- Citizen-identity entries (POSIX user + same-numbered group; §2.5 scoped reuse) ---
# Columns after gecos: tier, then the §2.5 lifecycle audit triple
# (retired / reclaimed / verifier). State is DERIVED: `-` retired ⇒
# live; retired set, reclaimed `-` ⇒ in quarantine (R8); both set with
# a verifier token ⇒ reclaimed (UID back in the R7 free pool). A
# retired:/reclaimed: row is KEPT here (never deleted — R7) and does
# NOT project a `u` line (§4.3, §9.1).
cid  name               bus  gecos                               tier     retired     reclaimed   verifier
---  -----------------  ---  ----------------------------------  -------  ----------  ----------  --------
600  cosmix-statecache  -    "Cosmix SPEC-18 reference citizen"  citizen  -           -           -

# `-` in the bus column means the default derivation applies (name minus
# "cosmix-" prefix). Entry 500 follows the default — it registers as
# `noded` (the historical `hub` alias was removed in the 2026-05-09
# cosmix-noded rename; the substrate has no central hub role, every
# mesh node runs its own `cosmix-noded`). The v1.4.0 observability-tier
# entries (508/509/511/512/513) likewise carry the default; their R6
# ABP names are reserved per L14 even though the upstream Go binaries
# do not currently ABP-register (see Appendix D 1.4.0 for the
# deployment-shape requirement that the package-native systemd unit
# be masked and the daemon be supervised by the Cosmix-managed
# `cosmix-<d>.service` unit). The v1.4.1 addition (514
# `cosmix-nodeexport`, R6 default `nodeexport`) carries the same
# deployment shape — see Appendix D 1.4.1.
#
# The shared-credential group block sits in the same 500–599 window as
# the daemon-identity block per §2.1. Group GIDs are picked from the
# next free number that does not collide with daemon-identity numbering.
# The 500–509 daemon-only reserve was fully consumed by v1.4.0 (508
# `cosmix-prometheus`, 509 `cosmix-grafana`); the daemon-identity stream
# has now crossed into the 510+ shared zone (511 `cosmix-loki`, 512
# `cosmix-alloy`, 513 `cosmix-pveexport`), skipping 510 which is held by
# `cosmix-tls`. The two-stream non-collision invariant still holds (each
# number appears in at most one stream). v1.4.2 adds a second shared-cred
# group, `cosmix-mesh` (GID 516, signed-inventory read access §3.3); the
# daemon stream skips 516 to preserve non-collision. Future shared-
# credential groups continue from the next free GID that does not collide
# with the daemon-identity frontier (519 as of v1.4.4, after nspawnd 518).
#
# The citizen-identity block lives in its own 600–699 window (§2.1),
# disjoint from 500–599, so citizen numbering never collides with
# daemon or shared-credential numbering. Citizen allocation is
# lowest-free in 600–699 (R7); a UID re-enters the free pool only
# after R8 (mesh-wide purge-verification AND the 30-day quarantine).
#
# The former 506 gap was RESOLVED by v1.3.0: `cosmix-dnsd` consumed the
# R2.a-reclaimed `506` slot (freed from `cosmix-cloudd` by bb87724,
# 2026-05-12 — never-functional pre-deployment reclamation). No daemon-
# stream gap remains. v1.4.0 makes no R2.a claim and introduces no new
# gap: 508 and 509 advance the append-only frontier in sequence, then
# (skipping the existing 510 shared-cred entry) 511/512/513 continue
# the sequence. v1.4.1 appends one further daemon-identity entry (514
# `cosmix-nodeexport`) at the next-sequential slot — no R2.a, no
# tombstone, no new gap. v1.4.2 appends one further daemon-identity
# entry (515 `cosmix-wgd`, the WireGuard mesh control plane, SPEC-13
# D0) at the next-sequential slot — no R2.a, no tombstone, no new gap.
# v1.4.3 appends 517 `cosmix-interactd`, skipping the already-deployed
# `cosmix-mesh` shared-credential GID 516. Its row reserves the stable
# namespace/props ownership identity; §7.3 governs the desktop sink's distinct
# logged-in-user runtime shape.
# v1.4.4 appends 518 `cosmix-nspawnd` (nspawn host executor, nspawn
# cluster-lite C1) at the next-sequential slot — no R2.a, no tombstone,
# no new gap. R6 default ABP/Bus service name `nspawnd`.
# The two-stream non-collision rule (§2.2) is preserved.
#
# Next free daemon UID: 519 (516 is held by the deployed `cosmix-mesh` shared-
#   credential group; 517 is `cosmix-interactd`, 518 is `cosmix-nspawnd`).
# Next free shared-credential GID: 519 (the shared-cred stream holds 510
#   `cosmix-tls` and 516 `cosmix-mesh`; the daemon stream now also holds
#   517 and 518).
# Next free citizen UID: 601 (lowest-free in 600–699; reclaimed UIDs
#   re-enter this pool only after R8 — §2.3 R7/R8, §2.5).
# Tombstones (kept for audit; SHALL NOT be reused per R2): none.
# Citizens in quarantine (retired:, not yet R8-reclaimed): none.
# Citizens reclaimed (UID returned to the R7 free pool): none.
```

The `tier` column is informational only. It does not influence
numeric allocation (R3) and MAY be revised without renumbering.

---
