# ADR: the protocol acronym is ABP — Agent Bus Protocol

- **Date:** 2026-08-14
- **Status:** ACCEPTED — standing name for the protocol across every repo and doc.
- **Decision authority:** Mark (explicit, 2026-08-14), answering a scoping question
  about whether "AMP verb" / "AMP wake" / "AMP citizen" was still correct usage for
  the control-plane vocabulary: *"in this context it should read ABP instead of AMP
  where ABP = Agent Bus Protocol."*
- **Trigger:** an audit prompted by another model surfacing pre-rename AMP doctrine
  on its first review of this repo. The 2026-08-06 cutover renamed the repo, crate
  and wire suffix to `bus` but left the acronym expanding to "Mesh" — a word the
  stack no longer used anywhere.
- **Relationship to neighbours:** completes the rename begun in
  `_plan/2026-08-05-amp-to-bus-rename.md` and gated by
  `_plan/2026-08-06-bus-cutover-gate.md`. Supersedes the 2026-05-24 "Agent Mesh
  Protocol" expansion, which itself superseded "AppMesh Protocol".

---

## Decision

**ABP — Agent Bus Protocol.** The three-letter token is kept, so the wordmark stays
a styled trigram and every "ABP verb / ABP wake / ABP citizen" construction reads
the way its AMP predecessor did. Only the expansion changes.

`AMP` and `ABP` name **the same protocol**. This is a naming correction, not a
protocol version, wire change, or migration: no bytes on the wire move, no header
changes, no node needs redeploying.

## Why

The 2026-08-06 cutover renamed `~/.amp`→`$COSMIX`, `cosmix-lib-amp`→`cosmix-lib-bus`,
`AmpTarget`→`BusTarget`, the `.amp`→`.bus` wire suffix and the mesh TLD
(`mesh_fqdn: "bus"`). After that, "AMP" was the only place left where the stack
still called itself a mesh. The mesh is the *transport substrate*; the protocol is
the *bus*. An acronym whose expansion names a layer it isn't is exactly the kind of
illegibility the three design criteria exist to catch — a fresh agent reading "Agent
Mesh Protocol" next to `cosmix-lib-bus` has to guess which one is current.

"Bus" also matches the lineage the mandate claims. AmigaOS/ARexx is *everything is a
message port*; a bus is what ports hang off. Mesh describes how the WireGuard overlay
reaches other nodes, which is a transport choice made per workload (SPEC 01 §8), not
the application protocol.

## Scope of the sweep

**Uppercase `AMP` → `ABP`; `Agent Mesh Protocol` → `Agent Bus Protocol`.**
878 sites across 87 files: `README.md`, `CODEX.md`, `CLAUDE.md`, `_spec/`,
`_decisions/`, `_doc/`.

**Lowercase `amp` was left untouched everywhere.** It never means the protocol — it
means paths (`~/.amp`), crate names, frontmatter tags, wire suffixes (`.amp`),
addresses (`alpha.amp`) and code identifiers. Those are the 2026-08-06 cutover's
business, and several are amplitude, not protocol: `amp_attack`, `amp_decay`,
`amp_sustain`, `amp_release` in musicd envelopes would be actively wrong to rename.
An uppercase-only rule is mechanically safe and needs no per-site judgement.

**Dated history keeps "AMP" as the contemporaneous name** — `_journal/`, executed
`_plan/` files, and `_spec/CHANGELOG.md`. Rewriting them would forge the record, and
the lineage is documented here and in `_doc/2026-07-23-branding.md` § Changelog, so a
reader who hits "AMP" in a journal can resolve it.

## Consequences

- **Reading old material:** AMP ≡ ABP. Anything dated before 2026-08-14 says AMP and
  is not thereby stale on that count alone.
- **Still owed** (tracked, not done here):
  - `$COSMIX` — the public docs upstream, 105 files. User-facing copy, so it needs
    a taste pass, not a sed, and it goes through the public-hygiene gate.
  - `$COSMIX` (13 files), `$COSMIX` (3), `$COSMIX` (1).
  - `_spec/2026-04-07-05-amp-display-protocol.md` and
    `_spec/2026-04-27-01b-amp-ui-vocabulary.md` still carry `amp-` filenames and
    `status: draft` even though `ui.*` was retired as a substrate primitive on
    2026-07-18 (`_decisions/2026-07-18-amp-as-control-plane.md`). Marking them
    retired is a normative status change, held for Mark.
  - `~/.cache/cosmix/man/amp.md` — a deleted upstream page still served in full by
    `mix man amp` from an expired cache, with no staleness warning. Separate Mix bug:
    a 404 means *deleted*, and should evict rather than fall through to the cache.
  - The memory store and `.agents/skills/cmctl-memory/references/` mirror.

## Rejected alternative

**Dropping the acronym and writing "bus" in prose** ("bus verb", "bus citizen"). This
is what a concurrent session's sweep was drifting toward. Rejected: "bus" is already
overloaded three ways in this stack (the repo, the crate, the `.bus` wire suffix), so
prose loses the ability to distinguish *the protocol* from *the transport* from *the
package* — and a lowercase common noun makes a poor wordmark next to CoS and MIX.

## Execution note (2026-08-16)

The sweep above was EXECUTED on 2026-08-16 — regenerated mechanically from this
ADR's rules after the D1 signed-inventory prerequisites landed (sequencing per
`_plan/2026-08-14-abp-namespace-unification.md` §5). Everything before this
note describes the 2026-08-14 state; its "still owed" list is now resolved:

- **cmctl**: 132 living files swept (1281 `AMP`→`ABP` sites, 11 "Agent Mesh
  Protocol" phrases, 50 spec-path refs) — wider than the original 87-file pass
  because living operational files (`_provisiond/`, `_toolsd/`, `_factory/`,
  `_share/`, unit descriptions, citizen scripts) were included this time. The
  three spec renames (`01`/`02`/`03` amp→bus) applied; this ADR restored.
  Dated history kept AMP: `_journal/`, `_plan/` prose (only path-refs to the
  renamed specs fixed), `_spec/CHANGELOG.md`, the archived
  `_doc/2026-07-17-CLAUDE.md`/`CODEX.md`, tierbench bench records, verbatim
  log quotes, spec `*Supersedes:*` version-lineage lines, and files whose only
  hits are dated notes about the separate 2026-08-06 AMP→BUS label flip.
- **cos**: 5 living doc-comment/design-doc sites. Crate CHANGELOG entries are
  all pre-2026-08-14 → kept.
- **cosmix**: by execution time only `ai.txt` still said AMP (the 105-file
  estimate was overtaken by doc regeneration); its rename-explainer now names
  ABP.
- **mix**: zero hits remained — nothing owed.
- **bus**: zero changes — README's "formerly AMP" is correct history.
- Still genuinely open from the list above: the 05/01b retired-status call
  (Mark's) and the `mix man amp` stale-cache bug.

One correction to "needs no per-site judgement": the uppercase rule is
mechanically safe for the *lowercase boundary*, but text whose subject is the
acronym itself still needs eyes — the sweep turned AGENTS.md's "`AMP` is the
legacy synonym" into nonsense (hand-restored), and verbatim records (a quoted
journald line, spec version-lineage lines) were restored to AMP on review.
