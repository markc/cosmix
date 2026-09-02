# Vendored upstream sources

## Bumping a vendored crate: the whole procedure, in order

Everything below this section is *why*. This is *what*, and it is the part you
have to do. Each step names the section holding its command and its rationale;
none of them is optional, and they are ordered because later ones depend on
earlier ones having happened.

| # | Do | Fails closed by | Where |
|---|---|---|---|
| 1 | Enumerate the fixes to reapply, **before touching anything** | `EXPECT=fixes`, `verdict=0` — plus you, reading the list | the `git log` block below, above § *smithay* |
| 2 | Import the new tarball; update the four version-coupled sites | *nothing* — rows 3–6 are its checks | numbered step 2 |
| 3 | Audit the version-anchored prose; make any edits it forces | **nothing — not a gate.** You, reading two greps | numbered step 2, audit sites |
| 4 | Check the provenance table against the tarball's own metadata | `provenance=0` | numbered step 2 |
| 5 | Check the dependency is routed to the vendored path | `verdict=0` | numbered step 2 |
| 6 | Commit rows 2–5 as **one** commit, then confirm nothing is left dirty | `verdict=0` over five paths | numbered step 2, end |
| 7 | Roll the enumeration baseline forward, **its own commit** | `EXPECT=pristine`, `verdict=0` **and** row 6's check again | numbered step 3 |
| 8 | Reapply each fix from the row-1 list | the tests at row 10 | numbered step 4 |
| 9 | Re-verify the tree against the pristine tarball | `EXPECT=fixes`, `verdict=0` | § *Re-verifying the pristine import* |
| 10 | Run the ten tests | `verdict=0` from all four blocks | § *After a bump: run these ten tests* |

Two rows earn their position rather than merely having one. **Row 3 precedes row
6** because re-verifying the ownership argument can force edits to
`crates/cosmix-comp/src`; audit last and those edits land after the commit gate
that should have caught them, and after the tests that should have run on them.
**Row 4 exists because nothing else covers it** — `cargo tree` speaks only for
source routing, the pristine comparison compares files and never reads the table,
and no test reads it either.

The four numbered steps live under "To move to a new upstream release", which is
inside § *smithay* — they are that crate's procedure. The wgpu pair has its own
smaller version-locked procedure below because its patch is one API pipeline,
not the Smithay defect series.

**Run every command in this file from `src/desktop/`** unless it sets `ROOT` itself.
This is the single most likely way to get a false pass: git commands with a
pathspec that matches nothing print nothing and exit **0**, so the wrong
directory is indistinguishable from a clean result. Measured — from the
repository root the step-1 enumeration reports 0 commits instead of 10, and the
step-3 gate reports clean over an uncommitted tree. The commands that most need
this now pin their own directory; the rest do not, so check where you are.

Two rows are safe to run on their own, against the tree as it stands with no
bump in progress: *Re-verify the tree against the pristine tarball* and *Run the
ten tests*. **No other row is** — several of them commit, and one moves the
enumeration baseline, which silently discards the reapply list for the next
bump. Named rather than numbered on purpose: this sentence previously cited two
row numbers, the table was reordered, and the numbers then pointed at the commit
gate and the baseline roll-forward — an invitation to destroy the very list this
procedure starts from.

Each subdirectory here is a published crate, extracted byte-for-byte from its
crates.io tarball and then patched in-tree. `src/desktop/Cargo.toml` redirects the
dependency with `[patch.crates-io]`, and sets `exclude = ["vendor"]` so
cargo resolves these as their crates.io packages rather than as members of this
workspace.

The **pristine import is its own commit** (`ff4e3c1`); everything after it is a
defect fix or a review correction to one. The intent was one commit per defect,
and that held for fixes 1–4 — but review rounds land as their own commits, so a
defect can span several. **Do not maintain that list by hand, and do not write
down a count that a later commit can change** — how many commits a fix spans in
total, how many the filter excludes, how many times this paragraph has been
wrong. Every one of those this file has carried went stale, including one added
by the commit that made it wrong. Say what the rule is; let the command say what
matches it.

Two things this rule deliberately does *not* forbid, because a stale one is at
least *visible* — nothing enforces either, and the second is only the better bet,
not a guarantee:

- A *closed* set. Naming the three commits that carry fix 5's behaviour, as the
  retention note below does, is history; no later commit can falsify it.
- A count of what this file itself describes — "six of the eight defects", "run
  these ten tests", the retention note's "three ways". A ninth defect changes those
  answers, and nothing stops it landing entirely under `vendor/smithay/` and
  `crates/cosmix-comp/` without touching this file. What makes it the safer kind
  is only that the count sits in the same paragraph as the thing it counts, so
  whoever documents the ninth defect is reading the stale number as they write.
  (This paragraph has proven its own point once already: fix 8 landed
  2026-09-02 and the "eighth defect" this line used to anticipate arrived while
  the line still called it hypothetical.)
  **If you are that person: fix them.** It works twice over: fix 6 was written by
  someone who read these numbers here, found them stale, and was told what to do
  about it by this paragraph — and fix 7 then did the same. Fix 7 also shows the
  limit. It renamed a section heading and left three cross-references pointing at
  the old name, in the one file whose whole job is to still be right after a
  bump. A count in the same paragraph as the thing it counts gets re-read; a
  count in a pointer three hundred lines away does not. `grep -n` for the old
  wording before you consider a rename done.

The distinction is where the referent lives. A count of *git history* — commits
per fix, commits excluded by the filter, times this paragraph has been wrong —
drifts every time someone commits, with nothing in the diff to prompt the fix.
That is the one that has gone stale here, repeatedly. Enumerate:

```sh
ROOT=$(git rev-parse --show-toplevel) &&
BASE=ff4e3c1 &&
: > /tmp/fixlist.txt &&
git -C "$ROOT/src/desktop" merge-base --is-ancestor "$BASE" HEAD &&
git -C "$ROOT/src/desktop" log --reverse --oneline "$BASE"..HEAD -- vendor/smithay/ \
  > /tmp/fixlist.txt
rc=$?
cat /tmp/fixlist.txt
case "$EXPECT" in
  fixes)    [ "$rc" -eq 0 ] && [ -s /tmp/fixlist.txt ] ;;
  pristine) [ "$rc" -eq 0 ] && [ ! -s /tmp/fixlist.txt ] ;;
  *)        false ;;
esac
verdict=$?
echo "verdict=$verdict EXPECT=$EXPECT fixes=$(wc -l < /tmp/fixlist.txt)"
( exit "$verdict" )
```

**`EXPECT` means the same thing here as in § *Re-verifying the pristine import*,
and the two must always agree**: `pristine` is the state right after an import or
a baseline roll-forward, where no fix has been reapplied — an empty list and an
empty diff are both *correct* there. `fixes` is every other time, including right
now and at the checklist row that reads this list. Unset fails on purpose; the
two expectations are opposites and guessing is how a block reports success it has
not earned.

Under `fixes` the assertions catch a baseline that does not resolve, one that is
not an ancestor of `HEAD`, and the destructive case — a baseline rolled all the
way to `HEAD`, which empties the reapply list and would otherwise exit 0.
**They still do not prove the baseline is the *right* commit.** Nudged forward by
one commit it drops that fix from the list silently, and every assertion here
passes. The only thing that proves it is § *Re-verifying the pristine import* run
against the baseline rather than `HEAD` — substitute `"$BASE":` for `HEAD:` in its
`git archive` and use `EXPECT=pristine`, because the baseline is by definition the
commit at which nothing had been reapplied yet. Do that whenever the count
surprises you.

