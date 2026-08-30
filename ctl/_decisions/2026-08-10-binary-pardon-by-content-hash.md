# Binary pardon by content hash

Date: 2026-08-10

## Decision

The public-hygiene gate may pardon a binary match only by the sha256 of the
complete git blob it scanned. Binary pardons live in the optional
`binary_exceptions` allowlist key, separate from text fingerprints. The gate
resolves the path beneath the immutable scan object, verifies that the result is
a blob, dumps those raw object bytes through a private temporary file, verifies
the byte count, and hashes the file with streaming sha256.

Any byte change produces a different digest and re-arms the gate. The working
file is not authoritative: `--tree` and `--range` scan committed objects, and
`--index` scans a frozen write-tree snapshot, so the pardon must identify those
same bytes.

## Context

`desktop/crates/cosmix-comp/assets/fonts/DejaVuSans.ttf` vendors DejaVu Sans for
compositor chrome typography. At byte offset 303580, glyph outline coordinate
data inside the `glyf` table happens to spell three bytes that read as a node name. The node-name
rule matches those bytes case-insensitively. The font was inspected by hand;
this is binary shape data, not mesh identity, and it cannot be sanitised without
changing the font.

## Rejected alternatives

- A path glob would exempt future, unknown contents at that path. That is the
  exact failure class behind the 2026-05-29 sanitisation regression.
- `--no-verify` bypasses the gate entirely, including every unrelated leak.
- Base64-encoding the font to dodge the scanner makes the gate worthless: an
  encoding trick must not turn guarded content into accepted content.
- Swapping to a font whose current bytes happen not to collide gives no
  verifiable glyph-coverage guarantee and relies on luck. The next font update
  simply re-rolls the collision dice.

## Residual

The pardon is keyed on content alone. Identical bytes at any path in any guarded
repo receive the same pardon. This is deliberate: identical content carries the
same inspected judgement, and the text fingerprint likewise hashes nothing
repo-identifying. It does mean the operator is judging the bytes, not their
location.

## Reversibility

Remove the `binary_exceptions` entry to re-arm the gate immediately. The schema
key is optional, so removing every binary pardon and then the key restores the
previous configuration shape without migration. The implementation can be
removed independently once no inspected binary requires it; no repository
object or public format is rewritten by this decision.
