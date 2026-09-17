# Scene panel runtime guarantees

CTK retains strong font data handles observed in changed shaped text and
editable layouts, including explicitly named and fallback-selected faces.
Rebuilding the sole Parley layout after source-cache pruning preserves the
font identity and reuses Bevy's glyph atlas. Used sources remain alive for the
process lifetime, matching the atlas lifetime; unused system families are not
loaded for retention. Unchanged layouts perform no retention reconciliation.

Panel drags stop at the available output budget, including when one pointer
event crosses the limit. `shell.panel.resize` returns rc 10 with `error_code`,
`edge`, `requested`, and `max` when the requested thickness exceeds that
budget. `error_code: PANEL_THICKNESS_BUDGET` preserves the structured refusal
for Mix callers, including a budget refusal at model application.
Successful Bus replies follow model application, so queued geometry
or opposing-panel changes cannot produce a false acceptance.
Missing model receipts expire after 120 service frames with
`PANEL_RESIZE_TIMEOUT`. Connection loss clears pending receipts.

Broker-stamped mesh callers can use every CTK app-control verb without
principal attestation. `COSMIX_MESH_OPEN=0` opts into local-only admission,
matching term. Local calls retain their registered-caller checks.
Operation validation and editing-state invariants apply to both paths.