It pins its own directory because the bare form is a fail-open: run
`git log … -- vendor/smithay/` from the repository root and the pathspec matches
nothing, so it prints **no commits and exits 0** — a reapply list that is empty
because you were standing in the wrong place looks exactly like a bump with
nothing to reapply. Measured: 0 from the root, non-zero from `src/desktop/` —
the exact count grows with every commit touching `vendor/smithay/` and has
now gone stale in this sentence three times, so it is deliberately not
written down: run the command. The `&&` is
load-bearing for the same reason as everywhere else here; outside a repository
`rev-parse` fails, and unchained the next command would run with `ROOT` empty.

That is the authoritative reapply list, in order. It stays correct across
ordinary commits with no maintenance — but *not* across a bump, which is the one
event that invalidates it, and the bump procedure below is written around that.
What the filter excludes is any commit that
touched nothing under `vendor/smithay/`. Note that is *not* the same as "commits
touching only this README", which is what this file used to claim — an excluded
commit may well have edited `crates/cosmix-comp/src/protocol/tests.rs` too. The
exclusion is right for a reason worth stating: a bump replaces only the
`vendor/smithay/` subtree, so a commit that changed nothing there has nothing to
reapply into it, and its outside-tree hunks were never reverted in the first
place.

To move to a new upstream release:

1. **Capture the list first**, before touching anything. After step 2 the command
   above no longer describes the fixes you are about to reapply.
