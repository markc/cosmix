# 🖧 CosMix

**CosMix is an agent-operable computing system: legible, modifiable, and
reconstructible by design.** Not a desktop with AI features — an AI-first
foundation that currently *manifests* as a Linux desktop plus a mesh service
stack. The intended primary *operator* is an AI agent; the intended primary
*user* is an AI-first developer driving the system through such agents.

This is the whole project in one repository: the Bus protocol, the Mix
language, the daemon family, the desktop, and the documentation site
([cosmix.dev](https://cosmix.dev)).

## Get it running

```sh
git clone https://github.com/markc/cosmix ~/Projects/cosmix
~/Projects/cosmix/bootstrap
. ~/Projects/cosmix/env
mix man overview
```

That is the entire install. `bootstrap` is a small POSIX-sh script — the one
thing that runs before `mix` exists — which installs a Rust toolchain via
rustup if you have none, builds `mix`, and hands over to `setup.mix`, which
builds everything else and installs the binaries. Nothing needs root.

- **Clone it anywhere.** The directory `bootstrap` lives in *is* `$COSMIX`;
  `~/Projects/cosmix` is only the documented default. Every path — source,
  binaries, config, state — derives from that one root, and the binaries
  find it from their own location, so no environment variable is required
  to *run* them (`env` is what you source to put `bin/` on your PATH).
- **Remove it by deleting the directory.** `rm -rf $COSMIX` removes the
  install completely; cloning and running `bootstrap` again recreates it.
- **Update:** `git pull && mix setup.mix` — the same script, re-run,
  rebuilds and reinstalls (it is idempotent).
- `setup.mix --desktop` also builds the desktop; `--system` additionally
  copies the binaries to `/opt/cosmix/bin` with sudo (the systemd units under
  `src/_etc` expect them there — installing *those* is deploy tooling's job).

## Layout — everything keyed off `$COSMIX`

```
$COSMIX/
├── bootstrap        the pre-mix installer (sh); its directory defines $COSMIX
├── setup.mix        everything after that, in Mix; re-run to rebuild/reinstall
├── env              generated: `. $COSMIX/env` exports COSMIX and adds bin/ to PATH
├── bin/             generated: installed binaries          ($COSMIX_BIN)
├── etc/ var/ run/ log/ tmp/   generated runtime tree       ($COSMIX_ETC …)
├── src/             ONE Cargo workspace                    ($COSMIX_SRC)
│   ├── crates/      every crate, flat: bus family, mix, substrate libs, daemons
│   ├── desktop/     the desktop — its own workspace + toolchain (Bevy/Smithay)
│   ├── _etc/        systemd units, polkit rules, sysusers, tmpfiles (shipped templates)
│   └── rust-toolchain.toml   the compiler every clone builds with
└── docs/            the cosmix.dev site: bus/ cos/ mix/ manuals (source of truth), bugs/, dev/
```

The runtime tree is resolved by one rule shared by `mix` and every daemon
(`src/crates/cosmix-lib-config/src/paths.rs`): `$COSMIX` from the
environment, else self-located from the running binary, else the documented
default; `COSMIX_<KIND>` overrides any single directory. A system install at
`/opt/cosmix/bin` with no checkout above it keeps the usual FHS/XDG
locations (`/etc/cosmix`, `/var/lib/cosmix`, `~/.config/cosmix`, …).

## What is in the workspace

CosMix is a one-way dependency chain — **bus ← mix ← cos** — that used to be
three repositories and is now three groups of crates in one workspace. The
name *is* the architecture: **Cos** + **Mix**.

| Group | Crates | What it is |
|---|---|---|
| **Bus** | `cosmix-lib-bus`, `-client`, `-buildinfo`, `-log`, `-props-core` | **The CosMix Agent Bus.** The wire protocol, client, and shared primitives every layer talks over. Depends on nothing. → [docs](https://cosmix.dev/bus/) |
| **Mix** | `cosmix-lib-mix`, `cosmix-mix`, `mix-bench` | **Mesh & Interprocess eXchange.** A pure-Rust, ARexx-inspired shell and scripting language with native keywords for routing messages across the mesh. Depends on bus. → [manual](https://cosmix.dev/mix/) |
| **Cos** | everything else under `src/crates/` | **Cooperative Operating System.** The daemon family — broker, mail, web, DNS, knowledge indexer, agent runtime — plus the foundation libraries they share. Depends on bus + mix. → [docs](https://cosmix.dev/cos/) |
| **Desktop** | `src/desktop/` | The compositor (`cosmix-comp`), the `ctk` toolkit and the apps. A separate workspace because it pins its own toolchain and vendors patched Smithay/wgpu. |

> *Origin note:* **CoS** was first named for the **Claude** and **Codex** agents
> that co-authored the codebase; it now reads as the **Cooperative Operating
> System** layer.

## Documentation — [cosmix.dev](https://cosmix.dev)

`docs/` is the GitHub Pages source for **[cosmix.dev](https://cosmix.dev)**.
The manuals are written there directly — `docs/mix/*.md` is what `mix man`
reads locally *and* what `cosmix.dev/mix/` serves — so a doc edit and the code
it describes land in the same commit.

| URL | Documents |
|---|---|
| **[cosmix.dev/bus/](https://cosmix.dev/bus/)** | Bus protocol family — wire protocol, vocabularies, client, per-crate references |
| **[cosmix.dev/mix/](https://cosmix.dev/mix/)** | Mix language manual — syntax, builtins, man pages, per-crate references |
| **[cosmix.dev/cos/](https://cosmix.dev/cos/)** | Daemon family + foundation libraries, per-crate references |
| **[cosmix.dev/bugs/](https://cosmix.dev/bugs/)** | Public bug reports — upstream defects found and root-caused during CosMix development |
| **[cosmix.dev/spec/](https://cosmix.dev/spec/)** | Draft architecture specifications — evidence-labelled contracts, limitations and conformance gates; publication is not authority cutover |
| **[cosmix.dev/history](https://cosmix.dev/history)** | The story so far — six months of decisions, reversals and what stuck, in one sitting |

Every doc has a clean URL — `cosmix.dev/<section>/<page>` — served by a
pre-generated HTML shell beside each markdown file. The shells are generated,
not written: after adding, renaming or removing a doc, regenerate and commit
them with `mix docs/build/gen-doc-pages.mix`. `docs/dev/{bus,mix,cos}/` keeps
each former repository's agent-facing notes (`README`, `CLAUDE.md`,
`AGENTS.md`), rewritten to the monorepo paths.

## The thesis

The system is the project. Everything else — the desktop, the Bus-flavoured
IPC, the sovereign mesh, the Mix language, the mail stack — is one of its
*surfaces*, not its identity.

Lineage: **AmigaOS / ARexx** (every app is an addressable message port),
**Smalltalk / Lisp machines** (a live, self-reflective image), **Plan 9**
(everything is a namespace). The novel contribution is an AI agent as a
first-class operator of self-observation, self-modification, and
self-reconstruction — which none of the precedents could have, because the
agents didn't exist.

### Three design criteria

Every architectural decision is filtered through three questions:

1. **More legible to agents?** — state queryable as structured data; code paths
   introspectable; events as accessible log streams; schemas agent-readable.
2. **More modifiable by agents?** — config mutable through structured channels,
   not ad-hoc file edits; component lifecycle agent-operable; schemas updatable
   at runtime.
3. **More reconstructible by agents?** — the build system itself agent-operable;
   sources/deps/artifacts navigable as structured data; the system able to
   rebuild parts of itself from source changes.

`bootstrap` + `setup.mix` are criterion 3 made literal: the whole system
rebuilds from one clone with one command, and the only script not written in
the system's own language is the one that exists to build that language.

## Public repo vs. private control

This repository is public and contains no operator-specific state: no real
host names, addresses, domains, keys or deploy targets (a hygiene gate on the
maintainer's side refuses commits that carry them). Operational
command-and-control — deploy scripts, mesh inventory, journals, private specs and
decision records — lives in a private control repo the maintainer keeps
beside this one. Contributions go through pull requests here.

The sanitised replacement specification candidate lives in `docs/spec/`.
Its draft publication does not replace the existing authoritative suite or
reassign runtime specification IDs; see its authority and conformance chapters.

## Status

CosMix is a research system under active development — a bet that AI-first
sovereign computing is real and that an agent-operable system is its right
shape. The code is *exploration residue*, not frozen canon; where a document
and the code disagree, the three criteria decide.

## License

MIT — see [LICENSE](LICENSE).

---

🖧 CosMix · an agent-operable computing platform · 2026 © Mark Constable &lt;mc@cosmix.dev&gt;
