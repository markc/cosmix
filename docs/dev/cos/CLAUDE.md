# CLAUDE.md — markc/cos

Guidance for Claude Code sessions working in `$COSMIX/`.

## What this repo is

The cosmix daemon family + substrate libraries. 28 workspace members organised in three groups:

- **Substrate libraries** (`cosmix-lib-*`, 13 crates) — code that runs inside daemons but isn't a daemon itself: config loaders, daemon framework, prop storage, mesh peering, logging, DNS codec, agent runtime helpers, display protocol.
- **Daemon-family crates** (14 members) — 8 long-running daemons (`cosmix-noded`, `cosmix-maild`, `cosmix-webd`, `cosmix-dnsd`, `cosmix-indexd`, `cosmix-disp-skia`, `cosmix-agentd`, `cosmix-nspawnd`) that hold SPEC-10 identities; plus 6 helper/CLI/adapter crates (`cosmix-mcp`, `cosmix-claud`, `cosmix-mds`, `cosmix-maild-auth`, `cosmix-maild-rules`, `cosmix-maild-bayesian`) that link into the daemons or expose subcommand binaries.
- **Apps** (`cosmix-mail`, 1 crate) — headless Bus citizens that render their UI through `cosmix-disp-skia`.

## Four-repo split

Part of the Cosmix four-repo constellation (extracted 2026-05-29). One-way dependency order — **bus ← mix ← cos** (and bus ← cos directly); the private **cosmix** hub orchestrates all three and is depended on by none.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **bus** | `$COSMIX/` | public · markc/bus | Bus protocol family — `cosmix-lib-bus` + `cosmix-lib-client` + `cosmix-lib-props-core` (3 crates). Depends on nothing. |
| **mix** | `$COSMIX/` | public · markc/mix | Mix language — `cosmix-lib-mix` + `cosmix-mix` + `mix-bench` (3 crates). Depends on bus. |
| **cos** | `$COSMIX/` | public · markc/cos | Substrate libraries + daemon family (27 crates). Depends on bus + mix. |
| **cosmix** | `$COSMIX/` | private · markc/cosmix | Orchestration hub: docs, specs, journals, mesh-private operational state, deploy scripts. No code; drives the three public repos. |

**→ This repo is `cos`** — the daemon family + substrate libraries; needs bus + mix present as sibling checkouts to build.

## Layout

```
$COSMIX/src/
├── Cargo.toml                  workspace (27 members)
├── _etc/
│   └── sysusers/
│       ├── cosmix.conf                   SPEC-10 daemon identities (UIDs 500+)
│       └── cosmix-nodeexport-foreign.conf  foreign-host node_exporter UID
└── crates/
    ├── cosmix-lib-*/           substrate libraries
    ├── cosmix-{noded,maild,webd,...}/  daemons
    └── cosmix-mail/            mesh-aware app
```

`_etc/sysusers/cosmix.conf` is normative: every daemon's UID assignment comes from there. The `cosmix-dnsd` test `spec10_identity_matches_checked_in_sysusers_fragment` cross-checks in-code SPEC-10 constants against this file — keep them in sync.

## Build / test / lint