2. Replace the tree with the new pristine tarball **in one commit**, and in that
   same commit update every place the old version number is *load-bearing*. There
   are four, and missing the first is the worst thing that can go wrong in this
   whole procedure:
   - `crates/cosmix-comp/Cargo.toml`, which pins `smithay = { version = "=0.7.0",
     … }`. **A `[patch]` whose version does not satisfy the dependency is not an
     error.** Cargo prints `warning: patch smithay v0.8.0 … was not used in the
     crate graph`, resolves the dependency from crates.io instead, and builds
     clean — against the *unpatched* upstream crate, with all eight fixes gone.
     Measured in a throwaway worktree: bumping only the vendored manifest left
     `cargo tree` selecting registry `smithay v0.7.0`. **Fixes 6 and 7 changed what this
     costs.** It used to be that nothing else caught this: the four live tests
     that would are `#[ignore]`d behind a feature, so the default suite stayed
     green. Their four tests are neither, so an unapplied patch now turns the
     ordinary `cargo test -p cosmix-comp` red — measured only indirectly, by removing the
     vendored guard and watching that test fail. That is **not** the same tree an
     unapplied patch produces, which loses all eight fixes and not just one hunk;
     it only reproduces the acceptance path the test reads.
     Do not let that soften row 5: four tests out of
     ten are a smoke alarm, not the check, and they say nothing about the other
     five fixes.
   - `src/desktop/Cargo.lock`.
   - the version, source, sha256 **and upstream commit** in the table below. The
     new tarball ships its own `.cargo_vcs_info.json`; leaving that last field is
     a bump that checksums clean while attributing the tree to the wrong upstream
     revision, which is the one field anyone diffing against upstream starts from.
   - the version, URL and checksum hard-coded in the verification block below —
     otherwise the verifier downloads the old tarball, compares it against the new
     tree, and reports wholesale drift that is entirely its own fault.

   Those four are the sites where a stale version number makes the bump *wrong*.
   They are not every place `0.7.0` appears. The rest are **audit sites** — prose
   whose reasoning was checked against a specific upstream, and which a bump can
   silently falsify without breaking anything:

   ```sh
   grep -rn '0\.7\.0'                crates/cosmix-comp/src   # the version you are LEAVING
   version_status=$?
   grep -rn 'drm_syncobj/mod\.rs'    crates/cosmix-comp/src   # citations of upstream source
   citation_status=$?
   echo "version_status=$version_status citation_status=$citation_status"
   ```

   **This block is not a gate, and deliberately does not re-raise a status.** It
   is two queries whose output you read; there is no exit code that means "the
   prose is still true". Both statuses are captured because otherwise the second
   grep overwrites the first, and a run where the version needle found nothing
   would be indistinguishable from one where it found everything. `1` means *no
   match*, which after a bump is a result to think about — see below — not a pass.

   **The first needle is the version you are leaving, not a constant.** Written
   as a literal it decays after one bump, and decays in the worst direction:
   history correctly keeps saying `0.7.0` forever, while the one comment you
   re-verified now says `0.8.0` — so the next bump greps `0.7.0`, gets a
   plausible five hits, and misses the only site that mattered. At the bump after
   this one, grep `0.8.0`.

   The second needle decays more slowly, which is why both are here: it keys on
   upstream *source* rather than upstream *version*, and it catches sites that
   carry no version string at all and the first query cannot see — today, the
   `destruction_hook` citation and the `Request::Destroy` one, both in `tests.rs`.
   More slowly is not never — if upstream ever moves `drm_syncobj/mod.rs`, this
   needle goes stale in exactly the same way, and the one historical hit (the
   stock-0.7.0 chain description, also in `tests.rs`) will keep it looking
   productive while the sites it exists for have gone quiet.

   So neither of these is an assertion, and neither one's exit status means
   anything — they are *read the output* queries, and the failure you are
   guarding against is a short answer, not an error. **Zero hits from either
   needle is not a clean result; it means the needle decayed.** This tree has
   carried at least three upstream citations in `tests.rs` since the import, and
   the second needle sees exactly those three. It no longer sees the
   version-anchored ownership argument in `protocol/mod.rs`: as of 2026-08-02 that
   argument cites its two upstream safety nets by symbol rather than by
   `drm_syncobj/mod.rs:LINE`, which is what the first needle is for — `0.7.0`
   still finds it. Do not "fix" the citation-needle count by putting a file path
   back into that comment; the line numbers are the thing that rotted. If either
   query comes back empty or markedly shorter than that, find out why before
   continuing — the likely cause is that upstream renamed the thing you are
   grepping for, which is also when the audit matters most.

   Six version hits today: four test comments naming what stock 0.7.0 did wrong,
   and one module doc-comment in `bindings.rs` recording which release a design
   consult was checked against. Re-read those five, but they are history and
   mostly stay true. The sixth is not history and must be re-verified by hand:
   the comment in `protocol/mod.rs` above `.take()` on `committed.release_point`
   justifies CosMix taking sole ownership of the committed release point *on the
   grounds that* it thereby disables two named upstream safety nets, now cited by
   symbol — `Cacheable::merge_into` and `destruction_hook`, both in
   `drm_syncobj/mod.rs`. If upstream has changed what either of those does, the
   ownership argument itself no longer holds.
   **Do not bulk-replace the version string in these**; the number is a claim
   about what was verified, and editing it without re-verifying converts a stale
   note into a false one.

   Those two were cited by line until 2026-08-02, as `:99-116` and `:267-273`.
   The second had already gone stale — our own destruction-hook fix moved the
   signalling, so `:267-273` pointed at unrelated prose and the comment still
   claimed the hook signalled only pending and current when it had been changed
   to signal the cached states too. That is the second time line numbers into a
   tree we ourselves patch have rotted, which is why they are gone: a line number
   into `vendor/smithay` is invalidated by our own fixes, not just by a bump. Cite
   symbols here.

   Then confirm the provenance table agrees with the tarball it describes. This
   is the one of the four sites with no other check on it — `cargo tree` below
   speaks only for source routing, the pristine comparison compares *files* and
   is blind to the table, and no test reads it. Left stale it attributes the new
   tree to the old upstream revision, which is the field anyone diffing against
   upstream starts from:

   ```sh
   ROOT=$(git rev-parse --show-toplevel) &&
   sha=$(grep -o '[0-9a-f]\{40\}' "$ROOT/src/desktop/vendor/smithay/.cargo_vcs_info.json") &&
   grep -q "^| upstream commit | \`$sha\`" "$ROOT/src/desktop/vendor/README.md"
   provenance=$?
   echo "provenance=$provenance"   # 0 = the table row names the tarball's commit
   ( exit "$provenance" )          # ...and so does the block, for anything scripting it
   ```

   Measured all three ways: 0 against the tree as committed; non-zero against a
   copy with the table's sha altered; and still non-zero when that copy *also*
   carries the correct sha in a prose line elsewhere. That third case is why the
   pattern is anchored to the table row and not a bare `grep -q "$sha"` over the
   file — the bare form assumes the sha appears nowhere but the table, and this
   document's own habit is to quote commits in prose. It checks the direction
   that goes wrong: the tarball carries the truth, the table is the
   hand-maintained copy.

   Then confirm the patch actually took:

   ```sh
   cargo tree --offline --locked -p cosmix-comp --all-features --target all \
     -i smithay > /tmp/tree.log 2>&1
   rc=$?
   cat /tmp/tree.log
   [ "$rc" -eq 0 ] && grep -q 'smithay v[0-9.]* (.*/vendor/smithay)' /tmp/tree.log
   verdict=$?
   echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
   ( exit "$verdict" )
   ```

   The grep is the whole check. `cargo tree` exits **0** when it resolves the
   registry crate — that is a successful resolution, just of the wrong package —
   so a block ending on cargo's status passes in exactly the case it exists to
   catch. Measured: with the patch version-mismatched, `cargo tree` prints
   registry `smithay v0.7.0` and exits 0.

   It must name the vendored path — `smithay v0.7.0 (…/src/desktop/vendor/smithay)`.
   A bare `smithay v0.7.0` with no path in parentheses is the registry crate, and
   means the patch was dropped. It catches more than a version mismatch: removing
   `[patch]`, pointing it elsewhere, or changing the dependency's source all fail
   the same assertion.

   `--locked` is not decoration. `--offline` does not imply it, so without it
   cargo will quietly repair a `Cargo.lock` you forgot to include in the import
   commit and then exit 0 — letting step 3 advance the baseline onto an
   incomplete import. `--all-features --target all` keeps the answer honest if
   the dependency ever becomes optional or target-specific; today it is neither,
   so the plain form gives the same result.

   Be clear about what this proves. It is a **source-routing** check: cargo is
   building the vendored path rather than the registry package. Run here, before
   step 4, the tree is deliberately pristine — so it passes with none of the
   fixes reapplied, and it can say nothing about whether `commit_hook`,
   `destruction_hook`, `CachedState::cached`, `destroy_syncobj_handle`,
   `DeviceFd::downgrade` or `DmabufParamsData::create_dmabuf` are present. The
   ten tests below answer a *different*
   question — they establish the **behaviours** of fixes 1-7, not the presence
   of any named symbol; a restructured upstream could pass all ten with every
   one of those names gone. Symbol presence is established by reading the
   source, and nothing automated here checks it — except the default suite's
   source-read guards (grep tests.rs for `vendored_` to enumerate them; a
   count written here went stale within one round, twice), all red on a
   plain `cargo test -p cosmix-comp` after a bump that breaks what they
   pin: fix 8's presence, the two xwm orderings cosmix-comp's unmap
   teardown and its serial classifier ride (the PRIMARY instrument for
   those orderings; the compositor's runtime `warn!` is only the secondary
   signal), the xwayland_shell.rs null population, and the routed-to-
   vendor/smithay identity every other pin silently depends on (two axes:
   Cargo.lock's missing `source =` line proves PATH resolution, the
   manifest text proves WHICH path — the lock cannot distinguish
   `vendor/smithay` from a repointed `vendor/smithay-next`). Note the
   front line for a dropped or mismatched patch in THIS workspace is not
   the plain-build warning documented above: `cosmix-comp`'s
   dev-dependency demands the CosMix-invented `cosmix_offline_test`
   feature unconditionally, so the default suite fails resolution HARD —
   the routing guard is defence-in-depth for the compound case (that
   demand removed too) and for the repoint, which errors nowhere. A bump
   that reds an ordering or population pin needs a human to re-verify in
   the new tree, not a mechanical count update. Worth re-running this
   command after step 4
   as well, since it is cheap and a reapply can go wrong in the other direction.

   One more limit, because it is the difference between "verified" and
   "verified in my working directory": `--locked` stops cargo *writing* a
   lockfile, but it says nothing about whether `src/desktop/Cargo.lock` was
   **committed**. A valid-but-uncommitted lockfile passes this command and every
   test below, and a clean checkout then gets the incomplete import. Confirm the
   commit, not the directory:

   ```sh
   ROOT=$(git rev-parse --show-toplevel) &&
   : > /tmp/dirty.txt &&
   git -C "$ROOT/src/desktop" status --porcelain \
     Cargo.lock crates/cosmix-comp/Cargo.toml vendor/smithay vendor/README.md \
     crates/cosmix-comp/src > /tmp/dirty.txt
   rc=$?
   cat /tmp/dirty.txt
   [ "$rc" -eq 0 ] && [ ! -s /tmp/dirty.txt ]
   verdict=$?
   echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
   ( exit "$verdict" )
   ```

   `git status --porcelain` exits **0** whether it prints nothing or prints five
   dirty files — its status answers "could I read the tree", not "is the tree
   clean". So the emptiness test is the check, and without it this block passed
   over an entirely uncommitted import.

   `crates/cosmix-comp/src` is in that list because the audit above can require
   edits there — re-verifying the ownership argument may change the comment, or
   the code. Without it the gate is blind to exactly the work the audit
   generates.

   **`vendor/README.md` is in that list because two of step 2's four sites live
   in it** — the provenance table and the verification block's hard-coded version,
   URL and checksum. Leave those unstaged and every check on this page still
   passes in your working directory, while a clean checkout gets the new tree
   described by the old provenance and verified against the old tarball. An
   earlier draft of this very command omitted it, and the commit that fixed the
   command was itself invisible to it — the whole commit touched nothing but this
   file. Expect this path to go dirty again in step 3; that is the baseline
   roll-forward, and it is deliberately a separate commit.

   Empty output means what you verified is what you committed — and this one
   pins its own directory because it has **two** ways to lie, both measured. Run
   it with `src/desktop/`-prefixed paths from `src/desktop/` and git warns
   `could not open directory 'src/desktop/src/desktop/'` to stderr, then exits 0 with no
   output. Run the unpinned form from the repository root and there is not even a
   warning: the pathspec simply matches nothing, and an entirely uncommitted
   import reports clean. Both are indistinguishable from a pass.
