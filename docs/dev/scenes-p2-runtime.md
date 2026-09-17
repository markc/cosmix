# Scene panel runtime guarantees

CTK retains strong font data handles for the faces in its current sans-serif
and monospace mappings. Rebuilding the sole Parley layout after source-cache
pruning preserves the font identity and reuses Bevy's glyph atlas. Changing
the mappings releases sources no longer used by either managed family.

Panel drags stop at the available output budget, including when one pointer
event crosses the limit. `shell.panel.resize` returns rc 10 with `error`,
`edge`, `requested`, and `max` when the requested thickness exceeds that
budget. Successful Bus replies follow model application, so queued geometry
or opposing-panel changes cannot produce a false acceptance.

Broker-stamped mesh callers can use every CTK app-control verb without
principal attestation. Local calls retain their registered-caller checks.
Operation validation and editing-state invariants apply to both paths.
