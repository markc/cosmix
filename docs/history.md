# The CosMix story so far

*March → August 2026, in one sitting. Written 2026-08-30, the day the project
became a single repository with a fresh history — this page is what that
history would have told you.*

CosMix is a bet that an AI agent can be the first-class operator of a
computing system: that legibility, modifiability and reconstructibility *by
agents* are the right design criteria, and that a system built on them ends up
being a better system for people too. Six months in, the bet has produced a
protocol family (the Bus), a language (Mix), a family of daemons, a sovereign
WireGuard mesh, a compositor and toolkit, and a working method. This is how
each of those came to be, what was tried and thrown away, and why the tree
you are looking at is shaped the way it is. Two words of vocabulary: a *node*
is a machine on the mesh; a *citizen* is a daemon or script registered on a
node's bus. "Shipped" means landed on the main branch — and, where it says
so, deployed to the fleet. The numbered SPECs are the project's specification
suite, kept in the control hub. The standing laws quoted in italics come from
the project mandate (`CLAUDE.md` in the starter control folder); the journals
show them being learned.

## March–April: from a pile of services to a substrate

The project did not start as an operating system. It started as a mail server
being hardened (STARTTLS, TLS-before-AUTH, bcrypt, DKIM) and a container
image that could provision a WireGuard node with zero touch. What made it
CosMix was the decision, in late March, to stop scripting the system in
several languages and build one of its own.

**Mix had first light on 2026-03-29**: a working interpreter with a
test-first core, an extension API, a shell, and the first message-bus
foundations — the ARexx idea (AmigaOS's scripting language, where every
program is an addressable message port) in a Rust body. Two days later it was
the *sole* scripting language: Lua and TOML scripting were removed and four
small services were folded into one node daemon; by 2026-04-10 the Bash
dispatch and test harnesses had been ported too, leaving Bash only for genuine
orchestration. The design rules set then: Mix keeps a synchronous, WASM-clean core, embedders absorb async
bridging through extensions, scripts orchestrate startup explicitly, and the
hub stays a dumb router.

April was about making that substrate real. Every mesh service was bound to
WireGuard addresses only — *network binding, not repository privacy, is the
primary security boundary* was decided on 2026-04-09 and still stands. The
workspace shrank to a lean daemon-centred set. Mix gained reactive handlers
(`on … end`, event-yielding sleep, topic lifecycle notifications) and the
first live dashboard that reacted to the mesh instead of polling it. The
knowledge layer arrived — an indexer with model circuit-breaking, a
self-scheduling memory system with trust, decay and superseded-history — and
the first agent daemon ran persistent sessions with tool loops against both
hosted and local models.

Two moments from this period set the tone. A *completed* Dioxus
application mesh with eighteen UI crates was abandoned on 2026-04-09 for a
daemons-plus-display architecture. And on 2026-04-27 an autonomous optimisation loop took the
interpreter's benchmarks from seconds to roughly fifty milliseconds — a
~130× speed-up — under a rule that was kept: a performance change
lives only if it passes paired benchmarks *and* the full suite, and
architectural wins outrank clever peepholes. By the end of April a
10,000-message saturation flood had been reproduced and survived, and the
project's method was recognisable: source-grounded design, real production
data as the acceptance gate, layered verification, and autonomous loops that
stay bounded when they fail.

## May: contracts instead of conventions

May turned local habits into explicit contracts. The internal hub was
renamed to `noded` in a hard cutover with no compatibility aliases — *the
wire identity and the prose role are different things* — and every daemon got
a dedicated identity and hardened unit. The **property substrate** (SPEC 12)
shipped, with mail accounts as its first consumer and live engine
configuration as its third: *the property row is the sole source of truth, and
corrupt durable state aborts startup rather than being repaired in place*.
Mail moved from legacy SQL to a message-store-backed JMAP implementation, and
on 2026-05-19 two independent mail daemons held a full bidirectional
conversation over the mesh through an ordinary desktop client.

Mesh DNS went generic: every node routes to its own loopback authoritative
daemon from one byte-identical resolver configuration, replacing per-link
routing. Mix gained SSH execution, quoting, timeouts and — in place of a
bespoke `json_path` builtin — embedded `jq` semantics. A fifth citizen joined the
mesh and an obsolete gateway identity retired.

Two structural decisions bracketed the month. Substrate-owned structured data
would use strict, static Mix syntax rather than YAML (a bulk migration was
rejected; each format moved when its consumer did). And the source was
**split into three public repositories** — protocol, language, daemons — with
a private control repository holding deployment scripts, because publishing
them would publish the operational topology. That split was the right call for
a public/private boundary and, as August would show, the wrong shape for a
newcomer's first build.

The month's operational lessons were the ones every fleet learns the hard
way and then writes down: `enable --now` does not refresh a running daemon;
`Requires=` propagates stops but not restarts; verify the daemon's own log,
not journald; validate every name, address and owning process independently,
because aggregate substring checks mask partial failure. And the discipline
that would later be named *convergence*: re-review every fix as fully fixed,
partially fixed, or unaddressed, because the first repair often leaves the
root cause elsewhere.

