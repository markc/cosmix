# Sanitizable config comes from the private overlay, never code literals

**Date:** 2026-07-29 · **Status:** accepted · **Extends:**
`2026-07-25-no-hardwired-mesh-values.md`

## Rule

Any deployment-specific value — real domain, email, IP address or range, host
name, node name, credential — that code destined for a public tree needs at
runtime MUST be read from a config channel whose *values* are seeded from the
private overlay (this repo). The code itself carries only:

- a **neutral default** (`""`, `example.*`, RFC 5737 addresses), and
- **fail-closed behaviour** when the value is unset: the affected feature
  refuses with a clear "not configured" error rather than falling back to a
  baked-in real value.

Approved channels, in preference order:

1. **Runtime settings store** the code already owns (webd vhost tree:
   `biz_settings` seeded by `_bin/biz_seed_settings.mix`; daemons: SPEC-12
   props namespaces).
2. **Per-deployment config** (`$SITE` vhost config, `*.conf.mix`) whose real
   instances live in this repo's `_etc/` overlay.
3. **Signed inventory** (`/var/lib/cosmix/noded/inventory.signed` + SPEC-13)
   for mesh roster/IP facts.

## Why not gitignored config files?

Considered (Mark raised it as an alternative) and rejected as the *mechanism*:

- A gitignored file is **unversioned** — no history, no review, no rollback,
  and it silently diverges per node.
- It is **unbacked-up by construction**: the one copy lives on the box that
  dies.
- A fresh clone **silently loses it**, and fail-closed code then refuses work
  with nothing in git explaining what value belongs there.

The private repo (`$CMCTL`) already *is* the overlay: versioned, backed up,
reviewed, and deliberately never public. Seed scripts here (pattern:
`_bin/biz_seed_ippool.mix`, `_bin/biz_seed_settings.mix`) are idempotent and
admin-respecting — they write a key only when missing or empty, so redeploys
never clobber an operator edit.

## First application (same day)

The shared vhost tree `_etc/cosmix/vhosts/shared/` was sanitized for its
planned move to the public cos repo:

- `provision_allowed_domains` default `"example.net"` → `""` (fail closed);
  the `svc_allowed_domains()` hardcoded fallback removed.
- LAN2 pool validator `biz_valid_lan2_ip` (hardcoded 192.168.2.x ranges) →
  `biz_valid_pool_ip` driven by new `provision_ip_pool` setting
  (comma-separated `A.B.C.D-N` last-octet ranges; empty = deny).
- PVE host hardcodes (`pve2`/`pve3` in form options, form default, and two
  validators) → new `provision_pve_hosts` setting via `biz_pve_hosts()`
  (first entry = form default).
- House-account bootstrap lookup dropped its legacy `admin@example.net` literal;
  a restored/legacy DB with a missing `house_customer_id` pin re-adopts its
  existing house row via the new privately-seeded `house_bootstrap_email`
  setting (prod DBs with the pin intact never enter this branch).
- All real domain/node/IP mentions in comments and UI strings genericized.

Real values for the new settings are seeded by `_bin/biz_seed_settings.mix` —
**run it against each billing vhost's cms.db BEFORE deploying the sanitized
handlers**. In the unseeded window it is not only *new* provision submissions
that fail closed: CT-reconciliation `adopt` resolutions and UI retries of
existing failed jobs also re-enter the pve/ip validation and refuse (worker
progress/success/failure reports, `retry_clean`/`cancel_clean`/`hold`, and DNS
reconciliation are unaffected). One unseeded-window hazard is NOT self-healing:
on a restored/legacy DB whose `house_customer_id` pin is missing, a request
warmed before `house_bootstrap_email` is seeded can miss the historical house
row and mint + pin a duplicate — later seeding does not repair the pin.
Seed-first makes the whole window zero.

**Known deliberate limit (deferred):** the new settings are not authoritative
end-to-end — the private CT worker and reconciler keep their own pve-host and
pool literals as defense-in-depth. Configuring a host/pool in `biz_settings`
that the workers don't know would have webd accept + enqueue jobs the worker
then fails. Acceptable while both live in this repo and change together (the
seed script and worker comments cross-reference each other); revisit if the
worker ever reads its targets from the job payload alone.

## Enforcement

Once the shared tree moves into cos, the existing public-hygiene pre-commit /
pre-push gate covers every future edit. Until then this rule is discipline,
not tooling — which is exactly why the tree should move soon.
