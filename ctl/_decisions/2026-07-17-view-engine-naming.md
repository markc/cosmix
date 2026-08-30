# View/engine naming for musicd surfaces (ADR)

**Status: ACCEPTED** (Mark, 2026-07-17 — "the table reads right, go ahead").
Owner-approved same-day; supersedes ad-hoc names (`ctk-mixer-<pid>`, "board").

## Decision

Every musicd-facing UI surface is named on two orthogonal axes:

- **view** — what it shows. Single lowercase plain-English noun; the role a
  person or agent asks for. Never studio jargon ("board" is out; "mixer" is
  the word everyone knows).
- **engine** — what draws it: the CONCRETE renderer arm (`skia`, `bevy`,
  `egui`, `iced`, `html`, …). Never a category ("gpu" is not an engine —
  bevy is also GPU; "egpu" reads as a class when it's a sibling).

**ABP registration:** `<view>-<engine>-<instance>` (instance = pid), e.g.
`mixer-bevy-534734`, `mixer-egui-…`, `mixer-html-…`. The bare `<view>` name
resolves to the current default engine at the launcher level — promoting a
bake-off winner is a default change, never a rename, and every arm stays
callable by full name forever.

**Role discovery:** names are for humans; discovery is by fields. Every
app-control surface (`ctk-app-control.v0+`, renderer-agnostic) reports
`view` and `engine` in `app.describe`, so an agent asks the roster "who
serves view=mixer" instead of parsing strings. Shipped for the bevy arm in
ctk 0.5.0; the egui/html arms adopt the same two fields.

**Launcher (future, L2):** a Mix script (`view mixer [engine]`) — running?
focus/report : spawn the view's default engine. Pure orchestration; no new
daemon. A `viewd` only if the script outgrows itself.

## The view vocabulary (reserve these; build over the next year+)

| view | what it is | status 2026-07-17 |
|---|---|---|
| `mixer` | faders/pans/mutes/solos + master + transport | LIVE: bevy arm (ctk 0.5.0, fully conformant) + skia arm (cos `b9c76f5`, PRE-DECISION — see scope note); egui + html arms next |
| `wave` | per-channel waveform display/edit | planned |
| `pianoroll` | MIDI note editing (Ardour-style) | planned |
| `tracks` | arranger/timeline (Ardour's "Editor") | planned |
| `transport` | compact play/locate/loop bar | planned |
| `meters` | dedicated meter bridge | planned |
| `spectrum` | analyzer | planned |
| `automation` | envelope lanes (may fold into `tracks`) | planned |
| `keys` | on-screen keyboard / MIDI input | planned |
| `library` | soundfont/sample/session browser | planned |
| `routing` | patchbay — who feeds whom | planned |
| `score` | notation | speculative |

Rules for future views: single word where possible, lowercase, the noun a
newcomer would guess; a view name is public API from first ship (renames are
one-way doors) — check this table first, extend it by ADR addendum.

## Scope note: the pre-decision skia arm

The disp-skia mixer predates this ADR: it registers under its own identity
and does not answer `app.describe` (it has no app-control surface at all —
that contract was born in ctk 0.4.0). It is **grandfathered, not exempt**:
the scheme binds every NEW surface immediately (egui/html arms conform from
first commit), and the skia arm adopts `mixer-skia-<pid>` + the describe
fields the next time it is materially touched. Until then, a launcher that
resolves `mixer` must special-case the skia arm's legacy identity — that
special case is the migration debt, documented here so it can't silently
become permanent.

## Consequences

- ctk 0.5.0 registers `mixer-bevy-<pid>` (+`-sub` telemetry plane) and its
  `app.describe` carries `view`/`engine` — a breaking service-name change,
  pre-1.0 MINOR bump.
- The mixer bench arms are `mixer-egui` and `mixer-html` (not "egpu").
- The app-control contract spec (when it graduates from draft — Mark gate)
  standardizes `view`/`engine` as required describe fields.

*Drafted by Claude Fable 5 from Mark's 2026-07-17 naming question; the
12-view table is Mark-approved verbatim intent, wording Claude's.*
