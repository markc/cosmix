# Foreman vendor-stream captures

These JSONL fixtures pin the driver input at the existing vendor executable
seam. Each file contains one metadata record, raw stdout line records with the
wall-clock delay since the preceding line, and exactly one exit record. The
loader rejects truncated captures instead of inventing an exit status.

To capture a Claude run, build `foreman-stream-fixture`, then set:

```text
FOREMAN_CLAUDE_BIN=/path/to/foreman-stream-fixture
FOREMAN_CAPTURE_VENDOR_BIN=/path/to/claude
FOREMAN_CAPTURE_FIXTURE=/tmp/claude.stream.jsonl
FOREMAN_CAPTURE_LANE=claude
```

Use `FOREMAN_CODEX_BIN` and the Codex binary for that lane. The wrapper receives
the arguments Foreman built, tees vendor stdout unchanged to the real parser,
records and flushes each line as it arrives, then records the child's code or
signal. A SIGKILL of the wrapper itself cannot write a final exit record; that
capture is deliberately rejected as incomplete rather than assigned a guessed
status.

Before committing a capture, replace task text, paths, session identifiers,
tool data, keys, account data, and hostnames with inert fixture values. Keep the
event shapes, ordering, usage figures, line deltas, and exit disposition. The
six files here cover clean completion, runner output-token ceiling, and
mid-stream non-zero death for both stdout drivers.

## Open timing finding

The injected replay clock makes captured line deltas authoritative for runner
duration, event timestamps, wall-budget elapsed time, stall elapsed time, and
reaper deadlines. The subprocess pipe is still scheduled by the host kernel.
A fixture whose next line sits exactly on a stall/wall deadline can therefore
race the receiver timeout at that boundary. None of the committed fixtures
uses a deadline-edge timing path. Closing this completely needs a scheduled
line-delivery transport at the existing executable seam; broad timestamp
normalisation or weakening the stall/budget comparisons would hide the leak
and is deliberately not done here.