3. **Roll the baseline forward**: change `ff4e3c1` in the enumeration command
   above to the sha of the commit you just made. This has to be its own commit —
   an import commit cannot contain its own sha — and it has to happen *before*
   any fix is reapplied. Then re-run the enumeration: it must come back **empty**.
   That is the check, and it is worth doing, because getting this wrong is silent.

   **Empty is necessary, not sufficient — run the committed-state check from step
   2 again afterwards.** The enumeration reads the sha out of your *working copy*
   of this file, so an edit you have not committed produces exactly the required
   empty result: right after the import, `HEAD` is the import commit, and any
   range ending at `HEAD` that starts there is empty whether or not the baseline
   was ever written down. Verified — `HEAD..HEAD` over `vendor/smithay/` is empty
   and exits 0. A clean checkout then still carries the old baseline, which is
   the precise state that makes the *next* bump replay a superseded import.
   A pristine import touches `vendor/smithay/` like any other commit, so a
   baseline left pointing at the previous import puts the superseded import in the
   *next* bump's list — where "walk the list and reapply each" replays an entire
   old upstream tree over the new one. **Nothing downstream catches that.** The
   eight `cosmix-comp` tests live outside `vendor/`, and replaying the fix commits
   restores the two vendored ones, so all ten pass — against the wrong upstream
   version. They test our fixes' behaviour, not which upstream the fixes are
   sitting on. That is the whole reason the empty enumeration is a check worth
   performing rather than a formality.
4. Walk the captured list and reapply each — every fix stands or falls on whether
   upstream has since fixed it.

Dropping one is never just
`git`-dropping the commit: the relationship between a fix and the test that
proves it differs per fix, so read the retention note below before dropping
anything.

## wgpu / wgpu-core 29.0.4 initial-usage patch

| item | value |
|---|---|
| source | `https://static.crates.io/crates/wgpu/wgpu-29.0.4.crate` |
| sha256 | `76e8840e1ba2881d4cbb18d2147627a56af426ff064c0401eb0c8410c6325d07` |
| source | `https://static.crates.io/crates/wgpu-core/wgpu-core-29.0.4.crate` |
| sha256 | `2f519832254e56965a9940c4af57dcb75f702b6f6fa4a0b172f685395843a4d7` |
| upstream commit | `e99f5305ded96ff7006f0714d043a7f735bd45c2` |
| imported | 2026-08-07 |
| licences | MIT OR Apache-2.0; both licence files retained in each directory |

`vendor/wgpu` and `vendor/wgpu-core` are the published crates, with one narrow
API pipeline added. Stock `Device::create_texture_from_hal` is unchanged and
continues to seed `TextureUses::UNINITIALIZED`. The additive
`create_texture_from_hal_with_initial_usage` path carries one caller-supplied
`TextureUses` through wgpu's public device API and core backend into
`wgpu-core::Device::create_texture_from_hal`, where it replaces
`UNINITIALIZED` only for that call. It returns the installed tracker seed so the
offline noop/HAL regression can observe the patched state before encoding the
first `RESOURCE` use. No wgpu-hal source is patched.

The only patched upstream files are:

- `vendor/wgpu/src/api/device.rs`
- `vendor/wgpu/src/backend/wgpu_core.rs`
- `vendor/wgpu-core/src/device/global.rs`
- `vendor/wgpu-core/src/device/resource.rs`

The compositor bridge is the sole new-API caller. Its raw Vulkan acquire ends
in `SHADER_READ_ONLY_OPTIMAL`, so it supplies `TextureUses::RESOURCE`; first
sampling then begins in the layout the image actually has instead of deriving
`VK_IMAGE_LAYOUT_UNDEFINED` and discarding client contents.

Cargo silently ignores an unsatisfied `[patch.crates-io]` version. After any
manifest, lockfile or wgpu version change, prove both overrides are active:

```sh
cargo tree -p cosmix-wgpu-dmabuf -i wgpu@29.0.4 \
  | grep -F 'wgpu v29.0.4 (' | grep -F '/vendor/wgpu)'
cargo tree -p cosmix-wgpu-dmabuf -i wgpu-core@29.0.4 \
  | grep -F 'wgpu-core v29.0.4 (' | grep -F '/vendor/wgpu-core)'
```

The compile-time half of the same assertion is the call to the additive method
in `crates/cosmix-wgpu-dmabuf/src/import.rs`: registry wgpu 29.0.4 does not
provide that symbol. The noop/HAL test additionally requires the returned seed
to be `RESOURCE` and encodes the first matching use:

```sh
cargo test -p cosmix-wgpu-dmabuf \
  import::tests::hal_import_seed_makes_the_first_resource_use_already_initialised -- --exact
```

To refresh either crate, extract the new published tarball into a temporary
directory first, verify its checksum and `.cargo_vcs_info.json`, then replace
the matching vendor directory wholesale before reapplying only the four file
changes above. Update both exact dependency versions together, run the two
`cargo tree` assertions before any behavioural test, and inspect the pristine
comparison rather than copying a registry working directory whose provenance
has not been checked.

## smithay

| | |
|---|---|
| version | 0.7.0 |
| source | `https://static.crates.io/crates/smithay/smithay-0.7.0.crate` |
| sha256 | `740cea6927892bc182d5bf70c8f79806c8bc9f68f2fb96e55a30be171b63af98` |
| upstream commit | `a166cf4c94b5aedc332a65aa1dd753e8148829c3` (`.cargo_vcs_info.json`) |

The sha256 above is the one `Cargo.lock` recorded for the registry package
before the patch was added, so it pins exactly what the unpatched build used.

Vendored because Cargo can only replace a **whole package source**. Six of the
eight defects we need fixed live in private items — `commit_hook`, `destruction_hook`
and the `GetSurface` handler in `src/wayland/drm_syncobj/mod.rs`;
`DrmTimelineDeviceSpecific` in `src/wayland/drm_syncobj/sync_point.rs`;
`CachedState`'s private `cache` field in `src/wayland/compositor/cache.rs`,
which fix 2 reaches by adding a `cached()` accessor beside it; and
`DmabufParamsData::create_dmabuf` in `src/wayland/dmabuf/mod.rs`, a private
method on a private type reached only from that module's own dispatch — so
there is no downstream Rust
interposition, trait impl, or wrapper type that can reach them. A hosted fork,
a submodule, or a build-time patch step were all rejected as heavier or less
legible than tracked source.

**Fix 7 is the exception, and it is worth being precise about why it still needs
vendoring.** It lives in `KeyboardInnerHandle::set_focus`, a `pub fn` in
`src/input/keyboard/mod.rs`, and what is missing is a call *inside* one of its
arms. A public item does not make it reachable: nothing downstream can inject a
statement into a function body, and `set_focus` is the only way to clear
keyboard focus, so there is no wrapper to interpose. What downstream *can* do —
and this is the difference from the other six — is **compensate** at each call
site by invoking `SeatHandler::focus_changed` itself after every
`set_focus(…, None, …)`. That was considered and rejected: it must be repeated
at every present and future call site (we have three), it cannot see the focus
clears smithay performs internally, and it is wrong in the presence of a
keyboard grab, which may defer or refuse the focus change — leaving the call
site to announce a change that did not happen while smithay and the client still
agree focus is held. Compensation is reachable; correctness is not.

