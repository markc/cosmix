# CosMix — the repository

The whole project in one tree: Bus protocol, Mix language, daemon family,
desktop, and the **GitHub Pages source for [cosmix.dev](https://cosmix.dev)**
(`docs/`). Rooted at `$COSMIX` (default `~/Projects/cosmix`); `README.md` is
the front door, `AGENTS.md` the layout + build + path rules. Read both first.

Two rules that outrank anything below:

- **This repo is public.** No real host names, addresses, domains, keys or
  operator home paths — ever. Sanitise to RFC 5737 addresses, `example.*`
  domains, `alpha/beta/gamma` node names, `/home/user`.
- **Everything is keyed off `$COSMIX`.** One root, one workspace
  (`src/`), one docs tree. Never hardcode where a checkout or an install
  lives; go through `cosmix-lib-config::paths` / `cosmix_paths`.

Per-area guidance from the former repositories is in `docs/dev/{bus,mix,cos}/`.
Public-safe architecture specifications belong in `docs/spec/`; their explicit
draft/acceptance status and source evidence govern how they may be used.
Operational state, journals, private specs and decision records remain in the
maintainer's private control repo. Never publish those wholesale.

The section below is maintained by the cosmix-mcp skill-learning loop
(`skills_graduate`): proven cosmix-domain skills get promoted here as
permanent rules. It only ever appends under the marker; everything above it
is human-authored and wins on any conflict. **This repo is public** — only
graduate skills whose text is public-safe; private / all-project workflow
skills belong to the `cmctl` domain.

## Graduated Skills (auto-generated)


### follow-up-review-bevy-wayland-dnd-dispositions

**When:** When re-reviewing revised Bevy/Wayland drag-and-drop plans after earlier findings were supposedly fixed, especially to catch partial dispositions, stale state contradictions, and incomplete convergence checks including schema coherence, completion contracts, and order-independent concurrent prerequisites.

**Approach:** 1. Map every prior disposition to the revised plan and classify it internally before reporting only residuals. 2. For app-defined acceptance, demand concrete Bevy Messages/resources, correlation or revision keys, fail-closed defaults, and ordered system sets from probe through app response to highlight/drop commit. 3. Check acceptance freshness when target is stationary but modifiers, pending operations, modal state, negotiated action, or async payload readiness changes; require final re-evaluation before app response set or defer commit. 4. Model external offers as async states so Drop can be held until payload fetch succeeds, and enumerate leave/drop/finish/cancel/window-close/worker-failure cleanup. Verify wl_data_device.leave is terminal only before drop; SCTK preserves dropped offers for later reads/Ask. 5. Treat CursorLeft as both cancellation and export-boundary input and require one atomic precedence rule. 6. Verify own-window echo correlation against actual wire identity; use a private unpredictable MIME nonce if no source ID is shared. 7. Read the exact dependency implementation for claims such as bounded dispatch_pending; audit explicit coalescing/overflow classes for enqueue queues. 8. Audit SeatHandler hotplug/removal callbacks and active-token cleanup. 9. For Ask path, demand correlated app-to-bridge outcome keyed by delivery_id (choice/dismissal message), final non-Ask set_actions() call, and finish only after successful completion; snapshot the whole accepted drop context (modifiers, position, offered types, action). 10. For concurrent Ask prerequisites (compositor final-action acknowledgement and DropComplete), use an order-independent latch: both must resolve before terminal path; require matching actions between them; DropComplete::Failed, missing acknowledgement, dismissal, or bounded completion timeout (never unbounded) must route through exactly-once terminal destroy/reject. 11. Ensure canonical delivery events do not duplicate owned payload/action/context fields. 12. Final convergence checks (round 5 confirmed): (a) event schema includes delivery_id on choice/dismissal and matched actions on ack/DropComplete for round-trip verification; (b) nonce retirement covers both echo and no-echo cases; (c) Ask uses order-independent latch, never sequential waits; (d) all promised DropComplete emissions (Copy and Move) are consistent; (e) queue-overflow cleanup exposes guaranteed app-visible terminal latch; (f) missing ack/DropComplete::Failed/dismissal/timeout destroy/reject through exactly-once terminal with matching-action guard. 13. Distinguish app-specific atomic operation-state rechecks at mutation boundary from CTK revision staleness—defensive rechecks are valid. 14. Cross-check each phase's acceptance criteria against the explicit dependency table to expose residual sequencing omissions (e.g., Phase N depends on Phase M but Phase M not yet fully converged); verify no circular or unsatisfied ordering dependencies exist before signalling phase complete.

**Watch out for:**
- A prose statement that an app system 'returns' acceptance is not executable in Bevy without a request/response and scheduling contract.
- Recomputing acceptance only on target changes leaves stale highlights and Wayland actions when modifiers, pending state, or payload readiness changes; final re-evaluation must precede app response set/defer commit.
- Wayland Drop can precede completion of asynchronous pipe reads, so concrete payload delivery needs a DroppedAwaitingPayload state.
- CursorLeft may be the only boundary signal; an unconditional cancellation handler can erase the payload before export starts.
- wl_data_offer has no shared outgoing wl_data_source transfer ID; local IDs cannot correlate own-window echo.
- wayland-client EventQueue::dispatch_pending processes all pending events and exposes no event-count budget; unbounded enqueue queues without explicit coalescing/overflow classes cause silent drops or duplicate processing.
- Ignoring seat or pointer-capability removal leaves stale grabs, data devices, and active transfers.
- Duplicating payload/action in both DndDrop and DropContext creates divergent sources of truth and may force unnecessary clones.
- wl_data_device.leave treated as safe post-drop; SCTK preserves dropped offers for later reads/Ask and leave is terminal only before drop.
- Ask path executed without correlated app-to-bridge outcome, final non-Ask set_actions() call, or finish post-completion leaves bridge/app state divergent.
- app-to-bridge choice/dismissal message lacks delivery_id correlation, breaking round-trip matching with compositor ack and DropComplete.
- Snapshot only target/action on acceptance instead of full context (modifiers, position, offered types, action), causing stale resume or failed correlated request.
- Canonical event schema lacks newly added correlation IDs (delivery_id, matched actions), breaking round-trip verification and correlated Ask/finish sequencing.
- Nonce retirement missing no-echo case, leaving echo identities unretired and risking future collision or replay.
- Compositor acknowledgement and DropComplete treated as sequenced instead of order-independent concurrent prerequisites; missing matching-action verification between them.
- Ask emits finish before compositor acknowledges the final non-Ask set_actions() call or DropComplete resolves, causing dangling bridge state or app continuation on stale action.
- Inconsistent DropComplete emission (e.g., Copy promised but not emitted) leaves app operations incomplete or bridge cleanup untraced.
- Queue-overflow fail-closed cleanup lacks guaranteed app-visible terminal latch, causing silent loss of queued operations or ambiguous app cleanup semantics.
- Missing acknowledgement, DropComplete::Failed, dismissal, or bounded completion timeout does not route through exactly-once terminal destroy/reject, causing orphaned Ask state or ambiguous app visibility.
- Completion timeout is unbounded or missing, leaving Ask state suspended indefinitely and blocking app cleanup.
- Acceptance criteria in one phase is satisfied without verifying prerequisite phases are complete according to the explicit dependency table; missing cross-check allows sequencing omissions to ship (e.g., Phase 4 filemgr acceptance depends on Phase 2 adoption but Phase 2 remains unconverged).

_Graduated from skill learning loop — confidence 99%, 5 uses, 5 successes._


### cold-review-dialog-wire-state-machine

**When:** Cold-review a public dialog wire schema or broker state machine with presenter leases, attempt tokens, deadlines, progress cancellation, and fair queues.

**Approach:** For dialog.v1 cold reviews, inspect schema validation separately from deserialisation resource limits; test shared internally-tagged enum tags and unknown-field acceptance—specifically, enum-level deny_unknown_fields requires unit variants to be empty struct variants (unit variants alone will not reject extra fields), and serde flattening is incompatible with strict deny_unknown_fields (must be removed); enumerate every terminal transition and every requeue/admission path; verify deadline checks occur on all mutations, not only a maintenance tick; distinguish broker-minted terminal CAS tokens (u64, consumed before validation) from internal presenter generation monotonic counters; model queue capacity and active/in-flight capacity as two separate bounded pools to ensure limit-1 requeue cases remain meaningful; treat externally supplied lease IDs and attempt tokens as an explicit unenforced freshness assumption; compare queued caps with total retained/in-flight records. Additionally: inspect whether a consumed rejected CAS has a recovery transition (missing transition leaves presenter irrecoverable); model presenter restart requeue as a batch operation rather than a loop, since state mutations during iteration alter eviction eligibility; account for pending event/report buffers in memory calculations—bounded terminal records alone do not bound deferred reports. Verify that strict presenter DTOs do not carry caller-asserted service identity when Bus provenance can be stamped—otherwise Stage 2 may mint authoritative leases from untrusted body data. Compare the broker's full mutable presentation snapshot against the wire DTO field-for-field; flag any omitted mutable fields (e.g., progress message) that require undocumented adapter rewrites to bridge. Identify no-op mutations that emit spurious self-transitions and verify they don't trigger unwanted state-machine side effects. **Critically:** audit quarantine termination paths—verify all graceful exits (Release, explicit Mark) are tracked by the same limit counter, not only Replace transitions; and enumerate all mutation edges fired by a single public call (e.g., maintain may fire two edges per resident *plus* open fires an additional edge), ensuring transition limits account for the full set, not just the primary per-resident edges.

**Watch out for:**
- A passing validate() does not bound allocations already performed by serde_json.
- Queue limits can hold on open() yet be exceeded by presenter-restart requeue.
- String-equality CAS only rejects stale actors if lease and attempt values are actually fresh.
- Terminal records can remain an unbounded memory sink even when live queues are capped.
- Enum-level deny_unknown_fields does not reject extra content in unit variants; they must become empty struct variants to enforce strict validation.
- Serde flattening is incompatible with deny_unknown_fields; strict validation requires removing all flattened fields.
- Queue capacity and active/in-flight capacity must be modelled as separate bounded pools to properly constrain a limit-1 requeue within a single queue entry.
- A consumed rejected CAS without a recovery transition leaves the presenter in an irrecoverable state.
- Modelling presenter restart requeue as a loop allows state changes mid-iteration to alter eviction eligibility, causing unpredictable retention.
- Bounded terminal records do not account for pending event/report buffers, allowing deferred reports to exhaust memory even when records are capped.
- Strict presenter DTOs carrying caller-asserted service identity allow Stage 2 to mint authoritative leases from untrusted body data.
- Wire DTO field-for-field mismatch: mutable fields (e.g., progress message) omitted from wire schema unless bridged by undocumented adapter rewrite.
- No-op mutations that emit self-transitions can cause state confusion and unwanted side effects in observers.
- Quarantine logic counting only Replace transitions can be bypassed by graceful Release before mark_presented, leaving an untracked terminal path.
- Bounded transition limits counting only primary mutations per resident miss additional mutation edges from the same public call (e.g., maintain+open), allowing overflows from an empty buffer.

_Graduated from skill learning loop — confidence 99%, 5 uses, 5 successes._


### review-public-hygiene-gate-changes

**When:** When reviewing changes to cmctl's public-repo hygiene gate, hook body, hook installer, or their regression harness.

**Approach:** 1. Read the cmctl mandate and review-stage prompts, then the targeted Mix/repo-hygiene memory. 2. Confirm staged, unstaged and untracked scope with `git status` and `git diff HEAD`. 3. Inspect production-script diffs separately from the large regression suite, tracing each hardened failure path against Git hook semantics and Mix builtin contracts. 4. Query the live Mix binary for any new primitives instead of inferring semantics. 5. Run `mix lint --deny-warnings` on every changed Mix script. 6. Test path grammars against legal POSIX whitespace (space, tab, newline; reject form-feed, vertical-tab, other controls) during parser validation. 7. Validate filesystem execution semantics: check ACL/noexec mount flags on hook directories, verify predictable temporary-root ownership (uid/gid invariants), and audit environment-marker namespace/re-entry paths for permission-boundary violations. 8. Check every parser field against its semantic invariant—separate paired controls (e.g., allowlist entry + scope) from discriminating probes (e.g., capability flags). 9. For repository dedupe, bind both canonical top AND absolute gitdir to prevent path-string aliasing bypasses; verify dedupe against both keys together, not top alone. 10. Implement two-pass preflight: first enumerate deterministic install-phase refusals (directory ownership, ACL/mount state, umask interaction), then check repository-resolution refusals separately. 11. Test older-runtime behaviour paths explicitly rather than trusting new syntax; run legacy fallback code paths end-to-end. 12. Validate that tests assert observable state changes, not exit codes alone: use byte-identical assertions (e.g., sentinel hook remains unchanged after a refusal) and top-level diagnostics, not downstream exit codes. 13. For operations that repeat (e.g., scans, searches), first establish a baseline: count how many legitimate iterations a correct run performs (e.g., measure ls-tree calls on known-good input before asserting duplicates). 14. Use focused mutant copies under /tmp to test each new row red without altering fixed repository sources—each mutation proves a specific sentinel catches the defect. 15. Run the full public-hygiene regression harness (247+ assertions), then validate the installed hooks with `--status` and run the real three-repo `--all` audit. 16. Add focused checks for edge cases the automated suite may miss: (a) core.hooksPath alias resolution (symlinks, indirect paths, canonical vs. top-string path binding), (b) TMPDIR and umask interaction during hook installation and verification, (c) URL fragment redaction under various content states, (d) `--status` output and behaviour under unwritable temporary directories, (e) hook execution under noexec mounts or restrictive ACLs. 17. Only report defects that survive the surrounding code, live-contract, filesystem-semantics and end-to-end checks.

**Watch out for:**
- Treating an exit-code-only test as proof that the intended hook or gate path ran; require byte-level assertions or state-change diagnostics instead.
- Inferring Mix builtin semantics from another language instead of checking `mix builtins`.
- Reviewing only unstaged diff or only production files and missing staged corrections or hollow tests.
- Calling an intentional fail-closed refusal a bypass without tracing what content can actually publish.
- Overlooking pathname byte/UTF-8 and trailing-whitespace semantics in Git output.
- Assuming the 247-assertion regression harness covers all edge cases without manually checking untested paths like core.hooksPath symlinks or permission-restricted operations.
- Testing only direct writable ancestors of .git/hooks without verifying core.hooksPath alias and symlink resolution under all configurations.
- Omitting explicit TMPDIR+umask interaction tests and --status validation under unwritable temp directories, allowing permission-related failures to ship.
- Ignoring filesystem execution semantics (noexec mounts, ACL restrictions) and assuming hook execution succeeds whenever file permissions allow read/write.
- Overlooking environment-marker namespace or re-entry issues in hook invocation paths, allowing privilege-escalation or permission-boundary violations.
- Assuming predictable temporary-root ownership without verifying uid/gid invariants across all host configurations, allowing credential-leakage or privilege-confusion bugs.
- Treating paired controls (e.g., allowlist entry + scope binding) as independent and missing semantic violations where one control depends on the other's state.
- Testing only new syntax paths and skipping legacy fallback code routes that older runtimes or configuration states must take.
- Measuring command/scan counts without establishing a baseline of legitimate operations; always count expected iterations on known-good input before asserting duplicated work.
- Deduping repository paths by top string alone without resolving to canonical path and matching against absolute gitdir, allowing duplicate hook installations or bypass via path aliasing.
- Using single-pass preflight that checks only repository-resolution refusals, missing deterministic install-phase refusals (directory ownership, ACL state, umask interaction) that must be enumerated separately.
- Overlooking POSIX whitespace in path grammars (tabs, form-feeds, vertical-tabs) during parser validation, allowing malformed paths through.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._


### harden-trayd-split-contract

**When:** Maintain or review cosmix-trayd/cosmix-tray after the headless daemon and SNI skin split, especially D-Bus snapshots, refresh lifecycle, manager-qualified systemd units, daemon restarts, sender-bound lease lifecycle, D-Bus session isolation, or UI lifecycle events in Plasma plugin.

**Approach:** Keep Bus entirely behind trayd; QML sends only UUIDs and metadata via D-Bus method call headers, never complex payloads or transport handles. Trayd is the resolver: it translates UUIDs to files, resolves xdg-open paths, configures transient units, and manages stop targets. Extract sender identity from the zbus method Sender header, not from app state or other channels. Revoke all leases (Bus subscriptions, policy, state) immediately on D-Bus unique-name loss via NameOwnerChanged signals to prevent orphaned references. Use a single bounded publication lane (not multiple or unbounded queues) for D-Bus property updates to avoid blocking the executor. For systemctl queries, represent each daemon as (manager, unit, status); query systemctl and systemctl --user independently, merge successes, and retain errors. Publish a GetSnapshot tuple cloned under one snapshot mutex for all clients. Model refresh as Mutex<Idle|Running{pending,started}>; overlapping requests set pending and the worker runs one coalesced follow-up before idle. In the SNI client, subscribe separately to Changed and org.freedesktop.DBus.NameOwnerChanged; install Unavailable on loss and Waiting plus Refresh on acquisition. Protect Pending/Connected handle delivery with one mutex. In the Plasma QML plugin: use exclusively async QtDBus method calls (never synchronous calls) to avoid blocking the Qt event loop; implement hidden-tab snapshot gating to defer updates when the plugin tab or popup is hidden, preventing stale state during rapid show/hide cycles; bind the call sender identity to the tab/popup lifetime and close the associated lease when the UI element closes. When spawning child processes via dbus-run-session or similar isolation gates, explicitly override DBUS_SESSION_BUS_ADDRESS to the real XDG_RUNTIME_DIR/bus so org.freedesktop.systemd1 activation reaches the user session broker, not the isolated bus. Verify handwritten XML against live gdbus introspection, test all state machines, and run memguard release/test plus clippy/fmt and no-Tokio checks.

**Watch out for:**
- zbus exposes an ordinary Rust tuple return as one D-Bus struct out argument; handwritten XML with multiple out args will not match live introspection.
- Two mutexes for handle and pending view permit a lost initial view during hand-off; use one enum under one mutex.
- An unconditional panic guard can clear a newer worker's Running state after the prior worker goes idle; disarm it on normal completion.
- Treating a non-empty partial manager error as total daemon failure hides units successfully returned by the other manager.
- Cargo commands may rewrite workspace-package version lines in Cargo.lock even with --locked; restore pre-existing lock entries when scope forbids Cargo.lock changes.
- Splitting Bus transport or subscription logic across skin clients and trayd allows duplicate or conflicting subscriptions and makes lease revocation fragmented; keep all Bus behind trayd.
- Extracting sender identity from app state or configuration instead of the zbus method header can route messages to the wrong identity, especially if clients reconnect or unique-names are reused.
- Not revoking leases on D-Bus unique-name loss leaves orphaned Bus subscriptions and stale state in trayd, causing ghost policy and missed lifecycle updates even after the sender disconnects.
- Using an unbounded queue or multiple bounded lanes for D-Bus property updates blocks the executor when one lane is full, causing timeouts and cascading failures; enforce a single bounded publication lane.
- Using synchronous QtDBus method calls in the Plasma QML plugin instead of async calls blocks the Qt event loop, causing UI hangs and unresponsiveness during trayd communication.
- Not closing sender-bound leases when Plasma plugin tabs or popups close leaves phantom leases that persist after the user dismisses the view, preventing proper cleanup and keeping stale sender references alive in trayd.
- Private dbus-run-session gates that do not override DBUS_SESSION_BUS_ADDRESS to the real XDG_RUNTIME_DIR/bus cause org.freedesktop.systemd1 activation to fail on the isolated bus, blocking systemd-run and systemctl calls from child processes.
- Not implementing snapshot gating for hidden Plasma plugin tabs or rapid show/hide cycles allows stale or intermediate state snapshots to propagate to the UI, causing visual inconsistencies or missed lifecycle updates when the tab reappears.
- Sending complex payloads, file handles, or resolver-dependent data from QML instead of simple UUIDs and metadata breaks the trayd resolver contract and reintroduces transport coupling; QML remains identity-only.
- Mixing async and sync QtDBus calls in the same Plasma plugin, even for different methods or daemon interfaces, causes event loop contention and UI hangs when the sync call blocks during trayd communication.

_Graduated from skill learning loop — confidence 98%, 5 uses, 5 successes._


### implement-midi2-phase10-packet-model

**When:** Implement or review Phase 1.0 MIDI 2.0 UMP packet/message types in cosmix-musicd without the normative PDF.

**Approach:** 1. Treat ni-midi2, AM_MIDI2.0Lib, and bl-midi2-rs as local spec witnesses and cite exact source test names; do not imply the gated PDF was read. 2. Keep Ump as a fixed [u32;4] plus MT-derived length and expose only a raw routing_nibble at that layer. 3. Make packet iteration total across all 16 MTs, with NeedMoreWords only for a truncated final packet. 4. Model MT2 and all defined MT4 statuses as Copy enums whose semantic payloads retain a private raw Ump, allowing byte-exact reserved-bit decode/re-encode. 5. Model Utility/System/SysEx7/Data128 in Message; known-family unknown statuses stay in that family's Unknown(Ump), while unknown MTs use Message::Unknown. Utility and unknown MTs have no semantic group. For SysEx7, retain the group in the bounded typed fragment to preserve routing context through byte-stream ↔ UMP translation paths; translators and stateful routing depend on this preservation. 6. Port NI known-word vectors for every MT4 status and hand-roll group/channel/value-domain loops. When parsing NI's witness code, note that parser_channel_voice_messages deliberately leaves one running-status data byte incomplete before a new status—track this partial-message abort with a local recovery counter. 7. Validate the packet model end-to-end through .cosump validation paths and SMF timeline bridge integration to ensure deterministic routing preservation and bounded SysEx framing carry through downstream consumers. During adversarial or fix rounds, treat the packet-layer type boundaries as immutable design anchors: state/timeline adapters conform to the packet model, not vice versa. 8. For CLI consumers (dump, playback, head, splice), handle EPIPE and broken pipes gracefully: detect early stream termination (e.g., `dump | head -5`) without spurious error logs, verify documented smoke-test paths work correctly, and exit cleanly when the pipe closes. 9. Run memguard cargo test --lib --no-default-features, zero-warning clippy on all targets with no defaults, cargo fmt --check, and git diff --check.

**Watch out for:**
- MIDI Song Position Pointer data is LSB-first; the NI word for group 9/value 0x34F4 is 0x19F2_7469.
- SysEx7 byte 1 is format in the high nibble and payload count in the low nibble; complete six-byte framing is 0x06, not 0x66.
- SysEx8 byte count includes the stream-id byte, so zero payload encodes count 1 and maximum 13-byte payload encodes count 14.
- Do not expose the raw bits 27..24 as group on Utility; expose routing_nibble on Ump and Option group on Message.
- Mask seven-bit controller/index fields in constructors, but retain decoded high/reserved bits through the private raw Ump.
- cargo fmt -p cosmix-musicd currently applies Rust 2024 formatting drift to pre-existing mixer_host.rs and tests/midi_synth.rs; audit and report those mechanical edits.
- NI's parser_channel_voice_messages deliberately leaves one running-status data byte incomplete before a new status; implement a local recovery counter to record the partial-message abort, not skip it silently.
- Discarding the SysEx group from the bounded typed fragment breaks routing preservation in byte-stream ↔ UMP translation; translators and stateful routing contexts depend on group retention.
- CLI dump/playback consumers must handle EPIPE and broken pipes gracefully; write failures from pipes closing (e.g., `dump | head -5`) should not log spurious errors or panic, only exit cleanly with partial output already written.

_Graduated from skill learning loop — confidence 97%, 5 uses, 5 successes._


### Import Wayland DMA-BUF into Bevy 0.19 on Vulkan

**When:** Implement or debug linux-dmabuf texture import for a Bevy 0.19 / wgpu 29 Smithay compositor.

**Approach:** 1. Create the Vulkan instance/device manually, adding VK_EXT_image_drm_format_modifier, VK_EXT_external_memory_dma_buf, VK_KHR_external_memory_fd and VK_EXT_physical_device_drm to wgpu-hal's required extensions, then wrap the exact instance/adapter/device/queue as Bevy RenderResources. 2. Query only RGB DRM fourcc/modifier pairs whose Vulkan modifier has one plane, SAMPLED_IMAGE support and importable external memory; build Smithay v5 default feedback from that device dev_t and set. 3. On commit, duplicate each plane fd and send owned dimensions/fourcc/modifier/offset/stride metadata across the existing protocol-to-render channel. 4. Create a modifier-tiled VkImage with explicit layouts and external-memory create info, intersect image memory requirements with vkGetMemoryFdPropertiesKHR bits, import each fd with VkImportMemoryFdInfoKHR, and bind planes (disjoint for multi-plane). 5. Wrap using wgpu-hal Vulkan Device::texture_from_raw with TextureMemory::External and a final drop callback, then Device::create_texture_from_hal. 6. Immediately after wrapping, prime wgpu tracking through transition_resources with an empty subresource selector and TextureUses::UNKNOWN so the untouched image becomes complex UNKNOWN without a HAL barrier, then perform the raw EXTERNAL GENERAL -> local SHADER_READ acquire; the first RESOURCE use will adopt the real state without transition. Before raw release, transition to RESOURCE so both sampled and culled images have a known tracked state. 7. After Bevy prepares GpuImage, replace its texture/view/descriptors in RenderSystems::PrepareAssets before draw preparation. Critically: immediately after replacing RenderAssets<GpuImage> textures and before PrepareBindGroups executes, clear/evict the ImageBindGroups cache entry for that AssetId<Image> to prevent stale bind groups from sampling the prior texture. 8. Disable Bevy pipelined rendering or explicitly queue main-world replace/unregister operations for render-thread retirement to prevent race conditions between main-world asset updates and render-world sampling. 9. Acquire EXTERNAL-to-wgpu queue ownership before rendering and release it during Cleanup. 10. Manage ImportedDmabufImages as a surface-owned registry (not frame-scoped): retain registration across transient RenderAssets<GpuImage> absence and import failures; store Pending/Applied state in render-world and implement retry logic for failed imports. Only unregister on explicit main-world surface unmap/destruction or mode switch. Refcount wl_buffer retention by Wayland object identity to ensure every fd is released exactly once per imported buffer. Retain the Smithay wl_buffer by token until the final Vulkan texture drop callback sends a release command to the protocol thread; explicitly retire coalesced frames and pending imports. 11. Reject XRGB formats unless an opaque sampling pipeline exists to prevent alpha-channel undefined behavior. 12. Test with a real Vulkan create/bind/destroy probe covering many sequential replacements, rejection paths (failed imports, dropped commitments), and callback-release cleanup before notifier.successful to verify no dangling fds or protocol handles remain. 13. Gate with memguard cargo check/test/clippy -D warnings/build and leave live GPU/client execution to the interactive session.

**Watch out for:**
- wgpu-core initially tracks create_texture_from_hal textures as UNINITIALIZED; the first internal transition is driver-sensitive for imported contents and must be live-tested on the target Mesa stack.
- Choosing a Vulkan adapter before a winit surface exists is only safe for the current single-GPU nested target; multi-GPU hosts need surface-aware selection.
- Do not select a memory type from image requirements alone; intersect with vkGetMemoryFdPropertiesKHR.memory_type_bits.
- Destroy an imported VkImage before freeing its bound VkDeviceMemory, and clean command pools/fences on every Vulkan error path.
- Do not release wl_buffers at commit or main-world asset replacement; Bevy render-world lag means the GPU may still sample them.
- Explicit sync is absent on wgpu 29; this design relies on Mesa implicit DMA-BUF synchronisation plus queue-family ownership transfer.
- ImageBindGroups caches bind group state by AssetId<Image>; for a stable surface asset whose imported TextureView changes per-commit, failing to clear the cache in PrepareAssets between RenderAssets<GpuImage> replacement and PrepareBindGroups causes stale bind groups and broken GPU sampling.
- Priming create_texture_from_hal from UNINITIALIZED to RESOURCE|COPY_SRC still emits a Vulkan UNDEFINED barrier and moves the imported-content discard risk earlier; use transition_resources with TextureUses::UNKNOWN and empty subresource selector instead to mark the image complex UNKNOWN without a HAL barrier.
- Not aligning raw Vulkan layouts with wgpu-core's internal layout tracking causes layout mismatch errors during shader sampling and potential validation layer failures.
- Not normalizing sampled/culled textures to RESOURCE state before release leaves stale wgpu state that corrupts subsequent texture operations or causes resource leaks.
- Unregistering ImportedDmabufImages on transient RenderAssets<GpuImage> absence or import failure causes render-world state loss; registration is surface-owned and must survive import retries, multiple replacements, and rejection paths until explicit main-world surface unmap/destruction or mode switch.
- Not retaining Pending/Applied state across render-world ticks allows retry logic to be lost on import failure, causing permanent sampling failure even when recovery is possible.
- Enabling Bevy pipelined rendering without deferring main-world replace/unregister to render-thread retirement causes race conditions where the main world modifies assets while the render thread is sampling, leading to undefined texture state or crashes.
- Not refcounting wl_buffer retention by Wayland object identity causes premature fd closure or double-release when multiple render passes reference the same buffer.
- Accepting XRGB formats without an opaque sampling pipeline causes alpha-channel undefined behavior or compositing corruption when the alpha channel is read by shaders or composited over other surfaces.
- Testing only with notifier.successful instead of a real Vulkan create/bind/destroy probe misses driver-specific crashes, state corruption, or layout conflicts that only manifest during actual GPU execution.
- Not testing rejection paths (failed imports, dropped commitments) and callback-release cleanup can hide dangling fds or protocol-handle leaks that accumulate across many failed imports.

_Graduated from skill learning loop — confidence 96%, 5 uses, 5 successes._


### Mirror Smithay SHM surfaces into Bevy ECS

**When:** Implement a nested Smithay compositor whose wl_shm xdg-shell surfaces are displayed and driven through a Bevy 0.19 application.

**Approach:** Spawn Smithay Display/EventLoop/calloop state on a dedicated named protocol thread; do NOT place it in Bevy NonSend or pump it in Bevy's main schedule. The protocol thread owns a cloned wl_display poll fd and polls socket accept, listener registry dispatch, and a calloop command channel. On surface commit, inspect SurfaceAttributes directly, take the current buffer, validate SHM offset/stride/dimensions with checked arithmetic, copy each row while the mapping is valid, convert Xrgb8888/Argb8888 BGRA bytes to Bevy RGBA, then DO NOT release the wl_buffer immediately. Assign stable compositor surface IDs and emit owned protocol events (map/update/unmap/destroy) into a small bounded non-blocking batch channel with per-surface coalescing to prevent a stalled Bevy renderer from back-pressuring protocol dispatch or accumulating unlimited frames. Bevy Last schedule reads the batch channel and mirrors events into ECS entities and Image assets. Bevy Last also collects Frame commands containing host pointer/keyboard input and sends them via the calloop command channel; only Frame commands complete client frame callbacks. For wl_surface.frame requests, deliver callbacks immediately for configured toplevel records even before their first buffer maps; do not filter callback delivery to mapped surfaces only, as clients such as alacritty request callbacks before committing their first buffer. For subsurface transaction handling: track xdg_surface state through sync/desync lifecycle and defer configure sends until parent transaction boundaries; GTK and Firefox clients use synchronized subsurfaces that require atomic composition where parent configure/buffer changes apply to all descendants together. Defer initial XDG configure until after the first wl_surface.commit on the toplevel, allowing subsurface hierarchy establishment; premature configure before subsurfaces commit causes clients like Firefox to remove buffers or ignore them because the surface tree is not yet synchronized. Maintain compositor-side geometry for hit testing and cascading. Translate Bevy pointer and physical-key events into Smithay seat events using Linux evdev keycodes plus the XKB offset. Distinguish legitimate static clients (committed once or twice with no further frame requests) from callback starvation (blocked client waiting indefinitely for frame callbacks); static clients complete naturally without repeated callbacks, while starvation implies protocol thread is not waking or frame callbacks are not dispatched. CRITICAL RESOURCE MANAGEMENT: retain wl_buffers on the Smithay thread until Vulkan texture drop callbacks complete; GPU references outlive CPU allocation and releasing before render completion causes use-after-free. Track buffer lifecycle through frame queuing and supersession: when a queued frame is superseded before rendering, release its token and wl_buffer reference immediately to prevent allocation exhaustion. Clean up pending SHM imports on all error paths and compositor shutdown; orphaned GPU resources cause descriptor pool exhaustion or OOM. Use AutoVsync so correctness does not rely on presentation mode. Add regression tests: (1) two sequential wl_display.sync requests with no Bevy Frame command, proving display-fd wakes without rendering; (2) synchronized subsurface parent+child geometry and buffer application exercising transaction-aware composition. Cover conversion, stride handling, buffer lifecycle, frame callback delivery, configure timing, and subsurface transactions with unit tests and run memguard-wrapped check, test, clippy -D warnings, and build gates.

**Watch out for:**
- Keeping Smithay state in Bevy NonSend and pumping in First schedule causes wgpu present/acquire to block indefinitely when the window is occluded or on another workspace, starving socket accept and registry dispatch; move protocol state to a dedicated thread with its own calloop and display-fd polling.
- Calling Smithay's generic on_commit_buffer_handler before custom import can consume or hide the buffer; inspect SurfaceAttributes directly for the SHM importer.
- Do not construct a Rust shared slice over client-writable SHM; copy raw rows inside Smithay's mapping closure while the mapping remains valid.
- Holding the NonSend compositor borrow while mutating Bevy ECS or Assets causes borrow conflicts; collect owned protocol events first, then apply them via the batch channel.
- ARGB8888 pixels are premultiplied alpha; unpremultiply RGB before uploading to a straight-alpha Bevy image.
- Bevy physical KeyCode must be mapped to Linux evdev codes, and Smithay/XKB expects the evdev code plus 8.
- Damage rectangles can be drained and logged, but mutating/replacing a Bevy Image still causes a full GPU upload unless a partial render-world upload path is implemented.
- Clear consumed buffer assignments, damage records, and frame callbacks so commits and callback completions are not repeated.
- Handle compositor surface destruction as well as explicit unmap so client crashes despawn ECS entities without killing the compositor.
- The batch channel between protocol thread and Bevy Last must be bounded and non-blocking with per-surface coalescing; unbounded or blocking channels cause back-pressure into protocol dispatch or accumulate unlimited pending frames when the renderer stalls.
- Frame commands must be sent via calloop command channel from Bevy Last, not from First or other schedules; client frame callbacks are only completed when Frame commands arrive, and out-of-order or missing commands cause client hangs or dropped visual updates.
- Releasing wl_buffers immediately after GPU texture creation or GPU import causes premature deallocation; Vulkan textures hold GPU references that outlive CPU allocation. Retain wl_buffers on the Smithay thread and release only from the texture drop callback path to ensure GPU work completes before client memory is freed.
- Queued frames that are superseded before rendering must release their wl_buffer tokens and drop buffer references; forgetting token release causes wl_buffer allocation exhaustion and client hangs waiting for buffer availability.
- Pending SHM imports that fail or are abandoned on error paths must be explicitly cleaned up; orphaned GPU resources or orphaned CPU mappings prevent resource reclamation and cause compositor OOM or Vulkan descriptor pool exhaustion.
- Changing buffer release timing from immediate-after-copy to drop-callback requires careful synchronization across thread boundaries; missing release barriers or synchronization allows the protocol thread to deallocate the buffer while the render thread still holds a Vulkan reference, causing use-after-free.
- Filtering wl_surface.frame callback delivery to mapped surfaces only deadlocks clients like alacritty that request callbacks before their first buffer commit; deliver frame callbacks immediately for configured toplevel records regardless of buffer mapping state.
- Sending initial XDG configure before subsurfaces are committed causes clients like Firefox and GTK to remove buffers or ignore committed buffers because the surface tree is not yet synchronized; defer initial configure until after the first toplevel commit and subsurface hierarchy is established.
- Neglecting transaction-aware subsurface composition where parent configure/buffer changes apply atomically to all descendants breaks synchronized subsurface clients like Firefox and GTK; track xdg_surface sync/desync state and defer configure propagation until parent transaction boundaries.
- Confusing legitimate static clients (that commit once or twice and never request further frame callbacks) with callback starvation (blocked client waiting for frame callbacks); static behavior is correct, starvation requires protocol thread diagnosis; mistaking static for starvation can trigger unnecessary protocol debugging or incorrect frame callback injection.
- Buffering all committed frames without accounting for subsurface parent synchronization causes child buffers to apply out-of-transaction, rendering ahead of parent configure and breaking visual coherence in multi-surface hierarchies like desktop decorations or complex GTK layouts.

_Graduated from skill learning loop — confidence 90%, 5 uses, 4 successes._
