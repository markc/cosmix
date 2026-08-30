# `_decisions/` — architectural decision records

Settled rulings that constrain future work. Unlike `_plan/` (deleted when
shipped), a decision stays while its ruling is in force; it leaves this
directory only when:

- a **later decision or owner pivot supersedes it** (delete; the superseding
  doc records what it replaced),
- its rulings are **promoted into `_spec/`** or fully absorbed by
  CLAUDE.md/CODEX.md (delete, with a `_spec/CHANGELOG.md` entry and citation
  repoints in the same commit), or
- it ruled on a **retired lane/mechanism** (delete; git history keeps it).

**Naming:** `YYYY-MM-DD-<kebab-slug>.md`, date = git-creation date. No
undated files (normalized 2026-07-23; renames carry citation repoints in the
same commit — never rename without sweeping CLAUDE.md/CODEX.md/README/_spec/
_doc/_plan/memories for the old name).

**Spec promotion:** a ruling graduates to `_spec/` when it is stable,
cross-cutting, and load-bearing (not app-local). The pending-promotion queue
lives in `_plan/2026-07-23-consolidated-backlog.md` § "Decisions / spec
promotions pending".

Last full triage: 2026-07-23 (40 → 26 docs; 3 rulings promoted into
`_spec/10`/`_spec/12`; journal `_journal/2026-07-23-decisions-triage.md`).
