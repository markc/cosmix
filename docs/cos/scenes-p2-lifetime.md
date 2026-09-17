# Scene font lifetime and resize receipts

CTK retains font data observed in changed shaped text and editable layouts.
Named families and fallback-selected faces have the same lifetime guarantee.
Unused system font families are not loaded for retention. Used Blobs remain
alive for the process lifetime because Bevy retains their atlases; dropping a
Blob earlier could give a returning face a new identity and a second atlas.
Idle layouts do not perform retention reconciliation.

CTK app and action control, including wallpaper preferences, accept mesh
deliveries without principal attestation. `COSMIX_MESH_OPEN=0` opts into
registered-local-only admission. A unique broker origin is still required.

Quoin resize replies follow model application. Budget refusals preserve
`PANEL_THICKNESS_BUDGET` and the edge, requested size and maximum even if the
geometry changes after admission; a resize overtaken by an output change
returns `PANEL_OUTPUT_CHANGED`. Missing model receipts expire after 120
service frames with `PANEL_RESIZE_TIMEOUT`; disconnect discards pending receipts
and queued replies from the old connection.