**Fix 8** (2026-09-02): `XWaylandClientData::disconnected`
(`src/xwayland/xserver.rs`) is made idempotent. Upstream does
`self.child.lock().unwrap().take().unwrap()`, so the second `disconnected`
for the same client panics on the `None`. Two kills can genuinely reach one
XWayland client from callers that do not know about each other: `Drop for
XWayland` kills it on teardown, and `cosmix-comp`'s explicit-sync fault path
(`disconnect_explicit_sync_client`) kills every client owning a DMA-BUF use
by client key, with no XWayland lifecycle knowledge — and Xwayland speaks
`linux-drm-syncobj-v1`. Both kills can land in one dispatch batch with no
`cleanup()` between them (`get_client_mut` returns killed clients and
`Client::kill` has no killed-guard), so `disconnected` runs twice. The fix
takes the child once, and a second call logs
`Xwayland client disconnected twice` and returns — the log line is
deliberate, because the removed panic was, accidentally, the only
enforcement of the compositor's one-deliberate-kill discipline. Like the
others this is non-interposable, though not via privacy: the `ClientData`
instance is constructed inside `XWayland::spawn`, so no downstream wrapper
can stand between the backend and this method. Its presence guard is
`vendored_xwayland_disconnected_stays_idempotent` in `cosmix-comp`'s
**default** test suite (no feature, no device, not `#[ignore]`d): it reads
the vendored source and reds if the `take().unwrap()` shape returns, so a
bump that silently drops this fix fails plain `cargo test -p cosmix-comp`.
Genuine upstream bug, worth reporting.

### Downstream API additions

Separate from the eight defect fixes above, `PointerHandle::current_pressed`
mirrors `current_location`: it locks the outer handle, clones the physically
pressed button list, and makes removal reconciliation available without entering
a pointer grab.

`LibSeatSession::new_with_deferred_disable` is the narrow session-lifecycle
extension used by `cosmix-comp`'s resumable KMS path. The ordinary
`LibSeatSession::new` notifier keeps its upstream `seat.disable()`-before-
consumer-callback behaviour. The deferred notifier cannot defer device
revocation: seatd 0.9.3
`seatd/seat.c:589-619` deactivates every device (`drm_drop_master` or evdev
revoke at lines 438-456) before sending the disable event, while
`libseat/backend/logind.c:363-403` sends `PauseDeviceComplete` immediately after
the callback returns and its `disable_seat` at lines 215-218 is a no-op. It
instead refuses new opens in the raw `SeatEvent::Disable`
callback, publishes a one-shot handle for the later protocol-level
`seat.disable()` acknowledgement, and keeps its calloop source dispatching while
a bounded helper waits. That lets the consumer order bounded cleanup and device
hand-back before acknowledging without deadlocking the session loop. An
acknowledgement, timeout, or vanished acknowledger lets the source call
`seat.disable()`; it then publishes `Paused` with a resumable/terminal outcome.
An enable received during that window is held until after `Paused`, so consumers
always classify the same pause generation before activation. The timeout is
supplied by the downstream caller rather than fixed in Smithay.

`X11Surface::set_wl_surface_offline`, `X11Surface::set_motif_hints_offline`
and `X11Surface::set_wl_surface_serial_offline`
(`src/xwayland/xwm/surface.rs`, added 2026-09-02 for the XWayland X-1 test
suite) are test-only escape hatches: `cosmix-comp`'s deterministic protocol
tests fabricate offline `X11Surface`s — a live X11 connection is
unconstructible in a unit test — and still need input forwarding, metadata
lookups, and the real `is_decorated()` predicate to reach real state. The
first sets the associated `wl_surface` directly, bypassing the xwayland-shell
serial handshake; the second sets the raw `_MOTIF_WM_HINTS` fields, bypassing
the X11 property machinery; the third sets the raw `WL_SURFACE_SERIAL`,
bypassing the client message, so the unmap ordering pin's serial classifier
(legal unpaired-serial null vs vendored ordering flip) is drivable offline in
both directions. Production code must never call any: all are
gated behind the vendored crate's `cosmix_offline_test` feature, which only
`cosmix-comp`'s `[dev-dependencies]` smithay entry enables — a plain
`cargo build --release` graph carries no dev-dependencies, so the setters do
not exist there and a production call site fails to compile. (`#[cfg(test)]`
was deliberately not used: it is false when the vendored crate is compiled as
a dependency, which would silently compile the setters into every build.)

### Re-verifying the pristine import

```sh
ROOT=$(git rev-parse --show-toplevel) &&
: > /tmp/pristine.diff &&
rm -rf /tmp/smithay-pristine /tmp/smithay-tracked &&
mkdir /tmp/smithay-pristine /tmp/smithay-tracked &&
curl -fsSLo /tmp/smithay-0.7.0.crate https://static.crates.io/crates/smithay/smithay-0.7.0.crate &&
echo "740cea6927892bc182d5bf70c8f79806c8bc9f68f2fb96e55a30be171b63af98  /tmp/smithay-0.7.0.crate" |
  sha256sum -c - &&
tar -xzf /tmp/smithay-0.7.0.crate -C /tmp/smithay-pristine --strip-components=1 &&
git -C "$ROOT" archive -o /tmp/smithay-tracked.tar HEAD:src/desktop/vendor/smithay &&
tar -xf /tmp/smithay-tracked.tar -C /tmp/smithay-tracked &&
git diff --no-index /tmp/smithay-pristine /tmp/smithay-tracked > /tmp/pristine.diff 2>&1
rc=$?
cat /tmp/pristine.diff
case "$EXPECT" in
  fixes)    [ "$rc" -eq 1 ] && [ -s /tmp/pristine.diff ] ;;
  pristine) [ "$rc" -eq 0 ] && [ ! -s /tmp/pristine.diff ] ;;
  *)        false ;;
esac
verdict=$?
echo "verdict=$verdict EXPECT=$EXPECT"   # 0 = pass. ANYTHING else is a failed bump.
( exit "$verdict" )
```

**Set `EXPECT` before running this** — `EXPECT=pristine` immediately after
importing the tarball, with no fix reapplied yet; `EXPECT=fixes` everywhere
else, including at the checklist row that runs it. Unset is a deliberate
failure, not a default, because the two expectations are exact opposites and
guessing wrong is how this block last reported success it had not earned:
`git diff --no-index` exits **0** on identical trees, so a block ending on its
status passed with **none of the local fixes reapplied** — the single worst
outcome a bump can produce, and the one this whole file exists to prevent.
Measured at the pristine baseline commit: empty diff, exit 0. At `HEAD`: 498
lines, exit 1.

Runs from `src/desktop/`, like everything else here, and leaves you there. Three
things in it are load-bearing rather than style, and each is a way an earlier
version of this recipe reported success it had not earned:

- **`git -C "$ROOT"`, not a bare `git archive`.** `git archive HEAD:<path>`
  resolves the path relative to the current directory, so from `src/desktop/` a bare
  invocation asks for `src/desktop/src/desktop/vendor/smithay` and git answers with an
  empty archive and **exit status 0**. The comparison then reports the whole
  crate as removed, which reads as catastrophic drift rather than as the operator
  error it is. Anchoring to the repository root fixes that without a `cd`, which
  matters because a `cd` would strand the shell at the root and every test
  command below resolves `vendor/smithay/Cargo.toml` relative to `src/desktop/`.
- **`&&` throughout.** Measured: with both `/tmp` directories present, identical,
  and unremovable, an unchained version runs its `rm`, `curl` and both `tar`
  steps, watches every one of them fail, and then compares the two survivors and
  exits **0**. A clean bill of health from a comparison that never happened is
  strictly worse than no recipe. Chained, the same setup stops at the first `rm`
  and exits 1.
- **`sha256sum -c`, not `sha256sum` and an eyeball.** The checksum is only a
  check if something fails when it does not match. `curl -f` is the same point
  one step earlier: without it an HTTP error page is downloaded, saved, and
  extracted as though it were the crate.