cos has two cross-repo dependencies:
- [bus](https://github.com/markc/bus) at `$COSMIX/` — for `cosmix-lib-bus`, `cosmix-lib-client`, `cosmix-lib-props-core`.
- [mix](https://github.com/markc/mix) at `$COSMIX/` — for `cosmix-lib-mix` (used by `cosmix-lib-config::mix_data` for the strict-data parser).

Both must be present as sibling checkouts under `$HOME`:

```sh
cd $COSMIX/src
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The zero-warning baseline is enforced.

**Do NOT wrap `cargo` in `memguard` as an agent** (dropped 2026-08-29). Memory caps
belong to the unit that runs the build (a build-cluster worker or a `MemoryMax`-capped
systemd unit), not to a wrapper the agent adds; inside the codex sandbox `systemd-run
--user` cannot reach the bus, so memguard either refuses to run the gate (foreman task
80: five tasks committed uncompiled code in one day) or falls through unguarded anyway.
Run plain `cargo build`/`cargo test`. Exit 137/143 = the unit cgroup OOM-killed the
build — report it, do not retry blind. (`cosmix-foreman` itself is halted as of
2026-08-30 until further notice; its fleet units no longer exist.)

## Cross-repo dep direction

- `cos → bus` (one-way) — every cos crate that talks Bus path-deps `cosmix-lib-bus` / `cosmix-lib-client`; daemons that own a SPEC 12 namespace also path-dep `cosmix-lib-props-core`.
- `cos → mix` (one-way) — five cos crates path-dep `cosmix-lib-mix`: `cosmix-lib-config`, `cosmix-lib-dns`, `cosmix-claud`, `cosmix-mcp`, `cosmix-maild`.
- cos never depends on the private cosmix orchestration hub or any mesh-private substrate context.

## Where to put new functionality

| Want to add … | Goes in |
|---|---|
| New daemon | `crates/cosmix-<name>d/` — new workspace member; add a row to `_etc/sysusers/cosmix.conf` (next free UID in the 500-599 daemon band) and bump the SPEC-10 version in `cosmix-dnsd/src/citizen.rs` (the test enforces alignment). |
| New Bus method on an existing daemon | That daemon's `bus/` or `*.rs` handler module — most daemons follow a `register_*` pattern. |
| New SPEC 12 property namespace | The daemon's `props/` module + `register_namespace` call at daemon startup. Schema declared with `cosmix-lib-props-store`'s `NamespaceSpec`. |
| New shared library code | `crates/cosmix-lib-<name>/` if non-trivial; otherwise an existing `cosmix-lib-*` crate where the functionality fits. |
| Anything wire-format / broker-client / SPEC 07 read surface | **Don't add here** — those live in [bus](https://github.com/markc/bus). |
| Interpreter / Mix language features | **Don't add here** — those live in [mix](https://github.com/markc/mix). |

## What goes here, what doesn't

✅ **Belongs in cos:**
- Daemon implementations + their substrate libraries.
- Per-daemon storage backends, audit, lifecycle machinery.
- TLS / ACME / SNI machinery (cosmix-lib-daemon's `tls` feature).
- TOML / config-file loaders (`cosmix-lib-config`).
- Broker URL auto-resolve helpers (`cosmix-lib-config::client_helpers`).
- Display-protocol consumers (`cosmix-lib-display`, `cosmix-disp-skia`).

❌ **Doesn't belong in cos:**
- Bus wire format, `BusMessage`, `NodedClient` — those live in [bus](https://github.com/markc/bus).
- Mix interpreter / lexer / evaluator / builtins — those live in [mix](https://github.com/markc/mix).
- Per-host mesh-deployment artifacts (hostnames, mesh IPs, delta/epsilon-specific configs, proxmox-specific tokens) — operational state, not cos identity. Test fixtures using public-domain examples (`example.com`) are fine.
- Project-mandate docs, decision history, journals, specs — those live in the private cosmix orchestration hub, not in cos.

## ⚠️ Sequencer-lane substrate is canonical — never revert to fix a surface

The 2026-07-18 Studio sequencer work landed permanent substrate features that
EVERY desktop surface and the Bus `mixer_daemon` build on: the `cosmix-song`
crate; `SourceProfile::MidiSynth` + the frame-keyed scheduler / note-chase /
idle-preview / bank-swap machinery in `cosmix-musicd` (`mixer.rs` +
`mixer_host.rs` song helpers, `RtCommand::NoteEvent`, `song_swap_rings`,
`render_song_stereo`/`render_song_channels`/`export_song_wav`);
`cosmix-mixer-schema` 0.5.0's `SOURCE_PROFILE_MIDI_SYNTH`; and `desktop/ctk`'s
`piano_roll`/`menu` widgets. Design record:
`desktop/apps/studio/README.md`.

**Standing rule (Mark, 2026-07-18):** when a `desktop/*` surface breaks or
lags against updated substrate crates,
**adapt the consumer forward — never revert, prune, or "simplify away" the
substrate**. A surface that doesn't drive a feature gets a compile-only
adaptation (precedent: `desktop/apps/studio/src/transport.rs`'s `MidiSynth`
match arm). Functions a given surface doesn't call are NOT dead code — they
are the Bus daemon's and the other surfaces' port-back path. A previous
session destroyed this class of work by reverting musicd to un-break a
surface; do not repeat that.

## Versioning

Each crate carries its own `version`. Daemon binaries follow their own semver cycles (e.g. `cosmix-maild` 0.1.0 is independent of `cosmix-webd` 0.1.0). Substrate libraries that a daemon API-depends on bump together with the daemon when the surface changes.

## License

MIT.


## Graduated Skills (auto-generated)

### cold-review-desktop-identity-migrations

**When:** Review a desktop app rename or workspace merge where runtime slugs, state roots, Bus identities, keymaps, and Cargo features move together.

**Approach:** 1. Compare each renamed file directly with its HEAD predecessor, because combined staged+unstaged rename detection can collapse substantial edits into delete/add noise. 2. Search live source separately from dated plans and changelogs for retired names. 3. Trace every persisted state reader and migration fallback, distinguishing intentionally removed dotdirs from still-promised shared migrations. 4. Audit feature-gated CTK modules against each app's declared feature set rather than relying on workspace-wide builds. 5. Cross-check keymap asset names, include_str paths, action modules, modal scope producers, and scope consumers. 6. Compare public identity registry claims with runtime constants, Cargo metadata, window IDs, Bus service names, and tests. 7. Verify moved documentation commands and paths against the filesystem: audit README examples against Cargo required-features; cross-check documentation claims against action handler implementations and changelogs to catch stale descriptions. 8. For operator-only state migrations, explicitly record the retired slug and verify the move on disk; distinguish complete (slug recorded, verified, directories removed) from unaddressed (directories still present — flag for deletion). 9. After manifest-only version bumps, inspect Cargo.lock for divergence from checked-in expectations; run all Cargo probes (including `cargo metadata --locked`) in a temporary mirror copy to avoid rewriting the live review tree, which can mask dependency version conflicts or missing feature constraints. 10. Test state migrations when the new root directory already exists from an earlier launch; catch conflict-on-relaunch bugs and verify the app handles pre-existing state gracefully. 11. Trace immutable config and provenance objects (e.g., RegisterProvenance, BusBridgeConfig) through their actual production code paths (worker setup, SupervisedClient reconnects); cross-reference against build-script output and compiled artefacts rather than relying on isolated test fixtures, which can hide integration failures.

**Watch out for:**
- Running package checks from a merged workspace can mask missing dependency features through feature unification; source-audit every gated API and prefer isolated feature checks only when builds are authorised.
- A rename that removes all retired-name strings can accidentally remove the only working migration path for existing state.
- Git diff HEAD may fail to recognise heavily modified renames when staged renames also have unstaged edits; compare HEAD:path to the new working file explicitly.
- An independent reviewer without a custom prompt may ignore a no-build constraint and recursively invoke review tooling; stop it and rely on read-only inspection.
- Documentation commands in README files may claim features or flags that don't match Cargo feature declarations; audit examples against Cargo.toml required-features.
- Documentation claims about functionality may diverge from action handler implementations; verify descriptions against handler code and changelogs to catch stale or aspirational documentation.
- Operator-only state migration dispositions can conflate 'migration is recorded and verified' with 'cleanup is scheduled but unaddressed'; explicitly distinguish when directories are still present and flag them for removal.
- Cargo.lock can diverge after manifest-only version bumps, masking dependency version conflicts or missing features; inspect the lock file to catch constraint mismatches that build success might hide.
- State migration tests that don't account for pre-existing target state roots can miss bugs where the app fails or corrupts state when relaunching into existing directories; test both first-launch and re-entry scenarios.
- Running `cargo metadata` or other Cargo probes directly in the review tree, even with `--locked --no-deps`, can rewrite workspace path-package versions in Cargo.lock; perform all Cargo inspections in a temporary mirror copy to preserve the live tree's state for audit.
- Helper tests and isolated test fixtures can exercise mocked or simplified components that pass in isolation but fail in production integrations; audit critical wiring paths (e.g., RegisterProvenance through BusBridgeConfig, worker setup, SupervisedClient reconnects) directly against production code rather than assuming test coverage verifies actual runtime behaviour.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._


### Review queued Wayland DnD causality

**When:** Reviewing a Wayland drag-and-drop bridge that buffers protocol callbacks before application delivery

**Approach:** Trace callback wire order through every internal queue and its class-based drain order, paying special attention to the relative ordering of lifecycle Worker events and action-class SelectedAction events. Verify that enter, motion, selected action, drop, worker completion, app acceptance, and terminal events preserve causality across one dispatch_pending batch. Correlate callbacks by wl_data_device/wl_data_offer identity rather than a global current transfer. Check protocol-version gates and all wl_data_offer finish/set_actions preconditions. Trace worker-owned FDs and deadlines through rejection, post-drop leave, teardown, and stalled sources. Identify wl_display.sync barriers after set_actions: action responses (selected_action, motion) caused by the request are protocol-ordered before the sync's done event, so capture the latest action in-flight while the barrier is outstanding and settle only after the full protocol batch completes. Test action/drop/leave batch sequences through the public accept wrapper to verify it correctly handles all ordering cases. Audit every unkeyed ProtocolEvent overflow separately to catch edge cases in overflow recovery and seat-to-device correlation. For SCTK callback integration tests: when the literal DataDeviceHandler cannot be invoked without a live proxy graph, factor shared capture/queue logic into intermediate state structs (e.g., TransportState::capture_enter) so unit tests exercise the shared path without the full SCTK machinery. Within independent codex reviewers, avoid recursive codex-review invocations.

**Watch out for:**
- Classified queues can reorder selected_action or motion after drop.
- Batching all dispatch_pending callbacks before returning app events can deny the app any chance to accept a fast enter+drop.
- A global current transfer misattributes stale or multi-seat callbacks.
- Version-gated SCTK methods can silently no-op while state advances.
- Worker-owned read FDs can outlive terminal state or hang forever without an EOF deadline.
- Waiting for a callback after wl_display.sync done can starve valid Ask negotiation when protocol-ordered action responses are guaranteed before done.
- Recursive codex-review invocations from within an independent codex reviewer create infinite reviewer loops.
- Lifecycle Worker events can drain before action-class SelectedAction events, allowing payload completion to validate stale action state.
- Public accept wrapper can fail to test action/drop/leave batch sequences, allowing ordering bugs in the accept path.
- Unkeyed ProtocolEvent overflows mishandled without seat-to-device correlation at discard time.
- SCTK DataDeviceHandler cannot be invoked without live Connection/QueueHandle/proxy graph; must factor shared capture/queue logic into intermediate state structs for unit testability.

_Graduated from skill learning loop — confidence 99%, 5 uses, 5 successes._


### review-wayland-dnd-receive-live-ordering

**When:** Cold-reviewing a Wayland wl_data_offer receive bridge that defers SCTK callbacks into classified/coalesced queues and snapshots drop context.

**Approach:** Read core wayland.xml and the pinned SCTK data_offer/data_device sources beside the pure state machine. Trace callbacks from dispatch_pending through the protocol queue, especially action or motion immediately followed by drop in one dispatch. Verify classified draining preserves protocol happens-before at lifecycle fences. Trace Ask from drop through final set_actions, post-drop leave, action acknowledgement, app completion, deadline and finish. Critically: explicitly preserve offer-to-transfer correlation and latest action state through post-drop leave and into the final Ask set_actions/action/sync/finish sequence—clearing correlation early will discard stale wl_data_offer.action callbacks before the sync barrier. Validate public action masks/preferences against source_actions. Trace real ReadPipe ownership, cancellation, cap overflow, worker results and exactly-once terminal delivery. Refresh SCTK DragOffer state from DataDeviceData at each callback boundary, never rely on cached dropped/left flags across dispatch rounds. Check callback sequence numbers for causality with protocol events—sequence only proves relative order at dispatch time, not that an event was generated after a client request (may have been pending earlier). After terminal cleanup signals, inspect post-terminal code in the same callback for ordinary events still being enqueued. Explicitly trace Worker lifecycle events against action-class events to ensure lifecycle transitions (spawn, cancel, EOF) happen at the correct protocol boundaries. Validate public zero drain budgets are enforced to prevent uncontrolled queue growth or silent drops. Check deadline settlement before expiry and verify WouldBlock handling in flush operations retries correctly without losing the final acknowledgement window. Inspect SCTK DataDevice event Enter replacement (data_device.rs:139-141) to identify when retained device keys are destroyed by the next wl_data_device.enter; distinguish device-key hover lifetime from offer-key post-drop lifetime. Check Proxy::is_alive before raw retained requests (accept, final-actions, finish) to avoid targeting a dead proxy after Enter replacement. Ensure source_actions has callback-time current value (analogous to callback_actions) rather than only barrier-time snapshot, so action validation gates use live state. Test the complete public path end-to-end with injected sync barriers rather than only the pure receive latch in isolation.

**Watch out for:**
- A pure state-machine test can manufacture action acknowledgements or event order that the live protocol queue does not guarantee.
- Coalescing actions/motions separately and draining lifecycle first can snapshot stale target, action, modifiers or revision at drop.
- SCTK intentionally retains a dropped offer after leave, but core wl_data_device.leave text still says the destination must destroy it; raw post-leave requests are compositor-specific.
- An atomic cancellation flag plus nonblocking read/sleep is eventual cleanup, not immediate FD/task cleanup, and a source that never closes EOF can keep the worker alive indefinitely.
- The wl_data_offer preferred action must be in the destination mask and, after Ask, in source_actions; a public API should enforce rather than trust glue code.
- SCTK DragOffer is a value type clone; an offer cached at enter may retain stale dropped/left flags unless explicitly refreshed from DataDeviceData on each callback.
- Callback sequence numbers assigned at dispatch do not prove causality—an event was already pending before the client request and appears later in the sequence; use protocol state and timing guarantees, not event ordering alone.
- Terminal cleanup code paths can enqueue ordinary events after signalling termination; check post-terminal code in the same callback for ongoing work that contradicts the terminal state.
- Worker lifecycle events (task spawn, cancel, EOF) misalign with action-class event boundaries, causing premature termination, stale action contexts, or orphaned ReadPipes.
- Public zero drain budgets are not validated, allowing queued actions to grow unbounded or drop silently without backpressure.
- Deadline settlement is not verified before timeout expiry, or WouldBlock handling in flush operations does not retry correctly, dropping the final data offer acknowledgement or leaving the protocol in a stalled state.
- Processing post-drop leave clears offer-to-transfer correlation prematurely, discarding the final Ask wl_data_offer.action callback before the sync barrier completes.
- Testing only the pure receive latch in isolation misses ordering bugs in the complete public path; end-to-end action settlement requires injected protocol synchronization points to verify the full Ask through finish sequence.
- Device keys retained after post-drop leave can be destroyed by a subsequent wl_data_device.enter before the bridge processes the new Enter, causing raw accept/final-actions/finish liveness gates to target a dead proxy.
- source_actions snapshot at barrier time becomes stale; callback-time action validation needs current source_actions (analogous to callback_actions) rather than once-captured barrier-time state.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._


### wayland-drag-icon-lifecycle

**When:** When adding compositor-owned drag icons to cosmix-wl-dnd or another SCTK 0.19.2 Wayland data-source transport.

**Approach:** Validate raster width, height, exact checked RGBA byte length, protocol-sized stride/dimensions, buffer-scale divisibility, and positive buffer scale before any request. Bind wl_compositor and wl_shm as optional source-side capabilities so incoming DnD construction remains unchanged. Allocate a fresh roleless surface and per-transfer SlotPool/Buffer. Validate the dependency chain early: bevy_resvg 2.4.0+ re-exports resvg, which re-exports usvg and tiny_skia; confirm tiny_skia resolves to 0.12+ so Pixmap::take returns its premultiplied RGBA byte store in little-endian Argb8888 format (byte layout [B, G, R, A]). Attach, damage, and commit can occur before or after wl_data_source.start_drag; ensure the surface has no other role assigned before start_drag, as the drag-icon role is established by the compositor during the call. Apply wl_surface.offset only at surface version >=5. Verify buffer-scale divisibility per the implementation constraint. Verify SHM pool size fits signed-i32 limits and accounts for SCTK's 64-byte slot rounding. Retain the surface, Buffer and pool in ActiveSource until the outgoing terminal; use one RAII owner and the same take-and-drop cleanup on start failure, teardown and bridge Drop. Preserve the existing None-icon public path by delegating through an internal optional-icon method. For wide per-filename raster designs, materialize composition at the export threshold (not eagerly); the composed raster is gesture-scoped and owned only by the active drag context, dropped when transfer state ends. Composition failures must log and retain the original square raster as fallback rather than propagating the error. Derive the hotspot from ExportIconRaster.logical_anchor (e.g., labelled output uses (24,24) while square output keeps its centred default). Test validation, byte conversion, buffer-scale divisibility, version guards, optional-global degradation, SHM pool sizing, generic destruction ownership, dependency-chain resolution, composition failure handling and hotspot derivation without faking a compositor; reserve wire ordering and rendering for a physical Wayland seat.

**Watch out for:**
- Assigning a different role to the surface before wl_data_source.start_drag prevents the drag-icon role from being established by the compositor.
- Calling damage_buffer below wl_surface v4 or offset below v5 is a protocol/version error; bind compositor v4+ and guard offset.
- Reusing a drag-icon surface is invalid because its role is permanent.
- Destroying at dnd_drop_performed or incoming echo Drop is too early; only outgoing terminal ends source ownership.
- Dropping wl_compositor or wl_shm from bridge construction requirements regresses incoming DnD on compositors without icon support.
- Writing Argb8888 in native-endian byte order instead of explicitly little-endian wl_shm format produces inverted channel order and wrong colors.
- Buffer-scale values that aren't properly divisible cause protocol errors or misaligned icon rendering.
- SHM pool size exceeding signed-i32 limits or not accounting for SCTK's 64-byte slot rounding causes allocation failures.
- Cargo commands in this workspace may rewrite a concurrent cosmix-lib-mix Cargo.lock path version; restore the protected worktree value after gates.
- Resolving a tiny_skia version older than 0.12 or with non-standard Pixmap::take semantics causes premultiplied-RGBA byte-format misalignment; audit the dependency chain (bevy_resvg → resvg → tiny_skia) early to catch incompatible transitive versions.
- Composing wide per-filename rasters eagerly or outside the active drag context causes unbounded memory growth; composition must occur at export threshold and the raster must be gesture-scoped.
- Composition failures that propagate errors instead of logging and retaining the square fallback raster break drag-icon fallback and interrupt the transfer.
- Deriving hotspot from a constant instead of ExportIconRaster.logical_anchor causes icon misalignment on labelled output; labelled (24,24) and square (centred default) require anchor-aware positioning.

_Graduated from skill learning loop — confidence 91%, 5 uses, 4 successes._


### design-cosmix-tray-daemon-skin-split

**When:** Designing or reviewing the CosMix tray split into a Rust trayd state/action daemon plus SNI and Plasma QML presentation skins.

**Approach:** 0. BLOCKERS CHECK: Before committing to the design, verify cross-repo dependency constraints: (a) inspect cosmix-lib-config's Cargo.toml and run `cargo tree -i cosmix-lib-mix` to confirm dependency chain; (b) explicitly check zbus version and validate whether blocking-api is gated as a separate feature (zbus 5.18.0+ requires `blocking-api` for `zbus::blocking`); (c) audit ksni's Cargo.toml to confirm its zbus feature enablement does NOT include blocking-api when the constraint is `features = ["async-io"]` only — this is a real coordination gap; (d) confirm scope boundary and user's explicit written acceptance of transitive config→mix→tokio path and D-Bus-as-authority contract (not projection). 1. Read every production module in desktop/apps/tray and split each by responsibility rather than moving whole files. 2. Keep desktop-entry parsing and state reducers in a pure cosmix-lib-tray core; keep raw icon names in daemon state and resolve icon themes per skin. 3. Make cosmix-trayd a graphical-session user daemon, with SPEC-10 namespace ownership but the logged-in user as runtime identity, following interactd's precedent. 4. Put observed apps, units, sources and bounded operations into SPEC-12 namespaces; expose thin allowlisted Bus actions and one scope-limited wake verb. 5. Establish D-Bus as the authority contract: property state is normative, Bus wakes reflect writes, and menu-open Refresh is required for live widget state (not avoided). 6. Use systemd D-Bus signals for subscription, Bus wakes from writers, connection lifecycle and a >=5-minute backstop; menu-open Refresh drives live widget mechanism per user contract. 7. Build the Plasma skin as QML visuals plus a small Qt6DBus/Qt6Qml C++ model plugin generated from checked-in introspection XML. 8. Keep the ksni SNI as a separate D-Bus client binary with its own singleton and icon-theme filter. 9. For zbus blocking-api coordination: disable the builder's default allow/replace cache behavior (retain DO_NOT_QUEUE) to prevent activation race with listener; pre-install a raw Changed match before any initial property read so listener subscription cannot race property fetch. 10. Subscribe to NameOwnerChanged on the daemon's D-Bus name to detect service restarts and ownership changes; emit PropertyChanged only after verified active ownership. 11. Atomic snapshot semantics: read all related properties together via a single GetAll or a wrapped batch method to prevent value divergence when multiple properties change during sequential reads. 12. Systemd targeting: use user-manager session contexts (--user flag, user services, systemd --user scope) consistently; never mix system-manager commands in a user-daemon's control flow. 13. Refresh deduplication: coalesce overlapping refresh requests with a timer or bounded queue; drop duplicate in-flight requests and emit the final batched state once, not intermediate ephemeral states. 14. Accept the config→Mix→Tokio transitive path explicitly if user approves in writing; do not attempt to work around it without explicit user redirection.

**Watch out for:**
- Moving process.rs wholesale leaks notify-send and hard-coded konsole presentation policy into the daemon.
- Treating D-Bus as a secondary projection instead of the authority contract creates state drift between properties and internal reducers.
- Using systemd ActiveState with a Type=simple noded unit as listener readiness races before the socket is bound.
- Building a bespoke D-Bus snapshot cache beside SPEC-12 props creates dual authority and state drift.
- Using the existing native Bus client/daemon libraries violates a no-direct-Tokio-runtime constraint without explicit user acceptance; confirm transitive config→mix→tokio path approval in writing.
- Letting a D-Bus method accept arbitrary unit names or argv widens authority beyond the discovered CosMix allowlist.
- Auto-starting the SNI on Plasma after the plasmoid lands creates duplicate tray surfaces.
- Assuming ksni's zbus dependency auto-includes blocking-api when it does not; async-io-only constraint clashes with any code requiring zbus::blocking, and ksni's Cargo.toml must be audited to confirm feature gates.
- Omitting menu-open Refresh from the design breaks live widget synchronization; D-Bus-as-authority requires refresh after user menu interaction.
- Allowing SSH control_host when the contract requires local systemd creates a hybrid authority and state inconsistency.
- Zbus builder's default allow/replace cache can race activation with listener startup, dropping initial Changed signals before subscription completes.
- Not pre-installing a raw Changed match before the initial property read leaves a window where property fetch completes before the listener is subscribed, causing the first change to be unobserved.
- Skipping early feature-gate validation (zbus blocking-api, ksni feature enablement, Cargo.toml inspection) before committing to an approach leaves coordination gaps undiscovered until implementation hits missing APIs.
- Not subscribing to NameOwnerChanged on the daemon's D-Bus name misses service restarts and ownership transfers, leaving stale state and undetected lifecycle transitions.
- Reading individual D-Bus properties sequentially without atomic snapshot semantics causes property-value divergence when multiple properties change during the read window; interleaved updates corrupt state.
- Using systemctl or systemd --system commands in a user-daemon's control flow targets the system manager instead of the user session manager, bypassing user-session authority and creating permission/lifecycle mismatches.
- Overlapping refresh requests are dropped without deduplication or cancellation, causing intermediate state changes to be lost and the UI to miss legitimate updates between batches.

_Graduated from skill learning loop — confidence 90%, 5 uses, 4 successes._


### review-ctk-contrast-token-contracts

**When:** Cold-review CTK theme contrast derivations, selected/hover row tokens, or theme-file colour override cascades.

**Approach:** Verify WCAG transfer threshold and RGB coefficients against independent chromatic reference points; inspect Bevy Color conversions and actual output clipping; check whether accepted theme syntax permits alpha/extended values; prove endpoint/fallback behaviour for finite inputs and probe the sampled search for non-monotonicity by testing immediately adjacent representable f32 values (not decimal probes at 1e-5 spacing) to pin exact branch boundaries—adjacent-ULP probing fully determines the semantic interval with no residual gap remaining; validate branch-logic robustness by measuring rounding precision magnitude inside relative_luminance (sub-1e-8-magnitude differences between candidate formulae confirm the closer-branch predicate); construct threshold-flip pairings that prove residual semantic impact; trace scheme/mode reselect plus shared/app overlay provenance and generic Value-map persistence; enumerate every runtime foreground painted on each selection/hover wash across CTK and apps; mutation-test constants that self-referential AA assertions may miss; audit test ingress points for feature-gated API references and verify tests run under all feature combinations; trace diagnostic emission ordering to ensure they occur only after transactional validation commits; probe for NaN comparisons that fail-open instead of fail-safe; inspect public ThemeSpec and apply_theme entry points for bypass paths that circumvent file-only validation schemas; position test-only call-site witnesses at or below the observable warning sink and verify warning dispatch is not deleted (deletion after witness increment remains invisible); run CTK all-features and affected app tests/clippy under memguard.

**Watch out for:**
- Srgba::hex accepts RGBA forms even when a local wrapper documents only RGB, so ignoring alpha can certify invisible text or derive against a wash that will composite differently.
- A derived-colour test that uses the same contrast helper for generation and assertion can pass when the WCAG helper itself is wrong; add independent chromatic and transfer-threshold vectors.
- Introducing a selected-row foreground token does not migrate CTK-owned or sibling-app selected-row consumers automatically.
- Hover keeps each row child's resting foreground; dim text and accent icons can fail AA on the hover wash even when primary text passes.
- A later overlay that changes a derivation input can silently clobber an explicit foreground from an earlier cascade layer unless explicit provenance is tracked.
- Documentation ranges can contradict the executable palette; print all scheme/mode pairings before repeating empirical claims.
- Tests can accidentally reference feature-gated APIs or constants that do not exist in all build configurations, allowing validation bugs to pass in CI under one feature set and fail in production under another.
- Diagnostics printed or logged before a transactional validation check commits can report success and be observable to callers or logs before the transaction actually fails, creating silent failures and inconsistent state.
- NaN comparisons in threshold or clipping logic that use fail-open operators (e.g., !(x < threshold) for out-of-range detection) silently accept NaN and pass invalid values, allowing invalid colours to propagate.
- Public programmatic ingresses (ThemeSpec constructors, apply_theme methods) can bypass file-only validation schemas, allowing callers to inject invalid token derivations or unvalidated colour values that would be rejected by the TOML parser.
- Test-only call-site witnesses placed above the observable warning sink or witness increment before warn_unless_aa can hide deletion of warning dispatch, leaving the validation silently disabled.
- Probing only one channel at the transfer cutoff bounds one side; without probing a second channel in the remaining interval or asserting both transfer branches, the exact normative join remains unpinned and transfer-threshold bugs can hide at the knife-edge boundary.
- Decimal probe spacing at 1e-5 or coarser granularity fails to resolve f32 bit-level branch boundaries; adjacent representable floats and threshold-flip pairings are required to pin exact semantic transitions and catch mutations that toggle branch logic without decimal appearance changes.

_Graduated from skill learning loop — confidence 96%, 5 uses, 5 successes._


### implement-ctk-sanitized-body-view-stage-a

**When:** Implement or revise CTK's Stage A untrusted HTML body reading path, sanitizer boundary, remote resource policy, styled-text projection, links, copy policy, accessibility or performance.

**Approach:** 1. Keep raw BodySource separate from private-field SanitizedHtml/SanitizedBody and accept only the latter in spawn/project APIs. 2. Configure Ammonia with a narrow tag/attribute/scheme allow-list, clean-content tags for executable/embed/form surfaces, deny relative URLs, strip styles, and retain only cid plus exact raster data:image MIME types for img src. 3. Inventory remote CSS url() and resource attributes before cleaning with escape-aware parsing limited to style attributes and inline style subtrees; use token-aware url()/@import parsing to avoid mismatching escape sequences that could obfuscate resource URLs. Separately parse image-set(...) candidates as resource URLs (not quoted text) to catch MIME spoof and protocol bypasses in candidate strings; url(...) extraction and image-set(...) extraction require distinct paths because cssparser tokenisation treats quoted url() as literal text. 4. Parse only Ammonia output with matching html5ever/RcDom and project into semantic block/style structs with bounded recursion. 5. Render blocks as flex-wrapped ordinary Bevy Text entities; make links separate underlined focusable entities that emit LinkActivated but never open; parse srcset attributes following strict candidate tokenisation rules (including data-URL comma handling) to prevent protocol bypasses in malformed candidates. 6. Since Bevy 0.19 Text has no reusable document selection, use visibly focusable per-block Ctrl/Command+C copy and document the limitation. 7. Use existing contrast-checked CTK tokens for pane/text/quote/focus/scrollbar, avoiding new palette surface. 8. Add hostile fixtures, demo, feature-off gates and full feature gates. 9. Verify RemoteRefs inventory uses DOM/CSS-token traversal instead of raw substring scanning to catch obfuscated protocols. 10. Bound recursion depth in project_list separately from the general projection depth to isolate nested-list complexity; enforce a practical upper depth limit (e.g. max 256 or 512) and test regressions against 4096-level lists before production. 11. Render truncation markers when depth or entity budgets are exceeded, with explicit 'halt traversal' and 'show marker' state so that an in-progress block can commit its accepted spans before halting; position the marker outside documented content budgets (not consuming from entity/block count limits) to ensure users see truncation without hiding content. Use AccessKit Role::Alert to ensure screen readers announce truncation to users. 12. For flat projections, track explicit parent-list-item ownership; spawn continuation blocks and nested lists under the correct parent item rather than at document level, preserving list structure semantics and visual hierarchy. 13. Register block-click handlers to explicitly ignore the original target entity when stopping Bevy event propagation, because observers on the same entity all execute despite propagation being stopped; failure to ignore the target causes duplicate handling. 14. Explicitly enable system_clipboard feature or document clipboard copy unavailability in the build. 15. Enforce block/entity budget limits (maximum children, maximum depth) and render changed-only to avoid repaint storms on large bodies. 16. Register links with AccessKit Click action handlers and ensure they are advertised as actionable. 17. Wrap list projections in AccessKit Role::List containers and list items in Role::ListItem. 18. Note: Stage A trust boundary (sanitizer + projection budgets) is independent of Blitz's own recursive DOM/layout path in Stage B; Stage A completion does not guarantee Stage B's resource policy or renderer safety. Stage B integration requires separate validation of renderer feature-gates, net provider completion semantics, and authority delegation.

**Watch out for:**
- Ammonia drops relative URL attributes before attribute_filter runs, so RemoteRefs inventory cannot rely only on that callback.
- Prefix-only data:image checks admit MIME spoof strings and SVG; parse the data URL header and match exact raster media types.
- Parsing the RcDom document root as inline content flattens all blocks; find the inserted body element before block projection.
- Preformatted spans must bypass ordinary leading/trailing whitespace trimming.
- ThemeBackgroundColor is immutable in Bevy Feathers; dynamic copy-focus paint must resolve tokens into mutable BackgroundColor.
- A flex document inside a scroll viewport needs flex_shrink=0 to retain intrinsic content height.
- Do not put max_width: percent(100) on text inside auto-sized parents; it can collapse to zero under Taffy.
- RemoteRefs implemented as raw substring scanner misses DOM/CSS-token obfuscation and protocol bypasses; must traverse the actual parsed DOM and CSS rules.
- project_list recursion depth not bounded separately from general projection depth allows nested lists to consume the entire depth budget and prevent sibling content.
- system_clipboard feature not explicitly enabled or documented means clipboard copy silently fails or panics at runtime depending on Bevy version.
- Large bodies without block/entity count limits or changed-only focus repaint cause repaint storms, memory bloat and UI hangs; enforce budgets.
- AccessKit links missing Click action advertisement or handler registration are not exposed as actionable to screen readers.
- List projections without AccessKit Role::List containers and Role::ListItem wrappers break semantic list structure for accessibility.
- srcset attribute parsing that does not follow strict candidate tokenisation rules (including data-URL comma handling) allows malformed candidates and protocol bypasses to propagate.
- CSS inventory that does not handle escape sequences in url()/@import or fails to limit parsing to style attributes/subtrees allows obfuscated resource URLs to bypass filtering.
- Truncation markers not rendered or missing AccessKit Role::Alert semantics leave users unaware that content was cut off, and screen readers cannot announce truncation.
- Recursion depth limits set without practical regression testing against 4096-level lists may cause UI hangs or excessive truncation in edge cases; upper bound must be verified before production.
- Truncation halt and marker state are conflated; a block in progress cannot commit accepted spans before halting, so incomplete blocks get lost or mangled when truncation triggers.
- Truncation markers placed inside entity/block budgets reduce capacity for actual content; markers must be outside documented budgets or consume from a separate counter.
- cssparser url() tokenisation treats quoted strings as literal text and does not extract resource URLs from image-set candidates; url(...) and image-set(...) require separate resource extraction paths to avoid missing protocol bypasses.
- Flat list projections spawn continuation blocks and nested lists at document level rather than under the correct parent list item, breaking list structure semantics and visual hierarchy.
- Bevy observers registered on the same entity all execute despite event propagation being stopped; block-click handlers that target BodyLink events must explicitly ignore the original target entity to avoid duplicate handling.
- anyrender version incompatibility: exact registry beta uses anyrender 0.11; anyrender_vello_cpu 0.15 (anyrender 0.12) builds beside it but cannot compose, compatible CPU backend is 0.14.
- CidNetProvider does not complete every request; DummyNetProvider drops handlers and can leave critical/image requests pending indefinitely with no error callback path, causing silent starvation.
- NetHandler lacks an error callback; empty bytes are the public fail-closed completion semantics, requiring all callers to explicitly handle empty payloads as distinct from success.
- Pending-image request keys are not cleared without passing the original resolved_url to the handler, causing key accumulation, stale pending state, and request deduplication failures.
- Enabling component defaults, high-level blitz/net, SVG, file-input features or granting NavigationProvider external authority in Stage B violates Stage A's trust model and reintroduces untrusted resource loading or code execution vectors.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._


### cold-review-ctk-body-view-trust-boundary

**When:** Cold-review or security-review CTK body_view, Ammonia policies, untrusted HTML projection, RemoteRefs, copy/accessibility, or Stage A mail rendering.

**Approach:** 1. Read Package 3 Stage A and house rules, then inventory all uncommitted paths including untracked fixtures. 2. Inspect Ammonia's exact pinned source to distinguish setters that replace defaults from auto-added behaviour such as rel; separately audit block-tag unwrap semantics (e.g., center, main, dl) that may remove tags and concatenate content into adjacent text nodes, breaking newsletter structure. 3. Probe sanitized output for base, relative/protocol-relative URLs, srcset/imagesrcset, SVG/MathML, data SVG, anchor schemes and encoded URL attributes; separately test WHATWG URL canonicalisation edge cases (https:\host, https:/host, https:host) which url::Url normalises to network URLs but prefix-matching may miss. 4. Audit RemoteRefs separately from sanitization: enumerate each resource-bearing element/attribute and CSS form, then identify both false negatives and false positives. Include canonicalisation probes in RemoteRefs inventory to catch elided-slash and backslash variants. 5. Trace type-state privacy and every path from sanitized href to private BodyLink and LinkActivated. 6. Review projection recursion independently for containers, lists and raw-text fallbacks; run a reduced-stack deep-list probe. Verify that project_list_item_children does not independently retype heading or structural elements into ListItem, losing semantic role and style. 7. Inspect Bevy 0.19 sources for clipboard backend gating, ScrollArea input clamping, AccessKit action requirements, and text selection APIs (ComputedTextBlock::buffer/Parley Layout, Cursor::from_point/Selection geometry, TextLayoutInfo.selection_rects); document selection geometry availability and render-vs-input constraints. 8. Count construction-time entities and per-frame queries for large untrusted bodies. 9. Separately verify transparent anchor handling at ALL projection contexts: detect <a> elements wrapping block content (headings, paragraphs, divs, sections) in the main container path AND in alternate projection contexts (table cells, td collectors, alternate flatteners) and confirm they preserve block structure and nesting rather than flattening into inline text concatenation or losing semantic boundaries. Specifically test <td><a><h2>..</h2><p>..</p></a></td> through each possible collector path. 10. Test Unicode whitespace handling: probe for correct preservation of NBSP (U+00A0), NNBSP (U+202F), and ideographic space (U+3000) in entities and literals; verify that char::is_whitespace and trim_* are not incorrectly applied to these non-collapsible spaces per CSS whitespace-collapse rules. 11. Audit AccessKit container nesting carefully: trace whether list-item text/link nodes are direct children of ListItem containers or wrapped under non-accessible intermediate containers; identify and verify hoisting paths where non-accessible wrappers cause nodes to bypass their intended parent and re-attach to a higher ancestor. 12. Run memguard tests/clippy, scoped rustfmt, and feature-off check; report no edits.

**Watch out for:**
- Ammonia's builder setters replace broad defaults, but link_rel still injects a fixed rel attribute after filtering; include it in the effective output allow-list.
- Ammonia's default unwrap for certain block tags (center, main, dl) removes those tags without cleanup, concatenating their content into adjacent sibling paragraphs and breaking ordinary newsletter structure and visual flow.
- A raw-source RemoteRefs scanner misses HTML entity decoding, object data, link href, imagesrcset, CSS escapes and @import, and can inventory URLs in inert text/title content.
- A general MAX_PROJECTION_DEPTH guard does not protect a separately recursive project_list function.
- project_list_item_children independently retypes a first heading or structural element into ListItem, losing heading semantic role and associated style information.
- Bevy Clipboard is in-process unless bevy/system_clipboard is enabled; a test round-tripping that resource does not prove OS copy.
- Bevy ScrollArea and Scrollbar clamp wheel/track/drag input themselves, while Bevy layout only clamps the computed offset; inspect the actual input path before filing the repo gotcha.
- Role::Link alone does not advertise or handle AccessKit Click; Role::ListItem children directly under Document do not provide list semantics.
- Bevy AccessKit only links direct accessible children to their parent; list-item text/link nodes under a non-accessible content wrapper are hoisted to the window root instead of nested under their intended ListItem, breaking hierarchy, focus order and semantic structure.
- Untrusted large bodies can create one block plus one-or-more span entities per block, and an unconditional update query can make idle frames O(blocks).
- A literal no-network-syscalls claim is broader than a structural no-remote-fetch guarantee and needs syscall tracing or narrower wording.
- Prefix matching against URL schemes (e.g., starts_with("https://")) misses WHATWG canonicalisation edge cases where url::Url accepts https:\host, https:/host, and https:host as network URLs, allowing remote fetches that bypass scheme-prefix validation.
- Transparent anchor elements (<a>) wrapping block content (headings, paragraphs, sections) are projected as inline containers, flattening descendant block structure and concatenating text rather than preserving semantic nesting; this breaks accessible link scope and visual hierarchy in mail bodies.
- A special block-anchor path at the main container is insufficient when alternate projection contexts (notably table-cell flatteners) still route valid <td><a><h2>..</h2><p>..</p></a></td> through collect_inline, collapsing descendant block structure and nesting.
- Rust char::is_whitespace and trim_* incorrectly collapse or strip NBSP (U+00A0), NNBSP (U+202F), and ideographic space (U+3000) even though CSS whitespace-collapse rules must preserve these as non-collapsible in HTML entities or literals.

_Graduated from skill learning loop — confidence 97%, 5 uses, 5 successes._


### cold-review-probe-fix-indirections

**When:** Cold-review a performance probe or follow-up fixes involving histogram summaries, selection-path equivalence, per-event/per-frame arithmetic, child-process status, load-bearing measurement comments, control-flow termination, skip accounting, or experimental-design stability.

**Approach:** 1. Trace each claimed fix to the exact current call path, not just the changed helper; distinguish atomic operations from sequences of secondary reads (e.g., update_subject store reads). 2. For histogram mitigations, test p50 and p99 independently and distinguish open-tail occupancy from equality with max at a closed bucket boundary. 3. For probe arithmetic, compare aggregate totals or normalise event means by event count/frame count; never divide means with different denominators. 4. Verify a shared callback still includes the producer/widget state transition, not merely downstream application work. 5. For process reaping, model out-of-order completions, try_wait errors, and aggregation timing; distinguish continuous status updates from tick-boundary aggregation; Child drop does not reap. 6. Verify status ownership and async completion race conditions; establish which tick boundary owns an async result and whether the aggregation window is guaranteed to observe it. 7. Audit documentation conclusions against what the instrument actually attributes. 8. Distinguish incomplete remote inventories from explicit truncation; trace where incomplete data is generated vs. where it is intentionally filtered. 9. Check for empty helper inputs and zero-length collections; validate loop advance/exit conditions explicitly and ensure no-op or undefined state does not prevent loop termination. 10. Audit skip/omit accounting through all data paths, including failed lookups (e.g., row_id not found, body disappears); trace whether skipped items are counted and reported or silently dropped. 11. Audit public upstream data structures and arguments that downstream code depends on; flag cases where workarounds hide data or state that should be explicit. 12. Validate experimental-design stability by running the same measurement command twice in separate processes and comparing partial-histogram traces: if whole-frame means diverge sharply (e.g., 187 ms then 21 ms) while partials remain stable (~1 ms), an unmeasured floor or warm-up artifact is present that steady-state benchmarks alone will not localise. For paired-sample or paired-in-process designs: verify warm-up is matched between control and treatment arms (same setup, iterations, initial state); reverse arm order (treatment-first instead of control-first) and confirm results are equivalent—fixed arm order masks scheduling artifacts, warm-up biases, and order-dependent effects. Test non-multiple frame counts (e.g., 7 or 9 frames when measurement code assumes multiples of 8) to expose edge-case control flow. Numerically verify that documentation claims about observed ranges, maxima, or minima match actual histogram data; do not assume a stated 'ceiling' is load-bearing without inspecting the data. Validate sign-test interpretation and statistical direction: confirm residuals are symmetric or apply appropriate signed-rank tests, and verify computed values match claimed relationships. 13. Run focused app and dependency tests under memguard, clippy with warnings denied, formatting, and the external command regression probe.

**Watch out for:**
- A p50-only saturation note misses p99 landing in the open bucket and fires falsely for exact closed-bucket ceilings.
- Dividing mean cost per event by mean cost per frame overstates recoverable share when events do not occur every frame.
- Calling the same downstream app function from a probe does not reproduce the widget's selection state, row rebinds, styling, or stable-ID lookup.
- Dropping Child after try_wait error can leave a zombie and silently preserve an optimistic status line.
- Controlled whole-frame differencing attributes the difference to the changed workload, not automatically to a specific uninstrumented schedule such as layout.
- A fixture corpus that cycles logical bodies may still duplicate owned body strings per row, defeating the intended sharing.
- A claimed atomic fix still performs multiple store reads in sequence (e.g., update_subject); secondary reads are not protected by the guard or aggregation boundary.
- Status or failure aggregation only occurs at tick boundaries; failures that occur between ticks or during async completion are lost or attributed to the wrong window.
- Incomplete remote inventory (partial data fetch, truncated response, in-flight request) is conflated with explicit truncation filtering, misattributing silent data loss.
- Empty helper inputs are not validated, allowing silent no-ops, undefined state, or cascading failures in downstream consumers.
- Async status completion and tick-boundary aggregation race; ownership of the result is unclear and the aggregation window may miss or double-count the update.
- Public upstream data that downstream code relies on for workarounds is hidden, undocumented, or inconsistently available across call paths.
- Zero model length never advances or exits the probe loop, causing the control flow to hang indefinitely or skip aggregation entirely.
- A body disappearing after row_id lookup is not counted as skipped, causing silent data loss and incorrect probe accounting.
- Whole-frame mean varies wildly between identical runs (e.g., 187 ms vs 21 ms) while partial histograms remain stable (~1 ms), indicating an unmeasured floor or warm-up artifact that the probe does not localise; steady-state-only benchmarks cannot detect this instability.
- Warm-up differences between control and treatment arms cause systematic bias; paired-in-process or paired-sample designs do not automatically equalize warm-up state, setup iterations, or initial conditions between arms.
- Fixed arm order (always control then treatment, or vice versa) masks scheduling artifacts, warm-up biases, and order-dependent effects; reversing arm order can yield materially different results when treatment applies first.
- Sign-test interpretation error: direction of difference, significance claim, or assumption of symmetric residuals contradicts computed values; paired differences may not be symmetric, requiring signed-rank tests or explicit tail validation.
- Documentation claims about observed ranges, maximum, or minimum values are not numerically spot-checked against actual histogram data; stated 'ceiling' may exceed observed max or confuse theoretical with empirically observed bounds.
- Non-multiple frame counts (e.g., 7 or 9 frames when code assumes multiples of 8 or powers of 2) are not tested, allowing edge-case control-flow or buffer behavior to hide in steady-state benchmarks.

_Graduated from skill learning loop — confidence 99%, 5 uses, 5 successes._

## No real environment values — ever

This is a PUBLIC repository. Never copy a real username, home directory,
hostname, domain, IP, or a Claude-projects transcript slug (the encoded
`-home-<user>--<repo>` form) from the machine you are running on into code,
tests, fixtures, docs or commit messages — not even as test data. Use
placeholders: `/home/alpha`, `-home-alpha--<repo>`, `example.org`, RFC 5737
addresses, node names alpha/beta/gamma. A pre-commit gate refuses commits
that carry real values, so the commit fails in your run; fix the fixture,
do not work around the gate. Production code must derive paths from `$HOME`
/ XDG at runtime, never embed them.
