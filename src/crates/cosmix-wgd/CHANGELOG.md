# Changelog

## 0.4.1 — 2026-08-16

- Recompile against the signed-endpoint routing view, preserving valid active
  member ports and rejecting malformed active endpoint fields through the
  shared strict interpreter.

## 0.4.0 — 2026-08-15

- Refuse generation-silent normal inventories after the noded-owned recovery
  floor has advanced above zero, matching noded's inventory fold while
  preserving generation-zero migration compatibility.