## June: enforced trust, and a product surface

June made the mesh trustworthy and gave it a face. **SPEC 13** landed on
2026-06-03: one generated inventory, signed, rollback-protected and
fail-closed, became the trust root for the whole mesh, with live recovery
proven. Broker admission followed the next day — challenge, proof,
identity-bound registration, close-on-refuse, hot-reload revocation — rolled
out *off → observe → enforce*, with cryptographic identity as the proof and
IP addresses used only for correlation. Observe-before-enforce paid for
itself immediately: it caught root-only seed files that would have made every
daemon unable to prove itself, as telemetry instead of an outage.

Generated DNS zones and WireGuard configuration removed duplicated address
maintenance. Fleet logging settled on journal-upload leaves feeding
one central reader (a per-node collector was too heavy at this scale). The
indexer stopped wedging on bulk work. The `vhostd` idea — one process for
mail and web — was rejected because merging them collapsed the fault,
compromise and TLS-key boundaries; *maild and webd stay separate processes,
trusted Mix may embed in-process, untrusted scripts go to pooled workers*.

Then the surface. An embedded-Mix CMS proved `webd` could host server-rendered
applications and shook four evaluator defects out of Mix in the process. A
shared multi-vhost handler set, an SSR personal-information suite (contacts,
calendar, mail), CalDAV/CardDAV over the existing stores, a legacy WordPress
archive imported with slugs and media intact, email-to-post publishing with
sender-locked opaque tokens, shared forms, one button system, one datatable
model with right-click actions and bulk selection — the second half of June
converged separate experiments into one coherent product surface.
`filesd` extended the same substrate model to durable markdown corpora
(*disk files are truth, the database is a rebuildable projection*), and a
dual-pane file manager ran on its sandboxed backend. Mix reached 0.19 with
math primitives and a source-verified manual; a seventh node joined the mesh
with enforced admission.

Deployment learned its most important rule on 2026-06-27: fleet rollout gates
on *semantic version*, because identical source rebuilds to different bytes;
SHA-based detection was abandoned. Nightly self-deployment stayed on, with
ordered rollout and health gates as the safety boundary.

## July: appliances, agents at the controls, and the method

July's centre of gravity moved from laboratory proofs to real workloads. A
lean Arch-based appliance self-provisioned from one image on two container
platforms, running the node daemon, mail and web with Mix as its shell; it
got reproducible manifests and signed releases, and stayed a *normally
updatable* system rather than an immutable image swap (that model was
dropped as premature). Customer container provisioning worked end to end —
preseed, certificates, DNS, web, mail, reboot verification, rollback — and
`provisiond` finished a nine-brick arc of fenced jobs, crash reconciliation
and fault injection: *design and fault-inject recovery around every external
commit boundary rather than inferring it from happy-path tests*. The public edge
moved behind fail-closed SNI demultiplexing. A cloud-suite replacement run
proved DAV migration, webmail parity and a native photo pipeline.

Mix crossed from shell to safe orchestration language: 0.28 brought builtin
introspection, isolated modules and copy-on-write collections; 0.30 brought
structured errors with tracebacks, validation at job boundaries, a linter,
argv-safe execution, private-CA HTTP and remote `ssh_exec`. *When Mix lacks a
primitive, add the primitive and dogfood it* became a standing rule; every
shell workaround is a missed test of the substrate.

Then the toolkit. On 2026-07-16 **CTK**, the CosMix toolkit, controlled a live audio mixer strip
over the Bus; a day later it exposed an app-generic control surface so an
agent could enumerate, inspect and drive native controls *without screen
scraping* — the thesis, demonstrated. A fused-versus-split benchmark measured
the Bus's cost as inaudible, so the split architecture stayed. CTK was folded
into the main tree; **CosMix Desktop** became one family with slug-derived
identities. Tower shipped as mesh mission control; an interaction daemon took
ownership of every system-to-human surface (all eleven dialog kinds, end to
end, from a registered Mix citizen). On 2026-07-30 the first CosMix
**compositor** ran nested — Smithay protocol handling, Bevy rendering,
shared-memory clients, DMA-BUF zero-copy — and by month's end it had pure KMS
topology and live identity probes.

The method was formalised on 2026-07-17: **workflow-as-code** as the default
for non-trivial work, and — retained explicitly — *cold review, fix by
severity, re-review until every finding is dispositioned*; round-two catches
are the norm. The same month the project pruned its own
memory: plans are deleted when they ship, decisions survive shipped work, and
on 2026-07-25 a **public-repository hygiene gate** shipped — a scanner and
git hooks that refuse any commit or push carrying a real host, address,
domain or home path — after exposed values were found and sanitised. Its
lesson: *before importing history into a public repository, scan every
commit, not just the final tree.*

## August: first light, signed routing, and the harness that ate the month

