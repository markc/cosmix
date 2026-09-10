# Owned editor core — stage C, round 1

This module is compiled but is not used by the REPL yet. The existing rustyline
editor remains active. There is no terminal I/O, input loop or evaluator access
in production code here; history interoperability tests alone use temporary files.

## Contracts

- `mod.rs`: owned prompt profiles and generation/revision-tagged control data.
  Prompt zero denotes startup. Mode effects carry a unique serial as well as
  the generation; only successful completion produces an acknowledgement.
  Shutdown also works before the first prompt. Restoration failure never yields
  a suspension permit. Input must deliver already-observed human activity first.
  Every observed editing action increments the admission revision, even a no-op;
  resize and suspend/resume leave it unchanged. The separate buffer revision
  tracks actual content/cursor changes.
- `buffer.rs`: UTF-8 byte cursors always lie on extended grapheme boundaries.
  Insertion/deletion resegments across neighbours. Words are alphanumeric runs;
  consecutive adjacent kills coalesce in direction order. Yank-pop requires an
  unchanged yank range and refuses to split newly merged graphemes. Each edit,
  paste or yank-pop is one undo unit. Cursor movement is not an undo unit.
  Text, undo snapshots and kill-ring retention have configurable bounds.
- `history.rs`: canonical rustyline 15 V2 encoding, legacy loading, trimmed
  submission, consecutive deduplication and 100-entry default retention. Search
  starts inclusively and does not wrap; match offsets denote the beginning of
  the match, including zero for prefix matches. Invalid escapes retain the raw
  record as rustyline does. Encoding an empty list returns just the V2 header;
  the later file owner decides whether to write it (rustyline skips empty saves).
- `render.rs`: visible cell runs and cursor/end positions, recomputed on resize.
  Escapes in prompts have zero width. Tabs use eight-cell stops; controls in
  input display as caret notation. Exact fit uses an explicit next-row cursor,
  not terminal pending-wrap state. A cluster wider than the whole terminal is
  represented by a one-cell replacement glyph. Width zero is rejected.

## Verification

Tests cover protocol transitions, stale generations/revisions/completions,
startup/shutdown and mode failures; combining characters, CJK, family ZWJ emoji,
insertion merges, empty edges, undo/redo branches, bounded rings, kill/yank and
random edit chains; V2 byte equality against an actual rustyline writer and
reader, legacy/CRLF/malformed records, deduplication and Unicode search; ANSI,
OSC, long prompts, width one, exact fit, wide-character prewrap, controls and
resize. The sanitized V2 fixture is independently checked against rustyline.

## Later integration

Input decoding, paste collection, search interaction, completion snapshots and
revision-tagged results, human-line completion, tty mode guards, control-FD
wakeup and screen output are subsequent work. The input layer must implement a
terminal-restored human-line return seam; `consume_reservation` handles only
remote admission. Restricted command dispatch must enforce the fixed profile's
allowlist without reaching a parked evaluator. File permissions, locking and
atomic history replacement also belong to that I/O layer. No real-PTY parity
or full stage-C completion is claimed by these pure tests.
