# Mix list-parameter mutation semantics — OPEN C2 decision

**Status:** ✅ **DECIDED — Option A** (Mark, 2026-07-04: "go with A, I trust your judgement").
Bug 1 is closed **as by-design**: Mix lists pass by value on purpose; `concat` (mix 0.21.8)
+ the return idiom is the sanctioned way to accumulate a sequence. No change to list
semantics. B and C remain documented below should the question ever be reopened.
**Date:** 2026-07-04 · **Author:** Claude (Opus 4.8) · **Class:** C2 (core data-model / language-identity)
**Related:** [[feedback_mix_list_param_push_noop]], [[feedback_mix_sort_by_hang_large_data]],
`$COSMIX` commits `30ff754` (sort_by fix), `5691ac6` (concat).

## The recorded "bug"

`push($listparam, x)` inside a function is a **silent no-op**: Mix lists pass **by value**,
so the helper mutates a throwaway copy and the caller's list is untouched. It "ate the
snare" when building a `.asc` score via helper functions.

## Why this is a C2 decision, not a bug fix

Making `push($param, x)` reach the caller **requires changing Mix's binding model** — there
is no purely-internal patch. It is the *same* rule that makes `$x = $x + 1` stay local and
that gives Mix its no-aliasing simplicity. Any fix is a change to *what Mix is*, shipped to
`mix` running as **root's login shell on 15 nodes**. Per `CLAUDE.md`, core-data-model /
language-identity changes stay behind Mark's gate.

## What is ALREADY shipped (practical impact resolved, semantics unchanged)

- **`concat($a, $b, …)`** (mix 0.21.8) — joins lists into one new list, one level, O(total),
  no mutation. The by-value-safe accumulation primitive: a helper **returns** its events, the
  caller `concat`s them in. Covers the `.asc`/large-sequence build need under any option below.
- Prominent manual docs: `functions.md` by-value binding-model page + `collections.md #concat`.
- (Bug 2 — the `sort_by` hang — is fully fixed and deployed at 0.21.7; unrelated to this.)

## The three options

### A — Keep value semantics; `concat` is the answer  *(RECOMMENDED)*
Declare the by-value behaviour **intended**. `concat` + docs (shipped) is the resolution.
- **Pros:** zero risk; consistent with Mix's pass-in/return/reassign identity; nothing to build.
- **Cons:** `push($param, …)` still won't mutate the caller (by design); footgun remains but is
  now signposted.
- **Reversibility:** n/a (no change). **Effort:** done.

### B — Reference-semantic lists (Python/Ruby/JS style)
`Value::List` becomes a shared mutable handle; `push($param, x)` mutates the caller's list.
- **Pros:** footgun disappears; familiar model.
- **Cons:** identity-level change — every existing script's mental model shifts; aliasing bugs
  become possible; blast radius spans every list clone/compare in the evaluator **plus every cos
  daemon that embeds Mix** (webd, cosmix-mcp, …); inconsistent with by-value numbers/strings/maps;
  **effectively one-way** once scripts rely on aliasing.
- **Reversibility:** very hard. **Effort:** large + risky (design pass required first).

### C — Opt-in mutation (a `ref`/cell value, or `$p&` reference-parameters)
Value semantics stays default; opt in explicitly where you want shared mutation.
- **Pros:** non-breaking (existing scripts untouched); reversible; fixes the trap when opted in.
- **Cons:** new concept + surface (`ref`/cell = a new `Value` variant touching equality/print/
  serialize; `$p&` = new syntax + copy-in/copy-out write-back threaded through the hot call path
  and every call site, with the sync fast-paths bailing when a by-ref param is present); two ways
  to hold a list.
- **Reversibility:** one revert (nothing depends on it yet). **Effort:** medium; needs a design
  sign-off on the surface/semantics before implementation.

## Recommendation

**A.** By-value is a deliberate, coherent part of Mix — it's why there are no aliasing surprises —
and the concrete `.asc`/large-text need is already met by `concat` + the return idiom. B buys
Python-familiarity at the cost of Mix's simplicity and a risky one-way migration across every
embedding daemon; C adds a whole concept for a pattern `concat` already covers.

## Decision hook

- **Pick A** → bug 1 is closed as *by-design*; concat is the sanctioned idiom; nothing more to build.
- **Pick B** → I scope the blast radius across the evaluator + every cos consumer, design it, then implement.
- **Pick C** → I design the surface (cell type vs `$p&` reference-parameters; copy-out vs true-ref;
  maps too?), get your nod on the shape, then implement behind the review loop.
