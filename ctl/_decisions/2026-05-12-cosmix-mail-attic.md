# cosmix-mail — Dioxus-era source, attic location

**Status:** historical. The crate described here is no longer part of the
Cosmix workspace. This document records what it was, where it now lives,
and why anyone might still want to look at it.

**Replaced by:** `planned/cosmix-mail-disp.md` (new ABP-Display-Protocol
client, in design).

## What was retired

The original `src/crates/cosmix-mail/` was a Dioxus-based JMAP mail
client targeting native desktop (via `dioxus-desktop` + `tao` +
WebKitGTK) and WASM (browser). It was deleted from the workspace on
**2026-04-09 in commit `6f8a799`** ("Security hardening + remove all
Dioxus crates, WG-only service binding"), as part of the wholesale
removal of all 18 Dioxus crates after the ABP Display Protocol path
was chosen — for the full pivot history read the deskd-rewrite plan, which
never lived in `_plan/` and went with the old `src/_doc/planned/` tree
(`git -C $CMCTL show f18e7443^:src/_doc/planned/deskd-rewrite.md`).

The last commit that *contained* the crate is its parent,
**`a9778d3`** ("Restructure repo: src/ layout, remove Lua, add README +
LICENSE", 2026-03-30-ish).

### What it had

- `src/main.rs` — Dioxus 0.7 entry point (desktop + WASM features).
- `src/jmap.rs` — ~450-line JMAP 0.6 client (session discovery,
  `Mailbox/get`, `Email/query`/`get`/`set`, `EmailSubmission/set`,
  blob up/download).
- `src/components/{compose,email_list,email_view,mailbox_list}.rs` —
  the UI; useful as a reference for "what a JMAP-driven mailbox
  view needs to render."
- `src/hub.rs` — early ABP hub integration stub.
- `Dioxus.toml`, `index.html`, `tailwind.css`, PWA `manifest.json`
  and service worker — WASM build scaffolding.
- ~1,526 deleted lines total (per the deletion commit diff).

### Why it's retired, not just refactored

The Dioxus architecture coupled mail to WebKitGTK (Linux desktop
target), to JavaScript-bundling (WASM target), and to Dioxus's lifecycle
model. The Cosmix Project Mandate (`CLAUDE.md`, 2026-04-25)
reframed Cosmix's primary operator as an AI agent and its primary UI
substrate as the ABP Display Protocol — a wire format that lets every
app be observed and driven via structured messages. A Dioxus app is
opaque to that goal: its widget tree lives in browser DOM /
platform-webview state, not in ABP messages. The replacement
(`planned/cosmix-mail-disp.md`) emits `ui.window` to
`cosmix-disp-skia` instead, putting all UI state on the wire by
construction.

## Where it lives now

**Repository:** [`markc/cosmix-mail-dioxus`](https://github.com/markc/cosmix-mail-dioxus)
(separate private GitHub repo, created 2026-05-12 to hold the
deleted-but-referable Dioxus source without polluting the main
`cosmix` working tree). Tree shape is flat — the contents of the
former `src/crates/cosmix-mail/` are at the repo root.

If other retired Dioxus-era crates ever need similar archival, each
gets its own `cosmix-<name>-dioxus` repo rather than a shared attic.
Per-component repos beat a kitchen-sink attic for discoverability
and for keeping unrelated history out of one another's `git log`.

To browse the source without cloning:

```bash
gh repo view markc/cosmix-mail-dioxus --web
```

To clone:

```bash
git clone git@github.com:markc/cosmix-mail-dioxus.git ~/.gh/cosmix-mail-dioxus
```

To recover the original tree (no attic needed). The commit predates the
repo split, so it lives in **`$CMCTL`'s history**, not in `$COSMIX` —
hence `-C`, and hence `archive` rather than `checkout`: a bare
`git checkout` would drop the crate into cmctl's own working tree, which
is not where you want cos source.

```bash
git -C $CMCTL show a9778d3:src/crates/cosmix-mail/Cargo.toml
# extract the crate anywhere you like, e.g. a scratch dir:
git -C $CMCTL archive a9778d3 src/crates/cosmix-mail | tar -x -C /tmp/old-mail
```

## What's salvageable for the new cosmix-mail

The new app does not re-use Dioxus code (it can't — different render
target), but several pieces of the old crate are useful as reference
*shape*:

1. **`jmap.rs` type signatures** — the request/response structs
   already round-tripped against a real JMAP server. Re-typing them
   for the new crate is fine, but the field names and capability
   negotiation are reusable.
2. **`components/compose.rs` field set** — the To/Subject/Body/CC/BCC
   layout decisions and validation rules. Translates directly to
   `ui.window` widget layout.
3. **`components/email_view.rs` body-rendering** — handling of
   `text/plain` vs `text/html` parts, attachment list. Mostly
   reusable as logic; rendering is `ui.markdown` instead of
   browser DOM.
4. **`hub.rs` ABP wiring** — historical only; new app uses
   `cosmix-lib-client` directly.

What is **not** worth salvaging:

- Dioxus lifecycle hooks (no analogue in ABP Display Protocol).
- WebKitGTK workarounds (`WEBKIT_DISABLE_COMPOSITING_MODE=1` etc.).
- The WASM/PWA scaffolding.
- Tailwind/CSS — styling now lives in `ui.theme` messages.

## Why an attic repo rather than an orphan branch in this repo

Considered alternatives:

- **`_attic/` directory in main tree** — rejected. Drags ~1.5k lines of
  dead Dioxus Rust into `grep`, `find`, doc-coverage, and
  `context_search`. Active workspace pollution.
- **Orphan branch `attic/dioxus-mail` in this repo** — viable; visible
  on GitHub branch picker. But blurs "this branch is alive" with
  "this branch is historical archive," and users running
  `git fetch` end up with both. Rejected in favour of clearer
  separation.
- **Pointer doc + commit-SHA only** — fragile. SHA survives only as
  long as the commit is reachable from some ref; if `main` is ever
  rewritten the SHA can be GC'd.
- **Single shared attic repo (e.g. `cosmix-attic`)** — considered.
  Rejected because a multi-component attic mixes unrelated histories
  in one `git log` and forces an internal taxonomy (`dioxus-era/...`)
  that nobody actually navigates by.
- **Per-component archival repo** (`cosmix-mail-dioxus`) — chosen.
  Strongest isolation, browsable on GitHub, freed-up the
  `cosmix-mail` crate slot in the workspace cleanly, and the
  retirement reason is encoded in the repo name itself.

## Related

- current architecture: `CODEX.md` crate map (the old architecture-overview decision was retired 2026-07-23, git history).
- deskd-rewrite plan — Dioxus pivot history (broader context for *why* the
  entire Dioxus stack was removed). Not in `_plan/`; removed with the old
  `src/_doc/planned/` tree —
  `git -C $CMCTL show f18e7443^:src/_doc/planned/deskd-rewrite.md`.
- `planned/cosmix-mail-disp.md` — the replacement app's plan.
- the retired infra/cosmix-os-vision doc (2026-07-23, git history) — references "cosmix-mail" as the future
  JMAP client surface; that promise now flows through the new app.
