# Teletypewriter FD handoff patch

Upstream: `https://github.com/raphamorim/rio`, revision
`932c1a7d9e07b4db5924f7a0dd689e823c3a1442`.
The workspace override retains two Unix hunks: the optional spawn FD mapping
API and the child-only `dup2`/source-close inside `pre_exec`.

The parent-side CLOEXEC-clear/spawn/reset alternative was inspected and rejected.
In `vendor/teletypewriter/src/unix/mod.rs`, `pre_exec` closes only the PTY
ends (lines 682–683), not every non-stdio FD. However, Term's Core mutex
(`apps/term/src/main.rs`) and Bus handler (`apps/term/src/bus.rs`) serialise
one TabSet only. `Terminal::start_session` and independently constructed
TabSets, including concurrent test fixtures, do not require that mutex.
There is no process-wide spawn lock covering an inheritable-FD window.
The child-only mapping preserves CLOEXEC in Term throughout all sibling spawns.

`vendor/teletypewriter/patch-record.json` records the exact upstream revision,
the full patched Unix file SHA-256 and both hunk SHA-256 values. Term's
`teletypewriter_patch_guard` tests check these plus the rio-vt/corcovado pins
and workspace override. Hunk hashes use trailing whitespace removal followed
by one LF; the whole-file hash is byte-exact. A revision or content change
requires a fresh upstream diff and an intentional recording update. Do not
update hashes merely to silence the guard.

Exit strategy: submit an upstream PR for an optional child-only FD mapping
spawn API, including seal/inheritance tests. Once an accepted upstream revision
provides equivalent semantics, pin rio-vt and teletypewriter together to it,
remove the local override and vendor copy, and run the production Term→Mix
proof plus sibling-inheritance tests. No upstream PR has been submitted by
this change; the patch remains until upstream support is verified.
