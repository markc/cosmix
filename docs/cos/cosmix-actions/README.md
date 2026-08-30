# cosmix-actions

`cosmix-actions` is the mesh-free action spine for Cosmix applications and CTK menus. It defines stable action names, serialisable metadata, a local handler registry, engine-independent keyboard input, layered strict-data keymaps, and deterministic chord resolution. In the `bus ← mix ← cos` dependency chain it sits in `cos`; it uses `cosmix-lib-config` for the Mix strict-data serde bridge and deliberately has no direct Bus or mesh dependency.

## Synopsis

Rendering engines translate keyboard events into `RawInput`. Applications register
`ActionMeta`, handlers, and enabled predicates in an `ActionRegistry`. A `Keymap`
maps input chords to `ActionId` values, while `resolve` and `resolve_timeout`
advance caller-owned `ResolveState`.

The crate does not read a wall clock. Callers supply a monotonic `Tick`, which
makes input replay and timeout testing deterministic.

The crate is a library. It has no binary, CLI, configuration search path, or
Cargo feature flags.

## Public API

| Area | Main items | Purpose |
|---|---|---|
| Action identity | `ActionId`, `ActionIdError` | Validate, intern, compare, display, and serialise stable action names |
| Input model | `Key`, `Modifiers`, `KeyStroke`, `Chord`, `RawInput`, `FocusContext`, `Tick` | Represent engine-normalised physical keyboard input |
| Keymaps | `Keymap`, `Binding`, `BindingOverride`, `EffectiveBinding` | Define app defaults and per-app user overrides |
| Keymap I/O | `load_keymap`, `parse_keymap`, `to_keymap_mix`, `save_keymap` | Read, validate, emit, and write strict-data `.mix` keymaps |
| Resolution | `ResolveState`, `Resolved`, `ResolveOutcome`, `resolve`, `resolve_timeout` | Resolve strokes and chord timeouts without engine or clock dependencies |
| Registry | `ActionRegistry`, `ActionMeta`, `ActionMetadata` | Join queryable metadata to app-local runtime behaviour |
| Invocation | `ActionArgs`, `ActionValue`, `ActionHandler`, `EnabledPredicate` | Validate typed arguments and invoke enabled handlers |
| Source policy | `ActionSource`, `ActionSources`, `InteractiveAction` | Control which ingress sources may request an action |
| Built-ins | `fable`, `fusion`, `theme` | Publish canonical action identifiers for bundled applications |

Most types are re-exported from the crate root. The `fable`, `fusion`, and
`theme` identifier sets remain public modules.

## Action identifiers

`ActionId::from_static` constructs a compile-time identifier from a string
literal. `ActionId::intern` validates and interns a runtime string for the
remainder of the process. `ActionId::validate_str` checks an untrusted name
without allocating or interning it.

Identifiers are non-empty ASCII strings. They may contain letters, digits,
`.`, `-`, `_`, `:`, and `/`. An identifier is at most 128 UTF-8 bytes.

Static identifiers do not consume the dynamic interner budget. Reusing an
already interned value also consumes no additional slot.

## Input model

`Key` covers ASCII letter and digit keys, navigation and editing keys, common
punctuation keys, and function keys `F1` through `F24`. Engine adapters pass
physical keys rather than locale-produced text; focused widgets remain
responsible for text editing.

`KeyStroke` combines one key with exact `Ctrl`, `Alt`, `Shift`, and `Super`
modifier state. `Chord` contains one to eight ordered strokes. Its display and
serde spelling uses forms such as `Ctrl+K` and `Ctrl+K, Ctrl+C`.

`BindingScope` supports:

- `global` when no modal owns input;
- `modal:name` for the named captured modal;
- `focus:name` for an application-defined focus tag.

Modal capture is exclusive. Focus-tag bindings outrank global bindings.
`allow_in_editable` controls whether a binding may resolve while an editable
widget has focus. `RepeatPolicy` either ignores or admits operating-system key
repeat presses.

## Keymap format

Keymaps use strict-data `.mix` and schema version 1:

```mix
{
  version: 1,
  chord_timeout_ms: 1000,
  defaults: [
    {
      action: "view.refresh",
      chord: ["Ctrl+R"],
      scope: "global",
      repeat: "ignore",
      allow_in_editable: false
    }
  ],
  custom: [
    {
      action: "view.refresh",
      chord: ["F5"],
      scope: "global",
      repeat: "ignore",
      allow_in_editable: false
    }
  ]
}
```

Any custom entry for an `(action, scope)` pair removes every default for that
pair. A custom entry with `chord: nil` leaves the action unbound in that scope.
Several non-`nil` custom entries may assign several replacement chords.

`Keymap::effective_bindings` applies this layering. `Keymap::binding_for`
returns a usable global accelerator label and omits shadowed or conflicted
bindings. `Keymap::diagnostics` reports replaced defaults, shadowed exact
bindings, and equal-priority conflicts.

## Resolution

`resolve` processes one `RawInput` event. `resolve_timeout` polls an existing
partial chord against the current focus context and keymap. Both functions
return emitted action identifiers, a `ResolveOutcome`, and non-fatal
`ResolveDiagnostic` values.

Resolution applies these rules:

1. A captured modal is exclusive; otherwise matching focus tags beat globals.
2. Custom exact bindings beat default exact bindings.
3. Longer chords preserve a shorter completed binding as a timeout fallback.
4. Equal-priority exact bindings for different actions report a conflict and do not fire.

Shared prefixes are not conflicts. Releases do not resolve actions. A pending
prefix is invalidated when relevant focus or effective keymap state changes.

## Action registry

`ActionMeta` carries the stable identifier, label, typed argument schema,
optional category, icon name, description, interaction metadata, and source
allowlist. It contains no handler or process pointer.

`ActionRegistry::register` joins metadata to an `ActionHandler` and an
`EnabledPredicate`. The registry rejects duplicate identifiers, malformed
schemas, and capacity overruns. It exposes stable-order metadata queries,
snapshots, a structural revision, and aggregate metadata accounting.

`invoke` validates and runs an app-local action. `invoke_from` additionally
enforces an explicit `ActionSource`. `validate_invocation` and
`validate_invocation_from` perform the same live enabled-state and typed
argument checks without running the handler.

`metadata_named` looks up an untrusted runtime name without interning it. It is
the appropriate lookup boundary for ingress adapters that must avoid consuming
the process-lifetime interner with unknown names.

Action arguments are string, finite number, or boolean values. Schemas may
mark fields as required and may either reject or permit undeclared fields.

## Source policy and interaction

`ActionSources::default` permits app, keyboard, mouse, and menu invocation. It
denies Bus, MIDI, and OSC until each source is explicitly enabled for the
action. `ActionSources::Bus` adds Bus to the permitted local sources.

An action with `InteractiveAction` metadata requires local interaction.
Interactive actions may name a typed non-interactive `direct_verb`, or declare
no remote equivalent. Metadata decoding and live registration reject every
interactive action whose source policy permits Bus.

The crate defines source-policy metadata but no Bus service or verb handlers.

## Bundled identifiers and keymaps

The `fable` module publishes canonical file, navigation, view, selection, and
application action identifiers. The `fusion` module publishes transport, song,
session, view, settings, and export identifiers. The `theme` module publishes
shared mode and colour-scheme identifiers.

`FABLE_DEFAULT_KEYMAP_MIX` and `FUSION_DEFAULT_KEYMAP_MIX` expose the checked-in
default keymaps as strings suitable for `parse_keymap`. Each application module
also exposes ordered menu and default-keymap identifier arrays.

## Bounds

| Boundary | Limit |
|---|---:|
| Action identifier length | 128 bytes |
| Dynamically interned identifiers per process | 16,384 |
| Strokes per chord | 8 |
| Distinct action identifiers per keymap | 1,024 |
| Default plus custom bindings per keymap | 4,096 |
| Keymap source size | 1 MiB |
| Metadata records per transactional decode | 4,096 |
| Argument fields per action | 128 |
| Metadata source or live-registry metadata | 1 MiB |
| Actions per live registry | 4,096 |

Keymap chord timeouts must be between 1 and 60,000 milliseconds.

## Dependencies

`cosmix-lib-config` supplies strict-data `.mix` decoding and encoding. `serde`
provides the serialisable data model. `thiserror` provides typed public errors.
