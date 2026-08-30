# No operational docs (_doc/_plan-style .md) in public repos — enforced

**Date:** 2026-07-28 · **Status:** adopted, enforced by the hygiene gate

## Decision

A markdown file under any underscore-prefixed directory (`_doc/`, `_plan/`,
`_journal/`, `_decisions/`, and anything shaped like them) must not be tracked
in a guarded public repo (amp/cos/mix). Operational docs are control overlay;
they live in the private hub (`$CMCTL/_doc/`, `_plan/`, …).

**The one exception:** `docs/_man/*.md` — the live reference manual that
`mix man` reads (mix and cos both ship one). That is public documentation of
the binaries, updated in the same commit as behaviour changes, and is exactly
what a public repo should carry.

## Enforcement

Rule `underscore_dir_doc` in `_bin/check-public-hygiene.mix`, added to the
pathname pass (`scan_pathnames`), so it runs in every tree-scanning mode:
pre-commit `--index`, pre-push `--range`, `--tree`, `--all`, and annotated-tag
chain walks. It is a **path** rule, not a content rule — a public README may
legitimately *mention* `_doc/…` in prose; the ban is on where a tracked file
sits.

Semantics, judged per path segment so the exemption cannot be smuggled:

- violates: any `*.md` (case-insensitive, trailing whitespace/newline in the
  name included — `$` is end-of-text in the in-process engine, so a bare
  `[.]md$` was dodgeable by `plan.md\n`) whose path contains a directory
  segment starting with `_`,
- unless every such segment is `_man` **directly preceded by** `docs` —
  `_plan/docs/_man/x.md` and `docsx/_man/x.md` still violate.
- Non-markdown files under underscore dirs are out of scope (`src/_etc/`
  legitimately ships systemd units in cos).

Hits are pardonable through the standard fingerprint allowlist (one exact
path, one entry) — same discipline as every other rule: sanitize (move the
file to cmctl), never widen. The rule id is **reserved**: a configured rule
reusing `underscore_dir_doc` (or any duplicated id) is refused at startup,
because fingerprints embed the id and a shared one lets a single pardon cover
two different rules' hits. Probes: T153a–T153m in
`_bin/test-public-hygiene.mix`.

The cold review of this change also surfaced (and the change fixes) a
pre-existing gate defect: `scan_pathnames` did its own second
`ls-tree --name-only` read, which a lying git shim could answer with an
empty-but-successful stream, silently emptying the whole pathname pass.
`scan()` now reads the path set once, `scan_symlinks` cross-validates it
against the full ls-tree and diff-tree views, and the pathname pass and
scope check consume that same validated set.

## Why

2026-07-28: six operational files were found already published in cos
(`desktop/_doc/2026-07-25-dcs-app-shell.md`, `desktop/ctk/_plan/*`,
`desktop/apps/studio/_plan/*`) and moved to cmctl (cos `87c230b`, cmctl
`1c12e450`). CLAUDE.md prose had said "mesh-private data lives ONLY here"
and the doc conventions since the repo split — prose alone failed again,
exactly as it did for mesh identity in 2026-05-29
(`2026-07-25-no-hardwired-mesh-values.md`). Same cure: make the rule
executable.

## Known residual

- The six moved files remain in cos **history**; the rule fires on trees it
  scans, so a future annotated tag pointing at an old commit, or an old branch
  tip carried into `--all`, can surface them. Disposition then: fingerprint
  pardons (history rewrite was declined for the node-name tags too).
- Like the whole gate, this is a local hook — `--no-verify`, another clone, or
  CI bypasses it. The rule above is still the rule; the gate guards this
  machine.