`--strip-components=1` into a directory this recipe created is what keeps the
*pristine* side pristine: `tar -xzf` overlays rather than replaces, so extracting
into `/tmp` and letting the tarball name its own directory silently inherits
whatever an earlier run left there.

Against the pristine-import commit that diff is empty. Against a later commit it
is exactly the set of local fixes, which is the point of keeping them separate.

**Compare the tracked content, not the working directory.** Two earlier versions
of this command were wrong in opposite directions, so both traps are worth
naming:

- Pointing `git diff --no-index` straight at `vendor/smithay` compares build
  output too. The tarball ships a `.gitignore` listing `target`, but `--no-index`
  does not consult it, so running the vendored tests documented below leaves a
  ~600M build tree inside the compared directory — measured once at 2738 files
  changed. It does not merely make the diff long; it hides real source drift in
  the noise, which is the only thing this command exists to catch.
- Switching to `diff -ru -x target` to dodge that trades the noise for blindness.
  Plain `diff` does not compare file modes: drop the `0755` on `compile_wlcs.sh`
  — the one executable in the tree — and `diff` exits clean while the script no
  longer runs. Measured: `git diff` reports that mode change, `diff -rq -x target`
  reports nothing. It also follows symlinks instead of comparing link text, and
  `-x target` hides that basename at any depth, so a future `src/target` would
  vanish silently.

`git archive HEAD:…` sidesteps all of it on the vendored side by construction: it
emits exactly the tracked files with their recorded modes, so untracked build
output cannot reach the comparison from there and no exclusion pattern is needed.
The pristine side has no such guarantee — it is whatever is sitting in `/tmp` —
which is what the deletion above is for. Comparing a commit other than `HEAD` is
a matter of naming it.

The same construction sets the one precondition this recipe has: it compares
`HEAD`, so **uncommitted edits to the vendored tree are invisible to it**. Commit
first, or you are verifying the tree you had rather than the one you have.

Note the tarball ships a `.gitignore` listing `Cargo.lock`, so the import needed
`git add -f` to stay complete; the file is tracked now, and `.gitignore` does not
apply to tracked files, so no later operation has to remember this.

### After a bump: run these ten tests

Nothing runs the first four for you. They are `#[ignore]`d and behind the
`explicit-sync-live-test` feature, because each opens a real DRM render node and
letting them into the default run would break the offline gate that asserts
`dev_dri=0 drm_ioctl=0` for every test binary. So a bump that silently drops one
of **fixes 1-4** leaves the default suite **green**, and these four tests are the
only thing that says otherwise. The same is true of fix 5, whose two tests are
`#[ignore]`d and live in a crate the workspace excludes. It is **not** true of
fixes 6 and 7: their four tests are ordinary, so dropping either of those turns
`cargo test -p cosmix-comp` red on its own — which is why the blocks for those
two assert a *count* rather than a result, since what the default suite cannot
notice is a test that has stopped existing.

```sh
COSMIX_TEST_RENDER_NODE=/dev/dri/renderD128 \
  cargo test -p cosmix-comp --features explicit-sync-live-test -- \
  --ignored --test-threads 1 \
  explicit_sync_buffer_without_points_reports_no_acquire_point \
  synchronized_subsurface_destruction_signals_committed_release_point \
  syncobj_extension_destroy_then_surface_destroy_signals_cached_release_point \
  recreated_syncobj_surface_still_validates_commits > /tmp/four.log 2>&1
rc=$?
cat /tmp/four.log
[ "$rc" -eq 0 ] &&
  [ "$(grep -c '^test result: ok\. 4 passed; 0 failed' /tmp/four.log)" -eq 1 ]
verdict=$?
echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
( exit "$verdict" )       # ...and so does the block, for anything scripting it
```

**Neither half of that verdict is redundant, and the count is the half people
drop.** Those names are *filters*, and libtest exits **0 when a filter matches
nothing** — measured in this exact binary: a deliberately absent name prints
`test result: ok. 0 passed; 0 failed; … 454 filtered out` and exits 0. So a bump
that *deletes* a test reads exactly like a bump that passes it, and cargo's
status alone can never tell you. Equally, the count alone cannot: read it
without `$status` and a compile error looks like a missing test.

Three details that are not style:

- `-eq 1`, not `-gt 0`. `grep -c` prints the number of matching *lines* and exits
  0 whenever there is at least one, so a two-binary run reporting `4 passed`
  twice satisfies a `-gt 0` check. Measured: two such lines print `2`, exit 0.
- The verdict is a `[` test, not a `grep`, and the block ends by *re-raising* it.
  Two drafts of this block failed open here, in different ways, and both were
  measured. The first ended on `grep '^test result'`, which matches the `FAILED.`
  summary just as happily as the `ok.` one — the same defect as
  `cargo test … | grep`, which reports grep's status instead of cargo's. The
  second computed the verdict correctly and then ended on `echo "verdict=$?"`,
  so the block's own status was the echo's: it printed `verdict=1` and exited 0.
  A human reading the output catches that; a CI job or `bash block.sh` checking
  `$?` does not. Hence `( exit "$verdict" )` — a subshell, so pasting the block
  into an interactive shell reports the status without closing the terminal.
  The `echo "…=$?"` form was then written a **third** time, in the provenance
  check added two commits later, by someone who had just fixed it here. Treat it
  as this document's most repeatable mistake. **Every block that is a gate ends
  by re-raising its own status.** When that sentence was first written it was
  false of five of the eight blocks here — three git commands and `cargo tree`
  that exit 0 on the failure they exist to catch, and a `git diff --no-index`
  that exits 0 on the worst outcome of all. They now assert. The one block that
  still does not re-raise is the pair of audit greps, and it says outright that
  it is not a gate: there is no exit code that means "this prose is still true".
  If you add a block, it is one or the other, and it must say which.
- Every block that writes a `/tmp` file **truncates it first**, as its own step in
  the `&&` chain. Without that, a chain that short-circuits early never reaches
  its redirect, so the `cat` that follows prints the *previous run's* file — an
  empty reapply list appears as ten commits, a dirty tree appears clean. The
  verdict is still correct in that case; the output an operator reads is not, and
  the output is what they act on.
- `450 filtered out` is *not* asserted. It tracks the crate's total test count
  and is expected to drift; pinning it turns every unrelated new test into a
  failed bump.

Measured both ways: `verdict=0` against a real run, and non-zero against the
zero-match run above.

One test per fix for the first four fixes, in commit order. Each was observed red
against stock 0.7.0 for the reason it names before its fix landed, so the default
reading of a red here after a bump is that the fix is still needed and was
dropped. **Default, not certainty** — there is one known way these go red with
every fix intact, and it is written up at the end of this file: upstream may
conformantly switch the buffer-with-neither-sync-point error from code 4 to code
5, which turns the first and fourth red together. Two reds, those two, is the
signature; check that before concluding a fix was dropped.

Fix five's **two** tests live inside the vendored Smithay crate so they can
inspect the private imported syncobj handle. `vendor/*` is excluded from the
desktop workspace, so the workspace command above cannot discover them; run them
separately:

```sh
COSMIX_TEST_RENDER_NODE=/dev/dri/renderD128 \
  cargo test --manifest-path vendor/smithay/Cargo.toml --lib \
  --no-default-features --features backend_drm,wayland_frontend -- \
  --ignored --test-threads 1 imported_ > /tmp/fix5.log 2>&1
rc=$?
cat /tmp/fix5.log
[ "$rc" -eq 0 ] &&
  [ "$(grep -c '^test result: ok\. 2 passed; 0 failed' /tmp/fix5.log)" -eq 1 ]
verdict=$?
echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
( exit "$verdict" )       # ...and so does the block, for anything scripting it
```

**`2 passed` is the assertion here, and it is not pedantry** — this command has a
live way to pass while the fix is half-reapplied. `imported_` is a substring
filter over however many tests exist, and the second test arrived a commit later
than the first: at `3279844` the file holds **one** `imported_` test, at
`61c0feb` it holds **two** (`git grep -c 'fn imported_' <sha> --
vendor/smithay/src/wayland/drm_syncobj/sync_point.rs`, measured). So a
step-4 walk that reapplies `3279844` and stops leaves the device-keyed leak in
place, runs one test, passes it, and exits 0. Reapply nothing at all and the
filter matches zero tests — still exit 0. Only the count separates the three
cases.

They are gated by `#[ignore]` alone rather than by a new vendor-side cargo
feature. A feature would be permanent cost at every bump and buys nothing: the
workspace exclusion already means these cannot be reached by
`cargo test --workspace`, so they can never enter the offline gate that asserts
`dev_dri=0 drm_ioctl=0`.

`…_on_device_update_and_timeline_drop` covers both destruction sites. Only its
first assertion goes red when the `Drop` is removed, since the test stops there;
the second guards the final-destruction site against a partial fix that destroys
the replaced handle but not the last one.

`…_when_only_a_device_fd_clone_keeps_the_file_open` covers something the first
cannot see. The first holds a live `DrmDeviceFd` throughout, so destruction keyed
on `self.device.upgrade()` would satisfy it just as well — which is why it alone
could not prove the fix. The second drops the last `DrmDeviceFd` and keeps the
file open only through a bare `DeviceFd` clone, the one case where that upgrade
fails. Restoring the device-keyed logic turns the second red and leaves the first
green — measured, not assumed.

Neither test can cover the fix's one standing assumption, so state it here:
destruction keys on a `Weak<OwnedFd>`, which tracks *that `Arc`*, not the kernel
open file description. A raw `dup` taken through `DeviceFd`'s public `AsFd` /
`AsRawFd` impls would keep the file and its handle table alive with no `Arc`
left to observe, and destruction would be skipped. Nothing in this crate or in
cosmix-comp duplicates the device fd that way — checked, not assumed — so every
path that exists today releases the handle; the type system does not enforce it,
so re-check after a bump that widens `DeviceFd` access. Retaining the `Arc`
strongly would make destruction unconditional and is rejected deliberately: it
pins the open file, and with it the kernel DRM file and device objects behind it
— though not the `/dev/dri` entry, which a hot-unplug removes regardless — for
as long as any imported timeline outlives the device: a leaked fd in place of a
leaked handle. It is not uniformly worse; see the comment on
`destroy_syncobj_handle` for the case where it wins.

Fix six's **one** test needs no device, no feature and no `--ignored` — as do
fix seven's three below, and those four are the only ones of the ten that do.
Run it anyway:

```sh
cargo test -p cosmix-comp -- --exact \
  protocol::tests::a_plane_set_that_is_not_exactly_zero_to_n_is_refused_as_incomplete \
  > /tmp/fix6.log 2>&1
rc=$?
cat /tmp/fix6.log
[ "$rc" -eq 0 ] &&
  [ "$(grep -c '^test result: ok\. 1 passed; 0 failed' /tmp/fix6.log)" -eq 1 ]
verdict=$?
echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
( exit "$verdict" )       # ...and so does the block, for anything scripting it
```

**Running it is about the count, not the result.** Unlike fixes 1–5, dropping
fix 6 turns the ordinary `cargo test -p cosmix-comp` **red** — the test drives a
`memfd_create` plane over a socket pair and touches no DRM, so it has no reason
to be `#[ignore]`d and none to sit behind a feature. What the default suite
cannot tell you is that the test still *exists*: an upstream restructure that
takes the test with it leaves the suite green and this crate unguarded. Hence
`--exact` and `1 passed`, for the same reason the blocks above assert `4 passed`
and `2 passed` — libtest exits 0 when a filter matches nothing.

`--exact` earns its place here beyond the usual: the name is a substring of
nothing today, but it shares a prefix style with the three sibling fixtures
added alongside it, and a later `…_is_refused_as_incomplete_for_v3` would make a
substring filter match two tests and fail the `1 passed` check for a reason that
has nothing to do with fix 6.

Fix 6 is the plane-index defect: `DmabufParamsData::create_dmabuf` exempted every
plane above index zero from the full-extent bounds check, as subsampled planes
legitimately are, and then relabelled planes by their position in the client's
`add` arrival order — so a lone plane declared at index 1 was exempted *and*
became plane zero. A 256-byte memfd was accepted as an 8192-byte ARGB8888 buffer;
`validate_dmabuf_metadata` cannot recover the check, because it does arithmetic
on the declared stride and never asks how large the plane really is. The fix
sorts by the declared index and requires the set to be exactly `0..n`,
`INCOMPLETE` otherwise — which the unstable protocol defines as "missing or too
many planes", as distinct from `PLANE_IDX` (index too large) and `PLANE_SET`
(index already set), both of which the `add` handler already raises.

Reapplying it is the fixes-3-and-4 case: the test lives in
`crates/cosmix-comp/src/protocol/tests.rs`, outside `vendor/`, so "revert only
the `vendor/smithay/` hunks" is exactly right if upstream fixes this themselves.
Two hunks, in one function: the guard, and the `add_plane` call that carries
`plane.plane_idx` instead of the loop position. Keep the second even if upstream's
own fix makes it redundant — the whole defect was a position standing in for an
index.

Fix seven's **three** tests need no device, no feature and no `--ignored`
either. Run them the same way:

```sh
cargo test -p cosmix-comp -- --exact \
  protocol::tests::a_live_key_serial_is_accepted_for_a_popup_grab_while_focus_is_held \
  protocol::tests::clearing_keyboard_focus_invalidates_a_held_keys_popup_grab_serial \
  protocol::tests::clearing_keyboard_focus_on_unmap_drops_activation_from_the_next_initial_configure \
  > /tmp/fix7.log 2>&1
rc=$?
cat /tmp/fix7.log
[ "$rc" -eq 0 ] &&
  [ "$(grep -c '^test result: ok\. 3 passed; 0 failed' /tmp/fix7.log)" -eq 1 ]
verdict=$?
echo "verdict=$verdict"   # 0 = pass. ANYTHING else is a failed bump.
( exit "$verdict" )       # ...and so does the block, for anything scripting it
```

Fix 7 is the focus-clear notification defect: `KeyboardInnerHandle::set_focus`
calls `SeatHandler::focus_changed` from both arms that install a new focus, but
not from the sole arm that clears it — `else if let Some(…) =
self.inner.focus.take()` sends `wl_keyboard.leave` and returns. (The
focus-unchanged arm rightly calls nothing, and `None` -> `None` matches no arm
at all; clearing is the one real change that goes unannounced.) Its own
doc comment says a `None` focus is a focus change like any other, so this reads
as an omission rather than a decision. Downstream then keeps every piece of
bookkeeping it keys on focus while the client has already been told focus is
gone. For us that is three things, and the tests above split them
deliberately rather than covering both from one fixture:

- the serial `xdg_popup.grab` validates against, so a popup could still be
  granted a grab on the strength of a keystroke whose focus has since been
  cleared — this takes **two** tests, not one. The rejection test holds a key
  down (the record is cleared on release, so a released key would prove
  nothing), clears focus by clicking empty space, and requires
  `xdg_popup.popup_done`. But `popup_done` is also what this compositor sends
  for an unknown seat or a missing root, so on its own that test cannot tell a
  lost serial from a compositor that grants no grabs at all — verified, not
  assumed: making `grab` dismiss unconditionally left the entire suite green.
  The positive control replays the same live serial down the same path one step
  earlier, while focus is still held, and requires the grab to be **accepted**;
- `xdg_toplevel::State::Activated`, which `focus_changed` is the sole writer of
  — `…_on_unmap_drops_activation_from_the_next_initial_configure` unmaps a
  focused toplevel with a null-buffer commit and requires the **next initial**
  configure after remap to omit it. Not an
  immediate deactivation configure: unmap calls `reset_xdg_configure_sequence`
  before the visibility pass, which closes the ordinary configure gate, so the
  pending state is the only place the difference survives to;
- the data-device focus, which has no honest client-only socket oracle here and
  is therefore covered by neither. Say so rather than implying three-for-two.

Mutation-proved as a set of three, six cases all killed: reverting the vendored
call kills both focus tests, removing the `invalidate_keyboard_action` call kills
only `…_invalidates_a_held_keys_popup_grab_serial`, and dropping the `unset`
branch of the activation loop kills only
`…_drops_activation_from_the_next_initial_configure` — so neither focus test
subsumes the other. The remaining two cases
are what the positive control is for, and it is the **only** test that kills
either: a `grab` that dismisses every popup unconditionally, and a `grab` that
passes every validation gate and is then silently never installed. Both left the
whole suite green before the control was written and then sharpened.

Reapplying it is the fixes-3-and-4 case again — the tests live in
`crates/cosmix-comp/src/protocol/tests.rs`, outside `vendor/`. The fix itself is
one statement plus its comment, in one arm. If upstream adds the call
themselves, drop ours and keep all three tests — the positive control included,
or the pair that remains cannot tell an invalidated serial from a compositor
that grants no grabs at all.

There is also a deliberately unfixed upstream defect in
`DrmTimelineDeviceSpecific::invalidate`: it writes one byte to an eventfd, where
Linux requires eight, so the write fails with `EINVAL` and is discarded. Do not
make that write succeed in isolation: it would turn the current no-op into a
fail-open readiness signal and could apply a transaction before its acquire
fence is satisfied.

If upstream has genuinely fixed one, keep its test — it then guards upstream's
fix instead of ours. **Do not do that by dropping the fix commit.** What dropping
costs differs by fix: for some it deletes the test outright, for others it only
strands the prose around a test that still guards, and for fix 5 it silently
reverts a hardened test oracle to one that can pass on a leaking build. Reapply
the fix's commits and revert only the part that reimplements the fix. Where that
part is differs three ways, and only the middle case is the simple one:

- **Fixes 1 and 2** (`e9ce7d9`, `973f0c3`) did not introduce their tests.
  `86ab8d5` did, before the vendoring existed, and it wrote them already
  demanding the *correct* behaviour — deliberately red against stock 0.7.0. The
  fix commits therefore flip no assertion; each only retires the "known defect"
  half of the `#[ignore]` reason and rewrites the commentary from "is red on
  purpose" to "is now a live regression test". So dropping one leaves a test
  that still asserts the right thing and still guards; what goes stale is only
  the prose around it, which reverts to announcing a defect that upstream has
  fixed. Revert the `vendor/smithay/` hunks and keep the updated `#[ignore]`
  reason and commentary.
- **Fixes 3, 4 and 7** (`65313fe`, `6379693`, and fix 7's own commit) introduce
  their tests, but into `crates/cosmix-comp/src/protocol/tests.rs`, outside
  `vendor/`. Here "revert only the `vendor/smithay/` hunks" is exactly right and
  needs no care.
- **Fix 5's load-bearing stages are exactly these three, in this order and all of
  them**: `d6a3d30`, `3279844`, `61c0feb`. That set is closed and cannot go stale
  — it is history. Reapplying a prefix of it is a trap, and each is load-bearing
  for a different reason:
  - `d6a3d30` destroys the handle, but keyed on the `DrmDeviceFd`. That misses
    the window where the last `DrmDeviceFd` is gone while a `DeviceFd` clone
    still holds the DRM file open — the handle is live there and stays leaked.
  - `3279844` replaces the export-based test oracle with `assert_destroyed`,
    a timeline query. The export allocated a descriptor, so under a low
    `RLIMIT_NOFILE` it failed with EMFILE whether or not the handle was
    destroyed, and the old `is_err()` check passed on a leaking build. Skip this
    one and the tests below can go green while leaking.
  - `61c0feb` re-keys destruction on the file itself (`device_file`,
    `destroy_syncobj_handle`, the `DrmFile` wrapper, `DeviceFd::downgrade`),
    tightens the oracle to ENOENT alone, and adds the second test — the only one
    that detects the `DeviceFd`-clone window. It *edits* `assert_destroyed`
    rather than introducing it, so it will not apply cleanly without `3279844`.
  The enumeration will also list commits after `61c0feb` that this file does not
  name. Most have been review rounds on fix 5 touching only comments — no
  behaviour, no assertion — and that is where the fix's one unenforced assumption
  is written down, worth reapplying, but losing it loses only prose.
  **Do not assume that of an unrecognised commit**, and this is no longer
  hypothetical: **fix 6 is in that suffix**, a real vendored behaviour change
  documented further down this file but named by no bullet here, and treating it
  as commentary would drop it during a bump while the four live tests stay green.
  Anything in the
  enumeration that is not one of the commits named in these three bullets must be
  read before it is classified. That is the whole rule: the prose classifies only
  what it names, and the enumeration is still the reapply list.

  The first and third stages put fix *and* test in the same vendored file,
  `vendor/smithay/src/wayland/drm_syncobj/sync_point.rs`, so reverting that
  file's hunks wholesale deletes the test along with the fix. It touches three
  separable places, disposed of differently: revert `impl Drop for
  DrmTimelineDeviceSpecific`, `destroy_syncobj_handle`, the `device_file` field
  and the `DrmFile` wrapper (plus `DeviceFd::downgrade` in `utils/fd.rs`); keep
  the appended `mod tests`, which is why it sits at end of file rather than
  beside the code it tests; and judge the comment inside `invalidate` on its own
  merits, since it documents the unfixed one-byte eventfd write, a different
  defect that an upstream leak fix says nothing about.

One case needs a code edit rather than a drop. A commit with a buffer and
neither sync point breaks two per-request rules at once — `set_acquire_point`
requires an acquire point (`no_acquire_point`, code 4) and `set_release_point`
requires a release point (`no_release_point`, code 5) — and nothing in the
protocol orders them, so either code is conformant. **Two** tests pin our choice
of 4, and a bump where upstream switches to 5 turns both red without either fix
having regressed:

- `explicit_sync_buffer_without_points_reports_no_acquire_point`
- `recreated_syncobj_surface_still_validates_commits`

Update the assertion in both, not just the first — leaving the second red reads
as a regression of the recreated-surface fix, which is a different fix entirely.