August belonged to the compositor. Explicit sync (advertised in production,
then withdrawn after permanent faults, then completed end to end), a single
input core for nested and live transports, the first live input run on real
hardware, the first steady KMS frames on 2026-08-05, a supervised render
pump, resume as a *cyclic* release/reacquire authority rather than a restart,
first live client content on 2026-08-06, exact fractional scaling,
server-side decorations on by default. Then the decisive reversal: on
2026-08-18 **atomic KMS presentation replaced Vulkan direct-display**,
because its page-flip wait is cancellable and deadline-bound — and
direct-display, its feature graph and public API were deleted once the soak
matrix passed. A Quoin shell skeleton and a disposable GPU-capable desktop
container passed their bring-up gates alongside.

The mesh finished its trust story: the **signed inventory became the sole
routing authority** (noded 0.13.0, 2026-08-16) with signed broker endpoints
and outbound hot-reload fencing across seven nodes; in prose the protocol's
uppercase name changed from *AMP* to *ABP* (Agent Bus Protocol), while *Bus*
stayed the transport and the lowercase wire and API names did not change. The
knowledge memory system that had scheduled itself since April was **removed**
on 2026-08-19 — polling, auto-graduation and observer-driven deployment had
become noisy, circular and unsafe — and post-commit events became the only
indexing trigger, in line with the standing law that *a backend is woken by a
bus event, with a timer only as a slow backstop for a missed wake*.

And then the harness. **Foreman** shipped on 2026-08-17: agent drivers, a
ledger, governed execution, verification tiers, merge authority, an
escalation ladder that climbed across the project's model families — an
unattended fleet that landed ninety-seven changes. It also taught the project
more about verification than anything before it:

- *the tested tree is part of the artefact* — verification means nothing
  while another writer can still edit it;
- shared build directories launder binaries across worktrees;
- a green gate is a hypothesis until a mutation on the production path fails
  it;
- unknown-host failures must never be charged to a branch;
- a sandbox that blocks the agent's own gate trains it to commit blind;
- reviewer families disagree, so charges must key to code-owned causes.

Two build workers carried
real gates by 2026-08-29. By then the harness was consuming more attention
than the compositor and desktop it existed to accelerate, so on 2026-08-30 it
was **decommissioned** — ledger, unpublished branches and worktree patches
retained — in favour of the inline loop it had been built to automate:
implement, verify independently, cold-review on two arms (two model
families), converge.

Meanwhile the substrate kept moving: Mix 0.60/0.61 made corrupt control-flow
values raise instead of fabricate and moved usage statistics into the
evaluator; the Bayesian mail classifier was carved out as a standalone engine
with exact untrain/relabel and byte-identical classification, and mail gained
shadow-trained, atomically-swapped per-account rebuilds.

## 2026-08-30: one repository, one root, one command

The decision that produced this tree came last. The
three-repository split, right for the public/private boundary, meant that
`git clone` of any single repository could not build — every crate reached
its siblings through relative paths into hidden directories no README
mentioned. So the code became **one repository**: one Cargo workspace for the
substrate and a separate one for the desktop (which pins its own toolchain),
the manuals living in the site
they are served from, and a `bootstrap` script (the only non-Mix script,
because it exists to build Mix) that turns a clone into an install in one
command. Everything is keyed off one root, `$COSMIX`; the binaries find it
from their own location; `/opt/cosmix/bin` remains the system tier for nodes.
A starter control folder ships in the tree so a new user gets a private hub
by copying a directory — no account with anyone required.

The old repositories, with their 1,361 commits, went to archival storage.
Preserving them was the first plan; it was reversed the same day when
the hygiene gate had to scan every one of them on every push, and because
the point of the fresh start was to have *no history to lean on*. This page
is the exception: the shape of the decisions, without the weight of the
commits.

## What stuck

- **Three criteria as the frame.** More legible, more modifiable, more
  reconstructible by agents — the stated test for every design call. The
  reversals above each fell for a concrete reason (the immutable appliance was
  premature, the fused mixer lost to addressability and independent daemon
  lifetime, the harness cost more attention than it saved), and each is a
  case the criteria call the same way.
- **Agentic-first; ceremony opt-in.** Correctness invariants — signed
  inventory, admission, fail-closed trust — stay unconditional. What becomes
  a flag is the human-in-the-loop ceremony: a mandatory prompt on a path an
  agent must drive is a bug.
- **The binary is the oracle.** Mix's manual is verified against the
  interpreter, and the interpreter documents its own builtins; a doc that
  disagrees with the code is fixed in the code.
- **Events, not polls.** A backend is woken by a bus event; a timer is at most
  a slow backstop for a missed wake. Nightly deployment is a schedule, not a
  poll.
- **Fix the language, not the script.** A missing Mix primitive is a Mix bug.
- **Convergence.** Cold review on two model families, fix by severity,
  re-review until every finding is dispositioned.
- **Verify the artefact, not the report.** The installed binary, the live
  behaviour, the unit's own invocation — never the build log, the summary, or
  "active".
- **Publish shapes, not identities.** The hygiene gate decides what may be
  public; the private hub holds the rest.
